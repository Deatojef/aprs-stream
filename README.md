# aprs-stream

A **disaggregated APRS pipeline**. One service captures RF, demodulates and decodes
APRS packets, and publishes fully-decoded, richly-typed frames onto the local
network. Downstream clients — igate, database logger, mapping/tracker, EOSS balloon
tracker — subscribe and consume typed frames without ever touching audio or parsing
AX.25 themselves.

The decode work happens **once**, in one place, and is shared.

```
                    ┌──────────────────────────────┐
  RTL-SDR (USB) ───▶│                              │
        or          │       aprs-streamd           │
  ka9q-radio RTP ──▶│  capture → demod → decode    │
                    └───────────────┬──────────────┘
                                    │  CBOR-encoded AprsFrame,
                                    │  one per UDP datagram
                                    ▼
                        APRS multicast group (LAN)
                                    │
            ┌───────────┬───────────┼───────────────┐
            ▼           ▼           ▼               ▼
          igate      logger      tracker      EOSS tracker
                  (each: a thin consumer of the aprs-stream crate)
```

## Why

The old model had every consuming app own the full RF → decode → use chain
independently. That meant N copies of the DSP, N tuners, and N chances to get AX.25
parsing subtly wrong. Here the base service owns it once and publishes a typed,
versioned frame that anything on the LAN can consume in a few lines of code.

The producer is deliberately **policy-free**: it emits *every* frame the decoder
yields — including duplicates and multipath copies — each tagged with quality
metadata (SNR, slicer diversity, audio levels). An igate wants to dedup; a
propagation logger specifically wants those duplicates. Policy belongs downstream.

## Two capture paths

| Source | How | When to use |
|---|---|---|
| **Direct SDR** | RTL-SDR over USB → fast-convolution channelizer → FM demod | Self-contained; no external dependencies |
| **ka9q-radio** | Subscribes to `radiod`'s RTP PCM multicast | You already run ka9q-radio |

Both feed the *same* decoder and emit the *same* wire format, so switching is a
one-section config edit. The SDR path slices **many frequencies from a single
wideband capture** using the fast-convolution technique from
[ka9q-radio](https://github.com/ka9q/ka9q-radio) — one wide FFT shared across all
channels, so each extra frequency costs very little.

## Installation

### From a release (recommended)

Each tagged release publishes a Debian package for both architectures on the
[Releases page](https://github.com/Deatojef/aprs-stream/releases) — grab the one
matching `dpkg --print-architecture`:

```sh
# arm64 (64-bit Raspberry Pi) or amd64 (x86_64 Debian/Ubuntu)
sudo apt install ./aprs-streamd_X.Y.Z-1_arm64.deb
```

That creates a locked-down `aprs-streamd` service user, installs the config to
`/etc/aprs-streamd/config.toml`, and enables + starts the systemd unit. Edit the
config, then restart:

```sh
sudo nano /etc/aprs-streamd/config.toml
sudo systemctl restart aprs-streamd
journalctl -u aprs-streamd -f
```

Checksums are published alongside the packages (`sha256sum -c SHA256SUMS`).
See **[deploy/README.md](deploy/README.md)** for upgrades, operation, and the
extra USB permissions the direct-SDR source needs.

### From source

```sh
cargo build --release
./target/release/aprs-streamd            # or pass a config path as argv[1]
```

## Quick start

Configure by editing `config.toml` — choose **one** of `[sdr]` or `[source]`.

Minimal direct-SDR config:

```toml
[sdr]
center_hz    = 145080000                 # offset so no channel sits on DC
channels_hz  = [144390000, 144340000]    # ssrc = freq_kHz
sample_rate  = 2160000
gain         = 402                       # tenths dB; see ARCHITECTURE.md

[emit]
group = "239.12.34.56"
port  = 17014
```

Watch the frames arrive:

```bash
./target/release/echo-consumer
```

```
[192.168.1.171:36202] v2 144.3900 MHz hits=5 crc_ok=true  [Mic-E position] KG0UL-9>TPPQRP,...
    ax25: KG0UL-9>TPPQRP via [W0CHC*,N0SZ-2*] direct=false heard_from=N0SZ-2 dti='`' ...
    rf: snr=16.8dB slicers=9 twist_db=[-12,-9,-6,-3,+0,+3,+6,+9,+12] mask=0x007c
```

## Consuming the stream

Any app becomes a consumer with the `aprs-stream` crate — no audio, no AX.25:

```rust
use aprs_stream::subscribe::{SubscribeConfig, Subscriber};

let sub = Subscriber::new(SubscribeConfig::new(group, port))?;
loop {
    let (frame, _from) = sub.recv_frame().await?;
    if let Some(pkt) = &frame.parsed {
        // match on the typed payload: Position, Status, Message, Telemetry, ...
    }
}
```

A git dependency on this repo resolves only the `aprs-stream` crate's tree, so
consumers do **not** compile the RTL-SDR or DSP stack.

## Layout

| Crate | Role |
|---|---|
| `aprs-stream` | **The shared contract.** Frame schema, CBOR codec, multicast emit/subscribe. This is what consumers depend on. |
| `aprs-modem` | Source-agnostic decode core: AFSK demod → HDLC → AX.25. |
| `aprs-sdr` | RTL-SDR capture, fast-convolution channelizer, FM demod, squelch/SNR. |
| `aprs-streamd` | The service binary: wires a source to the decoder and emits frames. |
| `examples/echo-consumer` | Minimal subscriber — the end-to-end smoke test. |

## Status

Running on a Raspberry Pi 5. Representative measurements from a 9-channel
2.16 MSPS deployment:

- **125.7 hours continuous** (5.2 days) — zero errors, zero dropped buffers, and
  all 7,543 status windows delivered on time with no gaps.
- **76,091 frames decoded**, averaging 605/hr with a 946/hr peak hour.
- **~6.5% of one core** fixed cost plus **~1.6% per channel** — 8 channels runs at
  19% of a single core, leaving ample headroom on a Pi 5.

## Documentation

- **[ARCHITECTURE.md](ARCHITECTURE.md)** — design decisions, the DSP chain, wire
  format, threading model, and a gain/tuning guide.
- **[deploy/README.md](deploy/README.md)** — systemd unit and Debian packaging.

## License

GPL-3.0
