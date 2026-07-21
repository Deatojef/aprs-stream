# Architecture

How `aprs-stream` is put together, why it's built this way, and what the numbers
mean when you tune it. See [README.md](README.md) for the overview.

---

## 1. Design principles

These were chosen deliberately and are load-bearing. Changing one has knock-on
effects across the codebase.

**Decode once, share widely.** The expensive, error-prone work — DSP and AX.25
parsing — happens in exactly one process. Everything else is a thin consumer of a
typed frame.

**The producer is policy-free.** It emits *every* frame the decoder yields:
duplicates, multipath copies, the same packet arriving via three different
digipeater paths. Each is tagged with quality metadata. An igate dedups; a
propagation logger wants precisely those duplicates. Pushing policy downstream is
what keeps the base service reusable — it's the whole point of the disaggregation.

**UDP multicast, not RTP.** APRS frames are discrete, bursty, self-contained
messages — log events, not a continuous timing-sensitive media stream. RTP's
sequencing and jitter machinery buys nothing. Multicast gives zero-state fan-out:
the producer never tracks subscribers.

**One frame per datagram.** The datagram boundary *is* the message boundary — no
length prefixing, no reassembly, no stream framing. A frame plus its CBOR wrapper
stays comfortably under the ~1472-byte safe UDP payload. Never let a single frame
exceed ~1400 bytes without revisiting this.

**The schema is a shared crate.** CBOR bytes carry no Rust type identity, so the
shared struct definition is what makes typed round-tripping valid. Both producer
and consumers depend on `aprs-stream`; nobody re-parses raw bytes.

**Connectionless and stateless.** The producer does not know who is listening, how
many, or whether anyone is. Group membership lives in the OS and switch (IGMP), not
in this code. A crashed consumer is a non-event. There is no producer-side
backpressure: a slow consumer simply drops datagrams, which is fine — APRS is
already a lossy best-effort RF medium. Fixes for slow consumers live consumer-side
(`SO_RCVBUF`, a decoupling queue), never in the producer.

---

## 2. Crate topology

```
                    ┌───────────────┐
                    │  aprs-stream  │  schema + CBOR + UDP transport
                    │   (contract)  │  ← consumers depend on ONLY this
                    └───────▲───────┘
                            │
     ┌──────────────┐   ┌───┴────────────┐   ┌─────────────┐
     │   aprs-sdr   │──▶│  aprs-streamd  │◀──│   aprs-rtp  │
     │ RTL-SDR +DSP │   │  the service   │   │  (crates.io)│
     └──────┬───────┘   └────────────────┘   └─────────────┘
            │
     ┌──────▼───────┐
     │  aprs-modem  │  AFSK → HDLC → AX.25 (source-agnostic)
     └──────────────┘
```

| Crate | Depends on | Notes |
|---|---|---|
| `aprs-stream` | `aprs-decode`, `ciborium`, `socket2` | No audio, no DSP. Deliberately small — it's the public contract. |
| `aprs-modem` | `tokio` | No sockets, no RTP. Consumes `AudioBlock`, yields `AprsPacket`. |
| `aprs-sdr` | `aprs-modem`, `rtl-sdr-rs`, `rustfft` | Capture + DSP. Usable as a library or a standalone binary. |
| `aprs-streamd` | all of the above + `aprs-rtp` | Wires a source to the emitter. |

`aprs-decode` (typed APRS payload parsing) and `aprs-rtp` (ka9q RTP capture) live
in their own repos and arrive from crates.io.

### Why `aprs-modem` exists

The decode core was originally inside `aprs-rtp`, coupled to its RTP front end. When
the direct-SDR path arrived, ~80% of `aprs-rtp` was the part worth keeping (AFSK,
HDLC, AX.25) and the RTP machinery was dead weight. That core was vendored into
`aprs-modem` with the transport stripped out, so both capture paths share one
decoder.

The cost is a duplicated modem (`aprs-rtp` still carries its own copy for its own
users). That's accepted for now; unifying means extracting a shared modem crate
that both depend on, which is a mechanical change to make later *with evidence*,
not speculatively.

---

## 3. The capture paths

Both paths converge on the same seam and the same decoder.

