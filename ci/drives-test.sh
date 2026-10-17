#!/usr/bin/env bash
# Drive redirection (RDPDR) against the synthetic desktop: the IronRDP test client shares a directory, the server mounts it
# (FUSE) and the `drives` scenario moves files both ways through the mount. Needs /dev/fuse and fusermount3.
# usage: ci/drives-test.sh [out-dir]   env: DRIVES_TEST_MIB (default 64)
set -uo pipefail
cd "$(dirname "$0")/.."
OUT="$(realpath -m "${1:-artifacts/drives-test}")"
PORT="${DRIVES_TEST_PORT:-33891}"
[[ -e /dev/fuse ]] && command -v fusermount3 >/dev/null || { echo "SKIP: /dev/fuse or fusermount3 missing"; exit 0; }
cargo build -q --release -p oxydesk -p rdp-test-client --features rdp-test-client/ironrdp || exit 1
rm -rf "$OUT"; mkdir -p "$OUT/state" "$OUT/share" "$OUT/mnt"
RUST_LOG=info target/release/oxydesk --test-mode --listen "127.0.0.1:$PORT" --state-dir "$OUT/state" --drives-dir "$OUT/mnt" \
  >"$OUT/server.log" 2>&1 & srv=$!
trap 'kill $srv 2>/dev/null; wait $srv 2>/dev/null' EXIT
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
timeout 180 target/release/rdp-test-client --out "$OUT" --port "$PORT" --no-nla --drive-share "$OUT/share" \
  --drive-mount "$OUT/mnt" --drive-mib "${DRIVES_TEST_MIB:-64}" >"$OUT/client.log" 2>&1
python3 - "$OUT/client.json" <<'P'
import json, sys
checks = {c["name"]: c for c in json.load(open(sys.argv[1]))["checks"]}
c = checks.get("drives")
ok = bool(c and c["passed"])
print(("PASS" if ok else "FAIL"), " drives ", c["detail"] if c else "scenario missing")
sys.exit(0 if ok else 1)
P
rc=$?
# Once the client has gone, nothing of its drive must stay mounted.
sleep 1
if grep -q "$OUT/mnt" /proc/mounts; then echo "FAIL  unmount  a drive is still mounted"; rc=1; else echo "PASS  unmount  no drive left mounted"; fi
exit $rc
