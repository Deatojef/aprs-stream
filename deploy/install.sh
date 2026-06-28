#!/usr/bin/env bash
#
# Install (or upgrade) aprs-streamd as a systemd service.
#
# Run as root from within an unpacked release bundle:
#   sudo ./install.sh
#
# Idempotent: safe to re-run to upgrade the binary. An existing
# /etc/aprs-streamd/config.toml is never overwritten.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SERVICE_USER="aprs-streamd"
BIN_DST="/usr/local/bin/aprs-streamd"
CONF_DIR="/etc/aprs-streamd"
CONF_DST="${CONF_DIR}/config.toml"
ENV_DST="/etc/default/aprs-streamd"
UNIT_DST="/etc/systemd/system/aprs-streamd.service"

die() { echo "error: $*" >&2; exit 1; }
note() { echo ">> $*"; }

[ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0)"

# These files ship in the release bundle alongside this script.
BIN_SRC="${SCRIPT_DIR}/aprs-streamd"
UNIT_SRC="${SCRIPT_DIR}/aprs-streamd.service"
ENV_SRC="${SCRIPT_DIR}/aprs-streamd.default"
CONF_SRC="${SCRIPT_DIR}/config.toml"
for f in "$BIN_SRC" "$UNIT_SRC" "$ENV_SRC" "$CONF_SRC"; do
    [ -f "$f" ] || die "missing bundled file: $f"
done

if [ "$(uname -m)" != "aarch64" ]; then
    note "warning: host arch is $(uname -m), but this build targets aarch64"
fi

# 1. Dedicated, locked-down system user.
if ! getent passwd "$SERVICE_USER" >/dev/null; then
    note "creating system user '$SERVICE_USER'"
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
else
    note "system user '$SERVICE_USER' already exists"
fi

# 2. Stop the running service (upgrade path).
if systemctl is-active --quiet aprs-streamd 2>/dev/null; then
    note "stopping running service for upgrade"
    systemctl stop aprs-streamd
fi

# 3. Binary.
note "installing binary -> ${BIN_DST}"
install -o root -g root -m 0755 "$BIN_SRC" "$BIN_DST"

# 4. Config (never clobber an existing one).
install -d -o root -g "$SERVICE_USER" -m 0750 "$CONF_DIR"
if [ -f "$CONF_DST" ]; then
    note "keeping existing config ${CONF_DST} (not overwritten)"
else
    note "installing default config -> ${CONF_DST}"
    install -o root -g "$SERVICE_USER" -m 0640 "$CONF_SRC" "$CONF_DST"
fi

# 5. Environment file (only if absent).
if [ -f "$ENV_DST" ]; then
    note "keeping existing ${ENV_DST}"
else
    install -o root -g root -m 0644 "$ENV_SRC" "$ENV_DST"
fi

# 6. Unit file.
note "installing unit -> ${UNIT_DST}"
install -o root -g root -m 0644 "$UNIT_SRC" "$UNIT_DST"

# 7. Enable + start.
note "reloading systemd and enabling service"
systemctl daemon-reload
systemctl enable --now aprs-streamd

cat <<EOF

aprs-streamd installed and started.

Next steps:
  1. Edit the RTP source in ${CONF_DST}  (set [source] host to your
     ka9q-radio multicast host), then:
        sudo systemctl restart aprs-streamd
  2. Check status and logs:
        systemctl status aprs-streamd
        journalctl -u aprs-streamd -f
EOF
