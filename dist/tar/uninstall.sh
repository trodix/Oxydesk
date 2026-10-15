#!/bin/sh
# Removes what install.sh installed (the PAM stacks too; /etc/rdsktp/rdsktp.conf and the user data in /var/lib/rdsktp are kept). usage: ./uninstall.sh [ROOT]
set -eu
ROOT="${1:-}"
[ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1 && systemctl disable --now rdsktp-authd rdsktp 2>/dev/null || true
rm -f "$ROOT"/usr/bin/rdsktp "$ROOT"/usr/bin/rdsktp-authd "$ROOT"/usr/bin/rdsktp-agent \
  "$ROOT"/usr/lib/cups/backend/rdsktp "$ROOT"/usr/lib/systemd/system/rdsktp.service \
  "$ROOT"/usr/lib/systemd/system/rdsktp-authd.service "$ROOT"/usr/lib/systemd/system/rdsktp-agent@.service \
  "$ROOT"/etc/pam.d/rdsktp "$ROOT"/etc/pam.d/rdsktp-session "$ROOT"/usr/lib/sysusers.d/rdsktp.conf \
  "$ROOT"/usr/share/applications/rdsktp.desktop "$ROOT"/usr/share/applications/rdsktp-agent.desktop
rm -rf "$ROOT"/usr/lib/rdsktp
[ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
echo "rdsktp removed"
