//! Wire schema for decoded APRS frames.
//!
//! This module is the single source of truth that makes typed round-tripping
//! valid: CBOR bytes do not carry Rust type identity, so producer and consumers
//! must share these exact definitions. Keep it free of any igate/tracker/app
//! policy — it is framing only.
//!
//! The parsed payload is `aprs_decode`'s own [`aprs_decode::AprsPacket`] (from /
//! to / via / data), reused directly so the typed payload schema has a single
//! source of truth and the producer never hand-maps a lossy subset. `aprs-decode`
//! is pure parse logic with no network or audio concerns, so depending on it here
//! does not violate the "framing and transport only" rule for this crate.
//!
//! Compatibility rules (see CLAUDE.md "Schema design notes"):
//! - `version` is the first field, present from day one.
//! - New/optional fields get `#[serde(default)]` so old consumers tolerate new
//!   producers; never enable `deny_unknown_fields`.
//! - Fields that aren't always measured are `Option<T>`.
//! - Raw AX.25 always rides as a true CBOR byte string via `serde_bytes`.

use serde::{Deserialize, Serialize};

/// Current wire-format version. Bump on any breaking schema change — including
/// any change to `aprs-decode`'s `AprsPacket`/`AprsData` representation, since
/// that is now part of this wire format.
pub const PROTOCOL_VERSION: u8 = 1;

/// One fully-decoded APRS frame, as published on the wire (one per datagram).
///
/// The producer is policy-free: every frame from the decoder is emitted, each
/// tagged with quality metadata. Consumers decide policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AprsFrame {
    /// Wire-format version tag. Always first. See [`PROTOCOL_VERSION`].
    pub version: u8,

    /// Capture-side provenance (when/where this was received).
    pub capture: CaptureMeta,

    /// RF / signal-quality metadata. Individual metrics are optional.
    pub rf: RfMeta,

    /// Whether the AX.25 frame's FCS (CRC) validated. The current producer
    /// (aprs-rtp) only emits FCS-valid frames, so this is `true` today; the
    /// field exists so a future producer can publish failures for SNR/
    /// propagation logging (decision #6) without a schema change.
    pub crc_ok: bool,

    /// Raw AX.25 frame bytes (FCS excluded), always present so nothing is ever
    /// lost. Encoded as a true CBOR byte string (no base64 / array-of-ints
    /// bloat). This is the lossless source of truth even when `parsed` is `None`.
    #[serde(with = "serde_bytes")]
    pub ax25: Vec<u8>,

    /// The parsed APRS packet (source/dest/path + typed payload), so consumers
    /// `match` instead of re-parsing. `None` when the frame is FCS-valid but
    /// `aprs-decode` could not parse it — the raw `ax25` is still present.
    #[serde(default)]
    pub parsed: Option<aprs_decode::AprsPacket>,
}

impl AprsFrame {
    /// Construct a frame stamped with the current [`PROTOCOL_VERSION`].
    pub fn new(
        capture: CaptureMeta,
        rf: RfMeta,
        crc_ok: bool,
        ax25: Vec<u8>,
        parsed: Option<aprs_decode::AprsPacket>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            capture,
            rf,
            crc_ok,
            ax25,
            parsed,
        }
    }
}

/// When and by what this frame was captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureMeta {
    /// Receive timestamp, epoch milliseconds (UTC). Explicit `u64` for
    /// cross-language portability — never `SystemTime` on the wire.
    pub received_at_ms: u64,

    /// Receiver / host provenance (e.g. the ka9q-radio multicast host).
    #[serde(default)]
    pub receiver: Option<String>,

    /// Decoder provenance (e.g. "aprs-rtp/0.2.0").
    #[serde(default)]
    pub decoder: Option<String>,

    /// ka9q-radio SSRC of the source audio channel (maps 1:1 to a frequency),
    /// if known. Lets a logger attribute frames to a specific channel.
    #[serde(default)]
    pub ssrc: Option<u32>,
}

/// RF / signal-quality metadata. Every metric is optional because not all are
/// available from a given decoder/receiver.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RfMeta {
    /// Channel center frequency in Hz, if known.
    #[serde(default)]
    pub frequency_hz: Option<u64>,

    /// Signal-to-noise ratio in dB, if measured. Not provided by the current
    /// producer; reserved for future decoders.
    #[serde(default)]
    pub snr_db: Option<f32>,

    /// Audio level measurements at decode time (direwolf-comparable scale).
    #[serde(default)]
    pub audio_level: Option<AudioLevel>,

    /// Number of independent slicers that decoded this frame. Higher = stronger/
    /// cleaner signal; this is the multi-slicer diversity metric a propagation
    /// logger wants.
    #[serde(default)]
    pub slicer_hits: Option<u8>,

    /// Bitmask of which slicers decoded this frame (bit `i` = slicer `i`).
    #[serde(default)]
    pub slicer_mask: Option<u16>,
}

/// Audio level measurements at packet-decode time, on direwolf's familiar
/// 0–~100 scale (mirrors `aprs_rtp::AudioLevel`, redefined here so the wire
/// schema stays self-contained).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioLevel {
    /// Overall received audio level (~100 = full-scale S16 audio).
    pub rec: u8,
    /// Mark-tone (1200 Hz) IQ envelope level.
    pub mark: u8,
    /// Space-tone (2200 Hz) IQ envelope level.
    pub space: u8,
}
