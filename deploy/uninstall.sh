#!/usr/bin/env bash
#
# Remove the aprs-streamd service. Run as root:
#   sudo ./uninstall.sh
#
# By default this keeps /etc/aprs-streamd (your config) and the service user so a
# later reinstall is seamless. Pass --purge to remove those too.
set -euo pipefail

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

SERVICE_USER="aprs-streamd"
BIN_DST="/usr/local/bin/aprs-streamd"
CONF_DIR="/etc/aprs-streamd"
ENV_DST="/etc/default/aprs-streamd"
UNIT_DST="/etc/systemd/system/aprs-streamd.service"

die() { echo "error: $*" >&2; exit 1; }
note() { echo ">> $*"; }

[ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0)"

if systemctl list-unit-files aprs-streamd.service >/dev/null 2>&1; then
    note "stopping and disabling service"
    systemctl disable --now aprs-streamd 2>/dev/null || true
fi

note "removing unit and binary"
rm -f "$UNIT_DST" "$BIN_DST"
systemctl daemon-reload

if [ "$PURGE" -eq 1 ]; then
    note "purging config, environment file, and service user"
    rm -rf "$CONF_DIR" "$ENV_DST"
    if getent passwd "$SERVICE_USER" >/dev/null; then
        userdel "$SERVICE_USER" 2>/dev/null || true
    fi
else
    note "kept ${CONF_DIR}, ${ENV_DST}, and user '${SERVICE_USER}' (use --purge to remove)"
fi

note "done"
