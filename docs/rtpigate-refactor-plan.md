# Plan: Refactor rtpigate into a thin aprs-stream consumer

## Context

`rtpigate` is the existing monolithic APRS igate. It owns the full chain itself:
subscribe to ka9q-radio RTP audio → demodulate AFSK → decode AX.25 → parse APRS →
filter → gate to APRS-IS, plus a live web dashboard. It works well and is fast.

The whole point of the `aprs-stream` project is to do the decode work **once**, in
`aprs-streamd`, and publish fully-decoded typed frames on the LAN so downstream
apps stop re-owning the RTP-listen → decode chain. Refactoring `rtpigate` to
consume that stream is the reference payoff of the disaggregation — and the first
real-world test of whether the `AprsFrame` schema is rich enough for a demanding
consumer.

This plan covers that refactor. It has two parts, in order:

1. **Enrich the `aprs-stream` schema** so an `AprsFrame` carries the AX.25-framing
   facts a consumer needs, independent of payload parseability. (Load-bearing; it
   stands alone and benefits every future consumer.)
2. **Port `rtpigate`** to subscribe to the multicast stream instead of running its
   own `aprs-rtp` listener.

Repos involved:
- `aprs-stream` (this repo) — schema + producer change.
- `rtpigate` (cloned at `../rtpigate`) — the consumer port.

`aprs-rtp` and `aprs-decode` are unchanged (crates.io deps).

## What was verified during exploration

- **Blast radius in rtpigate is small.** Only two of ten modules touch `aprs-rtp`:
  `ka9q.rs` (the RTP listener + `map_packet`) and `config.rs` (the `[decoder]`
  knobs). Everything else — `aprs_is.rs`, `igate.rs`, `gpsd.rs`, `sse.rs`,
  `history.rs`, `main.rs`, and the whole frontend — operates on rtpigate's
  internal `RTPPacket` / `DataItem` types, which do **not** change.
- **The producer already computes everything and discards it.** In
  `aprs-streamd/src/main.rs::map_frame`, the `aprs_rtp::AprsPacket` already carries
  `heard_direct`, `heard_from`, `via_heard`, `dti`, and `info_invalid_bytes`. The
  current `AprsFrame` simply doesn't copy them onto the wire.
- **H-bits survive into `parsed`, but only when parsing succeeds.**
  `aprs_decode::Digipeater::Callsign(callsign, heard)` carries the has-been-repeated
  flag, populated from the real AX.25 H-bit when decoded via `decode_ax25`
  (which `aprs-streamd` does use). But `parsed` is `None` whenever an FCS-valid
  frame's payload can't be parsed — and in that case all source/dest/path/heard
  facts vanish from the frame, even though the producer knew them.

## The schema gap (the crux)

`rtpigate::map_packet` extracts these from `aprs_rtp::AprsPacket`:

| rtpigate field | Source in aprs-rtp | Recoverable from current `AprsFrame`? |
|---|---|---|
| source, destination | `source`, `destination` | ✅ via `parsed.from` / `parsed.to` |
| via / path, digipeater_path, hops | `via` | ✅ via `parsed.via` |
| position / alt / symbol / object | (re-parse) | ✅ via `parsed.data` |
| frequency (MHz) | `freq_mhz` | ✅ via `rf.frequency_hz` (or `capture.ssrc`) |
| slicer_mask, slicer_hits, audio_level | same | ✅ via `rf` |
| receivetime | `received_at` | ✅ via `capture.received_at_ms` |
| **was_digipeated, heard_direct, heard_from, per-hop H-bits** | `via_heard`, `heard_direct`, `heard_from` | ⚠️ only inside `parsed.via`, i.e. **only when `parsed` is `Some`** |
| **info_bytes (verbatim 8-bit, byte-faithful igating)** | `info` | ⚠️ extractable from raw `ax25`, but requires re-parsing the AX.25 info boundary |
| **info_invalid_bytes (garble advisory)** | `info_invalid_bytes` | ❌ **lost** — not carried at all |
| ptype / dti, TNC2 `text` | `dti`, `text` | ⚠️ derivable, minor |

Two structural problems behind the ⚠️/❌ rows:

1. **AX.25-layer facts ride inside `parsed`.** When a frame is FCS-valid but the
   APRS payload won't parse (`parsed: None`), a consumer loses source/dest/path/
   heard entirely unless it re-parses `ax25` itself — exactly the per-consumer
   re-parsing the disaggregation exists to kill (CLAUDE.md decision #6: decode
   once, emit everything tagged, consumers decide policy).
