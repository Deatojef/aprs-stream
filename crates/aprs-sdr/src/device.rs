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

/// Starting gain for [`Gain::Auto`] before the software manager converges.
pub const AUTO_START_TENTHS: i32 = 300;

/// Tuner gain mode.
///
/// All of these drive the R820T2's *analog* LNA + mixer gain (ahead of the ADC);
/// the IF/VGA is pinned by the driver.
#[derive(Debug, Clone, Copy)]
pub enum Gain {
    /// Fixed tuner gain in tenths of a dB (e.g. 400 = 40 dB). Values above the
    /// tuner's ceiling (~496) are clamped by the driver.
    Manual(i32),
    /// Software gain manager: hold the measured noise floor at a setpoint by
    /// stepping the *fixed* gain slowly. Preferred over [`Gain::HardwareAgc`] —
    /// we control the time constants and it keys off the noise floor rather than
    /// chasing individual transmissions.
    Auto,
    /// The tuner's own hardware AGC. Rarely what you want for monitoring: it runs
    /// ~10 dB more fixed IF gain, reacts to total power across the whole captured
    /// span (so one strong signal desenses every channel), and is tuned for
    /// continuous broadcast rather than packet bursts. Kept for A/B testing.
    HardwareAgc,
}

pub struct SdrConfig {
    pub device_index: usize,
    /// Centre (tuner) frequency in Hz.
    pub center_freq: u32,
    /// Complex sample rate in Hz (e.g. 1_200_000).
    pub sample_rate: u32,
    /// Tuner gain mode.
    pub gain: Gain,
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
        match cfg.gain {
            Gain::Manual(g) => sdr.set_tuner_gain(TunerGain::Manual(g))?,
            // The software manager starts from a mid-scale fixed gain and converges
            // from there; it must never engage the tuner's own AGC.
            Gain::Auto => sdr.set_tuner_gain(TunerGain::Manual(AUTO_START_TENTHS))?,
            Gain::HardwareAgc => sdr.set_tuner_gain(TunerGain::Auto)?,
        }
        sdr.reset_buffer()?;
        Ok(Self { sdr })
    }

    /// Blocking read of raw interleaved u8 I/Q into `buf`; returns bytes read.
    pub fn read(&self, buf: &mut [u8]) -> rtl_sdr_rs::error::Result<usize> {
        self.sdr.read_sync(buf)
    }

    /// Change the fixed tuner gain at runtime (used by the software gain manager).
    pub fn set_gain_tenths(&mut self, tenths: i32) -> rtl_sdr_rs::error::Result<()> {
        self.sdr.set_tuner_gain(TunerGain::Manual(tenths))
    }

    /// The tuner's supported discrete gain values, in tenths of a dB (ascending).
    pub fn tuner_gains(&self) -> rtl_sdr_rs::error::Result<Vec<i32>> {
        self.sdr.get_tuner_gains()
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
