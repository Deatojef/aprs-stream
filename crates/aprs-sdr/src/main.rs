//! `aprs-sdr` spike binary: RTL-SDR → channelize → FM demod → decode → print.
//!
//! Configuration is via environment variables (a real config file lives in
//! `aprs-streamd`). Each has a sensible default; set a var only to override:
//!   APRS_SDR_DEVICE           dongle index                  (default 0)
//!   APRS_SDR_CENTER_HZ        tuner centre frequency, Hz     (default 144_590_000)
//!   APRS_SDR_CHANNELS_HZ      comma-separated channel freqs  (default 144_390_000)
//!   APRS_SDR_SAMPLE_RATE      complex sample rate, Hz        (default 1_200_000)
//!   APRS_SDR_GAIN_TENTHS      tuner gain, tenths dB (default 400); "hw-agc" =
//!                             the tuner's own AGC (diagnostic only)
//!   APRS_SDR_PPM              frequency correction, ppm      (default 0)
//!   APRS_SDR_FM_MAXDEV_HZ     FM deviation mapped to full-scale audio (level knob)
//!   APRS_SDR_SQUELCH_OPEN_DB  squelch open threshold, dB SNR
//!   APRS_SDR_SQUELCH_CLOSE_DB squelch close threshold, dB SNR
//!   APRS_SDR_DEEMPHASIS       "on" to enable ka9q-style de-emphasis (default off)
//!
//! The centre is deliberately offset from the channel(s) so no channel sits on the
//! RTL-SDR DC spike. Each channel's `ssrc = freq_kHz` (ka9q convention).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aprs_modem::DecoderConfig;
use aprs_sdr::{spawn, FmParams, Gain, SdrSourceConfig};
use tracing_subscriber::EnvFilter;

/// Default fixed tuner gain (tenths dB). High for sensitivity; lower it if a strong
/// local signal produces overload ghosts, or set `APRS_SDR_GAIN_TENTHS=auto`.
const DEFAULT_GAIN_TENTHS: i32 = 400;

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
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_opt_f32(key: &str) -> Option<f32> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn env_bool(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "on" | "true" | "yes")
    )
}

/// `APRS_SDR_GAIN_TENTHS`: unset → fixed default, a number → fixed,
/// "hw-agc" → the tuner's own AGC (diagnostic only).
fn parse_gain() -> Gain {
    match std::env::var("APRS_SDR_GAIN_TENTHS") {
        Ok(v) if v.eq_ignore_ascii_case("hw-agc") => Gain::HardwareAgc,
        Ok(v) => match v.parse::<i32>() {
            Ok(n) => Gain::Manual(n),
            Err(_) => {
                tracing::warn!("invalid APRS_SDR_GAIN_TENTHS '{v}'; using {DEFAULT_GAIN_TENTHS}");
                Gain::Manual(DEFAULT_GAIN_TENTHS)
            }
        },
        Err(_) => Gain::Manual(DEFAULT_GAIN_TENTHS),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let channels: Vec<u32> = std::env::var("APRS_SDR_CHANNELS_HZ")
        .unwrap_or_else(|_| "144390000".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let config = SdrSourceConfig {
        device_index: env_u32("APRS_SDR_DEVICE", 0) as usize,
        center_freq: env_u32("APRS_SDR_CENTER_HZ", 144_590_000),
        sample_rate: env_u32("APRS_SDR_SAMPLE_RATE", 1_200_000),
        gain: parse_gain(),
        freq_correction_ppm: env_u32("APRS_SDR_PPM", 0) as i32,
        channels,
        fm: FmParams {
            full_scale_dev_hz: env_opt_f32("APRS_SDR_FM_MAXDEV_HZ"),
            squelch_open_db: env_opt_f32("APRS_SDR_SQUELCH_OPEN_DB"),
            squelch_close_db: env_opt_f32("APRS_SDR_SQUELCH_CLOSE_DB"),
            deemphasis: env_bool("APRS_SDR_DEEMPHASIS"),
        },
        decoder: DecoderConfig::default(),
    };

    if let Err(e) = config.validate() {
        tracing::error!("{e}");
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let shutdown = Arc::new(AtomicBool::new(false));
    let sig_shutdown = shutdown.clone();
    rt.spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown signal received; stopping");
        sig_shutdown.store(true, Ordering::Relaxed);
    });

    // Start the SDR pipeline within the runtime (the decode stage uses tokio::spawn).
    let (mut packets, handles, frames) = {
        let _guard = rt.enter();
        spawn(config, shutdown.clone())
    };

    rt.block_on(async {
        while let Some(pkt) = packets.recv().await {
            frames.fetch_add(1, Ordering::Relaxed);
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

    handles.join();
    tracing::info!("exiting");
}
