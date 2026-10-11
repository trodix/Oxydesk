#!/usr/bin/env bash
# Real RDP clients (xfreerdp, Remmina) in a container against `rdsktp --test-mode`: nothing to install on the host
# except Docker or Podman. usage: ci/clients-test.sh [xfreerdp|remmina ...]   (default: both)
# env: CONTAINER_ENGINE (docker|podman, default: whichever is installed) RDSKTP (binary) CLIENTS_TEST_PORT (34100)
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

failed=0
for c in "${CLIENTS[@]}"; do
  # A fresh server per client: the synthetic desktop remembers the last size requested by a client.
  # RDSKTP_ECHO_FILES=copy: the echoed files are copied aside, a client drops the contents of its old clipboard.
  RDSKTP_ECHO_FILES=copy "$RDSKTP" --test-mode --listen "127.0.0.1:$PORT" --state-dir "$ART/state" >"$ART/server-$c.log" 2>&1 & srv=$!
  sleep 1
  mkdir -p "$ART/$c"
  # --network host: the client reaches the server on the host loopback (rootless Podman included).
  # :Z relabels the volume under SELinux; --userns keep-id is not needed, the script chowns the output.
  # /dev/fuse: FreeRDP exposes the files of the remote clipboard through a FUSE mount.
  "$ENGINE" run --rm --network host --device /dev/fuse --cap-add SYS_ADMIN --security-opt label=disable -e "RDP_PORT=$PORT" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
    -v "$PWD/$ART/$c:/out:Z" "$IMAGE" "$c" 2>&1 | tee "$ART/$c.txt" || failed=1
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
done
[[ $failed -eq 0 ]] && echo "== clients: passed" || echo "== clients: FAILED (logs and screenshots in $ART)"
exit $failed
