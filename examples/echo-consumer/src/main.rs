//! Minimal subscriber: join the APRS multicast group and print decoded frames.
//!
//! This is the end-to-end smoke test for the stream — a thin consumer of the
//! `aprs-stream` crate that never touches RTP audio or AX.25 parsing. It matches
//! directly on the typed payload (`aprs_decode::AprsData`).

use std::net::Ipv4Addr;
use std::path::Path;

use aprs_stream::aprs_decode::{AprsData, AprsPacket};
use aprs_stream::proto::Ax25Meta;
use aprs_stream::subscribe::{SubscribeConfig, Subscriber};
use serde::Deserialize;

/// Must match `aprs-streamd`'s default group / port. Used when no config file is
/// found and the matching env var is unset.
const DEFAULT_GROUP: Ipv4Addr = Ipv4Addr::new(239, 12, 34, 56);
const DEFAULT_PORT: u16 = 17_014;

/// Where we look for `aprs-streamd`'s config, in priority order: the deployed
/// location first, then a local `./config.toml` for development.
const SEARCH_PATHS: &[&str] = &["/etc/aprs-streamd/config.toml", "config.toml"];

/// The subset of `aprs-streamd`'s config file this consumer cares about: just the
/// `[emit]` group/port so it subscribes to the same place the producer publishes.
/// `deny_unknown_fields` is intentionally OFF so the `[sdr]`, `[decoder]`, etc.
/// tables are simply ignored rather than rejected.
#[derive(Debug, Default, Deserialize)]
struct StreamdConfig {
    #[serde(default)]
    emit: EmitSection,
}

#[derive(Debug, Deserialize)]
struct EmitSection {
    #[serde(default = "default_group")]
    group: Ipv4Addr,
    #[serde(default = "default_port")]
    port: u16,
}

impl Default for EmitSection {
    fn default() -> Self {
        Self {
            group: default_group(),
            port: default_port(),
        }
    }
}

fn default_group() -> Ipv4Addr {
    DEFAULT_GROUP
}
fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Read the first existing config file from [`SEARCH_PATHS`] and pull out the
/// emit group/port. Returns the defaults if no file is found; a malformed file is
/// surfaced as an error rather than silently ignored.
fn load_emit() -> Result<(Ipv4Addr, u16), Box<dyn std::error::Error>> {
    for path in SEARCH_PATHS.iter().map(Path::new) {
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: StreamdConfig = toml::from_str(&text)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        eprintln!("echo-consumer: using config {}", path.display());
        return Ok((cfg.emit.group, cfg.emit.port));
    }
    eprintln!("echo-consumer: no config file found, using defaults");
    Ok((DEFAULT_GROUP, DEFAULT_PORT))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Config file supplies the group/port; env vars still override for ad-hoc runs.
    let (cfg_group, cfg_port) = load_emit()?;
    let group: Ipv4Addr = parse_env("APRS_EMIT_GROUP", cfg_group);
    let port: u16 = parse_env("APRS_EMIT_PORT", cfg_port);

    let sub = Subscriber::new(SubscribeConfig::new(group, port))?;
    eprintln!("echo-consumer: joined {group}:{port}, waiting for frames...");

    loop {
        match sub.recv_frame().await {
            Ok((frame, from)) => print_frame(&frame, &from.to_string()),
            Err(e) => eprintln!("echo-consumer: skipping bad datagram: {e}"),
        }
    }
}

fn print_frame(frame: &aprs_stream::AprsFrame, from: &str) {
    let freq = frame
        .rf
        .frequency_hz
        .map(|hz| format!("{:.4} MHz", hz as f64 / 1e6))
        .unwrap_or_else(|| "?".into());

    match &frame.parsed {
        Some(pkt) => println!(
            "[{from}] v{} {freq} hits={} crc_ok={}  [{}] {}",
            frame.version,
            frame.rf.slicer_hits.unwrap_or(0),
            frame.crc_ok,
            payload_summary(&pkt.data),
            tnc2(pkt),
        ),
        None => println!(
            "[{from}] v{} {freq} crc_ok={}  <unparsed, {} ax25 bytes>",
            frame.version,
            frame.crc_ok,
            frame.ax25.len(),
        ),
    }

    print_meta(frame);
}

