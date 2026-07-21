//! Software loopback: synthesize a real APRS AFSK-1200 frame, FM-modulate it onto
//! a wideband complex carrier offset from centre, then run it through the actual
//! `Channelizer` + `FmDemod` + `aprs-modem` decode chain and assert the packet
//! round-trips. This proves the entire SDR DSP path end-to-end in software —
//! everything except the physical RTL-SDR I/O — closing the Phase-B milestone
//! without needing a dongle or live RF.

use aprs_modem::{decode_audio_stream, AudioBlock, DecoderConfig};
use aprs_sdr::channelize::{Channelizer, AUDIO_RATE};
use aprs_sdr::fm::{FmDemod, FmDemodConfig};
use num_complex::Complex;

const FS: f64 = 1_200_000.0; // wideband complex rate
const AUDIO_FS: u32 = AUDIO_RATE; // 24 kHz
const BAUD: u32 = 1_200;
const MARK_HZ: f32 = 1_200.0;
const SPACE_HZ: f32 = 2_200.0;
const CHANNEL_OFFSET: f64 = 200_000.0; // channel sits +200 kHz from centre
const DEVIATION: f64 = 3_000.0; // FM deviation, Hz
const SSRC: u32 = 144_390;

// --- AX.25 frame + AFSK synthesis (same conventions as aprs-modem's test) ---

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

fn address(call: &str, ssid: u8, last: bool) -> [u8; 7] {
    let mut out = [b' ' << 1; 7];
    let bytes = call.as_bytes();
    for i in 0..6 {
        let c = if i < bytes.len() { bytes[i] } else { b' ' };
        out[i] = c << 1;
    }
    let mut octet = 0x60 | ((ssid & 0x0F) << 1);
    if last {
        octet |= 0x01;
    }
    out[6] = octet;
    out
}

fn build_frame(dest: &str, source: &str, info: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&address(dest, 0, false));
    f.extend_from_slice(&address(source, 0, true));
    f.push(0x03);
    f.push(0xF0);
    f.extend_from_slice(info);
    let fcs = crc16(&f);
    f.extend_from_slice(&fcs.to_le_bytes());
    f
}

fn bits_lsb_first(bytes: &[u8]) -> Vec<bool> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).map(move |i| (b >> i) & 1 == 1))
        .collect()
}

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

/// Real 24 kHz AFSK audio for a frame (preamble flags + stuffed payload + flags).
fn afsk_audio(frame: &[u8]) -> Vec<f32> {
    let flag = bits_lsb_first(&[0x7E]);
    let mut logical = Vec::new();
    for _ in 0..64 {
        logical.extend_from_slice(&flag);
    }
    logical.extend(stuff(&bits_lsb_first(frame)));
    for _ in 0..3 {
        logical.extend_from_slice(&flag);
    }
    let raw = nrzi(&logical);

    let spb = (AUDIO_FS / BAUD) as usize; // 20 samples/bit
    let mut audio = Vec::with_capacity(raw.len() * spb);
    let mut phase = 0.0f32;
    for &r in &raw {
        let f = if r { MARK_HZ } else { SPACE_HZ };
        let dphi = 2.0 * std::f32::consts::PI * f / AUDIO_FS as f32;
        for _ in 0..spb {
            audio.push(phase.sin() * 0.5);
            phase += dphi;
        }
    }
    audio
}

/// FM-modulate 24 kHz audio onto a complex carrier at `CHANNEL_OFFSET`, sampled at
/// the wideband `FS` (zero-order-hold upsample by FS/AUDIO_FS = 50).
fn fm_modulate_wideband(audio: &[f32]) -> Vec<Complex<f32>> {
    let up = (FS / AUDIO_FS as f64).round() as usize; // 50
    let mut iq = Vec::with_capacity(audio.len() * up);
    let mut phase = 0.0f64;
    for &a in audio {
        for _ in 0..up {
            let inst = CHANNEL_OFFSET + DEVIATION * a as f64;
            phase += 2.0 * std::f64::consts::PI * inst / FS;
            iq.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
        }
    }
    iq
}

