//! `aprs-sdr` spike binary: RTL-SDR → channelize → FM demod → decode → print.
//!
//! Configuration is via environment variables (a real config file is future work).
//! Each has a sensible default in code; set a var only to override for an edge case:
//!   APRS_SDR_DEVICE           dongle index                  (default 0)
//!   APRS_SDR_CENTER_HZ        tuner centre frequency, Hz     (default 144_590_000)
//!   APRS_SDR_CHANNELS_HZ      comma-separated channel freqs  (default 144_390_000)
//!   APRS_SDR_SAMPLE_RATE      complex sample rate, Hz        (default 1_200_000)
//!   APRS_SDR_GAIN_TENTHS      manual gain (tenths dB); unset = hardware AGC
//!   APRS_SDR_PPM              frequency correction, ppm      (default 0)
//!   APRS_SDR_FM_MAXDEV_HZ     FM deviation mapped to full-scale audio (level knob)
//!   APRS_SDR_SQUELCH_OPEN_DB  squelch open threshold, dB SNR
//!   APRS_SDR_SQUELCH_CLOSE_DB squelch close threshold, dB SNR
//! The last three default to `FmDemodConfig::new` and only override when set.
//!
//! The centre is deliberately offset from the channel(s) so no channel sits on the
//! RTL-SDR DC spike. Each channel's `ssrc = freq_kHz` (ka9q convention).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aprs_modem::{decode_audio_stream, AudioBlock, DecoderConfig};
use aprs_sdr::channelize::{Channelizer, AUDIO_RATE};
use aprs_sdr::device::{bytes_to_iq, RtlSdrSource, SdrConfig, READ_BYTES};
use aprs_sdr::fm::{FmDemod, FmDemodConfig};
use tracing_subscriber::EnvFilter;

/// Resolve when the process should shut down: Ctrl-C (SIGINT) or SIGTERM (systemd).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// An optional f32 override — `None` unless the var is set to a parseable value.
/// Lets the canonical defaults in `FmDemodConfig::new` stand unless overridden.
fn env_opt_f32(key: &str) -> Option<f32> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let device_index = env_u32("APRS_SDR_DEVICE", 0) as usize;
    let center_freq = env_u32("APRS_SDR_CENTER_HZ", 144_590_000);
    let sample_rate = env_u32("APRS_SDR_SAMPLE_RATE", 1_200_000);
    let freq_correction_ppm = env_u32("APRS_SDR_PPM", 0) as i32;
    let gain_tenths_db = std::env::var("APRS_SDR_GAIN_TENTHS").ok().and_then(|v| v.parse().ok());
    let channels: Vec<u32> = std::env::var("APRS_SDR_CHANNELS_HZ")
        .unwrap_or_else(|_| "144390000".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if channels.is_empty() {
        tracing::error!("no valid channels in APRS_SDR_CHANNELS_HZ");
        std::process::exit(1);
    }
    for &ch in &channels {
        let offset = ch as i64 - center_freq as i64;
        if offset.unsigned_abs() > (sample_rate as u64 * 8 / 20) {
            tracing::warn!(
                "channel {} Hz is {} Hz from centre — near/outside usable bandwidth (~80% of {} Hz)",
                ch, offset, sample_rate
            );
        }
    }

    // Decode pipeline (async). Channels feed it AudioBlocks; it yields packets.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<AudioBlock>(64);
    let mut packets = rt.block_on(async { decode_audio_stream(DecoderConfig::default(), audio_rx) });

    // Shutdown flag: a signal watcher sets it; the DSP loop checks it between reads
    // and exits cleanly so the device is closed rather than killed mid-transfer.
    let shutdown = Arc::new(AtomicBool::new(false));
    let sig_shutdown = shutdown.clone();
    rt.spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown signal received; stopping");
        sig_shutdown.store(true, Ordering::Relaxed);
    });

    // Raw-I/Q hand-off from the reader thread to the DSP thread. RTL-SDR
    // `read_sync` doesn't keep the USB pipe filled between calls and the device's
    // hardware FIFO is tiny, so ANY gap between reads (e.g. time spent channelizing
    // N channels) loses samples and corrupts every channel. The dedicated reader
    // below does nothing but read back-to-back and drop buffers here, keeping the
    // pipe drained; the DSP thread consumes from the queue at its own pace.
    const RAW_QUEUE: usize = 32; // ~640 ms of slack at 48 KB / 20 ms per buffer
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(RAW_QUEUE);

    // Reader thread: open the device and read raw I/Q as fast as the USB allows.
    let reader_shutdown = shutdown.clone();
    let n_channels = channels.len();
    let reader = std::thread::spawn(move || {
        let cfg = SdrConfig {
            device_index,
            center_freq,
            sample_rate,
            gain_tenths_db,
            freq_correction_ppm,
        };
        let sdr = match RtlSdrSource::open(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to open RTL-SDR: {e:?}");
                return;
            }
        };
        tracing::info!(
            device_index, center_freq, sample_rate,
            "RTL-SDR open; {} channel(s)", n_channels
        );

        let mut dropped: u64 = 0;
        loop {
            if reader_shutdown.load(Ordering::Relaxed) {
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
            match raw_tx.try_send(buf) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    // DSP is behind — drop this buffer rather than stall the reader
                    // (which would then lose samples at the USB level instead).
                    dropped += 1;
                    if dropped % 50 == 1 {
                        tracing::warn!("DSP behind; dropping raw buffers (total {dropped})");
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
        // Clean shutdown: release the dongle so it isn't left mid-transfer.
        if let Err(e) = sdr.close() {
            tracing::warn!("error closing RTL-SDR: {e:?}");
        } else {
            tracing::info!("RTL-SDR closed");
        }
    });

    // DSP thread: channelize each raw buffer and FM-demodulate every channel.
    let dsp = std::thread::spawn(move || {
        // Optional FM/squelch overrides — applied on top of FmDemodConfig defaults.
        let fm_maxdev_hz = env_opt_f32("APRS_SDR_FM_MAXDEV_HZ");
        let squelch_open_db = env_opt_f32("APRS_SDR_SQUELCH_OPEN_DB");
        let squelch_close_db = env_opt_f32("APRS_SDR_SQUELCH_CLOSE_DB");
        let mut channelizer = Channelizer::new(sample_rate as f64);
        let mut demods: HashMap<u32, FmDemod> = HashMap::new();
        for &ch in &channels {
            let ssrc = ch / 1000; // freq_kHz
            let offset = ch as f64 - center_freq as f64;
            channelizer.add_channel(ssrc, offset);
            let mut fm_cfg = FmDemodConfig::new(AUDIO_RATE);
            if let Some(v) = fm_maxdev_hz {
                fm_cfg.full_scale_dev_hz = v;
            }
            if let Some(v) = squelch_open_db {
                fm_cfg.squelch_open_db = v;
            }
            if let Some(v) = squelch_close_db {
                fm_cfg.squelch_close_db = v;
            }
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

    // Print decoded packets until the pipeline closes.
    rt.block_on(async {
        while let Some(pkt) = packets.recv().await {
            let snr = pkt
                .signal
                .map(|s| format!("{:>4.1}dB", s.snr_db))
                .unwrap_or_else(|| "  -  ".to_string());
            println!(
                "{:.3} MHz  hits={:<2} snr={} rec={:<3}  {}",
                pkt.freq_mhz, pkt.slicer_hits, snr, pkt.audio_level.rec, pkt.text
            );
        }
    });

    let _ = dsp.join();
    let _ = reader.join();
    tracing::info!("exiting");
}
