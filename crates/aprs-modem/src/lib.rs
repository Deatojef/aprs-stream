//! `aprs-modem` — source-agnostic APRS decode core.
//!
//! Vendored from `aprs-rtp` (our own crate) with the RTP/socket front-end removed.
//! It consumes [`AudioBlock`]s — normalized mono `f32` audio tagged with an `ssrc`
//! — and yields fully-decoded [`AprsPacket`]s. The audio may come from anywhere
//! (an SDR channelizer, an RTP listener, a test vector); this crate does not know
//! or care. The decode chain is: AFSK demod → HDLC → AX.25 parse → TNC2 text.
//!
//! Primary entry point: [`decode_audio_stream`]. For single-channel/synchronous
//! use, drive [`StreamDecoder`] directly.

pub(crate) mod afsk;
pub(crate) mod aprs;
pub(crate) mod audio;
pub(crate) mod ax25;
pub mod config;
pub(crate) mod dsp;
pub mod error;
pub(crate) mod hdlc;
pub(crate) mod pipeline;

pub use audio::{AudioBlock, SignalMetrics};
pub use config::{DecoderConfig, FixBits};
pub use error::{Error, Result};
pub use pipeline::stream_decoder::StreamDecoder;

use std::time::SystemTime;
use tokio::sync::mpsc;

/// Audio level measurements at packet-decode time.
///
/// Reported on direwolf's familiar 0–~100 scale so values are directly
/// comparable to direwolf output, even though our internal audio is normalized
/// to the standard ±1.0 range (vs direwolf's ±2.0). The reporting constants in
/// `afsk::AfskDemodulator::audio_level` are doubled to compensate.
///
/// - `rec`   = `(raw_peak − raw_valley) × 100` — overall received level; ~200 for a full-scale 16-bit signal (peak-to-peak swing of 2.0 × 100).
/// - `mark`  = `mark_iq_peak × 200`             — 1200 Hz tone envelope.
/// - `space` = `space_iq_peak × 200`            — 2200 Hz tone envelope.
///
/// All three use a separate slower-tracking IIR (5× longer time constants than the
/// demodulation AGC) so values are stable across consecutive packets and can be
/// compared across different SSRCs on the same normalized audio scale.
///
/// Typical values for a well-adjusted APRS signal: rec 30–70, mark/space 10–40.
/// A pure full-scale tone yields mark/space ≈ 100 (IQ demodulation halves amplitude).
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioLevel {
    /// Overall received audio level (~100 = full-scale S16 audio).
    pub rec: u8,
    /// Mark-tone (1200 Hz) IQ envelope level.
    pub mark: u8,
    /// Space-tone (2200 Hz) IQ envelope level.
    pub space: u8,
}

/// A decoded APRS packet ready for downstream consumers.
#[derive(Debug, Clone)]
pub struct AprsPacket {
    /// Source channel identifier of the audio this packet was decoded from.
    /// By the ka9q-radio convention `ssrc = freq_kHz`, mapping 1:1 to a frequency.
    pub ssrc: u32,
    /// TNC2-format string: "SRC>DST,VIA,...:info"
    pub text: String,
    /// Validated AX.25 frame bytes excluding the FCS.
    /// All digipeater address H-bits are preserved for future heard-from analysis.
    pub raw_ax25: Vec<u8>,
    /// Wall-clock time the packet was decoded.
    pub received_at: SystemTime,
    /// Lowest-indexed slicer that successfully decoded this frame.
    pub first_slice: usize,
    /// Number of slicers (out of the configured total) that independently decoded
    /// this same frame within the same audio block.  Higher = stronger/cleaner signal.
    /// May undercount if slicers finish the frame across an audio-block boundary.
    /// Equal to `slicer_mask.count_ones()`.
    pub slicer_hits: u8,
    /// Audio levels at decode time, normalized for cross-packet and cross-SSRC comparison.
    pub audio_level: AudioLevel,
    /// Tuned frequency in MHz, derived from the SSRC (ka9q-radio convention:
    /// SSRC = frequency in kHz, so `freq_mhz = ssrc / 1000.0`).
    pub freq_mhz: f64,
    /// Source callsign-SSID (e.g. "WA0DE-9").
    pub source: String,
    /// Destination callsign-SSID (the AX.25 "to" address; APRS encodes
    /// equipment/software type here, e.g. "APDR15", "APAT51").
    pub destination: String,
    /// Digipeater path callsigns in order (excluding source and destination).
    pub via: Vec<String>,
    /// Parallel to `via`: true if that digipeater's H-bit ("has been
    /// repeated") is set in the received frame. This is what the TNC2 `*`
    /// marker after a callsign represents.
    pub via_heard: Vec<bool>,
    /// True if no digipeater H-bits are set — i.e. the source transmitter
    /// reached our receiver directly, not via any repeater hop.
    pub heard_direct: bool,
    /// The station whose signal physically reached our receiver: the last
    /// digipeater with its H-bit set, or the source callsign when
    /// `heard_direct` is true.
    pub heard_from: String,
    /// Bitmask of slicers that decoded this frame; see `slicer_hits`.
    pub slicer_mask: u16,
    /// APRS Data Type Identifier — the first byte of the info field. `None`
    /// only for the unusual empty-info UI frame.
    pub dti: Option<u8>,
    /// Raw info-field bytes (everything after the AX.25 control + PID). May
    /// contain non-ASCII bytes for Mic-E and binary telemetry payloads.
    pub info: Vec<u8>,
    /// Count of bytes in `info` that are almost certainly not real APRS payload:
    /// C0 control bytes (other than tab/CR/LF) plus any invalid-UTF-8 bytes.
    /// `0` for a clean frame. Advisory only — the raw bytes are left untouched.
    pub info_invalid_bytes: usize,
    /// RF signal quality (SNR / relative strength) measured by the audio source at
    /// capture time, taken from the `AudioBlock` in which this frame completed.
    /// `None` when the source didn't provide it. Flows to downstream consumers.
    pub signal: Option<SignalMetrics>,
}

/// Spawn the source-agnostic decode pipeline and return a channel of decoded
/// packets.
///
/// The analog of `aprs-rtp`'s `AprsListener::run`, minus the socket: instead of
/// joining an RTP group it consumes `audio_rx`. Each distinct `ssrc` seen on the
/// stream gets its own per-channel [`StreamDecoder`] running on a blocking DSP
/// thread. The returned receiver stays open until `audio_rx` closes.
pub fn decode_audio_stream(
    decoder: DecoderConfig,
    audio_rx: mpsc::Receiver<AudioBlock>,
) -> mpsc::Receiver<AprsPacket> {
    let (aprs_tx, aprs_rx) = mpsc::channel::<AprsPacket>(256);
    tokio::spawn(async move {
        pipeline::manager::run_blocks(audio_rx, decoder, aprs_tx).await;
    });
    aprs_rx
}
