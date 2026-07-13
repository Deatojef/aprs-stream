//! The audio hand-off type between an audio source and the decode pipeline.
//!
//! An `AudioBlock` is the sole seam the decoder consumes: normalized mono `f32`
//! samples at a known rate, tagged with the source channel id (`ssrc`) and an
//! optional RF signal-quality measurement. It is deliberately source-agnostic —
//! the samples may originate from an RTP stream, an SDR channelizer, or a test
//! vector. Relocated (unchanged) from `aprs-rtp`'s `rtp::session`, then extended
//! with the optional [`SignalMetrics`] the SDR front-end measures.

/// RF signal-quality measured by the audio source (e.g. the SDR FM stage), to be
/// carried through the decode and attached to the resulting packet's metadata.
#[derive(Debug, Clone, Copy)]
pub struct SignalMetrics {
    /// Estimated in-channel SNR in dB — instantaneous channel power relative to a
    /// slowly-tracked noise floor. Scale-invariant (independent of channelizer or
    /// RTL front-end gain): ~0 dB means noise only, higher means a stronger carrier
    /// above the floor.
    pub snr_db: f32,
    /// Relative channel power in dBFS. Uncalibrated — it depends on the channelizer
    /// gain and any RTL AGC — so it is meaningful for comparing signal strength
    /// across packets on the same configuration, not as an absolute dBm.
    pub rssi_dbfs: f32,
}

#[derive(Debug, Clone)]
pub struct AudioBlock {
    /// Source channel identifier. By the ka9q-radio convention `ssrc = freq_kHz`,
    /// so `freq_mhz = ssrc / 1000.0`. Each distinct `ssrc` gets its own decoder.
    pub ssrc: u32,
    /// Sample rate of `samples`, in Hz (e.g. 24000).
    pub sample_rate: u32,
    /// Normalized f32 samples in [-1.0, 1.0].
    pub samples: Vec<f32>,
    /// Optional RF signal quality measured by the source for this block. `None`
    /// when the source doesn't measure it (e.g. a plain RTP or test feed).
    pub signal: Option<SignalMetrics>,
}
