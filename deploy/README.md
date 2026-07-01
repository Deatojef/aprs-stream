# Deploying aprs-streamd

`aprs-streamd` ships as a Debian package (`.deb`) and runs as an unattended
systemd service — typically on a 64-bit Raspberry Pi (aarch64 / Raspberry Pi OS).
It listens to ka9q-radio RTP audio and republishes decoded, typed APRS frames on
the LAN. Once installed it just runs: auto-starts on boot, restarts on failure.

## Install (on the Pi)

Download the latest `aprs-streamd_X.Y.Z-1_arm64.deb` from the
[Releases page](https://github.com/Deatojef/aprs-stream/releases), then:

```sh
sudo apt install ./aprs-streamd_X.Y.Z-1_arm64.deb
```

`apt` pulls in any dependencies and runs the package's setup, which creates a
locked-down `aprs-streamd` system user, installs the config to
`/etc/aprs-streamd/config.toml`, installs and enables the systemd unit, and
starts the service.

(Verify the download first if you like: `sha256sum -c SHA256SUMS`.)

Then point it at your RTP source and restart:

```sh
sudo nano /etc/aprs-streamd/config.toml     # set [source] host
sudo systemctl restart aprs-streamd
```

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
sudo apt install ./aprs-streamd_X.Y.Z-1_arm64.deb   # newer version
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
for aarch64, builds the `.deb` with [`cargo-deb`](https://github.com/kornelski/cargo-deb),
and attaches it (plus `SHA256SUMS`) to a GitHub Release.

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