2. **The producer discards data it already has.** Carrying these costs the producer
   almost nothing — it's copying fields already present on the `aprs_rtp::AprsPacket`.

## Part 1 — Enrich the `aprs-stream` schema

Add an AX.25-framing metadata block to `AprsFrame`, populated by the producer from
the fields `aprs-rtp` already provides, so consumers never re-derive AX.25-layer
facts and never depend on payload parseability for framing.

### Schema change (`crates/aprs-stream/src/proto.rs`)

Add a new optional block. Proposed shape:

```rust
/// AX.25-layer framing facts, decoded once by the producer so consumers never
/// re-parse the frame to recover them. Independent of APRS payload parseability:
/// present even when `parsed` is `None`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Ax25Meta {
    /// Source callsign-SSID (e.g. "WA0DE-9").
    pub source: String,
    /// Destination / AX.25 "to" address (APRS tocall, e.g. "APDR15").
    pub destination: String,
    /// Digipeater path in order, each with its has-been-repeated (H-bit) flag.
    /// The `bool` is the TNC2 `*` marker. Excludes source and destination.
    #[serde(default)]
    pub via: Vec<ViaHop>,
    /// True iff no digipeater H-bits are set (heard directly, no repeater hop).
    pub heard_direct: bool,
    /// Station whose signal physically reached the receiver: the last H-bit-set
    /// digipeater, or the source when `heard_direct`.
    pub heard_from: String,
    /// APRS Data Type Identifier (first info-field byte). `None` for empty-info UI.
    #[serde(default)]
    pub dti: Option<u8>,
    /// Byte offset of the info field within `ax25` (after addresses + control +
    /// PID). Lets a consumer slice the verbatim 8-bit info payload with no
    /// AX.25 re-parsing — needed for byte-faithful igating.
    #[serde(default)]
    pub info_offset: Option<u32>,
    /// Advisory count of info bytes that are almost certainly not real APRS
    /// payload (C0 controls + invalid UTF-8). 0 for a clean frame. Frame is still
    /// FCS-valid; raw bytes untouched. Mirrors aprs-rtp's `info_invalid_bytes`.
    #[serde(default)]
    pub info_invalid_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViaHop {
    pub call: String,
    pub heard: bool,
}
```

Add to `AprsFrame`:

```rust
    /// AX.25-layer framing facts (source/dest/path/heard/dti), decoded once by
    /// the producer. `#[serde(default)]` so older consumers tolerate its absence
    /// and older frames (pre-enrichment) deserialize with an empty block.
    #[serde(default)]
    pub ax25_meta: Option<Ax25Meta>,
```

Design notes:
- **Additive & backward-compatible.** `#[serde(default)]`, no `deny_unknown_fields`
  (already off), so the deployed `aprs-streamd` and `echo-consumer` keep working.
- **`PROTOCOL_VERSION` bumps to `2`** (resolved). The change is additive, so a bump
  isn't strictly required — but the frame's meaning materially grew, version drift
  is already tolerated by design, so bumping and documenting it is honest
  signposting at zero compatibility cost. Update the `PROTOCOL_VERSION` doc comment
  to note what `2` added.
- **`info_offset`, not carried `info` bytes** (resolved). The offset keeps the
  frame small (bytes already live in `ax25`) and is sufficient for byte-faithful
  igating: the consumer slices `ax25[info_offset..]`. Honors the "stay well under
  1400 bytes" constraint.
- Keep `proto.rs` free of igate policy — this is pure framing, which is in-scope
  for the schema crate.

### Slicer ladder on the wire (`RfMeta`)

Resolved decision #3: the **producer publishes the slicer gain ladder in the
stream** (single source of truth) rather than rtpigate keeping a local `[decoder]`
mirror. On stateless, join-anytime multicast a periodic "announce" datagram is
fragile — a consumer that joins between announces can't label its waterfall — so
carry the ladder as a **self-describing per-frame block** instead. It's static
per producer session and tiny (~8 `f32`s), so the per-frame cost is negligible and
every frame a consumer ever receives is fully self-describing. No second message
type, no join-timing window.

Add to `RfMeta` (which already holds `slicer_hits` / `slicer_mask`):