/// Deterministic wideband complex noise (xorshift), amplitude `amp`.
fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex<f32>> {
    let mut s = seed | 1;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0 // ~[-1,1]
    };
    (0..n)
        .map(|_| Complex::new(next() * amp, next() * amp))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squelch_mutes_noise_and_reports_snr() {
    let frame = build_frame("APRS", "N0CALL", b">squelch test");
    let audio = afsk_audio(&frame);
    let signal_iq = fm_modulate_wideband(&audio);

    // 0.4 s of noise, the packet, then 0.4 s of noise — a realistic quiet channel.
    let gap = (FS * 0.4) as usize;
    let mut iq = noise(gap, 0.05, 1);
    let signal_start = iq.len();
    iq.extend_from_slice(&signal_iq);
    let signal_end = iq.len();
    iq.extend(noise(gap, 0.05, 99));

    let mut channelizer = Channelizer::new(FS);
    channelizer.add_channel(SSRC, CHANNEL_OFFSET);
    // Pin the output level so the audio-energy assertions are independent of the
    // default gain (this test is about squelch behavior, not level).
    let mut fm_cfg = FmDemodConfig::new(AUDIO_FS);
    fm_cfg.full_scale_dev_hz = 5_000.0;
    let mut demod = FmDemod::new(fm_cfg);

    // Process, tracking per-block audio energy + SNR against the region each block
    // falls in (blocks are 480 output samples = 24 000 input samples wide).
    let block_in = 24_000usize;
    let mut recovered = Vec::new();
    let mut in_pos = 0usize;
    let (mut noise_audio, mut sig_audio) = (0.0f32, 0.0f32);
    let (mut noise_n, mut sig_n) = (0usize, 0usize);
    let (mut noise_snr, mut sig_snr) = (f32::MIN, f32::MIN);
    for chunk in iq.chunks(262_144) {
        for block in channelizer.process(chunk) {
            let fm = demod.process(&block.samples);
            let energy: f32 = fm.audio.iter().map(|s| s.abs()).sum::<f32>() / fm.audio.len() as f32;
            let is_signal = in_pos >= signal_start && in_pos < signal_end;
            // Noise stats: leading region after the floor has settled (~200 ms), plus
            // the trailing region a couple blocks after the packet (squelch closed).
            let is_settled_noise = (in_pos >= block_in * 10 && in_pos < signal_start)
                || in_pos >= signal_end + block_in * 2;
            if is_signal {
                sig_audio += energy;
                sig_n += 1;
                sig_snr = sig_snr.max(fm.signal.snr_db);
            } else if is_settled_noise {
                noise_audio += energy;
                noise_n += 1;
                noise_snr = noise_snr.max(fm.signal.snr_db);
            }
            recovered.extend_from_slice(&fm.audio);
            in_pos += block_in;
        }
    }
    let noise_audio = noise_audio / noise_n as f32;
    let sig_audio = sig_audio / sig_n as f32;
    eprintln!(
        "noise: audio={noise_audio:.4} snr={noise_snr:.1}dB | signal: audio={sig_audio:.4} snr={sig_snr:.1}dB"
    );

    // Squelch mutes the noise regions but passes the packet.
    assert!(
        sig_audio > 0.05,
        "signal region should carry audio: {sig_audio}"
    );
    assert!(
        noise_audio < 0.02,
        "noise region should be squelched: {noise_audio}"
    );
    // SNR metric clearly separates carrier from noise.
    assert!(sig_snr > 6.0, "signal SNR should be high: {sig_snr}");
    assert!(
        sig_snr > noise_snr + 3.0,
        "signal SNR {sig_snr} vs noise {noise_snr}"
    );

    // And the packet still decodes cleanly through the squelched chain.
    let (tx, rx) = tokio::sync::mpsc::channel::<AudioBlock>(4);
    let mut packets = decode_audio_stream(DecoderConfig::default(), rx);
    tx.send(AudioBlock {
        ssrc: SSRC,
        sample_rate: AUDIO_FS,
        samples: recovered,
        signal: None,
    })
    .await
    .unwrap();
    drop(tx);
    let pkt = tokio::time::timeout(std::time::Duration::from_secs(10), packets.recv())
        .await
        .expect("timeout")
        .expect("packet should decode despite squelch");
    assert_eq!(pkt.source, "N0CALL");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wideband_fm_signal_decodes_through_sdr_dsp() {
    let info = b">aprs-sdr loopback test";
    let frame = build_frame("APRS", "N0CALL", info);
    let audio = afsk_audio(&frame);
    let iq = fm_modulate_wideband(&audio);

    // Real SDR DSP chain: channelize the +200 kHz channel, FM-demodulate it. This
    // test isolates DSP correctness, so squelch is disabled (the carrier is present
    // from sample 0, giving the squelch no noise reference to open against — squelch
    // behavior is covered by `squelch_mutes_noise_and_reports_snr`).
    let mut channelizer = Channelizer::new(FS);
    channelizer.add_channel(SSRC, CHANNEL_OFFSET);
    let mut fm_cfg = FmDemodConfig::new(AUDIO_FS);
    fm_cfg.squelch_open_db = -100.0;
    fm_cfg.squelch_close_db = -200.0;
    let mut demod = FmDemod::new(fm_cfg);

    // Run the wideband I/Q through the real channelizer + FM demod, collecting the
    // recovered 24 kHz audio. Feeding it as a single AudioBlock keeps this a
    // deterministic DSP-correctness proof; the real-time per-block cadence (and its
    // back-pressure) is exercised by the live SDR path, which sizes USB reads to one
    // block so blocks arrive at ~20 ms real time rather than in a burst.
    let mut recovered = Vec::new();
    for chunk in iq.chunks(262_144) {
        for block in channelizer.process(chunk) {
            recovered.extend_from_slice(&demod.process(&block.samples).audio);
        }
    }

    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<AudioBlock>(4);
    let mut packets = decode_audio_stream(DecoderConfig::default(), audio_rx);
    audio_tx
        .send(AudioBlock {
            ssrc: SSRC,
            sample_rate: AUDIO_FS,
            samples: recovered,
            signal: None,
        })
        .await
        .expect("send audio block");
    drop(audio_tx);

    let pkt = tokio::time::timeout(std::time::Duration::from_secs(10), packets.recv())
        .await
        .expect("decode did not complete within 10s")
        .expect("expected a decoded packet from the SDR DSP chain, got none");

    assert_eq!(pkt.source, "N0CALL");
    assert_eq!(pkt.destination, "APRS");
    assert_eq!(pkt.info, info);
    assert!((pkt.freq_mhz - 144.390).abs() < 1e-9);
    assert!(pkt.slicer_hits >= 1);
}
