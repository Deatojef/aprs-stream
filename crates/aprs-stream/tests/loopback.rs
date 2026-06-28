//! End-to-end transport smoke test: encode -> UDP -> receive -> decode.
//!
//! Uses a loopback unicast destination rather than multicast routing so the test
//! is deterministic across environments. It still exercises the full
//! `Emitter` -> socket -> `Subscriber` -> codec path (the `Subscriber` binds the
//! wildcard address, so it receives the loopback datagram on its port).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use aprs_stream::emit::{EmitConfig, Emitter};
use aprs_stream::proto::{AprsFrame, AudioLevel, CaptureMeta, RfMeta};
use aprs_stream::subscribe::{SubscribeConfig, Subscriber};

fn sample_frame() -> AprsFrame {
    let packet = aprs_stream::aprs_decode::AprsPacket::decode_textual(
        b"W1AW-9>APRS,WIDE1-1:!4903.50N/07201.75W-end-to-end test",
    )
    .expect("parse");
    let ax25 = packet.encode_ax25().expect("encode ax25");
    AprsFrame::new(
        CaptureMeta {
            received_at_ms: 1_719_580_800_123,
            receiver: Some("packet.local".into()),
            decoder: Some("aprs-rtp/0.2.0".into()),
            ssrc: Some(144_390),
        },
        RfMeta {
            frequency_hz: Some(144_390_000),
            snr_db: None,
            audio_level: Some(AudioLevel {
                rec: 52,
                mark: 24,
                space: 21,
            }),
            slicer_hits: Some(6),
            slicer_mask: Some(0b0011_1110),
        },
        true,
        ax25,
        Some(packet),
    )
}

/// True multicast path. Ignored by default because multicast loopback depends on
/// host/network config (IGMP, interface selection) that isn't guaranteed in CI
/// or sandboxes. Run explicitly with `cargo test -- --ignored`.
#[tokio::test]
#[ignore = "requires working multicast loopback"]
async fn emit_then_receive_over_multicast() {
    let group = Ipv4Addr::new(239, 12, 34, 58);
    let port = 17_100;

    let sub = Subscriber::new(SubscribeConfig {
        group,
        port,
        interface: Ipv4Addr::LOCALHOST,
        recv_buffer_bytes: None,
    })
    .expect("subscriber");

    let emitter = Emitter::new(EmitConfig {
        interface: Ipv4Addr::LOCALHOST,
        destinations: vec![SocketAddr::from((group, port))],
        multicast_ttl: 1,
    })
    .expect("emitter");

    let frame = sample_frame();
    emitter.send_frame(&frame).await.expect("send");

    let (got, _from) = tokio::time::timeout(Duration::from_secs(2), sub.recv_frame())
        .await
        .expect("recv timed out")
        .expect("recv");
    assert_eq!(got, frame);
}

#[tokio::test]
async fn emit_then_receive_round_trips_over_udp() {
    // An unlikely-to-collide group/port pair for the test.
    let group = Ipv4Addr::new(239, 12, 34, 57);
    let port = 17_099;

    let sub = Subscriber::new(SubscribeConfig::new(group, port)).expect("subscriber");

    let emitter = Emitter::new(EmitConfig {
        interface: Ipv4Addr::LOCALHOST,
        destinations: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))],
        multicast_ttl: 1,
    })
    .expect("emitter");

    let frame = sample_frame();
    let sent = emitter.send_frame(&frame).await.expect("send");
    assert!(sent < 1400, "frame should fit a safe UDP payload: {sent} bytes");

    let (got, _from) = tokio::time::timeout(Duration::from_secs(2), sub.recv_frame())
        .await
        .expect("recv timed out")
        .expect("recv");

    assert_eq!(got, frame, "frame must survive the full UDP round-trip");
}
