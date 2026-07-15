//! FM discriminator + squelch + audio conditioning, one instance per channel.
//!
//! The core is ka9q `fm.c`'s product detector: `phase = arg(s[n]·conj(s[n-1]))`,
//! the instantaneous frequency, which is **amplitude-invariant** (so the
//! channelizer's absolute gain is irrelevant to the recovered audio). Around it:
//!
//! - **Squelch.** Without a carrier the discriminator emits full-scale random
//!   phase (±π) — noise that pins downstream level meters and needlessly churns the
//!   AFSK slicer bank. An SNR-based squelch (channel power vs. a slowly-tracked
//!   noise floor, with hysteresis) mutes the audio between packets, exactly as ka9q
//!   does. Thresholds are deliberately generous so weak signals the slicer bank can
//!   still rescue keep the squelch open (per the "emit everything" design).
//! - **Signal metric.** The same power/noise-floor machinery yields a
//!   scale-invariant [`SignalMetrics`] (SNR in dB above the floor, plus a relative
//!   RSSI) attached to every block and carried to the decoded packet's metadata.
//! - **Conditioning.** DC blocker (removes residual carrier offset), optional
//!   de-emphasis, and output gain, mirroring ka9q's chain.

use aprs_modem::SignalMetrics;
use num_complex::Complex;

/// De-emphasis time constant used by ka9q for NBFM (`modes.c`).
const DEEMPHASIS_TC: f32 = 530.5e-6;

/// Audio + per-block signal quality returned by [`FmDemod::process`].
pub struct FmBlock {
    pub audio: Vec<f32>,
    pub signal: SignalMetrics,
}

pub struct FmDemodConfig {
    pub sample_rate: u32,
    /// FM deviation (Hz) that maps to full-scale (±1.0) audio: the output gain is
    /// derived as `fs / (2π · full_scale_dev_hz)`, so a larger value means a lower
    /// level. This only scales the reported `rec`/`mark`/`space` level metric — the
    /// AFSK demod has its own AGC — so tune it to land `rec` in a useful range.
    pub full_scale_dev_hz: f32,
    /// Enable ka9q-style de-emphasis (default off for the spike).
    pub deemphasis: bool,
    /// Squelch opens when the in-channel SNR rises above this many dB.
    pub squelch_open_db: f32,
    /// Squelch closes when SNR falls below this (hysteresis; keep < open).
    pub squelch_close_db: f32,
}

impl FmDemodConfig {
    /// Canonical defaults — chosen so the app runs well without tuning; override
    /// individual fields only for edge cases.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            // 20 kHz maps to full scale — lands `rec` in direwolf's ~30–70 range for
            // typical signals and keeps strong ones off the rail.
            full_scale_dev_hz: 20_000.0,
            deemphasis: false,
            // Generous: kill clear no-carrier noise while still opening on weak
            // carriers (a few dB above the floor). Tighten for cleaner metadata at the
            // cost of the very weakest catches.
            squelch_open_db: 3.0,
            squelch_close_db: 1.5,
        }
    }
}

pub struct FmDemod {
    prev: Complex<f32>,
    // One-pole DC blocker: y[n] = x[n] - x[n-1] + r·y[n-1].
    dc_r: f32,
    dc_x1: f32,
    dc_y1: f32,
    // One-pole de-emphasis low-pass.
    deemph_on: bool,
    deemph_a: f32,
    deemph_y1: f32,
    gain: f32,
    // Squelch / SNR state (linear power).
    fast_pow: f32,    // fast-tracked channel power envelope
    noise_floor: f32, // tracked noise floor
    a_fast: f32,      // envelope smoothing coefficient
    a_track: f32,     // floor tracking while squelched closed (learns the noise level)
    a_up: f32,        // floor slow-rise while open (can't be pulled down by a carrier)
    open_lin: f32,    // open threshold as a linear power ratio
    close_lin: f32,   // close threshold as a linear power ratio
    open: bool,
    // Warmup: accumulate the initial power to seed the floor at the true noise mean
    // rather than a single high-variance sample.
    warmup: u32,
    warmup_sum: f32,
    warmup_cnt: u32,
}

