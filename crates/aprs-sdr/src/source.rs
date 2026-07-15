//! Library entry point: assemble the full RTL-SDR → channelize → FM demod →
//! decode pipeline and hand back a stream of decoded [`AprsPacket`]s, so a
//! producer (e.g. `aprs-streamd`) can embed the SDR as a source. This is the
//! reusable analog of the binary's `main`.
//!
//! [`spawn`] must be called from within a multi-threaded Tokio runtime (the decode
//! stage uses `tokio::spawn` / `spawn_blocking`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aprs_modem::{decode_audio_stream, AprsPacket, AudioBlock, DecoderConfig};
use tokio::sync::mpsc;

use crate::channelize::{Channelizer, AUDIO_RATE};
use crate::device::{bytes_to_iq, Gain, RtlSdrSource, SdrConfig, READ_BYTES};
use crate::fm::{FmDemod, FmDemodConfig};

/// FM front-end tuning. Each `Option` overrides the corresponding
/// [`FmDemodConfig`] default only when `Some`; `deemphasis` toggles ka9q-style
/// de-emphasis (off by default).
#[derive(Debug, Clone, Default)]
pub struct FmParams {
    pub full_scale_dev_hz: Option<f32>,
    pub squelch_open_db: Option<f32>,
    pub squelch_close_db: Option<f32>,
    pub deemphasis: bool,
}

/// Everything needed to run an SDR source.
#[derive(Debug, Clone)]
pub struct SdrSourceConfig {
    pub device_index: usize,
    /// Tuner centre frequency, Hz.
    pub center_freq: u32,
    /// Complex sample rate, Hz.
    pub sample_rate: u32,
    pub gain: Gain,
    /// Noise-floor setpoint (dBFS) for [`Gain::Auto`]. Ignored otherwise.
    ///
    /// The tuner gain is stepped to hold the measured floor here. Higher (e.g.
    /// -20) means more gain — better noise figure and weak-signal sensitivity, but
    /// less ADC headroom before strong signals clip. Lower (e.g. -35) is the
    /// opposite. The RTL's 8-bit ADC only has ~48 dB of range, so this is the
    /// trade. Measure with the `front-end level` log line before changing it.
    pub auto_target_floor_dbfs: f32,
    pub freq_correction_ppm: i32,
    /// Channel centre frequencies, Hz. Each becomes `ssrc = freq_kHz`.
    pub channels: Vec<u32>,
    pub fm: FmParams,
    pub decoder: DecoderConfig,
}

/// Default noise-floor setpoint for [`Gain::Auto`].
///
/// **This must be calibrated per site — `Gain::Auto` is experimental.** The
/// floor-to-gain mapping is only stable when the captured span contains nothing
/// but noise between bursts. [`LevelStats::floor_dbfs`] rejects *intermittent*
/// signals (a low percentile lands on idle reads) but cannot reject a
/// **continuous** in-band carrier — a permanent pager/repeater/spur anywhere in
/// the span raises every percentile, so the "floor" becomes carrier+noise and the
/// setpoint no longer corresponds to a fixed gain. Measured floor-vs-gain has
/// drifted several dB between runs for that reason.
///
/// Prefer a fixed [`Gain::Manual`] chosen from observed catch rate for production;
/// use `Auto` where conditions genuinely move (e.g. a balloon flight).
pub const DEFAULT_AUTO_FLOOR_DBFS: f32 = -25.0;

impl SdrSourceConfig {
    /// Reject a configuration that can't work before opening the device. Channels
    /// beyond ±Fs/2 would alias to a bogus frequency, so they're a hard error;
    /// channels past ~80% of the usable band get a warning (attenuated edge).
    pub fn validate(&self) -> Result<(), String> {
        if self.channels.is_empty() {
            return Err("no channels configured".into());
        }
        let nyquist = self.sample_rate as f64 / 2.0;
        let usable = self.sample_rate as f64 * 0.4; // 80% of ±Fs/2
        for &ch in &self.channels {
            let offset = (ch as f64 - self.center_freq as f64).abs();
            if offset >= nyquist {
                return Err(format!(
                    "channel {ch} Hz is {offset:.0} Hz from centre {} Hz — beyond ±Fs/2 ({nyquist:.0} Hz); it would alias. Raise the sample rate or move the centre.",
                    self.center_freq
                ));
            }
            if offset > usable {
                tracing::warn!(
                    "channel {ch} Hz is {offset:.0} Hz from centre — within Nyquist but past ~80% of the usable band; expect attenuation"
                );
            }
        }
        Ok(())
    }
}

