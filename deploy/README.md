# Deploying aprs-streamd

`aprs-streamd` runs as an unattended systemd service — typically on a 64-bit
Raspberry Pi (aarch64 / Raspberry Pi OS). It listens to ka9q-radio RTP audio and
republishes decoded, typed APRS frames on the LAN. Once installed it just runs:
auto-starts on boot, restarts on failure.

## Install (on the Pi)

Grab the latest `aprs-streamd-vX.Y.Z-aarch64-linux.tar.gz` from the
[Releases page](https://github.com/Deatojef/aprs-stream/releases):

```sh
tar xzf aprs-streamd-vX.Y.Z-aarch64-linux.tar.gz
cd aprs-streamd-vX.Y.Z-aarch64-linux
sudo ./install.sh
```

`install.sh` creates a locked-down `aprs-streamd` system user, installs the
binary to `/usr/local/bin`, the config to `/etc/aprs-streamd/config.toml` (only
if not already present), and the systemd unit, then enables and starts it.

Then point it at your RTP source and restart:

```sh
sudo nano /etc/aprs-streamd/config.toml     # set [source] host
sudo systemctl restart aprs-streamd
```

Verify the checksum first if you like:

```sh
sha256sum -c SHA256SUMS    # run from the directory containing the .tar.gz
```

## Operate

```sh
systemctl status aprs-streamd        # is it running?
journalctl -u aprs-streamd -f        # live logs
sudo systemctl restart aprs-streamd  # after a config change
```

Log verbosity is controlled by `RUST_LOG` in `/etc/default/aprs-streamd`
(default `info`); e.g. `RUST_LOG=aprs_rtp=debug,info` for verbose decoder logs.
Restart after editing.

## Upgrade

Download the newer release, extract, and re-run `sudo ./install.sh`. The binary
is replaced and the service restarted; your existing config is left untouched.

## Uninstall

```sh
sudo ./uninstall.sh            # removes service + binary, keeps config & user
sudo ./uninstall.sh --purge    # also removes /etc/aprs-streamd and the user
```

## Cutting a release (maintainers)

From a clean `main` on your dev machine:

```sh
./deploy/release.sh X.Y.Z
```

This runs the pre-flight gate (fmt, clippy, tests), bumps the `aprs-streamd`
crate version, commits, and pushes the `vX.Y.Z` tag. The
[`release.yml`](../.github/workflows/release.yml) workflow then cross-compiles
for aarch64 and attaches the bundle + `SHA256SUMS` to a GitHub Release.

## Files

| File | Installed to | Purpose |
|------|--------------|---------|
| `aprs-streamd` | `/usr/local/bin/aprs-streamd` | the service binary |
| `aprs-streamd.service` | `/etc/systemd/system/` | hardened systemd unit |
| `aprs-streamd.default` | `/etc/default/aprs-streamd` | `RUST_LOG` and env overrides |
| `config.toml` | `/etc/aprs-streamd/config.toml` | service configuration |