/// Second line dumping the v2 framing metadata, so it's obvious at a glance
/// whether the producer is actually populating `ax25_meta` / `slicer_gains` on
/// the wire. A `<none>` here means the frame arrived without that block —
/// either a pre-v2 producer or a mapping bug.
fn print_meta(frame: &aprs_stream::AprsFrame) {
    match &frame.ax25_meta {
        Some(m) => {
            let dti = m
                .dti
                .map(|b| format!("'{}'", b as char))
                .unwrap_or_else(|| "none".into());
            // Prove info_offset points at real bytes: slice the raw AX.25 with it
            // and show how many info bytes that yields (byte-faithful igating path).
            let info_len = m
                .info_offset
                .and_then(|off| frame.ax25.get(off as usize..))
                .map(<[u8]>::len);
            println!(
                "    ax25: {}>{} via [{}] direct={} heard_from={} dti={} info@{} len={} invalid={}",
                m.source,
                m.destination,
                render_via(m),
                m.heard_direct,
                m.heard_from,
                dti,
                m.info_offset
                    .map(|o| o.to_string())
                    .unwrap_or_else(|| "?".into()),
                info_len
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                m.info_invalid_bytes,
            );
        }
        None => println!("    ax25: <none>"),
    }

    // SNR is provided by the SDR producer (None from the RTP producer).
    let snr = frame
        .rf
        .snr_db
        .map(|s| format!("{s:.1}dB"))
        .unwrap_or_else(|| "-".into());
    match &frame.rf.slicer_gains {
        Some(g) => {
            // Show the ladder as twist dB (20·log10 gain) — the waterfall labels.
            let twist: Vec<String> = g
                .iter()
                .map(|x| format!("{:+.0}", 20.0 * x.log10()))
                .collect();
            println!(
                "    rf: snr={} slicers={} twist_db=[{}] mask={:#06x}",
                snr,
                g.len(),
                twist.join(","),
                frame.rf.slicer_mask.unwrap_or(0),
            );
        }
        None => println!("    rf: snr={snr} slicer_gains <none>"),
    }
}

/// Render the digipeater path with TNC2 `*` markers on heard (H-bit-set) hops.
fn render_via(m: &Ax25Meta) -> String {
    if m.via.is_empty() {
        return "(none)".into();
    }
    m.via
        .iter()
        .map(|h| format!("{}{}", h.call, if h.heard { "*" } else { "" }))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reconstruct the canonical TNC2 line (`FROM>TO,VIA:info`) for display. Info
/// fields can contain non-UTF-8 bytes (Mic-E, binary telemetry), so render
/// lossily; fall back to the structured header if re-encoding fails.
fn tnc2(pkt: &AprsPacket) -> String {
    match pkt.encode_textual() {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => format!("{}>{} <unencodable info>", pkt.from, pkt.to),
    }
}

/// A short, human-friendly one-liner per payload kind.
fn payload_summary(data: &AprsData) -> String {
    match data {
        AprsData::Position(_) => "Position".into(),
        AprsData::MicE(_) => "Mic-E position".into(),
        AprsData::Message(_) => "Message".into(),
        AprsData::Status(_) => "Status".into(),
        AprsData::Object(_) => "Object".into(),
        AprsData::Item(_) => "Item".into(),
        AprsData::Weather(_) => "Weather".into(),
        AprsData::Telemetry(_) => "Telemetry".into(),
        AprsData::Capabilities(_) => "Capabilities".into(),
        AprsData::Query(_) => "Query".into(),
        AprsData::GridLocator(_) => "GridLocator".into(),
        AprsData::Nmea(_) => "NMEA".into(),
        AprsData::ThirdParty(_) => "ThirdParty".into(),
        AprsData::UserDefined(_) => "UserDefined".into(),
        AprsData::Unknown { dti, .. } => format!("Unknown(dti={})", *dti as char),
        // AprsData is #[non_exhaustive]: tolerate future variants.
        _ => "Other".into(),
    }
}

fn parse_env<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tnc2_round_trips_full_packet() {
        let line = b"W1AW-9>APRS,WIDE1-1,WIDE2-2:!4903.50N/07201.75W-Test";
        let pkt = AprsPacket::decode_textual(line).unwrap();
        assert_eq!(tnc2(&pkt), String::from_utf8_lossy(line));
        assert_eq!(payload_summary(&pkt.data), "Position");
    }

    #[test]
    fn render_via_marks_heard_hops() {
        use aprs_stream::proto::ViaHop;
        let m = Ax25Meta {
            via: vec![
                ViaHop {
                    call: "W1XYZ-1".into(),
                    heard: true,
                },
                ViaHop {
                    call: "WIDE2-1".into(),
                    heard: false,
                },
            ],
            ..Default::default()
        };
        assert_eq!(render_via(&m), "W1XYZ-1*,WIDE2-1");
        assert_eq!(render_via(&Ax25Meta::default()), "(none)");
    }
}
