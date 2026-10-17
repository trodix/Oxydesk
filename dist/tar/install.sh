#!/bin/sh
# Installs the binaries, units and PAM stacks of the archive. usage: ./install.sh [PREFIX-ROOT]   (default: /)
# Run as root. Dependencies to install by hand: fuse3, openh264 (libopenh264), libpipewire, libkscreen (kscreen-doctor), cups and ghostscript (printers).
set -eu
cd "$(dirname "$0")"
ROOT="${1:-}"
if [ -z "${PAM:-}" ]; then if [ -f "$ROOT/etc/debian_version" ]; then PAM=debian; else PAM=rpm; fi; fi
install -d "$ROOT/usr/bin" "$ROOT/usr/lib/oxydesk" "$ROOT/usr/lib/cups/backend" "$ROOT/usr/lib/systemd/system" \
  "$ROOT/etc/pam.d" "$ROOT/usr/lib/sysusers.d" "$ROOT/usr/share/applications"
install -m755 bin/* "$ROOT/usr/bin/"
install -m755 lib/oxydesk/oxydesk-testapp "$ROOT/usr/lib/oxydesk/"
install -m700 lib/cups-backend/oxydesk "$ROOT/usr/lib/cups/backend/oxydesk"
install -m644 systemd/* "$ROOT/usr/lib/systemd/system/"
for f in oxydesk oxydesk-session; do
  [ -e "$ROOT/etc/pam.d/$f" ] || install -m644 "pam/$PAM/$f" "$ROOT/etc/pam.d/$f"   # an existing (edited) stack is kept
done
[ -e "$ROOT/etc/oxydesk/oxydesk.conf" ] || { install -d "$ROOT/etc/oxydesk"; install -m644 share/oxydesk-server.conf "$ROOT/etc/oxydesk/oxydesk.conf"; }   # an edited file is kept
install -m644 share/oxydesk.conf "$ROOT/usr/lib/sysusers.d/oxydesk.conf"
install -m644 share/oxydesk.desktop share/oxydesk-agent.desktop "$ROOT/usr/share/applications/"
if [ -z "$ROOT" ]; then
  command -v systemd-sysusers >/dev/null 2>&1 && systemd-sysusers /usr/lib/sysusers.d/oxydesk.conf || true
  command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
fi
echo "oxydesk installed (PAM stacks: $PAM). Enable with: systemctl enable --now oxydesk-authd"