/// How often the reader emits the `status:` line (decode rate + RF conditions).
///
/// 60 s keeps a multi-day run's log manageable (~1.4k lines/day) and makes the
/// `frames` count a meaningful rate on its own rather than a 0-or-2 coin flip.
/// It also bounds how often the [`GainManager`] can act, since it is consulted
/// once per report.
const LEVEL_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Front-end level statistics over a reporting window — the numbers you need to
/// pick a gain, and the inputs a future gain manager would close its loop on.
///
/// The RTL-SDR's ADC is only 8-bit (~48 dB of range), so gain setting is a
/// balance: enough that the band noise lifts clear of the ADC's own quantization
/// floor (below that you lose weak signals), but not so much that strong signals
/// rail the converter (which makes intermod spurs across the band).
///
/// - `floor_dbfs` — the **noise floor**: a low percentile of per-read power, so
///   transmissions (which are narrow and sparse) don't poison it. A plain mean is
///   useless here — one packet in the window inflates it by tens of dB. This is
///   the classic "raise gain until the noise floor lifts off the ADC's own
///   quantization floor" target, and the control input for automatic gain.
/// - `mean_dbfs` — average wideband power including signals (floor + activity).
/// - `peak_dbfs` — loudest sample in the window: the headroom indicator.
/// - `clipped`   — samples pegged at the 0/255 rails. Persistently non-zero means
///   the front end is overloading; back the gain off.
#[derive(Default)]
struct LevelStats {
    /// Normalized mean complex power per read buffer (~11–20 ms each). Kept so the
    /// floor can be taken as a percentile rather than a signal-polluted mean.
    block_power: Vec<f64>,
    bytes: u64,
    clipped: u64,
    /// Largest normalized |I| or |Q| seen.
    peak: f32,
}

impl LevelStats {
    fn accumulate(&mut self, buf: &[u8]) {
        // RTL-SDR delivers offset-binary u8; 127.4 is the zero level, so ±127.4
        // is full scale.
        const CENTER: f32 = 127.4;
        let mut sum_power = 0.0f64;
        let mut pairs = 0u64;
        for pair in buf.chunks_exact(2) {
            let i = (pair[0] as f32 - CENTER) / CENTER;
            let q = (pair[1] as f32 - CENTER) / CENTER;
            sum_power += (i * i + q * q) as f64;
            let m = i.abs().max(q.abs());
            if m > self.peak {
                self.peak = m;
            }
            if pair[0] == 0 || pair[0] == 255 {
                self.clipped += 1;
            }
            if pair[1] == 0 || pair[1] == 255 {
                self.clipped += 1;
            }
            pairs += 1;
        }
        self.bytes += buf.len() as u64;
        if pairs > 0 {
            self.block_power.push(sum_power / pairs as f64);
        }
    }

    /// Noise floor: the `pct`-th percentile of per-read power. With APRS traffic
    /// idle most of the time, the low percentile lands on quiet reads.
    fn percentile_dbfs(&self, pct: f64) -> f32 {
        if self.block_power.is_empty() {
            return f32::NEG_INFINITY;
        }
        let mut v = self.block_power.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (((v.len() - 1) as f64) * pct / 100.0).round() as usize;
        10.0 * v[idx].max(1e-12).log10() as f32
    }

    fn floor_dbfs(&self) -> f32 {
        self.percentile_dbfs(10.0)
    }

    fn mean_dbfs(&self) -> f32 {
        if self.block_power.is_empty() {
            return f32::NEG_INFINITY;
        }
        let mean = self.block_power.iter().sum::<f64>() / self.block_power.len() as f64;
        10.0 * mean.max(1e-12).log10() as f32
    }

    fn peak_dbfs(&self) -> f32 {
        20.0 * self.peak.max(1e-6).log10()
    }

    fn clip_pct(&self) -> f32 {
        if self.bytes == 0 {
            return 0.0;
        }
        100.0 * self.clipped as f32 / self.bytes as f32
    }
}

