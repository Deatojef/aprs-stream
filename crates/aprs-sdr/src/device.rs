//! RTL-SDR front-end: open the dongle, configure it, and stream raw I/Q.
//!
//! Thin wrapper over `rtl-sdr-rs` (pure-Rust, over libusb). The dongle delivers
//! interleaved unsigned-8-bit I/Q; [`bytes_to_iq`] converts to normalized
//! `Complex<f32>`. Reads are synchronous and belong on a blocking thread.

use num_complex::Complex;
use rtl_sdr_rs::{DeviceId, RtlSdr, TunerGain};

/// USB bulk reads must be a multiple of 512 bytes. Sized to ~one 20 ms
/// channelizer block at 1.2 MSPS (24 064 complex samples), so each read yields
/// roughly one block and the decode pipeline self-paces at real time — mirroring
/// ka9q/RTP's 20 ms cadence and avoiding bursts that would overrun the per-channel
/// decoder queue. `94 * 512 = 48 128` bytes = 24 064 I/Q samples.
pub const READ_BYTES: usize = 94 * 512;

pub struct SdrConfig {
    pub device_index: usize,
    /// Centre (tuner) frequency in Hz.
    pub center_freq: u32,
    /// Complex sample rate in Hz (e.g. 1_200_000).
    pub sample_rate: u32,
    /// Manual tuner gain in tenths of a dB, or `None` for hardware AGC.
    pub gain_tenths_db: Option<i32>,
    /// Frequency correction in ppm.
    pub freq_correction_ppm: i32,
}

pub struct RtlSdrSource {
    sdr: RtlSdr,
}

impl RtlSdrSource {
    pub fn open(cfg: &SdrConfig) -> rtl_sdr_rs::error::Result<Self> {
        let mut sdr = RtlSdr::open(DeviceId::Index(cfg.device_index))?;
        sdr.set_sample_rate(cfg.sample_rate)?;
        sdr.set_center_freq(cfg.center_freq)?;
        if cfg.freq_correction_ppm != 0 {
            sdr.set_freq_correction(cfg.freq_correction_ppm)?;
        }
        match cfg.gain_tenths_db {
            Some(g) => sdr.set_tuner_gain(TunerGain::Manual(g))?,
            None => sdr.set_tuner_gain(TunerGain::Auto)?,
        }
        sdr.reset_buffer()?;
        Ok(Self { sdr })
    }

    /// Blocking read of raw interleaved u8 I/Q into `buf`; returns bytes read.
    pub fn read(&self, buf: &mut [u8]) -> rtl_sdr_rs::error::Result<usize> {
        self.sdr.read_sync(buf)
    }

    pub fn close(mut self) -> rtl_sdr_rs::error::Result<()> {
        self.sdr.close()
    }
}

/// Convert interleaved u8 I/Q bytes to normalized `Complex<f32>` in ~[-1, 1],
/// appending to `out`. The RTL-SDR zero level is 127.4.
pub fn bytes_to_iq(buf: &[u8], out: &mut Vec<Complex<f32>>) {
    out.reserve(buf.len() / 2);
    for pair in buf.chunks_exact(2) {
        let i = (pair[0] as f32 - 127.4) / 127.4;
        let q = (pair[1] as f32 - 127.4) / 127.4;
        out.push(Complex::new(i, q));
    }
}
