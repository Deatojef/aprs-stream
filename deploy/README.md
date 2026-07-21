# Deploying aprs-streamd

`aprs-streamd` ships as a Debian package (`.deb`) and runs as an unattended
systemd service — on a 64-bit Raspberry Pi (aarch64 / Raspberry Pi OS) or any
x86_64 Debian/Ubuntu host. It captures APRS from a directly-attached RTL-SDR or
from ka9q-radio RTP audio, and republishes decoded, typed frames on the LAN. Once
installed it just runs: auto-starts on boot, restarts on failure.

## Install

Each release ships two packages — pick the one for your hardware:

- **`aprs-streamd_X.Y.Z-1_arm64.deb`** — 64-bit Raspberry Pi / other aarch64.
- **`aprs-streamd_X.Y.Z-1_amd64.deb`** — x86_64 Debian/Ubuntu.

Download it from the
[Releases page](https://github.com/Deatojef/aprs-stream/releases) (run
`dpkg --print-architecture` if unsure which you need), then:

```sh
sudo apt install ./aprs-streamd_X.Y.Z-1_<arch>.deb
```

`apt` pulls in any dependencies and runs the package's setup, which creates a
locked-down `aprs-streamd` system user, installs the config to
`/etc/aprs-streamd/config.toml`, installs and enables the systemd unit, and
starts the service.

(Verify the download first if you like: `sha256sum -c SHA256SUMS`.)

Then point it at an audio source and restart:

```sh
sudo nano /etc/aprs-streamd/config.toml     # set [source] (RTP) or [sdr]
sudo systemctl restart aprs-streamd
```

## Extra setup for the direct-SDR source

Skip this if you use the ka9q-radio `[source]` path — it needs nothing beyond the
network.

Running `[sdr]` means the service talks to a USB dongle, which needs three things
the default install does **not** provide:

**1. Release the dongle from the DVB driver.** The kernel claims RTL-SDR hardware
as a TV tuner by default:

```sh
echo -e 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830' \
  | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
sudo modprobe -r dvb_usb_rtl28xxu rtl2832_sdr rtl2832    # or reboot
```

**2. Let the service user reach the device.** The service runs as the unprivileged
`aprs-streamd` user, which has no USB access by default:

```sh
# udev rule granting the plugdev group access to RTL-SDR dongles
echo 'SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0660", GROUP="plugdev"' \
  | sudo tee /etc/udev/rules.d/60-rtlsdr.rules
sudo usermod -aG plugdev aprs-streamd
sudo udevadm control --reload-rules && sudo udevadm trigger
```

**3. Allow device access through the unit's hardening.** The shipped unit sets
`PrivateDevices=true`, which gives the service a private `/dev` containing only
pseudo-devices — so USB nodes are invisible and the dongle **cannot** be opened.
The package ships a ready-made drop-in that relaxes exactly this and nothing else;
copy it into place (drop-ins survive package upgrades, and the default unit stays
hardened for RTP users):

```sh
sudo mkdir -p /etc/systemd/system/aprs-streamd.service.d
sudo cp /usr/share/doc/aprs-streamd/sdr-override.conf \
        /etc/systemd/system/aprs-streamd.service.d/sdr.conf
sudo systemctl daemon-reload
sudo systemctl restart aprs-streamd
```

It sets `PrivateDevices=false` (so `/dev/bus/usb` is reachable),
`DeviceAllow=char-usb_device rw` (only USB character devices — the rest of `/dev`
stays denied), and `SupplementaryGroups=plugdev` to match the udev rule above.
Edit the group if your rule uses a different one.

If any of these are missing you'll see `failed to open RTL-SDR` in the journal.
Confirm the dongle is visible with `lsusb | grep -i realtek`.

## Operate

```sh
systemctl status aprs-streamd        # is it running?
journalctl -u aprs-streamd -f        # live logs
sudo systemctl restart aprs-streamd  # after a config change
```

Log verbosity is set via `RUST_LOG` in `/etc/default/aprs-streamd`
(default `info`); e.g. `RUST_LOG=aprs_rtp=debug,info` for verbose decoder logs.
Restart after editing.

## Upgrade

```sh
sudo apt install ./aprs-streamd_X.Y.Z-1_<arch>.deb   # newer version
```

`/etc/aprs-streamd/config.toml` and `/etc/default/aprs-streamd` are marked as
**conffiles**: your edits are preserved across upgrades. If a future package ships
a changed default, dpkg prompts you to keep yours, take the new one, or view the
diff.

## Remove

```sh
sudo apt remove aprs-streamd     # stop + remove, keep config
sudo apt purge aprs-streamd      # also remove config and the aprs-streamd user
```

## Cutting a release (maintainers)

From a clean `main` on your dev machine:

```sh
./deploy/release.sh X.Y.Z
```

This runs the pre-flight gate (fmt, clippy, tests), bumps the `aprs-streamd`
crate version, commits, and pushes the `vX.Y.Z` tag. The
[`release.yml`](../.github/workflows/release.yml) workflow then cross-compiles
for both aarch64 and x86_64 (a build matrix), builds a `.deb` for each with
[`cargo-deb`](https://github.com/kornelski/cargo-deb), and attaches both (plus a
combined `SHA256SUMS`) to a GitHub Release.

Packaging is defined in `crates/aprs-streamd/Cargo.toml` under
`[package.metadata.deb]`; the systemd unit, env file, and maintainer scripts
(`deploy/aprs-streamd.service`, `deploy/aprs-streamd.default`, `deploy/debian/`)
are pulled in from there.

## Installed layout

| Source | Installed to | Purpose |
|--------|--------------|---------|
| `target/.../aprs-streamd` | `/usr/bin/aprs-streamd` | the service binary |
| `deploy/aprs-streamd.service` | `/usr/lib/systemd/system/` | hardened systemd unit |
| `deploy/aprs-streamd.default` | `/etc/default/aprs-streamd` | `RUST_LOG` and env overrides (conffile) |
| `config.toml` | `/etc/aprs-streamd/config.toml` | service configuration (conffile) |
