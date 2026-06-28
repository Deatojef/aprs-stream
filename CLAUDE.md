# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this project is

A disaggregated APRS pipeline. A single base service demodulates and decodes
APRS packets from ka9q-radio RTP audio, then publishes fully-decoded, richly-typed
APRS frames onto the local network. Downstream clients (igate, database logger,
mapping/tracker, EOSS tracker, etc.) subscribe and consume typed frames without
ever touching RTP audio or doing AX.25 parsing themselves.

This replaces the monolithic model where each consuming app (e.g. `rtpigate`)
independently owned the full RTP-listen → decode → use chain. The decode work
happens **once**, in one place, and is shared.

```
ka9q-radio (RTP multicast, PCM audio)
        │
        ▼
┌─────────────────────────┐
│  aprs-streamd (service) │   owns aprs-rtp + aprs-decode
│  RTP → decode → frame   │
└───────────┬─────────────┘
            │  CBOR-encoded AprsFrame, one per UDP datagram
            ▼
   APRS multicast group (LAN)
            │
   ┌────────┼────────┬───────────────┐
   ▼        ▼        ▼               ▼
 igate   logger   tracker      EOSS tracker
        (each: thin consumer of aprs-stream crate)
```

## Existing prior work (do not reinvent)

- `aprs-rtp` — listens to ka9q-radio RTP audio. Already built. Stays in its own
  repo; pulled in as a git dependency, never folded into this workspace.
- `aprs-decode` — decodes APRS/AX.25 packets from that audio. Already built.
  Stays in its own repo; pulled in as a git dependency, never folded in.
- `rtpigate` — existing monolithic igate built on the two crates above. Works
  well; fast and robust. It will eventually be refactored into a thin consumer
  of the new stream, but that is NOT part of the initial scope here.

## Architectural decisions (settled — do not relitigate)

These were deliberately chosen. Treat them as constraints, not open questions.

1. **Transport: UDP multicast.** Not RTP. APRS frames are discrete, bursty,
   self-contained messages (log-event-like), not a continuous timing-sensitive
   media stream. RTP's sequencing/jitter machinery buys nothing here. Multicast
   gives zero-state fan-out: the producer never tracks subscribers.

2. **One decoded frame per UDP datagram.** The datagram boundary IS the message
   boundary — no length-prefixing, no reassembly, no stream framing. APRS frames
   are small; a frame plus its CBOR metadata wrapper stays comfortably under the
   ~1472-byte safe UDP payload (1500 MTU − IP/UDP headers). Never let a single
   frame exceed ~1400 bytes without revisiting this.

3. **Payload encoding: CBOR via `ciborium`.** Not `serde_cbor` (unmaintained/
   archived). CBOR chosen for native byte strings (raw AX.25 rides as a true CBOR
   byte string, no base64 bloat), real binary floats for quality metrics, compact
   size, and clean cross-language support (Python `cbor2`, JS `cbor-x`) for future
   non-Rust consumers.

4. **Schema lives in one shared crate.** Both producer and consumers depend on it.
   It is the single source of truth that makes typed round-tripping valid — CBOR
   bytes do NOT carry Rust type identity, so the shared struct definition is what
   makes re-hydration work. No app re-parses raw bytes.

5. **Versioned messages.** A `version: u8` (or equivalent format tag) is the first
   field of the frame, present from day one, so future producers/consumers can
   adapt or reject mismatched formats.

6. **Producer is policy-free: emit everything.** The base service publishes ALL
   decoded frames — including CRC failures, duplicates, and multipath copies —
   each tagged with quality metadata. It does NOT dedup or filter. Consumers
   decide policy: an igate dedups; an SNR/propagation logger WANTS the failures
   and duplicate copies. Pushing policy downstream is what keeps the base service
   maximally reusable. This is the whole point of the disaggregation.

7. **Multicast is the same-L2 default; unicast/TCP is the escape hatch.** Cross-VLAN
   multicast is unreliable on the target network gear (Unifi IGMP-snooping pain;
   a socat unicast relay was the practical past workaround). So the emit side must
   be designed as "send these bytes to a configurable list of destinations" (one
   of which is the multicast group), NOT multicast-only. An optional TCP fan-out
   for cross-VLAN / reliability-sensitive / future web (HTTPS/WebSocket) clients
   is a SEPARATE, optional component — keep the common multicast path connectionless.

## Connection model (important)

The multicast producer is **connectionless and stateless** with respect to
consumers. It does not know who is listening, how many, or whether anyone is.
There is NO per-client connection management on the multicast path, by design —
group membership lives in the OS/switch (IGMP), not in this code. A vanished or
crashed consumer is a non-event.

The ONLY place connection state legitimately exists is the optional TCP fan-out
task, which holds a list of accepted clients and replays the stream to them.
Keep that state boundary sharp: stateless multicast emitter as the core; stateful
fan-out strictly opt-in and isolated.

