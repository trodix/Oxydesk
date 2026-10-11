#!/usr/bin/env bash
# mstsc (VM Windows 11) against the login mode (PAM + per-user agent) of the Debian 13 test VM: the whole scenario of
# vm/win11.sh (connection, drives, printers, clipboard, audio, resize...) and the host side of the drive and printer
# tests, done from here through SSH (sshfs for the mount, `ssh -L` for the socket of the printer). Needs:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep      (Debian VM up, in login mode, with CUPS)
#   a Windows VM image prepared as for `cargo xtask test --client` (vm/win11.sh)
# usage: RDP_TEST_PASSWORD=... ci/mstsc-login-test.sh [out-dir]
# env: VM_RDP_PORT (3390) VM_USER (dev) VM_SSH_PORT (2222)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
command -v sshfs >/dev/null || { echo "sshfs is required" >&2; exit 2; }
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 2; }
PORT="${VM_RDP_PORT:-3390}"; VUSER="${VM_USER:-dev}"; SSH_PORT="${VM_SSH_PORT:-2222}"
ART="$(realpath -m "${1:-artifacts/mstsc-login-test}")"
KEY="$PWD/vm/state/id_ed25519"
VM="bash vm/vm.sh"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the Debian VM first (see the header)" >&2; exit 2; }
fusermount3 -u -z "$ART/mnt" 2>/dev/null
rm -rf "$ART"; mkdir -p "$ART/mnt" "$ART/rdsktp-printers"
RUNDIR="$($VM ssh 'echo /run/user/$(id -u)')"
SSH_OPTS=(-i "$KEY" -p "$SSH_PORT" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
bg=()
cleanup() { fusermount3 -u -z "$ART/mnt" 2>/dev/null; for p in "${bg[@]}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

$VM ssh "mkdir -p $RUNDIR/rdsktp-drives" >/dev/null 2>&1
sshfs "$VUSER@127.0.0.1:$RUNDIR/rdsktp-drives" "$ART/mnt" -p "$SSH_PORT" \
  -o "IdentityFile=$KEY,IdentitiesOnly=yes,StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR,cache=no,BatchMode=yes" || exit 2
# The socket of the guest's printer is forwarded here once the agent has made it.
(
  sock="$RUNDIR/rdsktp-printers/RdsktpTest.sock"
  for _ in $(seq 600); do $VM ssh "test -S $sock" >/dev/null 2>&1 && break; sleep 0.5; done
  # The CUPS queue of the user comes with the socket.
  for _ in $(seq 20); do $VM ssh "lpstat -v $VUSER-RdsktpTest" > "$ART/lpstat.txt" 2>&1 && break; sleep 0.5; done
  exec ssh "${SSH_OPTS[@]}" -o ExitOnForwardFailure=yes -N -L "$ART/rdsktp-printers/RdsktpTest.sock:$sock" "$VUSER@127.0.0.1"
) & bg+=($!)
cargo build -q -p xtask || exit 2
cargo xtask host-check --drives "$ART/mnt" --printers "$ART/rdsktp-printers" --within 400 > "$ART/host-check.txt" 2>&1 & hc=$!
WIN_DRIVES=1 WIN_PRINTERS=1 WIN_EXPECT_RESIZE=0 WIN_LAYOUT_CHECK=0 bash vm/win11.sh test "10.0.2.2:$PORT" "$VUSER" 2>&1 | tee "$ART/win11.log"
rc=${PIPESTATUS[0]}
wait "$hc"; hrc=$?
cat "$ART/host-check.txt"
grep -q "rdsktp:" "$ART/lpstat.txt" 2>/dev/null && echo "PASS queue_cups $(cat "$ART/lpstat.txt")" || { echo "FAIL queue_cups $(cat "$ART/lpstat.txt" 2>&1)"; hrc=1; }
[[ $rc -eq 0 && $hrc -eq 0 ]] && { echo "== mstsc login: passed"; exit 0; }
echo "== mstsc login: FAILED (logs in $ART)"; exit 1