```rust
    /// Per-slicer space-gain ladder the producer's decoder is running (linear
    /// gain per slicer, ordered by slicer index). Static for a producer session.
    /// Column count for a slicer waterfall = `slicer_gains.len()`; a column's
    /// twist in dB = `20 * log10(gain)`. Carried per-frame so a consumer joining
    /// mid-stream is never missing it. `None` if the producer didn't supply it.
    #[serde(default)]
    pub slicer_gains: Option<Vec<f32>>,
```

Producer: `aprs-streamd` computes the ladder once at startup from its
`DecoderConfig` (`slicers`, `min_twist_db`, `max_twist_db` — the same
uniform-in-dB spread `rtpigate::space_gains` used) and stamps it onto every frame's
`RfMeta`. This is the logic that currently lives in rtpigate's `space_gains()`,
moving to the producer where the decoder config actually lives.

### Producer change (`crates/aprs-streamd/src/main.rs`)

In `map_frame`, populate `ax25_meta` from the `aprs_rtp::AprsPacket` fields already
in hand (`source`, `destination`, `via`, `via_heard`, `heard_direct`, `heard_from`,
`dti`, `info_invalid_bytes`). Compute `info_offset` as `raw_ax25.len() - info.len()`
(info is the trailing slice of the AX.25 frame), or by walking the address field to
the H-bit terminator + control + PID. Everything needed is on `pkt`; no new work in
the hot path beyond a few field copies and one `Vec` zip.

Also stamp `rf.slicer_gains`: compute the ladder once at startup from the
`DecoderConfig` and clone it onto every frame (see "Slicer ladder on the wire"
above). Bump `PROTOCOL_VERSION` to `2`.

### Tests (`crates/aprs-stream`)

- Round-trip an `AprsFrame` with a populated `ax25_meta` through CBOR; assert
  every field survives, `via` H-bits included.
- Round-trip a **pre-enrichment** frame (no `ax25_meta`, no `slicer_gains`) to
  prove `#[serde(default)]` deserializes it cleanly — backward-compat guard.
- Assert `ax25[info_offset..]` equals the expected verbatim info bytes for a known
  fixture (guards the offset math that byte-faithful igating relies on).
