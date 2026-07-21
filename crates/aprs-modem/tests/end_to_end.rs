//! End-to-end proof of the source-agnostic seam: synthesize a real AFSK-1200
//! AX.25 UI frame as 24 kHz audio, push it through `decode_audio_stream` as a
//! single `AudioBlock`, and assert the decoded `AprsPacket` round-trips.
//!
//! This exercises the *new* wiring (AudioBlock → per-SSRC dispatch → StreamDecoder
//! → AprsPacket) on top of the vendored decode DSP, closing Milestone A. The
//! modulator mirrors the conventions the framer's own unit tests use (LSB-first
//! bits, bit-stuffing on payload only, NRZI with logical-1 = no transition) and
//! the FCS/CRC in `hdlc::fec` (CRC-16 X.25, poly 0x8408, init 0xFFFF, complemented,
//! stored little-endian).

use aprs_modem::{AudioBlock, DecoderConfig, decode_audio_stream};

const SAMPLE_RATE: u32 = 24_000;
const BAUD: u32 = 1_200;
const MARK_HZ: f32 = 1_200.0;
const SPACE_HZ: f32 = 2_200.0;
const SSRC: u32 = 144_390; // ka9q convention: freq_kHz -> 144.390 MHz

// --- AX.25 framing helpers -------------------------------------------------

/// CRC-16 X.25 (matches `aprs_modem` internal `hdlc::fec::crc16`).
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        let mut b = byte;
        for _ in 0..8 {
            let bit = (crc ^ b as u16) & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= 0x8408;
            }
            b >>= 1;
        }
    }
    !crc
}

/// Encode one AX.25 address field (7 bytes): 6 chars << 1, then the SSID octet
/// (reserved bits 5,6 set; SSID in bits 1..4; H-bit in bit 7; end-of-address in bit 0).
fn address(call: &str, ssid: u8, last: bool, h_bit: bool) -> [u8; 7] {
    let mut out = [b' ' << 1; 7];
    let bytes = call.as_bytes();
    for i in 0..6 {
        let c = if i < bytes.len() { bytes[i] } else { b' ' };
        out[i] = c << 1;
    }
    let mut ssid_octet = 0x60 | ((ssid & 0x0F) << 1);
    if h_bit {
        ssid_octet |= 0x80;
    }
    if last {
        ssid_octet |= 0x01;
    }
    out[6] = ssid_octet;
    out
}

/// Build the AX.25 UI frame (dest, source, control 0x03, PID 0xF0, info) with the
/// 2-byte little-endian FCS appended — exactly the bytes the HDLC layer expects.
fn build_frame(dest: &str, source: &str, info: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&address(dest, 0, false, false));
    f.extend_from_slice(&address(source, 0, true, false));
    f.push(0x03); // UI control
    f.push(0xF0); // PID: no layer 3
    f.extend_from_slice(info);
    let fcs = crc16(&f);
    f.extend_from_slice(&fcs.to_le_bytes());
    f
}

// --- Bit stream + AFSK modulation ------------------------------------------

fn bits_lsb_first(bytes: &[u8]) -> Vec<bool> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).map(move |i| (b >> i) & 1 == 1))
        .collect()
}

/// Bit-stuff: insert a 0 after every run of five 1s (payload only, never flags).
fn stuff(bits: &[bool]) -> Vec<bool> {
    let mut out = Vec::new();
    let mut ones = 0u8;
    for &b in bits {
        out.push(b);
        if b {
            ones += 1;
            if ones == 5 {
                out.push(false);
                ones = 0;
            }
        } else {
            ones = 0;
        }
    }
    out
}

/// NRZI encode: logical 0 = transition, logical 1 = no transition (start false).
fn nrzi(bits: &[bool]) -> Vec<bool> {
    let mut raw = Vec::with_capacity(bits.len());
    let mut prev = false;
    for &b in bits {
        if !b {
            prev = !prev;
        }
        raw.push(prev);
    }
    raw
}

/// Modulate NRZI raw bits to continuous-phase AFSK: raw=1 -> mark (1200 Hz),
/// raw=0 -> space (2200 Hz). One bit = SAMPLE_RATE/BAUD samples.
fn modulate(raw: &[bool]) -> Vec<f32> {
    let spb = (SAMPLE_RATE / BAUD) as usize; // 20 samples/bit
    let mut samples = Vec::with_capacity(raw.len() * spb);
    let mut phase = 0.0f32;
    for &r in raw {
        let f = if r { MARK_HZ } else { SPACE_HZ };
        let dphi = 2.0 * std::f32::consts::PI * f / SAMPLE_RATE as f32;
        for _ in 0..spb {
            samples.push(phase.sin() * 0.5);
            phase += dphi;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
        }
    }
    samples
}

/// Assemble a full transmission: leading silence + preamble flags + stuffed
/// payload + trailing flags, NRZI-encoded and AFSK-modulated.
fn synthesize(frame: &[u8]) -> Vec<f32> {
    let flag_bits = bits_lsb_first(&[0x7E]);
    let mut logical = Vec::new();
    for _ in 0..64 {
        logical.extend_from_slice(&flag_bits); // TXDELAY preamble
    }
    logical.extend(stuff(&bits_lsb_first(frame)));
    for _ in 0..3 {
        logical.extend_from_slice(&flag_bits); // closing flags
    }
    let mut samples = vec![0.0f32; 480]; // 20 ms lead-in for AGC
    samples.extend(modulate(&nrzi(&logical)));
    samples
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_afsk_frame_decodes_through_audio_stream() {
    let info = b">aprs-modem end-to-end test";
    let frame = build_frame("APRS", "N0CALL", info);
    let samples = synthesize(&frame);

    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<AudioBlock>(4);
    let mut packets = decode_audio_stream(DecoderConfig::default(), audio_rx);

    audio_tx
        .send(AudioBlock {
            ssrc: SSRC,
            sample_rate: SAMPLE_RATE,
            samples,
            signal: None,
        })
        .await
        .expect("send audio block");
    drop(audio_tx); // close the source so the pipeline can wind down

    let pkt = tokio::time::timeout(std::time::Duration::from_secs(5), packets.recv())
        .await
        .expect("decode did not complete within 5s")
        .expect("expected a decoded packet, got none");

    assert_eq!(pkt.source, "N0CALL", "source callsign");
    assert_eq!(pkt.destination, "APRS", "destination callsign");
    assert_eq!(pkt.info, info, "info field bytes");
    assert_eq!(pkt.ssrc, SSRC);
    assert!((pkt.freq_mhz - 144.390).abs() < 1e-9, "freq from ssrc");
    assert!(pkt.slicer_hits >= 1, "at least one slicer decoded it");
    assert!(pkt.heard_direct, "no via -> heard direct");
}
