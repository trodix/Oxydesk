#!/usr/bin/env bash
# Drive and printer redirections of real RDP clients (xfreerdp, Remmina, in a container) against the login mode (PAM +
# per-user agent) of the Debian 13 test VM. The VM must be up and deployed in login mode, with CUPS:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep
# The agent of the VM serves the redirections; they are checked from here through SSH: the drive mount is read with
# sshfs, the socket of the printer is forwarded by `ssh -L`, and a job also goes through the CUPS queue (`lp -o raw`).
# usage: RDP_TEST_PASSWORD=... ci/clients-login-test.sh [xfreerdp|remmina ...]   (default: both)
# env: CONTAINER_ENGINE VM_RDP_PORT (3390) VM_USER (dev)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
ENGINE="${CONTAINER_ENGINE:-}"
for e in podman docker; do [[ -n "$ENGINE" ]] || ! command -v "$e" >/dev/null || ENGINE="$e"; done
[[ -n "$ENGINE" ]] || { echo "neither podman nor docker found (or set CONTAINER_ENGINE)" >&2; exit 2; }
command -v sshfs >/dev/null || { echo "sshfs is required" >&2; exit 2; }
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 2; }
CLIENTS=("$@"); [[ ${#CLIENTS[@]} -gt 0 ]] || CLIENTS=(xfreerdp remmina)
PORT="${VM_RDP_PORT:-3390}"; VUSER="${VM_USER:-dev}"
ART="$(realpath -m artifacts/clients-login-test)"
IMAGE=rdsktp-clients:dev
VM="bash vm/vm.sh"
# shellcheck disable=SC1091
KEY="$PWD/vm/state/id_ed25519"; SSH_PORT="${VM_SSH_PORT:-2222}"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the VM first (see the header)" >&2; exit 2; }

fusermount3 -u -z "$ART/mnt" 2>/dev/null
rm -rf "$ART" 2>/dev/null || { [[ "$ENGINE" == podman ]] && podman unshare rm -rf "$ART"; } || { echo "cannot clean $ART" >&2; exit 2; }
mkdir -p "$ART/mnt" "$ART/sock"
"$ENGINE" build -q -t "$IMAGE" docker/clients >/dev/null || { echo "image build failed" >&2; exit 2; }
cargo build -q --release -p rdp-test-client --features ironrdp || exit 2
CHECK="${CARGO_TARGET_DIR:-target}/release/rdp-test-client"
RUNDIR="$($VM ssh 'echo /run/user/$(id -u)')"
failed=0
SSH_PIDS=()
cleanup() {
  fusermount3 -u -z "$ART/mnt" 2>/dev/null
  for p in "${SSH_PIDS[@]}"; do kill "$p" 2>/dev/null; done
}
trap cleanup EXIT

for c in "${CLIENTS[@]}"; do
  mkdir -p "$ART/$c"
  # Each run starts with no connection: the agent outlives them, the redirections go away with the client.
  $VM ssh "rm -rf $RUNDIR/rdsktp-printers/* 2>/dev/null; true" >/dev/null 2>&1
  env=(-e "RDP_PORT=$PORT" -e "RDP_USER=$VUSER" -e "RDP_PASS=$RDP_TEST_PASSWORD" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)")

  # --- Drives: the client shares a directory, the agent mounts it in the VM (FUSE), sshfs shows the mount here.
  name=Partage; [[ "$c" == remmina ]] && name=out_share
  "$ENGINE" run --rm --network host --device /dev/fuse --cap-add SYS_ADMIN --security-opt label=disable "${env[@]}" \
    -v "$ART/$c:/out:Z" "$IMAGE" "$c" drives >"$ART/$c.drives.txt" 2>&1 & cpid=$!
  $VM ssh "mkdir -p $RUNDIR/rdsktp-drives" >/dev/null 2>&1
  sshfs "$VUSER@127.0.0.1:$RUNDIR/rdsktp-drives" "$ART/mnt" -p "$SSH_PORT" \
    -o "IdentityFile=$KEY,IdentitiesOnly=yes,StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR,cache=no,BatchMode=yes"
  "$CHECK" --out "$ART/$c" --drives-only --drive-share "$ART/$c/share" --drive-mount "$ART/mnt" --drive-name "$name" \
    --drive-mib 16 --timeout 90 >"$ART/$c.drives.json" 2>&1; rc=$?
  touch "$ART/$c/drives.done"
  wait "$cpid" 2>/dev/null
  fusermount3 -u -z "$ART/mnt" 2>/dev/null
  cat "$ART/$c.drives.txt"
  if [[ $rc -eq 0 ]]; then echo "PASS  login/$c/drives_files  $(grep -o '"detail": *"[^"]*"' "$ART/$c.drives.json" | head -1)"
  else echo "FAIL  login/$c/drives_files  $(cat "$ART/$c.drives.json")"; failed=1; fi
  grep -q FAIL "$ART/$c.drives.txt" && failed=1

  # The previous client's printers go away a moment after its disconnection.
  for _ in $(seq 60); do $VM ssh "test -z \"\$(ls $RUNDIR/rdsktp-printers 2>/dev/null)\"" >/dev/null 2>&1 && break; sleep 0.5; done
  # --- Printers: a queue of the container records the jobs. A job goes to the socket of the agent (forwarded here),
  # then one through the CUPS queue of the user in the VM.
  "$ENGINE" run --rm --network host --security-opt label=disable "${env[@]}" \
    -v "$ART/$c:/out:Z" "$IMAGE" "$c" printers >"$ART/$c.printers.txt" 2>&1 & cpid=$!
  sock="$RUNDIR/rdsktp-printers/Recorder.sock"
  for _ in $(seq 120); do $VM ssh "test -S $sock" >/dev/null 2>&1 && break; sleep 0.5; done
  rm -f "$ART/sock/Recorder.sock"
  ssh -i "$KEY" -p "$SSH_PORT" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    -o ExitOnForwardFailure=yes -N -L "$ART/sock/Recorder.sock:$sock" "$VUSER@127.0.0.1" & fwd=$!
  SSH_PIDS+=("$fwd")
  "$CHECK" --out "$ART/$c" --printers-only --printer-socket "$ART/sock" --printer-record "$ART/$c/printed" \
    --printer-name Recorder --printer-kib 256 --timeout 90 >"$ART/$c.printers.json" 2>&1; rc=$?
  if [[ $rc -eq 0 ]]; then echo "PASS  login/$c/printers_socket  $(grep -o '"detail": *"[^"]*"' "$ART/$c.printers.json" | head -1)"
  else echo "FAIL  login/$c/printers_socket  $(cat "$ART/$c.printers.json")"; failed=1; fi
  kill "$fwd" 2>/dev/null
  # Through CUPS: the queue exists, takes a raw job and the bytes come out unchanged.
  queue="$VUSER-Recorder"
  head -c 300000 /dev/urandom > "$ART/$c/cups.bin"
  before=$(ls "$ART/$c/printed" 2>/dev/null | wc -l)
  if $VM ssh "lpstat -v $queue" >"$ART/$c.lpstat.txt" 2>&1 && \
     $VM ssh "lp -d $queue -o raw" < "$ART/$c/cups.bin" >"$ART/$c.lp.txt" 2>&1; then
    ok=1
    for _ in $(seq 60); do
      for f in "$ART/$c"/printed/*; do cmp -s "$f" "$ART/$c/cups.bin" && ok=0; done
      [[ $ok -eq 0 ]] && break; sleep 0.5
    done
    if [[ $ok -eq 0 ]]; then echo "PASS  login/$c/printers_cups  300000 bytes through $queue"
    else echo "FAIL  login/$c/printers_cups  job not recorded ($(cat "$ART/$c.lp.txt"))"; failed=1; fi
  else
    echo "FAIL  login/$c/printers_cups  $(cat "$ART/$c.lpstat.txt" "$ART/$c.lp.txt" 2>/dev/null | head -3)"; failed=1
  fi
  touch "$ART/$c/printers.done"
  wait "$cpid" 2>/dev/null
  cat "$ART/$c.printers.txt"
  grep -q FAIL "$ART/$c.printers.txt" && failed=1
  # The queue and the socket go away with the connection.
  sleep 3
  if $VM ssh "lpstat -v $queue" >/dev/null 2>&1; then echo "FAIL  login/$c/printers_cleanup  queue $queue still there"; failed=1
  else echo "PASS  login/$c/printers_cleanup  queue removed on disconnect"; fi
done
[[ $failed -eq 0 ]] && echo "== clients login: passed" || echo "== clients login: FAILED (logs in $ART)"
exit $failed
