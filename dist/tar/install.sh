#!/bin/sh
# Installs the binaries, units and PAM stacks of the archive. usage: ./install.sh [PREFIX-ROOT]   (default: /)
# Run as root. Dependencies to install by hand: fuse3, openh264 (libopenh264), libpipewire, libkscreen (kscreen-doctor), cups and ghostscript (printers).
set -eu
cd "$(dirname "$0")"
ROOT="${1:-}"
if [ -z "${PAM:-}" ]; then if [ -f "$ROOT/etc/debian_version" ]; then PAM=debian; else PAM=rpm; fi; fi
install -d "$ROOT/usr/bin" "$ROOT/usr/lib/rdsktp" "$ROOT/usr/lib/cups/backend" "$ROOT/usr/lib/systemd/system" \
  "$ROOT/etc/pam.d" "$ROOT/usr/lib/sysusers.d" "$ROOT/usr/share/applications"
install -m755 bin/* "$ROOT/usr/bin/"
install -m755 lib/rdsktp/rdsktp-testapp "$ROOT/usr/lib/rdsktp/"
install -m700 lib/cups-backend/rdsktp "$ROOT/usr/lib/cups/backend/rdsktp"
install -m644 systemd/* "$ROOT/usr/lib/systemd/system/"
for f in rdsktp rdsktp-session; do
  [ -e "$ROOT/etc/pam.d/$f" ] || install -m644 "pam/$PAM/$f" "$ROOT/etc/pam.d/$f"   # an existing (edited) stack is kept
done
[ -e "$ROOT/etc/rdsktp/rdsktp.conf" ] || { install -d "$ROOT/etc/rdsktp"; install -m644 share/rdsktp-server.conf "$ROOT/etc/rdsktp/rdsktp.conf"; }   # an edited file is kept
install -m644 share/rdsktp.conf "$ROOT/usr/lib/sysusers.d/rdsktp.conf"
install -m644 share/rdsktp.desktop share/rdsktp-agent.desktop "$ROOT/usr/share/applications/"
if [ -z "$ROOT" ]; then
  command -v systemd-sysusers >/dev/null 2>&1 && systemd-sysusers /usr/lib/sysusers.d/rdsktp.conf || true
  command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
fi
echo "rdsktp installed (PAM stacks: $PAM). Enable with: systemctl enable --now rdsktp-authd"
