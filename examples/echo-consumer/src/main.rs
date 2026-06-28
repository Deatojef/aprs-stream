//! Minimal subscriber: join the APRS multicast group and print decoded frames.
//!
//! This is the end-to-end smoke test for the stream — a thin consumer of the
//! `aprs-stream` crate that never touches RTP audio or AX.25 parsing. It matches
//! directly on the typed payload (`aprs_decode::AprsData`).

use std::net::Ipv4Addr;

use aprs_stream::aprs_decode::{AprsData, AprsPacket};
use aprs_stream::subscribe::{SubscribeConfig, Subscriber};

/// Must match `aprs-streamd`'s default group / port.
const DEFAULT_GROUP: Ipv4Addr = Ipv4Addr::new(239, 12, 34, 56);
const DEFAULT_PORT: u16 = 17_014;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let group: Ipv4Addr = parse_env("APRS_EMIT_GROUP", DEFAULT_GROUP);
    let port: u16 = parse_env("APRS_EMIT_PORT", DEFAULT_PORT);

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
}