/// Software gain manager for [`Gain::Auto`].
///
/// Holds the measured **noise floor** at a setpoint by stepping the *fixed* tuner
/// gain through the tuner's discrete table. Keying off the noise floor (a low
/// percentile, see [`LevelStats`]) rather than signal peaks is the whole point:
/// a single strong transmission must not ratchet the gain down and desensitize
/// every channel for everything that follows — which is exactly what the tuner's
/// hardware AGC does wrong.
///
/// Deliberately slow and hysteretic: the band noise floor drifts over minutes, so
/// there is no reason to react faster, and every change briefly disturbs the
/// stream (I²C register writes) and re-baselines the per-channel SNR estimate.
/// Sustained clipping is a safety backstop that can force a step down.
struct GainManager {
    /// Supported gains, tenths dB, ascending.
    table: Vec<i32>,
    /// Index into `table` of the current gain.
    idx: usize,
    /// Noise-floor setpoint, dBFS.
    target_dbfs: f32,
    /// Deadband around the setpoint — no change while inside it.
    tolerance_db: f32,
    last_change: Instant,
}

/// Don't step more often than this: the floor moves over minutes, not seconds.
const GAIN_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// Clipping above this share of samples forces a step down regardless of floor.
const GAIN_CLIP_LIMIT_PCT: f32 = 0.01;

impl GainManager {
    fn new(table: Vec<i32>, start_tenths: i32, target_dbfs: f32) -> Option<Self> {
        if table.is_empty() {
            return None;
        }
        // Start at the table entry nearest the configured starting gain.
        let idx = table
            .iter()
            .enumerate()
            .min_by_key(|(_, g)| (*g - start_tenths).abs())
            .map(|(i, _)| i)
            .unwrap_or(0);
        Some(Self {
            table,
            idx,
            target_dbfs,
            tolerance_db: 2.0,
            last_change: Instant::now(),
        })
    }

    /// Decide a new gain from this window's stats, or `None` to hold.
    /// `floor_dbfs` is the signal-immune noise floor; `clip_pct` the safety input.
    fn decide(&mut self, floor_dbfs: f32, clip_pct: f32) -> Option<i32> {
        if self.last_change.elapsed() < GAIN_MIN_INTERVAL {
            return None;
        }
        // Safety backstop first: real clipping means back off now.
        let step_down = clip_pct > GAIN_CLIP_LIMIT_PCT
            || floor_dbfs > self.target_dbfs + self.tolerance_db;
        let step_up = !step_down && floor_dbfs < self.target_dbfs - self.tolerance_db;

        let next = if step_down && self.idx > 0 {
            self.idx - 1
        } else if step_up && self.idx + 1 < self.table.len() {
            self.idx + 1
        } else {
            return None; // in the deadband, or already at a rail of the table
        };
        self.idx = next;
        self.last_change = Instant::now();
        Some(self.table[next])
    }
}

/// The reader + DSP threads behind a running source; join to shut down cleanly.
pub struct SdrHandles {
    reader: JoinHandle<()>,
    dsp: JoinHandle<()>,
}

impl SdrHandles {
    /// Wait for both threads to finish (after the shared shutdown flag is set and
    /// the device has been released).
    pub fn join(self) {
        let _ = self.dsp.join();
        let _ = self.reader.join();
    }
}

