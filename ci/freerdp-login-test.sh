#!/usr/bin/env bash
# FreeRDP against the login mode (PAM + per-user agent) of the Debian 13 test VM: H.264, clients without H.264
# (uncompressed pixels through EGFX) and without EGFX (bitmap updates), back to H.264 on the same agent,
# wrong password refused. The VM must be up and deployed in login mode:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep
# usage: RDP_TEST_PASSWORD=... ci/freerdp-login-test.sh [out-dir]
# env: FREERDP (default sdl-freerdp) VM_RDP_PORT (3390) VM_USER (dev) FREERDP_TEST_SECONDS (10)
set -uo pipefail
cd "$(dirname "$0")/.."
FREERDP="${FREERDP:-sdl-freerdp}"
PORT="${VM_RDP_PORT:-3390}"; USER_NAME="${VM_USER:-dev}"; SECS="${FREERDP_TEST_SECONDS:-10}"
OUT="$(realpath -m "${1:-artifacts/freerdp-login-test}")"
command -v "$FREERDP" >/dev/null || { echo "missing client: $FREERDP" >&2; exit 1; }
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 1; }
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the VM first (see the header)" >&2; exit 1; }
rm -rf "$OUT"; mkdir -p "$OUT"
export SDL_VIDEODRIVER=offscreen
FILTERS="/log-filters:com.freerdp.channels.rdpgfx.client:DEBUG,com.freerdp.gdi:DEBUG,com.freerdp.core:INFO"
failed=0
check() { if [[ "$2" -eq 0 ]]; then echo "PASS  $1  $3"; else echo "FAIL  $1  $3"; failed=1; fi; }
client() { # name, password, extra args...: returns the client's exit code (124 = still connected at the end)
  local name="$1" pw="$2"; shift 2
  timeout "$SECS" "$FREERDP" "/v:127.0.0.1:$PORT" "/u:$USER_NAME" "/p:$pw" /cert:ignore /log-level:WARN "$FILTERS" "$@" \
    >"$OUT/$name.log" 2>&1
}
count() { grep -c "$2" "$OUT/$1.log"; }

client h264 "$RDP_TEST_PASSWORD"; rc=$?
n=$(count h264 "Got GFX RDPGFX_CODECID_AVC420")
check login_h264 "$([[ $rc -eq 124 && $n -ge 20 ]] && echo 0 || echo 1)" "exit $rc, $n H.264 frames"
check login_graphics_reset "$(grep -q "RecvResetGraphicsPdu: width: 1920 height: 1080" "$OUT/h264.log"; echo $?)" "1920x1080"

client noavc "$RDP_TEST_PASSWORD" "/gfx:AVC420:off,AVC444:off"; rc=$?
u=$(count noavc "Got GFX RDPGFX_CODECID_UNCOMPRESSED"); a=$(count noavc "Got GFX RDPGFX_CODECID_AVC420")
check login_no_h264 "$([[ $u -ge 20 && $a -eq 0 ]] && echo 0 || echo 1)" "exit $rc, $u uncompressed frames, $a H.264"

client nogfx "$RDP_TEST_PASSWORD" /network:modem; rc=$?
b=$(count nogfx "gdi_surface_bits"); a=$(count nogfx "Got GFX RDPGFX_CODECID_AVC420")
check login_no_gfx "$([[ $b -ge 20 && $a -eq 0 ]] && echo 0 || echo 1)" "exit $rc, $b bitmap updates, $a H.264"

# The agent outlives the connections: a client with H.264 right after the fallbacks gets its stream back.
client h264_again "$RDP_TEST_PASSWORD"; rc=$?
n=$(count h264_again "Got GFX RDPGFX_CODECID_AVC420"); u=$(count h264_again "Got GFX RDPGFX_CODECID_UNCOMPRESSED")
check login_back_to_h264 "$([[ $n -ge 20 && $u -eq 0 ]] && echo 0 || echo 1)" "exit $rc, $n H.264 frames, $u uncompressed"

client wrong "wrong-$RANDOM"; rc=$?
check login_wrong_password "$([[ $rc -ne 124 && $(count wrong "Got GFX") -eq 0 ]] && echo 0 || echo 1)" "exit $rc, no frame"

echo
[[ $failed -eq 0 ]] && echo "freerdp-login: all checks passed" || echo "freerdp-login: failures (logs in $OUT)"
exit $failed
