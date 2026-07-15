//! `aprs-streamd` — the base service binary.
//!
//! Decodes APRS frames from one of two audio sources — ka9q-radio RTP (`[source]`)
//! or a direct RTL-SDR (`[sdr]`) — and publishes fully-decoded, typed frames via
//! `aprs-stream::emit`. The decode work happens here, once, and is shared by all
//! downstream consumers. The producer is policy-free: it emits every frame the
//! decoder yields, tagged with quality metadata — no dedup, no filtering.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aprs_modem::AprsPacket as SdrPacket;
use aprs_rtp::{AprsListener, AprsPacket as RtpPacket};
use aprs_stream::emit::{EmitConfig, Emitter};
use aprs_stream::proto::{AprsFrame, AudioLevel, Ax25Meta, CaptureMeta, RfMeta, ViaHop};
use tracing_subscriber::EnvFilter;

mod config;
use config::{Config, GainSetting, SdrSection};

#[tokio::main]
async fn main() {
    // Initialize logging first so everything below (including aprs-rtp's internal
    // `tracing` output) is captured. Honors RUST_LOG; defaults to `info`.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    if let Err(e) = run().await {
        // Display (not Debug) so config errors read cleanly.
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (path, cfg) = Config::load()?;
    tracing::info!("loaded config from {}", path.display());

    // Exactly one audio source.
    match (cfg.source.is_some(), cfg.sdr.is_some()) {
        (true, true) => return Err("configure only one of [source] (RTP) or [sdr]".into()),
        (false, false) => return Err("no audio source: configure [source] (RTP) or [sdr]".into()),
        _ => {}
    }

    let emitter = Emitter::new(EmitConfig {
        interface: cfg.emit.interface,
        destinations: cfg.emit.destinations(),
        multicast_ttl: cfg.emit.ttl,
    })?;
    tracing::info!(
        "publishing to {:?} (TTL {})",
        cfg.emit.destinations(),
        cfg.emit.ttl
    );

    // The slicer gain ladder is static for the session (derived from the decoder
    // config); compute it once and stamp a copy onto every frame's RfMeta so
    // consumers can label a slicer waterfall without owning the decoder.
    let slicer_gains = space_gains(
        cfg.decoder.slicers,
        cfg.decoder.min_twist_db,
        cfg.decoder.max_twist_db,
    );

    if let Some(sdr) = cfg.sdr {
        run_sdr(sdr, &cfg.decoder, &emitter, &slicer_gains).await
    } else {
        run_rtp(cfg.source.unwrap(), cfg.decoder, &emitter, &slicer_gains).await
    }
}

/// RTP path (ka9q-radio): the original pipeline, unchanged.
async fn run_rtp(
    source: aprs_rtp::config::SourceConfig,
    decoder: aprs_rtp::config::DecoderConfig,
    emitter: &Emitter,
    slicer_gains: &[f32],
) -> Result<(), Box<dyn std::error::Error>> {
    let receiver = source.host.clone();
    tracing::info!("listening on RTP {}:{}", source.host, source.port);

    let mut rx = AprsListener::new(source, decoder).run().await?;
    while let Some(pkt) = rx.recv().await {
        let frame = map_frame_rtp(pkt, &receiver, slicer_gains);
        if let Err(e) = emitter.send_frame(&frame).await {
            tracing::warn!("emit error: {e}");
        }
    }
    tracing::info!("RTP stream closed, exiting.");
    Ok(())
}

