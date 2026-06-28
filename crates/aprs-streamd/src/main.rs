//! `aprs-streamd` — the base service binary.
//!
//! Owns `aprs-rtp` + `aprs-decode` and publishes fully-decoded, typed frames via
//! `aprs-stream::emit`. The decode work happens here, once, and is shared by all
//! downstream consumers. The producer is policy-free: it emits every frame the
//! decoder yields, tagged with quality metadata — no dedup, no filtering.

use std::time::{SystemTime, UNIX_EPOCH};

use aprs_rtp::{AprsListener, AprsPacket as RtpPacket};
use aprs_stream::emit::{EmitConfig, Emitter};
use aprs_stream::proto::{AprsFrame, AudioLevel, CaptureMeta, RfMeta};
use tracing_subscriber::EnvFilter;

mod config;
use config::Config;

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

    let receiver = cfg.source.host.clone();

    let emitter = Emitter::new(EmitConfig {
        interface: cfg.emit.interface,
        destinations: cfg.emit.destinations(),
        multicast_ttl: cfg.emit.ttl,
    })?;

    tracing::info!(
        "listening on RTP {}:{}, publishing to {:?} (TTL {})",
        cfg.source.host,
        cfg.source.port,
        cfg.emit.destinations(),
        cfg.emit.ttl
    );

    let mut rx = AprsListener::new(cfg.source, cfg.decoder).run().await?;

    while let Some(pkt) = rx.recv().await {
        let frame = map_frame(pkt, &receiver);
        if let Err(e) = emitter.send_frame(&frame).await {
            // Best-effort medium: log and keep going rather than tearing down.
            tracing::warn!("emit error: {e}");
        }
    }

    tracing::info!("RTP stream closed, exiting.");
    Ok(())
}

/// Map a decoded RTP packet into the wire frame, parsing the AX.25 body into a
/// typed packet (left `None` if the FCS-valid frame can't be parsed).
fn map_frame(pkt: RtpPacket, receiver: &str) -> AprsFrame {
    let parsed = aprs_decode::AprsPacket::decode_ax25(&pkt.raw_ax25).ok();

    AprsFrame::new(
        CaptureMeta {
            received_at_ms: epoch_millis(pkt.received_at),
            receiver: Some(receiver.to_string()),
            decoder: Some("aprs-rtp/0.2.0".to_string()),
            ssrc: Some(pkt.ssrc),
        },
        RfMeta {
            frequency_hz: Some((pkt.freq_mhz * 1_000_000.0).round() as u64),
            snr_db: None,
            audio_level: Some(AudioLevel {
                rec: pkt.audio_level.rec,
                mark: pkt.audio_level.mark,
                space: pkt.audio_level.space,
            }),
            slicer_hits: Some(pkt.slicer_hits),
            slicer_mask: Some(pkt.slicer_mask),
        },
        // aprs-rtp only yields FCS-valid frames.
        true,
        pkt.raw_ax25,
        parsed,
    )
}

fn epoch_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
