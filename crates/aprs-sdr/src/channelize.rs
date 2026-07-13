//! Overlap-save fast-convolution channelizer — a Rust port of the essential DSP
//! in ka9q-radio's `filter.c`.
//!
//! One wide **forward** FFT of the complex SDR input is computed per block and
//! shared across all channels (the whole CPU win). Each channel then extracts a
//! contiguous slice of master bins, circularly shifted so the channel centre
//! lands at DC, multiplies by a per-channel frequency response, and runs a small
//! **inverse** FFT to produce a decimated complex baseband stream. Adding
//! channels only adds a bin-slice + small IFFT each — the forward FFT is paid once.
//!
//! Sizing (ka9q identities), with `Fs = 1.2 MSPS`, blocktime 20 ms, overlap 5:
//!   `L  = Fs·blocktime      = 24000`  new input samples per block
//!   `M  = L/(overlap-1)+1   = 6001`   filter length; `M-1 = 6000` overlap
//!   `N  = L + M - 1         = 30000`  forward FFT size (= 2⁴·3·5⁴, small-prime)
//!   `olen = audio·blocktime = 480`    valid decimated samples per block per channel
//!   `Ns = olen·N/L          = 600`    inverse FFT size
//!   `D  = N/Ns = Fs/audio   = 50`     decimation (exact integer)
//!
//! Absolute gain is intentionally not normalized: the downstream FM discriminator
//! is phase-based (amplitude-invariant) and the AFSK demod has its own AGC, so
//! only the *frequency selection and shape* matter here.

use std::sync::Arc;

use num_complex::Complex;
use rustfft::{Fft, FftPlanner};

const BLOCKTIME: f64 = 0.02;
const OVERLAP: usize = 5;
/// Audio output rate every channel is decimated to (matches ka9q NBFM default).
pub const AUDIO_RATE: u32 = 24_000;

/// One decimated channel output for a single block.
pub struct ChannelBlock {
    /// Source channel id (ka9q convention `ssrc = freq_kHz`).
    pub ssrc: u32,
    /// `olen` complex baseband samples at [`AUDIO_RATE`].
    pub samples: Vec<Complex<f32>>,
}

/// Per-channel state: frequency offset, response window, and inverse FFT.
struct Channel {
    ssrc: u32,
    /// Circular bin shift that moves this channel's centre to DC.
    shift: isize,
    /// Frequency response magnitude per slave bin `k` (length `ns`).
    response: Vec<f32>,
    inv: Arc<dyn Fft<f32>>,
    /// Reusable slave spectrum / time buffer (length `ns`).
    slave: Vec<Complex<f32>>,
}

pub struct Channelizer {
    fs: f64,
    n: usize,
    l: usize,
    m1: usize, // M-1, the overlap length
    ns: usize,
    olen: usize,
    fwd: Arc<dyn Fft<f32>>,
    /// Overlap carried from the previous block (length `m1`).
    overlap: Vec<Complex<f32>>,
    /// New samples accumulated toward the next `L`-sample block.
    pending: Vec<Complex<f32>>,
    /// Reusable master block / spectrum buffer (length `n`).
    block: Vec<Complex<f32>>,
    channels: Vec<Channel>,
}

impl Channelizer {
    /// Build a channelizer for a complex input at `fs` samples/sec.
    pub fn new(fs: f64) -> Self {
        let l = (fs * BLOCKTIME).round() as usize;
        let m1 = l / (OVERLAP - 1); // M-1
        let n = l + m1;
        let d = (fs / AUDIO_RATE as f64).round() as usize;
        let ns = n / d;
        let olen = l / d;

        let mut planner = FftPlanner::new();
        let fwd = planner.plan_fft_forward(n);

        Self {
            fs,
            n,
            l,
            m1,
            ns,
            olen,
            fwd,
            overlap: vec![Complex::new(0.0, 0.0); m1],
            pending: Vec::with_capacity(l),
            block: vec![Complex::new(0.0, 0.0); n],
            channels: Vec::new(),
        }
    }

    /// Register a channel tuned `offset_hz` away from the SDR centre frequency.
    /// `ssrc` tags every block this channel produces.
    pub fn add_channel(&mut self, ssrc: u32, offset_hz: f64) {
        let hz_per_bin = self.fs / self.n as f64;
        let shift = (offset_hz / hz_per_bin).round() as isize;

        // Response window, indexed by slave bin. Slave bin k maps to signed master
        // offset kk (in bins → Hz); pass ±fpass flat, raised-cosine to zero by fstop
        // (kept below the ±ns/2 slice edge so nothing wraps/aliases in).
        let fpass = 7_000.0;
        let fstop = 11_500.0;
        let half = self.ns as isize / 2;
        let response = (0..self.ns)
            .map(|k| {
                let kk = if (k as isize) < half {
                    k as isize
                } else {
                    k as isize - self.ns as isize
                };
                let f = (kk as f64 * hz_per_bin).abs();
                let mag = if f <= fpass {
                    1.0
                } else if f >= fstop {
                    0.0
                } else {
                    0.5 * (1.0 + (std::f64::consts::PI * (f - fpass) / (fstop - fpass)).cos())
                };
                mag as f32
            })
            .collect();

        let mut planner = FftPlanner::new();
        let inv = planner.plan_fft_inverse(self.ns);

        self.channels.push(Channel {
            ssrc,
            shift,
            response,
            inv,
            slave: vec![Complex::new(0.0, 0.0); self.ns],
        });
    }