There is no producer-side backpressure (a slow consumer simply drops datagrams —
acceptable, since APRS is already a lossy best-effort RF medium). Fixes for slow
consumers live consumer-side (`SO_RCVBUF`, a decoupling queue), never in the producer.

## Layout (single Cargo workspace, repo: `aprs-stream`)

This repo holds two crates: the reusable `aprs-stream` library and the
`aprs-streamd` service binary that is its first/reference consumer.

`aprs-rtp` and `aprs-decode` stay in their own separate repos and are pulled in
as **git dependencies** — they are NOT workspace members and will not be folded
in. They are independent reusable libraries; only `aprs-streamd` depends on them.

Downstream apps (igate, logger, trackers) depend on the `aprs-stream` *crate*
via a git dependency on this repo — a git dep resolves only that crate's
dependency tree, so consumers do NOT compile `aprs-streamd` or its RTP/audio
stack. The repo boundary and the crate boundary are distinct; sharing a repo
costs nothing here.

```
/Cargo.toml                  # workspace (members: aprs-stream, aprs-streamd)
/crates/
  aprs-stream/               # shared library: schema + ser/de + transport helpers
    src/
      lib.rs
      proto.rs               # AprsFrame, nested types, payload enum, version
      codec.rs               # ciborium encode/decode helpers + cbor->json debug
      emit.rs                # multicast/unicast emitter (per-destination list)
      subscribe.rs           # multicast join (socket2), recv loop, decode
      fanout.rs              # OPTIONAL stateful TCP fan-out (feature-gated)
  aprs-streamd/              # the base service binary
    src/main.rs              # wires aprs-rtp -> aprs-decode -> aprs-stream::emit
/examples/
  echo-consumer/             # minimal subscriber: join group, print frames
```

`aprs-streamd/Cargo.toml` depends on the sibling `aprs-stream` (path dep within
the workspace) plus the two external crates as git deps, e.g.:

```toml
[dependencies]
aprs-stream  = { path = "../aprs-stream" }
aprs-rtp     = "0.2.0"
aprs-decode  = "0.1.2"
```

If `aprs-stream` later earns a fully independent release life, extracting it to
its own repo is a cheap mechanical move — do it with evidence, not speculatively.

## Schema design notes

- One rich top-level `AprsFrame` struct composed of nested typed sub-structs:
  RF metadata (frequency, SNR / decision-variable quality metrics), capture
  metadata (receive timestamp, decoder/receiver provenance), the raw AX.25 bytes
  (`Vec<u8>` → CBOR byte string, always present so nothing is ever lost), and a
  parsed payload.
- Parsed payload as an `enum` (`Position`, `Status`, `Message`, `Telemetry`,
  `Raw(Vec<u8>)`, …) so consumers `match` on packet type directly instead of
  re-parsing. Decide enum tagging representation before shipping (default
  externally-tagged vs `#[serde(tag = "type")]` internally-tagged — the latter
  is friendlier for future non-Rust consumers). This is a wire-format choice.
- Timestamps as explicit `u64` epoch-millis (or `time`/`chrono` with a documented
  serde format), NOT `SystemTime`, for cross-language portability.
- Put `#[serde(default)]` on new/optional fields as a habit for forward/backward
  compatibility. Leave `deny_unknown_fields` OFF so older consumers tolerate
  newer producers. `Option<T>` for fields that aren't always measured (e.g. SNR).
- Provide a `cbor -> json` debug helper / subcommand to recover human-inspectability
  (the one real thing lost vs JSON-on-the-wire).

## Socket/runtime notes

- Async Rust on Tokio.
- Use the `socket2` crate for the consumer side: need `SO_REUSEADDR`, explicit
  interface selection for the multicast join, and `SO_RCVBUF` control — beyond
  what std `UdpSocket` exposes.
- Producer: set multicast TTL explicitly (default 1 = stay on-subnet). On a
  multi-homed host, bind the send to the correct interface explicitly rather
  than letting the OS choose.
- Emit side takes a configurable list of destinations (multicast group + any
  explicit unicast targets) so cross-VLAN is a config detail, not a redesign.

## Initial scope (first milestone)

1. Workspace + `aprs-stream` crate skeleton.
2. `AprsFrame` schema (with version field, nested types, payload enum) +
   ciborium round-trip + cbor→json debug helper. Unit test the round-trip.
3. Multicast emitter (per-destination list) and subscriber (socket2 join + recv +
   decode) helpers.
4. `aprs-streamd` wiring `aprs-rtp` → `aprs-decode` → emit.
5. `examples/echo-consumer` that joins the group and prints decoded frames —
   the end-to-end smoke test.

OUT of scope for the first milestone: refactoring `rtpigate`, the optional TCP
fan-out, any web/HTTPS path, non-Rust consumers. Design for them, don't build
them yet.

## Conventions

- Rust 2021+, async on Tokio.
- Keep `aprs-decode` pure decode logic — no network concerns leak into it.
- Keep `aprs-stream` free of igate/tracker/app-specific policy — it is framing
  and transport only.
- Prefer small, composable helpers over a monolithic service struct.
