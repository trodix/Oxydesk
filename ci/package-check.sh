#!/usr/bin/env bash
# Checks an installed rdsktp (any package format) inside a container: files and modes, system user, PAM stack, libraries, ELF
# architecture, systemd units, then the server really starts (test mode) and answers on its port. Runs as root.
# usage: ci/package-check.sh deb|rpm|tar
set -uo pipefail
FORMAT="${1:-}"; fail=0
ok()  { echo "PASS  $1"; }
bad() { echo "FAIL  $1  ${2:-}"; fail=1; }
check() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1" "$2"; fi; }

for f in /usr/bin/rdsktp /usr/bin/rdsktp-authd /usr/bin/rdsktp-agent /usr/lib/rdsktp/rdsktp-testapp; do check "exec $f" "test -x $f"; done
check "cups backend 0700 root" "[ \"\$(stat -c '%a %U' /usr/lib/cups/backend/rdsktp)\" = '700 root' ]"
for u in rdsktp.service rdsktp-authd.service rdsktp-agent@.service; do
  check "unit $u" "test -f /usr/lib/systemd/system/$u -o -f /lib/systemd/system/$u"
done
check "sysusers" "test -f /usr/lib/sysusers.d/rdsktp.conf"
check "user rdsktp" "getent passwd rdsktp"

# PAM stacks: every included stack exists on this distribution.
for s in rdsktp rdsktp-session; do
  check "pam $s" "test -f /etc/pam.d/$s"
  for inc in $(awk '$2 ~ /include|substack/ {print $3}' "/etc/pam.d/$s" 2>/dev/null); do
    check "pam $s -> $inc" "test -f /etc/pam.d/$inc"
  done
done

# Libraries and architecture.
arch="$(uname -m)"; case "$arch" in x86_64) elf="Advanced Micro Devices X86-64" ;; aarch64) elf="AArch64" ;; esac
for b in rdsktp rdsktp-authd rdsktp-agent; do
  check "ldd $b" "! ldd /usr/bin/$b 2>&1 | grep -q 'not found'"
  check "ELF $b is $arch" "readelf -h /usr/bin/$b | grep -q '$elf'"
done

# Units are accepted by systemd.
if command -v systemd-analyze >/dev/null; then
  for u in rdsktp.service rdsktp-authd.service; do
    p=/usr/lib/systemd/system/$u; [ -f "$p" ] || p=/lib/systemd/system/$u
    check "systemd-analyze verify $u" "! systemd-analyze verify $p 2>&1 | grep -E 'Unknown (key|section)|Invalid|Failed to parse|bad setting'"
  done
fi

# The server starts and listens (test mode: synthetic desktop, no compositor needed).
mkdir -p /tmp/st; RUST_LOG=info /usr/bin/rdsktp --test-mode --listen 127.0.0.1:3399 --state-dir /tmp/st >/tmp/rdsktp.log 2>&1 &
pid=$!
listening=1; for _ in $(seq 50); do (exec 3<>/dev/tcp/127.0.0.1/3399) 2>/dev/null && { listening=0; break; }; sleep 0.2; done
kill $pid 2>/dev/null; wait $pid 2>/dev/null
[ $listening -eq 0 ] && ok "server listens" || bad "server listens" "$(tail -3 /tmp/rdsktp.log)"
/usr/bin/rdsktp --help >/dev/null 2>&1 && ok "rdsktp --help" || bad "rdsktp --help"
[ $fail -eq 0 ] && echo "== $FORMAT: passed" || echo "== $FORMAT: FAILED"
exit $fail