```
 DIRECT SDR                                      ka9q-radio
 ──────────                                      ──────────
 RTL-SDR USB (8-bit I/Q)                         radiod (RTP multicast)
        │                                               │
 fast-convolution channelizer                    aprs-rtp: RTP receive
   (one shared wide FFT)                           + PCM decode
        │                                               │
 per-channel FM demod                                   │
   + squelch + SNR                                      │
        │                                               │
        └──────────▶ AudioBlock ◀───────────────────────┘
                   (24 kHz f32)
                        │
                   aprs-modem
             AFSK → HDLC → AX.25
                        │
                    AprsPacket
                        │
                  aprs-streamd
             map → CBOR → multicast
```

### The `AudioBlock` seam

```rust
pub struct AudioBlock {
    pub ssrc: u32,                        // ka9q convention: freq_kHz
    pub sample_rate: u32,                 // 24_000
    pub samples: Vec<f32>,                // normalized [-1.0, 1.0]
    pub signal: Option<SignalMetrics>,    // SNR/RSSI, if the source measured it
}
```

This is the *only* thing the decoder consumes. It is deliberately source-agnostic:
the samples may come from an SDR channelizer, an RTP stream, or a test vector. The
demodulator is built from `sample_rate` at construction, so nothing is hardwired to
24 kHz.

`ssrc = freq_kHz` (so 144.390 MHz → 144390) follows ka9q-radio's convention, which
makes `freq_mhz = ssrc / 1000.0` and keeps frequency identity consistent across
both paths.

---

## 4. The SDR DSP chain

### 4.1 Fast-convolution channelizer

A port of the essential DSP in ka9q-radio's `filter.c`: an **overlap-save
fast-convolution filterbank**. One wide forward FFT of the complex input is computed
per block and **shared across every channel**. Each channel then extracts a
contiguous slice of master bins, circularly shifted so its centre lands at DC,
multiplies by a per-channel frequency response, and runs a small inverse FFT to
produce a decimated complex baseband stream.

Adding a channel costs only a bin-slice, a windowed multiply, and one small IFFT —
the expensive wideband FFT is paid once regardless of channel count. **This is the
entire reason many channels are cheap.**

Sizing follows ka9q's identities (`BLOCKTIME` = 20 ms, `OVERLAP` = 5):

| Quantity | Formula | At 1.2 MSPS | At 2.16 MSPS |
|---|---|---|---|
| `L` — new input samples/block | `Fs · blocktime` | 24 000 | 43 200 |
| `M-1` — overlap | `L/(OVERLAP-1)` | 6 000 | 10 800 |
| `N` — forward FFT | `L + M - 1` | 30 000 | 54 000 |
| `olen` — valid output/block | `audio · blocktime` | 480 | 480 |
| `Ns` — inverse FFT | `olen · N / L` | 600 | 600 |
| `D` — decimation | `N/Ns = Fs/audio` | 50 | 90 |

Overlap-save discards the first `Ns - olen` samples of each inverse FFT (circular
convolution garbage) and keeps the last `olen`. Choosing `Fs` so that `N` factors
into small primes keeps `rustfft` on its fast paths — 30 000 = 2⁴·3·5⁴ is ideal.

Per-channel response is a raised-cosine window: flat to ±7 kHz, tapering to zero by
±11.5 kHz, kept inside the `±Ns/2` slice edge so nothing wraps or aliases in.

**Channels beyond ±Fs/2 are rejected at startup**, not silently aliased — a request
for a frequency outside the captured span would otherwise wrap to a bogus bin and
produce phantom packets labelled with the wrong frequency.

### 4.2 FM demodulation

The classic product detector: `phase = arg(s[n] · conj(s[n-1]))` — the instantaneous
frequency. It is **amplitude-invariant**, which is why the channelizer's absolute
gain is irrelevant to the recovered audio.

Post-processing mirrors ka9q's chain: DC blocker (removes residual carrier offset),
optional de-emphasis, and output gain.

