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
///
/// - `1`: initial schema (capture/rf/crc_ok/ax25/parsed).
/// - `2`: added [`Ax25Meta`] framing block (`ax25_meta`) and the per-frame slicer
///   gain ladder (`RfMeta::slicer_gains`). Both additive and `#[serde(default)]`,
///   so `1`-era consumers still decode `2` frames; the bump is honest signposting.
pub const PROTOCOL_VERSION: u8 = 2;

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

    /// Whether the AX.25 frame's FCS (CRC) validated. Both current producers
    /// (the direct-SDR and RTP capture paths) only emit FCS-valid frames, so this
    /// is `true` today; the field exists so a future producer can publish failures
    /// for SNR/propagation logging without a schema change.
    pub crc_ok: bool,

    /// Raw AX.25 frame bytes (FCS excluded), always present so nothing is ever
    /// lost. Encoded as a true CBOR byte string (no base64 / array-of-ints
    /// bloat). This is the lossless source of truth even when `parsed` is `None`.
    #[serde(with = "serde_bytes")]
    pub ax25: Vec<u8>,

    /// AX.25-layer framing facts (source/dest/path/heard/dti + info offset),
    /// decoded once by the producer so consumers never re-parse the frame to
    /// recover them. Independent of APRS payload parseability — present even when
    /// `parsed` is `None`. `#[serde(default)]` so `1`-era frames (which lack it)
    /// decode to `None`.
    #[serde(default)]
    pub ax25_meta: Option<Ax25Meta>,

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
        ax25_meta: Option<Ax25Meta>,
        parsed: Option<aprs_decode::AprsPacket>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            capture,
            rf,
            crc_ok,
            ax25,
            ax25_meta,
            parsed,
        }
    }
}

/// AX.25-layer framing facts, decoded once by the producer so consumers never
/// re-parse the frame to recover them. Present even when the APRS payload could
/// not be parsed (`AprsFrame::parsed == None`), which is exactly when a consumer
/// would otherwise be forced to re-decode the raw AX.25 itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ax25Meta {
    /// Source callsign-SSID (e.g. "WA0DE-9").
    pub source: String,

    /// Destination / AX.25 "to" address (APRS tocall, e.g. "APDR15").
    pub destination: String,

    /// Digipeater path in order, each with its has-been-repeated (H-bit) flag —
    /// the TNC2 `*` marker. Excludes source and destination.
    #[serde(default)]
    pub via: Vec<ViaHop>,

    /// True iff no digipeater H-bits are set: heard directly, no repeater hop.
    pub heard_direct: bool,

    /// Station whose signal physically reached the receiver: the last H-bit-set
    /// digipeater, or the source callsign when `heard_direct`.
    pub heard_from: String,

    /// APRS Data Type Identifier — the first info-field byte. `None` only for the
    /// unusual empty-info UI frame.
    #[serde(default)]
    pub dti: Option<u8>,

    /// Byte offset of the info field within [`AprsFrame::ax25`] (after the address
    /// field + control + PID). Lets a consumer take the verbatim 8-bit info
    /// payload as `ax25[info_offset..]` with no AX.25 re-parsing — needed for
    /// byte-faithful igating. `None` if the producer couldn't determine it.
    #[serde(default)]
    pub info_offset: Option<u32>,

    /// Advisory count of info bytes almost certainly not real APRS payload (C0
    /// control bytes plus invalid UTF-8, e.g. a stuck transmitter's trailing
    /// `0xFF`). `0` for a clean frame. The frame is still FCS-valid and the raw
    /// bytes are untouched — a consumer may downrank/refuse a suspect frame.
    #[serde(default)]
    pub info_invalid_bytes: u32,
}

/// One digipeater-path element with its AX.25 has-been-repeated (H-bit) flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViaHop {
    /// The path callsign-SSID (e.g. "W1XYZ-1", "WIDE2-1").
    pub call: String,
    /// True if this hop's H-bit is set (it repeated the frame): the TNC2 `*`.
    pub heard: bool,
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

    /// Per-slicer space-gain ladder the producer's decoder is running: linear
    /// gain per slicer, ordered by slicer index. Static for a producer session,
    /// but carried on every frame so a consumer joining the stream mid-flight is
    /// never missing it (there is no producer-side connection state to replay it).
    /// A slicer-diversity waterfall gets its column count from `slicer_gains.len()`
    /// and each column's twist label as `20 * log10(gain)`. `None` if the producer
    /// didn't supply it.
    #[serde(default)]
    pub slicer_gains: Option<Vec<f32>>,
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
