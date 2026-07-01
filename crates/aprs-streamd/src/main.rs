//! `aprs-streamd` — the base service binary.
//!
//! Owns `aprs-rtp` + `aprs-decode` and publishes fully-decoded, typed frames via
//! `aprs-stream::emit`. The decode work happens here, once, and is shared by all
//! downstream consumers. The producer is policy-free: it emits every frame the
//! decoder yields, tagged with quality metadata — no dedup, no filtering.

use std::time::{SystemTime, UNIX_EPOCH};

use aprs_rtp::{AprsListener, AprsPacket as RtpPacket};
use aprs_stream::emit::{EmitConfig, Emitter};
use aprs_stream::proto::{AprsFrame, AudioLevel, Ax25Meta, CaptureMeta, RfMeta, ViaHop};
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

    // The slicer gain ladder is static for the session (derived from the decoder
    // config), so compute it once here — before the config is moved into the
    // listener — and stamp a copy onto every frame's RfMeta so consumers can label
    // a slicer waterfall without owning the decoder. See proto::RfMeta::slicer_gains.
    let slicer_gains = space_gains(
        cfg.decoder.slicers,
        cfg.decoder.min_twist_db,
        cfg.decoder.max_twist_db,
    );

    let mut rx = AprsListener::new(cfg.source, cfg.decoder).run().await?;

    while let Some(pkt) = rx.recv().await {
        let frame = map_frame(pkt, &receiver, &slicer_gains);
        if let Err(e) = emitter.send_frame(&frame).await {
            // Best-effort medium: log and keep going rather than tearing down.
            tracing::warn!("emit error: {e}");
        }
    }

    tracing::info!("RTP stream closed, exiting.");
    Ok(())
}

/// Linear gain per slicer for the decoder's `n`-rung ladder, spread uniformly in
/// twist dB across `[min_db, max_db]` (uniform-in-dB == geometric-in-linear-gain).
/// Replicates `aprs-rtp`'s internal `afsk::slicer::space_gains` (which is
/// `pub(crate)`, so not importable) so the ladder we publish matches what the
/// decoder actually ran. A single slicer uses unity gain (0 dB).
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

/// Map a decoded RTP packet into the wire frame, parsing the AX.25 body into a
/// typed packet (left `None` if the FCS-valid frame can't be parsed). The
/// AX.25-layer facts `aprs-rtp` already computed are carried in `ax25_meta` so
/// consumers never re-parse the frame — even when `parsed` is `None`.
fn map_frame(pkt: RtpPacket, receiver: &str, slicer_gains: &[f32]) -> AprsFrame {
    let parsed = aprs_decode::AprsPacket::decode_ax25(&pkt.raw_ax25).ok();

    // The info field is the trailing slice of the (FCS-excluded) AX.25 frame, so
    // its start offset is `len(ax25) - len(info)`. `checked_sub` guards the
    // theoretically-impossible case of info longer than the frame.
    let info_offset = pkt
        .raw_ax25
        .len()
        .checked_sub(pkt.info.len())
        .map(|n| n as u32);

    let via = pkt
        .via
        .iter()
        .cloned()
        .zip(pkt.via_heard.iter().copied())
        .map(|(call, heard)| ViaHop { call, heard })
        .collect();

    let ax25_meta = Ax25Meta {
        source: pkt.source.clone(),
        destination: pkt.destination.clone(),
        via,
        heard_direct: pkt.heard_direct,
        heard_from: pkt.heard_from.clone(),
        dti: pkt.dti,
        info_offset,
        info_invalid_bytes: pkt.info_invalid_bytes as u32,
    };

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
            slicer_gains: Some(slicer_gains.to_vec()),
        },
        // aprs-rtp only yields FCS-valid frames.
        true,
        pkt.raw_ax25,
        Some(ax25_meta),
        parsed,
    )
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
        // The aprs-rtp default: 8 slicers across -12..+9 dB → a 3 dB step with one
        // rung landing exactly on 0 dB (unity gain).
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
        // Uniform in dB == constant ratio between adjacent linear gains.
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