**De-emphasis** (default off) is a tunable because the audio-conditioning stage
changed owners. ka9q-radio's NBFM demod applies de-emphasis, and the vendored AFSK
demodulator was calibrated against that audio. The direct-SDR path outputs flat
discriminator audio instead. De-emphasis is a one-pole low-pass (~300 Hz corner),
which attenuates the 2200 Hz space tone about 5 dB more than the 1200 Hz mark tone —
i.e. it shifts the **twist** by ~5 dB, complementing transmitters that pre-emphasize.
The multi-slicer bank is designed to absorb twist either way, so this is a
second-order effect, but it is measurable and therefore configurable.

### 4.3 Squelch and SNR

Without a carrier, an FM discriminator emits full-scale random phase (±π). Left
alone this pins downstream level meters and churns the slicer bank on noise. An
SNR-based squelch mutes the audio between packets.

The control signal is **channel power relative to a tracked noise floor**, which is
scale-invariant (immune to channelizer and tuner gain):

- A warmup period seeds the floor from the mean of the first ~50 ms, rather than a
  single high-variance sample.
- While **closed**, the floor tracks the noise level (~200 ms) so the baseline SNR
  sits near 0 dB.
- While **open**, the floor may only *rise*, and very slowly (~60 s) — effectively
  frozen for the 0.5–1 s of a packet, so a carrier neither drags it down nor inflates
  it (which would compress the reported SNR).
- Hysteresis (open 3.0 dB / close 1.5 dB by default) prevents chatter. Thresholds are
  deliberately generous: losing a weak packet is worse than passing some noise.

The same machinery yields the **SNR carried on every frame** — the quality metadata
the propagation-logger use case was designed around. Because the producer emits every
copy, the same transmission arriving via three digipeater paths shows up as three
frames at three different SNRs.

---

## 5. Threading and the real-time constraint

This is the part most likely to bite anyone modifying the capture path.

```
 reader thread          DSP thread              tokio runtime
 ─────────────          ──────────              ─────────────
 read_sync(48 KB)  ──▶  channelize          ──▶ per-SSRC StreamDecoder
 (tight loop)           FM demod × N            (spawn_blocking)
        │               AudioBlock                     │
        └── bounded ────┘                              ▼
          queue (32)                            AprsPacket → emit
```

**`rtlsdr_read_sync` does not keep the USB pipeline filled between calls, and the
RTL2832's hardware FIFO is tiny.** Any gap between reads loses samples. Originally
the read and DSP shared a thread, so the gap grew with channel count: at 1 channel it
decoded fine, at 4 it missed packets, and at 8 it went deaf entirely — sample loss
corrupts *every* channel, not just the marginal ones.

The fix is a **dedicated reader thread** that does nothing but read back-to-back and
hand raw buffers to a bounded queue. The DSP thread consumes at its own pace. If the
DSP falls behind, the reader drops whole buffers (logged) rather than stalling —
dropping a 20 ms buffer is far better than corrupting the stream at the USB level.

`READ_BYTES` (48 128 = 94 × 512, a USB-legal multiple) is sized to roughly one 20 ms
channelizer block, so the pipeline self-paces at real time. The `RAW_QUEUE` depth of
32 gives ~640 ms of slack for scheduling jitter.

Measured over 125.7 hours at 2.16 MSPS × 9 channels: **zero dropped buffers**.

### Graceful shutdown

SIGINT/SIGTERM sets a flag the reader polls between reads; it then breaks, closes the
dongle, and the pipeline winds down in order (reader → DSP → decoders → emitter).
This matters: killing the process mid-USB-transfer leaves the dongle wedged and the
next start hangs on open. Systemd sends SIGTERM, so this is not optional in
production.

---

## 6. Decode pipeline

`aprs-modem` runs one `StreamDecoder` per SSRC on a blocking thread:

1. **AFSK demodulation** (direwolf "Profile A" lineage): bandpass pre-filter → IQ
   mixing at the mark/space tones → root-raised-cosine low-pass → envelope magnitude
   → AGC → multi-slicer DPLL.
2. **Slicer bank.** N parallel slicers (default 8, commonly 9), each compensating a
   different mark/space amplitude imbalance — *twist* — spread uniformly in dB across
   `[min_twist_db, max_twist_db]`. Whatever the net twist of a given transmitter and
   receiver chain, some rung is matched to it. The number that decoded a frame
   (`slicer_hits`) is a useful signal-quality proxy in its own right.