/// Start the pipeline. Returns the decoded-packet stream, thread handles, and a
/// decoded-frame counter.
///
/// The caller should bump the counter for each packet it takes off the stream; the
/// reader reads-and-resets it every reporting window and folds the count into the
/// same `status:` line as the RF measurements. That keeps catch rate and RF
/// conditions on one timestamped line, so a dip in one can be read against the
/// other without correlating two logs.
///
/// `shutdown` is polled by the reader between reads; set it (e.g. from a signal
/// handler) to stop — the reader then releases the dongle and the pipeline winds
/// down, closing the returned receiver. Must run inside a Tokio runtime.
pub fn spawn(
    config: SdrSourceConfig,
    shutdown: Arc<AtomicBool>,
) -> (mpsc::Receiver<AprsPacket>, SdrHandles, Arc<AtomicU64>) {
    let (audio_tx, audio_rx) = mpsc::channel::<AudioBlock>(64);
    let packets = decode_audio_stream(config.decoder.clone(), audio_rx);

    // RTL-SDR `read_sync` doesn't keep the USB pipe filled between calls and the
    // device FIFO is tiny, so any gap between reads (e.g. channelizing N channels)
    // loses samples. The reader does nothing but read back-to-back and drop buffers
    // here; the DSP thread consumes at its own pace.
    const RAW_QUEUE: usize = 32; // ~640 ms slack at 48 KB / 20 ms per buffer
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(RAW_QUEUE);

    // Reader thread — open the device and read as fast as USB allows.
    let dev = SdrConfig {
        device_index: config.device_index,
        center_freq: config.center_freq,
        sample_rate: config.sample_rate,
        gain: config.gain,
        freq_correction_ppm: config.freq_correction_ppm,
    };
    let n_channels = config.channels.len();
    let auto_floor = config.auto_target_floor_dbfs;
    // Decoded-frame counter: bumped by the consumer, drained by the reader into
    // its periodic status line.
    let frames = Arc::new(AtomicU64::new(0));
    let reader_frames = frames.clone();
    let reader = std::thread::spawn(move || {
        let mut sdr = match RtlSdrSource::open(&dev) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to open RTL-SDR: {e:?}");
                return;
            }
        };
        tracing::info!(
            device_index = dev.device_index,
            center_freq = dev.center_freq,
            sample_rate = dev.sample_rate,
            "RTL-SDR open; {} channel(s)",
            n_channels
        );

        // Software gain manager, only for Gain::Auto.
        let mut gain_mgr = match dev.gain {
            Gain::Auto => match sdr.tuner_gains() {
                Ok(table) => {
                    let mgr = GainManager::new(table, crate::device::AUTO_START_TENTHS, auto_floor);
                    if let Some(m) = &mgr {
                        tracing::info!(
                            "auto gain: {} steps ({}..{} tenths dB), targeting floor {:.1} dBFS",
                            m.table.len(),
                            m.table.first().copied().unwrap_or(0),
                            m.table.last().copied().unwrap_or(0),
                            auto_floor,
                        );
                    }
                    mgr
                }
                Err(e) => {
                    tracing::warn!("auto gain unavailable (no gain table: {e:?}); holding fixed");
                    None
                }
            },
            _ => None,
        };

        let mut dropped: u64 = 0;
        let mut level = LevelStats::default();
        let mut last_report = Instant::now();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let mut buf = vec![0u8; READ_BYTES];
            let n = match sdr.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!("RTL-SDR read error: {e:?}");
                    break;
                }
            };
            buf.truncate(n);

            // Front-end level/clipping instrumentation (measured on the raw ADC
            // bytes, before any DSP touches them).
            level.accumulate(&buf);
            if last_report.elapsed() >= LEVEL_REPORT_INTERVAL {
                // One line with decode rate and RF conditions together, so a dip in
                // catch rate can be read straight against the floor/clipping that
                // caused it. The raw clipped count matters as well as the
                // percentage: a handful of railed samples out of tens of millions
                // rounds to 0.0000% but still means the front end touched the rails.
                tracing::info!(
                    "status: frames={}  floor={:.1} dBFS  mean={:.1} dBFS  peak={:.1} dBFS  clipped={} ({:.4}%)",
                    reader_frames.swap(0, Ordering::Relaxed),
                    level.floor_dbfs(),
                    level.mean_dbfs(),
                    level.peak_dbfs(),
                    level.clipped,
                    level.clip_pct(),
                );

                // Automatic gain: hold the noise floor at the setpoint.
                if let Some(mgr) = &mut gain_mgr {
                    if let Some(tenths) = mgr.decide(level.floor_dbfs(), level.clip_pct()) {
                        match sdr.set_gain_tenths(tenths) {
                            Ok(()) => tracing::info!(
                                "auto gain -> {:.1} dB (floor {:.1} dBFS, target {:.1})",
                                tenths as f32 / 10.0,
                                level.floor_dbfs(),
                                auto_floor,
                            ),
                            Err(e) => tracing::warn!("failed to set gain {tenths}: {e:?}"),
                        }
                    }
                }

                level = LevelStats::default();
                last_report = Instant::now();
            }

            match raw_tx.try_send(buf) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    dropped += 1;
                    if dropped % 50 == 1 {
                        tracing::warn!("DSP behind; dropping raw buffers (total {dropped})");
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
        if let Err(e) = sdr.close() {
            tracing::warn!("error closing RTL-SDR: {e:?}");
        } else {
            tracing::info!("RTL-SDR closed");
        }
    });

    // DSP thread — channelize each raw buffer and FM-demodulate every channel.
    let dsp = std::thread::spawn(move || {
        let mut channelizer = Channelizer::new(config.sample_rate as f64);
        let mut demods: HashMap<u32, FmDemod> = HashMap::new();
        for &ch in &config.channels {
            let ssrc = ch / 1000; // freq_kHz
            let offset = ch as f64 - config.center_freq as f64;
            channelizer.add_channel(ssrc, offset);
            let mut fm_cfg = FmDemodConfig::new(AUDIO_RATE);
            if let Some(v) = config.fm.full_scale_dev_hz {
                fm_cfg.full_scale_dev_hz = v;
            }
            if let Some(v) = config.fm.squelch_open_db {
                fm_cfg.squelch_open_db = v;
            }
            if let Some(v) = config.fm.squelch_close_db {
                fm_cfg.squelch_close_db = v;
            }
            fm_cfg.deemphasis = config.fm.deemphasis;
            demods.insert(ssrc, FmDemod::new(fm_cfg));
            tracing::info!(ssrc, offset_hz = offset, "channel ready");
        }

        let mut iq = Vec::with_capacity(READ_BYTES / 2);
        'dsp: while let Ok(buf) = raw_rx.recv() {
            iq.clear();
            bytes_to_iq(&buf, &mut iq);
            for block in channelizer.process(&iq) {
                let demod = demods.get_mut(&block.ssrc).expect("demod for ssrc");
                let fm = demod.process(&block.samples);
                let ab = AudioBlock {
                    ssrc: block.ssrc,
                    sample_rate: AUDIO_RATE,
                    samples: fm.audio,
                    signal: Some(fm.signal),
                };
                if audio_tx.blocking_send(ab).is_err() {
                    break 'dsp; // consumer gone
                }
            }
        }
    });

    (packets, SdrHandles { reader, dsp }, frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager on a simple table, with the rate limit already satisfied.
    fn mgr(start: i32) -> GainManager {
        let mut m = GainManager::new(vec![0, 100, 200, 300, 400, 496], start, -25.0).unwrap();
        m.last_change = Instant::now() - GAIN_MIN_INTERVAL - Duration::from_secs(1);
        m
    }

    #[test]
    fn starts_at_nearest_table_entry() {
        // 300 is in the table; 250 should snap to the nearest (200 or 300).
        assert_eq!(mgr(300).table[mgr(300).idx], 300);
        let m = mgr(260);
        assert_eq!(m.table[m.idx], 300);
    }

    #[test]
    fn holds_inside_the_deadband() {
        // Floor exactly on target: no change.
        assert_eq!(mgr(300).decide(-25.0, 0.0), None);
        // Within tolerance either side: still no change.
        assert_eq!(mgr(300).decide(-26.5, 0.0), None);
        assert_eq!(mgr(300).decide(-23.5, 0.0), None);
    }

    #[test]
    fn floor_too_high_steps_down_too_low_steps_up() {
        // Floor well above target (too much gain) -> step down.
        assert_eq!(mgr(300).decide(-15.0, 0.0), Some(200));
        // Floor well below target (too little gain) -> step up.
        assert_eq!(mgr(300).decide(-40.0, 0.0), Some(400));
    }

    #[test]
    fn clipping_forces_a_step_down_even_when_floor_looks_fine() {
        // Floor is on target, but the front end is railing: back off anyway.
        let mut m = mgr(300);
        assert_eq!(m.decide(-25.0, GAIN_CLIP_LIMIT_PCT * 10.0), Some(200));
    }

    #[test]
    fn rate_limited_between_changes() {
        let mut m = mgr(300);
        assert_eq!(m.decide(-15.0, 0.0), Some(200)); // first step allowed
        assert_eq!(m.decide(-15.0, 0.0), None); // immediate re-step suppressed
    }

    #[test]
    fn clamps_at_the_ends_of_the_table() {
        // At the bottom rung, a step-down request can't go lower.
        let mut low = mgr(0);
        assert_eq!(low.decide(-5.0, 1.0), None);
        // At the top rung, a step-up request can't go higher.
        let mut high = mgr(496);
        assert_eq!(high.decide(-50.0, 0.0), None);
    }
}