/// SDR path (direct RTL-SDR): assemble the `aprs-sdr` pipeline, map its packets
/// (carrying the measured SNR), and emit. Stops cleanly on SIGINT/SIGTERM so the
/// dongle is released rather than killed mid-transfer.
async fn run_sdr(
    sdr: SdrSection,
    decoder: &aprs_rtp::config::DecoderConfig,
    emitter: &Emitter,
    slicer_gains: &[f32],
) -> Result<(), Box<dyn std::error::Error>> {
    let sdr_config = aprs_sdr::SdrSourceConfig {
        device_index: sdr.device,
        center_freq: sdr.center_hz,
        sample_rate: sdr.sample_rate,
        gain: to_gain(&sdr.gain),
        auto_target_floor_dbfs: sdr.auto_floor_dbfs,
        freq_correction_ppm: sdr.ppm,
        channels: sdr.channels_hz.clone(),
        fm: aprs_sdr::FmParams {
            full_scale_dev_hz: sdr.fm_maxdev_hz,
            squelch_open_db: sdr.squelch_open_db,
            squelch_close_db: sdr.squelch_close_db,
            deemphasis: sdr.deemphasis,
        },
        decoder: to_modem_decoder(decoder),
    };
    sdr_config.validate()?;

    let receiver = format!("rtl-sdr#{}", sdr.device);

    // Shutdown flag driven by a signal watcher; the SDR reader closes the dongle
    // when it's set.
    let shutdown = Arc::new(AtomicBool::new(false));
    let sig = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown signal received; stopping");
        sig.store(true, Ordering::Relaxed);
    });

    let (mut packets, handles, frames) = aprs_sdr::spawn(sdr_config, shutdown);
    while let Some(pkt) = packets.recv().await {
        // Counted before emit: this is the decode/catch rate, which is what the
        // RF stats in the same status line explain.
        frames.fetch_add(1, Ordering::Relaxed);
        let frame = map_frame_sdr(pkt, &receiver, slicer_gains);
        if let Err(e) = emitter.send_frame(&frame).await {
            tracing::warn!("emit error: {e}");
        }
    }
    // Join the reader/DSP threads off the async runtime.
    let _ = tokio::task::spawn_blocking(move || handles.join()).await;
    tracing::info!("SDR source closed, exiting.");
    Ok(())
}

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

/// Convert the `[sdr]` gain setting into the driver's gain mode.
fn to_gain(g: &GainSetting) -> aprs_sdr::Gain {
    match g {
        GainSetting::Tenths(n) => aprs_sdr::Gain::Manual(*n),
        GainSetting::Mode(s) if s.eq_ignore_ascii_case("auto") => aprs_sdr::Gain::Auto,
        GainSetting::Mode(s) if s.eq_ignore_ascii_case("hw-agc") => aprs_sdr::Gain::HardwareAgc,
        GainSetting::Mode(s) => {
            tracing::warn!("unknown gain '{s}' (expected a number, \"auto\", or \"hw-agc\"); using fixed 400 (40 dB)");
            aprs_sdr::Gain::Manual(400)
        }
    }
}

/// Map the shared `[decoder]` tuning (modeled on aprs-rtp) onto aprs-modem's
/// identical `DecoderConfig`, so both source paths decode the same way.
fn to_modem_decoder(d: &aprs_rtp::config::DecoderConfig) -> aprs_modem::DecoderConfig {
    aprs_modem::DecoderConfig {
        mark_hz: d.mark_hz,
        space_hz: d.space_hz,
        baud: d.baud,
        slicers: d.slicers,
        min_twist_db: d.min_twist_db,
        max_twist_db: d.max_twist_db,
        fix_bits: match d.fix_bits {
            aprs_rtp::config::FixBits::None => aprs_modem::FixBits::None,
            aprs_rtp::config::FixBits::Single => aprs_modem::FixBits::Single,
            aprs_rtp::config::FixBits::Double => aprs_modem::FixBits::Double,
        },
    }
}

/// Linear gain per slicer for the decoder's `n`-rung ladder, spread uniformly in
/// twist dB across `[min_db, max_db]` (uniform-in-dB == geometric-in-linear-gain).
/// Replicates the decoders' internal `space_gains` so the ladder we publish matches
/// what the decoder actually ran. A single slicer uses unity gain (0 dB).
fn space_gains(n: usize, min_db: f32, max_db: f32) -> Vec<f32> {
    match n {
        0 => Vec::new(),
        1 => vec![1.0],
        _ => {
            let step_db = (max_db - min_db) / (n - 1) as f32;
            (0..n)
                .map(|i| 10f32.powf((min_db + i as f32 * step_db) / 20.0))
                .collect()
        }
    }
}

/// Map a decoded RTP packet into the wire frame.
fn map_frame_rtp(pkt: RtpPacket, receiver: &str, slicer_gains: &[f32]) -> AprsFrame {
    let ax25_meta = Ax25Meta {
        source: pkt.source.clone(),
        destination: pkt.destination.clone(),
        via: zip_via(&pkt.via, &pkt.via_heard),
        heard_direct: pkt.heard_direct,
        heard_from: pkt.heard_from.clone(),
        dti: pkt.dti,
        info_offset: info_offset(pkt.raw_ax25.len(), pkt.info.len()),
        info_invalid_bytes: pkt.info_invalid_bytes as u32,
    };
    build_frame(
        receiver,
        "aprs-rtp/0.2.0",
        pkt.ssrc,
        pkt.received_at,
        pkt.freq_mhz,
        None, // aprs-rtp does not measure SNR
        pkt.audio_level.rec,
        pkt.audio_level.mark,
        pkt.audio_level.space,
        pkt.slicer_hits,
        pkt.slicer_mask,
        slicer_gains,
        pkt.raw_ax25,
        ax25_meta,
    )
}