3. **HDLC framing**: NRZI decode, bit de-stuffing, flag detection.
4. **FCS validation** with optional single/double-bit error recovery.
5. **AX.25 parse** into addresses, digipeater path with H-bits, and info field.

Frames identical within a 3-second window are suppressed (the same physical
transmission caught by multiple slicers), but genuinely distinct copies — same packet
via different digipeater paths — are emitted separately by design.

---

## 7. Wire format

CBOR via `ciborium`, chosen for native byte strings (raw AX.25 rides as a true CBOR
byte string, no base64 bloat), real binary floats for quality metrics, compact size,
and clean cross-language support (Python `cbor2`, JS `cbor-x`).

```rust
pub struct AprsFrame {
    pub version: u8,                       // PROTOCOL_VERSION, always first
    pub capture: CaptureMeta,              // timestamp, receiver, decoder, ssrc
    pub rf: RfMeta,                        // frequency, snr_db, audio_level, slicer_*
    pub crc_ok: bool,
    pub ax25: Vec<u8>,                     // raw frame, FCS excluded — never lossy
    pub ax25_meta: Option<Ax25Meta>,       // pre-parsed AX.25 facts
    pub parsed: Option<aprs_decode::AprsPacket>,  // typed payload
}
```

Three deliberate choices:

- **`version` is first and present from day one**, so a consumer can adapt or reject
  a mismatched format.
- **The raw AX.25 bytes are always carried**, so nothing is ever lost and a
  byte-faithful igate path stays possible even when parsing fails.
- **`ax25_meta` carries the AX.25 facts the decoder already computed** (source, dest,
  digipeater path with heard-bits, `heard_from`, DTI, info offset) so consumers never
  re-parse the frame — even when `parsed` is `None`.

Compatibility rules: new fields get `#[serde(default)]`; `deny_unknown_fields` stays
**off** so older consumers tolerate newer producers. Timestamps are explicit
`u64` epoch-millis, not `SystemTime`, for cross-language portability.

`RfMeta.slicer_gains` carries the producer's full twist ladder on *every* frame —
there is no connection state to replay it, so a consumer joining mid-flight is never
missing the labels for a slicer-diversity waterfall.

---

## 8. Transport

Multicast is the same-L2 default. The emitter takes a **configurable list of
destinations** — the multicast group plus any explicit unicast targets — so
cross-VLAN delivery is a config detail rather than a redesign. (Cross-VLAN multicast
is unreliable on typical prosumer gear thanks to IGMP-snooping quirks; a unicast
relay has historically been the practical workaround.)

Multicast TTL defaults to 1 (stay on-subnet). On a multi-homed host, bind the send
interface explicitly rather than letting the OS choose.

The consumer side uses `socket2` for `SO_REUSEADDR`, explicit interface selection on
the group join, and `SO_RCVBUF` control — all beyond what std's `UdpSocket` exposes.

---

## 9. Tuning guide

### Gain — the one setting that matters most

`gain` is in **tenths of a dB** and drives the R820T2's **analog LNA + mixer** stages
ahead of the ADC (the IF/VGA is pinned by the driver). It is not baseband or digital
gain. The tuner's ceiling is ~**496** (49.6 dB); values above that are clamped, and
requesting ≥497 actually lands slightly *below* 496 because the last mixer step is
negative.

The RTL-SDR's ADC is **8-bit — roughly 48 dB of total dynamic range**. Gain setting
is therefore a balance:

- **Too low** — band noise sits in the ADC's own quantization floor and weak signals
  are lost.
- **Too high** — strong signals rail the converter, producing intermod spurs that
  make one transmission appear on several channels at once.

`gain = "hw-agc"` engages the tuner's own AGC. **It is a diagnostic option, not a
recommendation.** It runs ~10 dB more fixed IF gain than any manual setting, reacts
to total power across the whole captured span (so one strong signal desenses every
channel), and is designed for continuous broadcast rather than packet bursts. In
testing it was the only configuration that produced overload ghosting.

**Pick a fixed gain empirically from catch rate**, then sanity-check it against the
status line. A deployment at 40.2 dB measured a mean clipping rate of 0.0004% with no
correlation between the worst clipping windows and reduced catch rate.

