#!/bin/sh
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers /usr/lib/sysusers.d/rdsktp.conf || true
fi
exit 0