impl FmDemod {
    pub fn new(cfg: FmDemodConfig) -> Self {
        let dt = 1.0 / cfg.sample_rate as f32;
        let deemph_a = dt / (DEEMPHASIS_TC + dt);
        let fs = cfg.sample_rate as f32;
        // Map the configured full-scale deviation to output gain.
        let gain = fs / (2.0 * std::f32::consts::PI * cfg.full_scale_dev_hz);
        Self {
            prev: Complex::new(0.0, 0.0),
            dc_r: 0.995, // highpass corner ~19 Hz at 24 kHz
            dc_x1: 0.0,
            dc_y1: 0.0,
            deemph_on: cfg.deemphasis,
            deemph_a,
            deemph_y1: 0.0,
            gain,
            fast_pow: 0.0,
            noise_floor: 0.0,
            // ~10 ms power envelope (smooth enough that noise fluctuation doesn't trip
            // the squelch). While closed, the floor tracks the noise level over ~200 ms.
            // While open it may only rise, and very slowly (~60 s) — effectively frozen
            // for the ~0.5–1 s of a packet, so the carrier neither drags it down nor
            // inflates it (which would compress the reported SNR). It resumes tracking
            // as soon as the squelch closes after the packet.
            a_fast: 1.0 / (0.010 * fs),
            a_track: 1.0 / (0.200 * fs),
            a_up: 1.0 / (60.0 * fs),
            open_lin: 10f32.powf(cfg.squelch_open_db / 10.0),
            close_lin: 10f32.powf(cfg.squelch_close_db / 10.0),
            open: false,
            warmup: (0.050 * fs) as u32, // 50 ms to seed the floor
            warmup_sum: 0.0,
            warmup_cnt: 0,
        }
    }

    /// Demodulate a block of complex baseband into real audio + signal metrics.
    ///
    /// The reported `SignalMetrics` are the peak in-channel SNR / power over the
    /// block — representative of a packet's carrier when one is present.
    pub fn process(&mut self, iq: &[Complex<f32>]) -> FmBlock {
        let mut audio = Vec::with_capacity(iq.len());
        let mut peak_snr = 0.0f32;
        let mut peak_pow = 0.0f32;

        for &x in iq {
            // In-channel power for squelch / SNR.
            let p = x.norm_sqr();

            // Product detector runs every sample (keep `prev` continuous), even
            // during warmup / when squelched.
            let d = x * self.prev.conj();
            self.prev = x;
            let mut s = d.im.atan2(d.re);

            // Warmup: seed the floor from the mean of the first ~50 ms, muting output.
            if self.warmup > 0 {
                self.warmup_sum += p;
                self.warmup_cnt += 1;
                self.warmup -= 1;
                if self.warmup == 0 {
                    let mean = (self.warmup_sum / self.warmup_cnt as f32).max(1e-12);
                    self.fast_pow = mean;
                    self.noise_floor = mean;
                }
                audio.push(0.0);
                continue;
            }

            self.fast_pow += self.a_fast * (p - self.fast_pow);
            // Floor: while closed, track the noise level (both directions, ~200 ms) so
            // the baseline SNR stays ~0 dB; while open, only rise (~2 s) so a carrier
            // can't drag it down and a long packet can't inflate it enough to close.
            if !self.open {
                self.noise_floor += self.a_track * (self.fast_pow - self.noise_floor);
            } else if self.fast_pow > self.noise_floor {
                self.noise_floor += self.a_up * (self.fast_pow - self.noise_floor);
            }
            let snr = self.fast_pow / self.noise_floor.max(1e-12);
            if snr > peak_snr {
                peak_snr = snr;
            }
            if self.fast_pow > peak_pow {
                peak_pow = self.fast_pow;
            }

            // Hysteretic squelch.
            if self.open {
                if snr < self.close_lin {
                    self.open = false;
                }
            } else if snr > self.open_lin {
                self.open = true;
            }

            // DC block (remove residual carrier/tuning offset).
            let y = s - self.dc_x1 + self.dc_r * self.dc_y1;
            self.dc_x1 = s;
            self.dc_y1 = y;
            s = y;

            // Optional de-emphasis low-pass.
            if self.deemph_on {
                self.deemph_y1 += self.deemph_a * (s - self.deemph_y1);
                s = self.deemph_y1;
            }

            // Mute when squelched: silence, not noise phase.
            audio.push(if self.open { s * self.gain } else { 0.0 });
        }

        let signal = SignalMetrics {
            snr_db: 10.0 * peak_snr.max(1e-12).log10(),
            rssi_dbfs: 10.0 * peak_pow.max(1e-12).log10(),
        };
        FmBlock { audio, signal }
    }
}