### Reading the status line

Once a minute, the reader logs decode rate and RF conditions together, so a dip in
one can be read against the other from a single file:

```
status: frames=14  floor=-35.9 dBFS  mean=-21.3 dBFS  peak=-9.4 dBFS  clipped=1 (0.0000%)
```

| Field | Meaning |
|---|---|
| `frames` | Packets decoded in the last window (all channels combined). |
| `floor` | Noise floor — a low percentile of per-read power, so intermittent transmissions don't poison it. |
| `mean` | Average wideband power including signals. A large `mean − floor` gap means the band was busy. |
| `peak` | Loudest sample. `0.0 dBFS` means something touched the rails. |
| `clipped` | Samples pegged at the 0/255 rails, with the percentage. |

**Watch the raw `clipped` count, not just the percentage.** A window is tens of
millions of samples, so a handful of railed samples rounds to `0.0000%` while still
indicating the front end grazed the rails. Sustained counts in the thousands with a
percentage above ~0.01% mean real overload; back the gain off.

A caveat worth knowing: `floor` rejects *intermittent* signals but **cannot reject a
continuous in-band carrier**, which raises every percentile. Observed floor-vs-gain
has drifted well over 10 dB between runs at fixed gain for that reason. Read it as a
guide, not an absolute — and note this is exactly why automatic gain control keyed on
the wideband floor was tried, measured, and removed.

### Other knobs

| Setting | Default | Effect |
|---|---|---|
| `fm_maxdev_hz` | 20000 | FM deviation mapped to full-scale audio. Only scales the reported `rec` level (the AFSK demod has its own AGC) — tune it to land `rec` in a useful range, not to change decoding. |
| `squelch_open_db` / `squelch_close_db` | 3.0 / 1.5 | Lower passes weaker carriers at the cost of more inter-packet noise. |
| `deemphasis` | false | See §4.2. Worth A/B testing against catch rate. |
| `slicers` | 8 | More rungs = better weak-signal capture at higher CPU cost. |
| `min_twist_db` / `max_twist_db` | −12 / +9 | Ladder range in twist dB. Narrow it to concentrate resolution where your signals actually land. |

### Choosing frequencies

Every channel must fall inside the captured span. Keep them within
**±(0.8 · sample_rate/2)** of the centre, and offset the centre so no channel sits on
0 Hz (the RTL DC spike). To cover a set of frequencies, centre on their midpoint and
choose `sample_rate ≥ span / 0.8`. Spans beyond ~2 MHz approach the RTL-SDR's
reliable ceiling — an Airspy would give real headroom.

---

## 10. Performance

Measured on a Raspberry Pi 5, 2.16 MSPS, 9 channels:

| | |
|---|---|
| Fixed cost (USB I/O + shared forward FFT) | **~6.5%** of one core |
| Marginal cost per channel | **~1.6%** of one core |
| 1 channel / 8 channels | 8.1% / 19.0% of one core |
| Extrapolated: 24 channels | ~44% of one core |

The sub-linear scaling is the fast-convolution payoff: the wideband FFT is amortized
across all channels. With four cores available, channel count is not the binding
constraint — the SDR's instantaneous bandwidth is.

Stability over 125.7 hours continuous: zero errors, zero dropped buffers, zero
decoder respawns, and all 7,543 one-minute status windows delivered with no gaps
(60–61 s intervals throughout).

---

## 11. Known limitations

- **No per-channel breakdown in the status line.** `frames` is the total across all
  channels; you can't tell from the log alone whether one channel has gone deaf.
- **Automatic gain control was removed.** A software noise-floor-targeting manager was
  built, measured, and deleted: the wideband floor proved unusable as a control input
  (continuous carriers contaminate it, and it doesn't correlate with catch rate). A
  narrowband per-channel floor would be the right input if this is revisited.
- **The modem is duplicated** between `aprs-modem` and `aprs-rtp`. See §2.
- **RTL-SDR only** on the direct path. The device layer is small and isolated;
  Airspy would need a SoapySDR backend.
- **No TCP fan-out.** Designed for (cross-VLAN, web/WebSocket consumers) but not
  built. It would be a separate, opt-in, stateful component — the common multicast
  path stays connectionless.
