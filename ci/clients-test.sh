#!/usr/bin/env bash
# Real RDP clients (xfreerdp, Remmina) in a container against `rdsktp --test-mode`: nothing to install on the host
# except Docker or Podman. usage: ci/clients-test.sh [xfreerdp|remmina ...]   (default: both)
# env: CLIENTS_FULL=0 (only the drive and printer redirections) CLIENTS_DRIVES=0 CLIENTS_PRINTERS=0 CONTAINER_ENGINE (docker|podman, default: whichever is installed) RDSKTP (binary) CLIENTS_TEST_PORT (34100)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
ENGINE="${CONTAINER_ENGINE:-}"
for e in podman docker; do [[ -n "$ENGINE" ]] || ! command -v "$e" >/dev/null || ENGINE="$e"; done
[[ -n "$ENGINE" ]] || { echo "neither podman nor docker found (or set CONTAINER_ENGINE)" >&2; exit 2; }
CLIENTS=("$@"); [[ ${#CLIENTS[@]} -gt 0 ]] || CLIENTS=(xfreerdp remmina)
PORT="${CLIENTS_TEST_PORT:-34100}"
ART=artifacts/clients-test
# Files written by a rootless Podman container may belong to a sub-uid: remove them from inside its user namespace.
rm -rf "$ART" 2>/dev/null || { [[ "$ENGINE" == podman ]] && podman unshare rm -rf "$ART"; } || { echo "cannot clean $ART" >&2; exit 2; }
mkdir -p "$ART"
IMAGE=rdsktp-clients:dev

echo "== engine: $ENGINE"
"$ENGINE" build -q -t "$IMAGE" docker/clients >/dev/null || { echo "image build failed" >&2; exit 2; }
RDSKTP="${RDSKTP:-}"
if [[ -z "$RDSKTP" ]]; then
  cargo build -q -p rdsktp || exit 2
  RDSKTP="${CARGO_TARGET_DIR:-target}/debug/rdsktp"
fi

# The checker is the test client's `drives` scenario, without the connection.
cargo build -q --release -p rdp-test-client --features ironrdp || exit 2
DRIVE_CHECK="${CARGO_TARGET_DIR:-target}/release/rdp-test-client"
failed=0
for c in "${CLIENTS[@]}"; do
  # A fresh server per client: the synthetic desktop remembers the last size requested by a client.
  # RDSKTP_ECHO_FILES=copy: the echoed files are copied aside, a client drops the contents of its old clipboard.
  mkdir -p "$ART/mnt"
  RDSKTP_ECHO_FILES=copy "$RDSKTP" --test-mode --listen "127.0.0.1:$PORT" --state-dir "$ART/state" --drives-dir "$ART/mnt" --printers "$ART/rdsktp-printers" --printers-cups off >"$ART/server-$c.log" 2>&1 & srv=$!
  sleep 1
  mkdir -p "$ART/$c"
  # Drive redirection: the client shares a directory of its container, the server mounts it on this host, and the
  # files are checked from here while the client stays connected (the container waits for drives.done).
  if [[ "${CLIENTS_DRIVES:-1}" == 1 && -e /dev/fuse ]] && command -v fusermount3 >/dev/null; then
    name=Partage; [[ "$c" == remmina ]] && name=out_share
    "$ENGINE" run --rm --network host --device /dev/fuse --cap-add SYS_ADMIN --security-opt label=disable -e "RDP_PORT=$PORT" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
      -v "$PWD/$ART/$c:/out:Z" "$IMAGE" "$c" drives >"$ART/$c.drives.txt" 2>&1 & cpid=$!
    "$DRIVE_CHECK" --out "$ART/$c" --drives-only --drive-share "$ART/$c/share" --drive-mount "$ART/mnt" --drive-name "$name" \
      --drive-mib 16 --timeout 60 >"$ART/$c.drives.json" 2>&1; rc=$?
    touch "$ART/$c/drives.done"
    wait "$cpid" 2>/dev/null
    cat "$ART/$c.drives.txt"
    if [[ $rc -eq 0 ]]; then echo "PASS  $c/drives_files  $(grep -o '"detail": *"[^"]*"' "$ART/$c.drives.json" | head -1)"
    else echo "FAIL  $c/drives_files  $(cat "$ART/$c.drives.json")"; failed=1; fi
    grep -q FAIL "$ART/$c.drives.txt" && failed=1
  else
    echo "SKIP  $c/drives  no /dev/fuse or fusermount3 on the host"
  fi
  # Printer redirection: the client shares a CUPS queue of its container that records the jobs; a job sent from here to
  # the socket of the desktop must come out there, byte for byte (the container waits for printers.done).
  if [[ "${CLIENTS_PRINTERS:-1}" == 1 ]]; then
    "$ENGINE" run --rm --network host --security-opt label=disable -e "RDP_PORT=$PORT" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
      -v "$PWD/$ART/$c:/out:Z" "$IMAGE" "$c" printers >"$ART/$c.printers.txt" 2>&1 & cpid=$!
    "$DRIVE_CHECK" --out "$ART/$c" --printers-only --printer-socket "$ART/rdsktp-printers" --printer-record "$ART/$c/printed" \
      --printer-name Recorder --printer-kib 256 --timeout 60 >"$ART/$c.printers.json" 2>&1; rc=$?
    touch "$ART/$c/printers.done"
    wait "$cpid" 2>/dev/null
    cat "$ART/$c.printers.txt"
    if [[ $rc -eq 0 ]]; then echo "PASS  $c/printers_files  $(grep -o '"detail": *"[^"]*"' "$ART/$c.printers.json" | head -1)"
    else echo "FAIL  $c/printers_files  $(cat "$ART/$c.printers.json")"; failed=1; fi
    grep -q FAIL "$ART/$c.printers.txt" && failed=1
  fi
  # --network host: the client reaches the server on the host loopback (rootless Podman included).
  # :Z relabels the volume under SELinux; --userns keep-id is not needed, the script chowns the output.
  # /dev/fuse: FreeRDP exposes the files of the remote clipboard through a FUSE mount.
  if [[ "${CLIENTS_FULL:-1}" == 1 ]]; then
    "$ENGINE" run --rm --network host --device /dev/fuse --cap-add SYS_ADMIN --security-opt label=disable -e "RDP_PORT=$PORT" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
      -v "$PWD/$ART/$c:/out:Z" "$IMAGE" "$c" 2>&1 | tee "$ART/$c.txt" || failed=1
  fi
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
done
[[ $failed -eq 0 ]] && echo "== clients: passed" || echo "== clients: FAILED (logs and screenshots in $ART)"
exit $failed