    /// Valid decimated samples produced per channel per block.
    pub fn olen(&self) -> usize {
        self.olen
    }

    /// Feed complex input samples. Returns zero or more `ChannelBlock`s — one per
    /// channel for each complete `L`-sample input block that became available.
    pub fn process(&mut self, input: &[Complex<f32>]) -> Vec<ChannelBlock> {
        let mut out = Vec::new();
        self.pending.extend_from_slice(input);

        while self.pending.len() >= self.l {
            // Assemble the N-sample overlap-save window: [previous M-1 | new L].
            self.block[..self.m1].copy_from_slice(&self.overlap);
            self.block[self.m1..].copy_from_slice(&self.pending[..self.l]);

            // Carry the last M-1 of this block's new data as the next overlap.
            self.overlap
                .copy_from_slice(&self.pending[self.l - self.m1..self.l]);
            self.pending.drain(..self.l);

            // One shared forward FFT (in place; `block` now holds the master spectrum).
            self.fwd.process(&mut self.block);

            for ch in &mut self.channels {
                out.push(extract_channel(
                    ch,
                    &self.block,
                    self.n,
                    self.ns,
                    self.olen,
                ));
            }
        }
        out
    }
}

/// Slice the master spectrum for one channel, shape it, inverse-FFT, and drop the
/// contaminated overlap-save prefix — leaving `olen` valid decimated samples.
fn extract_channel(
    ch: &mut Channel,
    master: &[Complex<f32>],
    n: usize,
    ns: usize,
    olen: usize,
) -> ChannelBlock {
    let half = ns as isize / 2;
    for k in 0..ns {
        let kk = if (k as isize) < half {
            k as isize
        } else {
            k as isize - ns as isize
        };
        let bin = (ch.shift + kk).rem_euclid(n as isize) as usize;
        ch.slave[k] = master[bin] * ch.response[k];
    }

    ch.inv.process(&mut ch.slave);

    // Overlap-save: the first (Ns - olen) samples are circular-convolution garbage.
    // Scale by 1/N to undo the unnormalized forward/inverse FFT pair, so a
    // full-scale input maps to ~unit baseband — the FM demod is scale-invariant, but
    // this keeps the reported RSSI in a sane dBFS range.
    let scale = 1.0 / n as f32;
    let samples = ch.slave[ns - olen..].iter().map(|c| c * scale).collect();
    ChannelBlock {
        ssrc: ch.ssrc,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 1_200_000.0;

    /// A complex exponential at the channel's tuned offset should land in-band;
    /// one far outside the passband should be strongly rejected.
    #[test]
    fn passes_in_band_rejects_out_of_band() {
        let offset = 200_000.0;
        let n_samples = 24_000 * 4; // several blocks

        let power_at = |tone_hz: f64| -> f32 {
            let mut ch = Channelizer::new(FS);
            ch.add_channel(144_390, offset);
            let mut input = Vec::with_capacity(n_samples);
            let mut phase = 0.0f64;
            let dphi = 2.0 * std::f64::consts::PI * tone_hz / FS;
            for _ in 0..n_samples {
                input.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
                phase += dphi;
            }
            let blocks = ch.process(&input);
            // Average per-sample power over the last block (steady state).
            let last = blocks.last().expect("at least one block");
            let sum: f32 = last.samples.iter().map(|c| c.norm_sqr()).sum();
            sum / last.samples.len() as f32
        };

        let in_band = power_at(offset + 1_000.0); // 1 kHz from channel centre
        let out_band = power_at(offset + 60_000.0); // well outside the passband
        assert!(
            in_band > out_band * 1_000.0,
            "expected strong rejection: in_band={in_band}, out_band={out_band}"
        );
    }

    #[test]
    fn sizing_matches_ka9q_identities() {
        let ch = Channelizer::new(FS);
        assert_eq!(ch.l, 24_000);
        assert_eq!(ch.m1, 6_000);
        assert_eq!(ch.n, 30_000);
        assert_eq!(ch.ns, 600);
        assert_eq!(ch.olen, 480);
    }
}