- Assert the producer's ladder computation matches the expected uniform-in-dB
  spread (port `space_gains`'s test vector, if any) so waterfall labels stay
  truthful.
- Extend `to_json` debug output coverage if it enumerates fields.

### Verification (Part 1)

- `cargo test` / `clippy -D warnings` / `fmt --check` clean.
- Run `aprs-streamd` against the live Pi source; run `echo-consumer` and the
  `cbor -> json` debug helper; confirm the new block is populated and `heard_*` /
  `via` H-bits look right against known local stations.
- Confirm frame size stays comfortably under ~1400 bytes for representative
  packets (the offset-not-bytes choice matters here).

## Part 2 — Port rtpigate to the stream

### Dependency changes (`rtpigate/Cargo.toml`)

- **Remove** `aprs-rtp`.
- **Add** `aprs-stream = { git = "https://github.com/deatojef/aprs-stream" }`.
- Keep `aprs-decode` (still used for symbol/position extraction), or use the copy
  re-exported by `aprs-stream` to avoid a version-skew footgun.

### `ka9q.rs` → stream listener

Rename to something honest (e.g. `stream.rs` / `listener.rs`). The module splits
cleanly into a part that changes and a part that doesn't:

- **Replace** the outer/inner `AprsListener::new(...).run()` RTP loop with an
  `aprs_stream::subscribe::Subscriber` joined to the multicast group. Keep the
  capped-exponential-backoff reconnect wrapper (5s→300s) around socket setup.
- **Rewrite `map_packet`**: `AprsFrame` → `RTPPacket` instead of
  `aprs_rtp::AprsPacket` → `RTPPacket`. Field-by-field this becomes almost a
  straight copy once Part 1 lands:
  - `source`/`destination`/`heard_direct`/`heard_from`/`dti` ← `ax25_meta`.
  - `via` / `digipeater_path` / `was_digipeated` ← `ax25_meta.via` (+ its H-bits).
  - `info_bytes` ← `ax25[info_offset..]`; `info` ← lossy-UTF-8 of that.
  - `info_invalid_bytes` ← `ax25_meta.info_invalid_bytes`.
  - `latitude`/`longitude`/`altitude_ft`/`object_name`/symbol ← `frame.parsed`
    (already parsed! rtpigate can drop its own `decode_ax25`/`decode_textual`
    fallback, or keep it only for the `parsed == None` path).
  - `frequency` ← `rf.frequency_hz` (Hz→MHz) or `capture.ssrc`/1000.
  - `slicer_mask` ← `rf.slicer_mask`; `receivetime` ← `capture.received_at_ms`.
  - `raw` (TNC2 text) ← reconstruct via `aprs_decode`'s `encode_textual` from
    `parsed` / `ax25_meta` (resolved decision #4 — no wire cost; a fixture test
    confirms it's lossless).
- **Keep unchanged**: all the telemetry aggregation in the back of the module —
  packet-statistics series, the slicer-diversity waterfall accumulation, station
  tracking, satellite log. This is consumer policy and is exactly what should live
  in a consumer.

### Slicer waterfall consideration

Resolved (#3): the slicer **count** and **gain ladder** now arrive on every frame
as `rf.slicer_gains` (producer-published). So:
- `slicer_mask` per-frame drives the waterfall cells (unchanged).
- Column count = `rf.slicer_gains.len()`; each column's twist label =
  `20 * log10(gain)`. rtpigate's `space_gains()` / `twist_db_to_gain()` /
  `slicer_zone()` helpers stay in the consumer for *rendering* the labels, but they
  now consume the ladder from the frame instead of re-deriving it from a local
  `[decoder]` config. Guard against a `None`/empty ladder (older producer) by
  falling back to a neutral placeholder — no panic.

### `config.rs`

- Drop `DecoderConfig` / `[decoder]` entirely — the decoder now lives in the
  producer, and the slicer ladder arrives on the wire.
- Replace `[rtp] host/port` (audio multicast group) with a `[stream]` section:
  multicast group + port + interface to subscribe to, matching `aprs-stream`'s
  `Subscriber` config. Keep `[satellite]`, `[aprsis]`, `[location]`, `[http]`,
  `[gpsd]` as-is.

### Untouched

`aprs_is.rs`, `igate.rs`, `gpsd.rs`, `sse.rs`, `history.rs`, `main.rs` (aside from
renamed listener wiring), and the entire frontend. Their contract is `RTPPacket` /
`DataItem`, which is preserved.

### Verification (Part 2)

- Build rtpigate with no `aprs-rtp` in the tree (`cargo tree` confirms the RTP/DSP
  stack is gone).
- Run `aprs-streamd` on the Pi and rtpigate on another LAN host; confirm the
  dashboard shows live packets, correct `heard_direct`/`digipeated` labeling,
  populated station table, and a working slicer waterfall.
- Confirm igating to APRS-IS (test with a read-only passcode first) reforms `qAO`
  lines correctly and the byte-faithful path (`for_rxigate_bytes`) emits verbatim
  info bytes for a Mic-E/binary fixture.
- Confirm dedup, drop accounting, and beaconing behave as before (unchanged code,
  but validate end-to-end).

## Sequencing & effort

1. **Part 1 (schema enrichment)** — small, ~half a day. Load-bearing; ship and
   deploy it first. Unblocks *all* consumers, not just rtpigate. Deployed
   `aprs-streamd`/`echo-consumer` keep working (additive change).
2. **Part 2 (rtpigate port)** — medium. Mechanical once Part 1 lands; the fiddly
   bit is the `map_packet` rewrite. The aggregation and igate logic are untouched.

Do Part 1 as its own change (its own commit/tag on `aprs-stream`), verify on
hardware, then port rtpigate against the shipped schema.

## Resolved decisions

1. **`PROTOCOL_VERSION` → 2.** Additive change, but the frame's meaning materially
   grew; bump and document. Version drift is tolerated by design.
2. **`info_offset`, not carried `info` bytes.** No byte duplication; consumer
   slices `ax25[info_offset..]`; stays under the ~1400-byte budget.
3. **Producer publishes the slicer ladder in the stream**, carried per-frame as
   `rf.slicer_gains` (self-describing; robust to join-anytime multicast). The
   `space_gains` computation moves from rtpigate into the producer; rtpigate's
   rendering helpers stay but consume the ladder from the frame.
4. **Reconstruct TNC2 `text` via `encode_textual`** consumer-side (no wire cost);
   a fixture test confirms it's lossless. Only carry `text` on the wire if that
   test ever shows loss.

## Out of scope

- Any change to `aprs-rtp` / `aprs-decode`.
- The optional TCP fan-out, web/HTTPS path, non-Rust consumers.
- rtpigate frontend changes beyond what the (unchanged) `RTPPacket` contract needs.
