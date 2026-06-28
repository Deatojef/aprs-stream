//! `aprs-stream` — shared schema, CBOR codec, and UDP transport for the
//! disaggregated APRS pipeline.
//!
//! This crate is the single source of truth that both the `aprs-streamd`
//! producer and all downstream consumers depend on. It is framing and transport
//! only — no igate/tracker/app policy lives here.
//!
//! - [`proto`] — the versioned [`AprsFrame`] schema and nested types.
//! - [`codec`] — `ciborium` encode/decode plus a `cbor -> json` debug helper.
//! - [`emit`] — connectionless multicast/unicast emitter (per-destination list).
//! - [`subscribe`] — multicast join + recv + decode helper.

pub mod codec;
pub mod emit;
pub mod proto;
pub mod subscribe;

// Re-export the parser crate so consumers can `match` on the typed payload
// (`aprs_stream::aprs_decode::AprsData`) without taking a separate dependency.
pub use aprs_decode;

pub use codec::{decode, encode, to_json, CodecError};
pub use emit::{EmitConfig, EmitError, Emitter};
pub use proto::{AprsFrame, AudioLevel, CaptureMeta, RfMeta, PROTOCOL_VERSION};
pub use subscribe::{RecvError, SubscribeConfig, Subscriber};