/// Map a decoded SDR packet into the wire frame, carrying the measured SNR.
fn map_frame_sdr(pkt: SdrPacket, receiver: &str, slicer_gains: &[f32]) -> AprsFrame {
    let ax25_meta = Ax25Meta {
        source: pkt.source.clone(),
        destination: pkt.destination.clone(),
        via: zip_via(&pkt.via, &pkt.via_heard),
        heard_direct: pkt.heard_direct,
        heard_from: pkt.heard_from.clone(),
        dti: pkt.dti,
        info_offset: info_offset(pkt.raw_ax25.len(), pkt.info.len()),
        info_invalid_bytes: pkt.info_invalid_bytes as u32,
    };
    build_frame(
        receiver,
        "aprs-sdr/0.1.0",
        pkt.ssrc,
        pkt.received_at,
        pkt.freq_mhz,
        pkt.signal.map(|s| s.snr_db),
        pkt.audio_level.rec,
        pkt.audio_level.mark,
        pkt.audio_level.space,
        pkt.slicer_hits,
        pkt.slicer_mask,
        slicer_gains,
        pkt.raw_ax25,
        ax25_meta,
    )
}

/// Shared frame assembly for both sources — parses the AX.25 body into a typed
/// packet (left `None` if the FCS-valid frame can't be parsed) and stamps metadata.
#[allow(clippy::too_many_arguments)]
fn build_frame(
    receiver: &str,
    decoder: &str,
    ssrc: u32,
    received_at: SystemTime,
    freq_mhz: f64,
    snr_db: Option<f32>,
    rec: u8,
    mark: u8,
    space: u8,
    slicer_hits: u8,
    slicer_mask: u16,
    slicer_gains: &[f32],
    raw_ax25: Vec<u8>,
    ax25_meta: Ax25Meta,
) -> AprsFrame {
    let parsed = aprs_decode::AprsPacket::decode_ax25(&raw_ax25).ok();
    AprsFrame::new(
        CaptureMeta {
            received_at_ms: epoch_millis(received_at),
            receiver: Some(receiver.to_string()),
            decoder: Some(decoder.to_string()),
            ssrc: Some(ssrc),
        },
        RfMeta {
            frequency_hz: Some((freq_mhz * 1_000_000.0).round() as u64),
            snr_db,
            audio_level: Some(AudioLevel { rec, mark, space }),
            slicer_hits: Some(slicer_hits),
            slicer_mask: Some(slicer_mask),
            slicer_gains: Some(slicer_gains.to_vec()),
        },
        // Both decoders only yield FCS-valid frames.
        true,
        raw_ax25,
        Some(ax25_meta),
        parsed,
    )
}

/// The info field is the trailing slice of the (FCS-excluded) AX.25 frame, so its
/// start offset is `len(ax25) - len(info)`. `checked_sub` guards the impossible
/// case of info longer than the frame.
fn info_offset(ax25_len: usize, info_len: usize) -> Option<u32> {
    ax25_len.checked_sub(info_len).map(|n| n as u32)
}

fn zip_via(via: &[String], via_heard: &[bool]) -> Vec<ViaHop> {
    via.iter()
        .cloned()
        .zip(via_heard.iter().copied())
        .map(|(call, heard)| ViaHop { call, heard })
        .collect()
}

fn epoch_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::space_gains;

    #[test]
    fn ladder_default_8_is_uniform_in_db_with_a_unity_rung() {
        // 8 slicers across -12..+9 dB → a 3 dB step with one rung on 0 dB (unity).
        let g = space_gains(8, -12.0, 9.0);
        assert_eq!(g.len(), 8);
        assert!(
            (g[4] - 1.0).abs() < 1e-5,
            "rung 4 should be 0 dB (unity): {g:?}"
        );
        assert!(
            g.windows(2).all(|w| w[1] > w[0]),
            "ladder must increase: {g:?}"
        );
        let ratio = g[1] / g[0];
        assert!(
            g.windows(2).all(|w| (w[1] / w[0] - ratio).abs() < 1e-4),
            "linear gains should be geometric: {g:?}"
        );
    }

    #[test]
    fn ladder_edge_cases() {
        assert!(space_gains(0, -12.0, 9.0).is_empty());
        assert_eq!(space_gains(1, -12.0, 9.0), vec![1.0]);
    }
}
