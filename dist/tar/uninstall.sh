#!/bin/sh
# Removes what install.sh installed (the PAM stacks too; /etc/oxydesk/oxydesk.conf and the user data in /var/lib/oxydesk are kept). usage: ./uninstall.sh [ROOT]
set -eu
ROOT="${1:-}"
[ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1 && systemctl disable --now oxydesk-authd oxydesk 2>/dev/null || true
rm -f "$ROOT"/usr/bin/oxydesk "$ROOT"/usr/bin/oxydesk-authd "$ROOT"/usr/bin/oxydesk-agent \
  "$ROOT"/usr/lib/cups/backend/oxydesk "$ROOT"/usr/lib/systemd/system/oxydesk.service \
  "$ROOT"/usr/lib/systemd/system/oxydesk-authd.service "$ROOT"/usr/lib/systemd/system/oxydesk-agent@.service \
  "$ROOT"/etc/pam.d/oxydesk "$ROOT"/etc/pam.d/oxydesk-session "$ROOT"/usr/lib/sysusers.d/oxydesk.conf \
  "$ROOT"/usr/share/applications/oxydesk.desktop "$ROOT"/usr/share/applications/oxydesk-agent.desktop
rm -rf "$ROOT"/usr/lib/oxydesk
[ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
echo "oxydesk removed"
