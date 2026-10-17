#!/usr/bin/env bash
# Compatibility test with FreeRDP (the engine of xfreerdp, Remmina, wlfreerdp): the SDL client runs headless
# (SDL_VIDEODRIVER=offscreen) against the synthetic desktop of `oxydesk --test-mode`. The checks read the
# client's own traces (capabilities confirmed, graphics reset, surfaces, H.264 frames received) and the
# server's log. What cannot be seen without a display (pixels, keyboard, clipboard) is not covered here.
# usage: ci/freerdp-test.sh [out-dir]
# env: FREERDP (default sdl-freerdp), OXYDESK (server binary), FREERDP_SESSION=kwin (real KWin session of
# this host with the test application, instead of the synthetic desktop)
set -uo pipefail
cd "$(dirname "$0")/.."
FREERDP="${FREERDP:-sdl-freerdp}"
OXYDESK="${OXYDESK:-target/debug/oxydesk}"
OUT="$(realpath -m "${1:-artifacts/freerdp-test}")"
PORT="${FREERDP_TEST_PORT:-33890}"
SECONDS_RUN="${FREERDP_TEST_SECONDS:-6}"
command -v "$FREERDP" >/dev/null || { echo "missing client: $FREERDP (Fedora: sudo dnf install freerdp)" >&2; exit 1; }
if [[ "${FREERDP_SESSION:-}" == kwin ]]; then
  cargo build -q -p oxydesk -p oxydesk-testapp --features oxydesk/kwin || exit 1
  SESSION=(--session kwin --kwin-socket oxydesk-freerdp-test
    --session-app "$(realpath target/debug/oxydesk-testapp) --tone --clipboard-echo")
else
  [[ -x "$OXYDESK" ]] || cargo build -q -p oxydesk || exit 1
  SESSION=(--test-mode)
fi
rm -rf "$OUT"; mkdir -p "$OUT/state"
export SDL_VIDEODRIVER=offscreen
FILTERS="/log-filters:com.freerdp.channels.rdpgfx.client:DEBUG,com.freerdp.gdi:DEBUG,com.freerdp.core:INFO"
failed=0
SERVER_PID=""

check() { # name, condition (0 = ok), detail
  if [[ "$2" -eq 0 ]]; then echo "PASS  $1  $3"; else echo "FAIL  $1  $3"; failed=1; fi
}

start_server() { # name, server args...
  local name="$1"; shift
  RUST_LOG=info "$OXYDESK" "${SESSION[@]}" --listen "127.0.0.1:$PORT" --state-dir "$OUT/state" "$@" \
    >"$OUT/$name.server.raw" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && return 0; sleep 0.1; done
  echo "server not listening (see $OUT/$name.server.raw)" >&2; return 1
}

stop_server() { # name
  kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null
  sed 's/\x1b\[[0-9;]*m//g' "$OUT/$1.server.raw" >"$OUT/$1.server.log"
  rm -f "$OUT/$1.server.raw"
}

client() { # logfile, extra args... ; returns the client's exit code (124 = still connected at the end)
  local log="$1"; shift
  timeout "${CLIENT_SECONDS:-$SECONDS_RUN}" "$FREERDP" /v:127.0.0.1:"$PORT" /u:test /p:"${FREERDP_PASSWORD:-x}" /cert:ignore \
    /log-level:WARN "$FILTERS" "$@" >"$log" 2>&1
}

frames() { grep -c "Got GFX RDPGFX_CODECID_AVC420" "$1"; }
surfaces() { grep -c "RecvCreateSurfacePdu" "$1"; }
# Errors that are not the consequence of the end-of-test signal.
client_errors() { grep "\[ERROR\]" "$1" | grep -v -E "fsig_term_handler|CONNECT_CANCELLED|handleShow" || true; }

# 1. TLS connection, H.264 through EGFX, audio channel
start_server tls || exit 1
client "$OUT/tls.client.log"; rc=$?
stop_server tls
n=$(frames "$OUT/tls.client.log")
check connect_tls "$([[ $rc -eq 124 ]] && echo 0 || echo 1)" "client still connected after ${SECONDS_RUN}s (exit $rc)"
check gfx_negotiated "$(grep -q "RecvCapsConfirmPdu" "$OUT/tls.client.log"; echo $?)" "EGFX capabilities confirmed"
check graphics_reset "$(grep -q "RecvResetGraphicsPdu: width: 1920 height: 1080" "$OUT/tls.client.log"; echo $?)" "1920x1080"
check h264_frames "$([[ $n -ge 20 ]] && echo 0 || echo 1)" "$n AVC420 frames in ${SECONDS_RUN}s (>= 20)"
check audio_stream "$(grep -q "audio: stream started" "$OUT/tls.server.log"; echo $?)" "server opened the sound channel"
check client_no_error "$([[ -z "$(client_errors "$OUT/tls.client.log")" ]]; echo $?)" "$(client_errors "$OUT/tls.client.log" | head -1)"

# 1b. Client that declares it cannot decode H.264: uncompressed pixels through EGFX, no AVC420
start_server nohh264 || exit 1
client "$OUT/nohh264.client.log" "/gfx:AVC420:off,AVC444:off"
stop_server nohh264
u=$(grep -c "Got GFX RDPGFX_CODECID_UNCOMPRESSED" "$OUT/nohh264.client.log")
check h264_declined "$([[ $u -ge 10 && $(frames "$OUT/nohh264.client.log") -eq 0 ]] && echo 0 || echo 1)" "$u uncompressed frame(s), $(frames "$OUT/nohh264.client.log") H.264"
check h264_declined_flag "$(grep -q "AVC_DISABLED" "$OUT/nohh264.server.log"; echo $?)" "AVC_DISABLED kept in the confirmed capabilities"

# 1c. Client without EGFX (slow network profile): bitmap updates after the fallback delay
start_server nogfx || exit 1
CLIENT_SECONDS=$((SECONDS_RUN + 5)) client "$OUT/nogfx.client.log" /network:modem "/log-filters:com.freerdp.gdi:DEBUG"
stop_server nogfx
b=$(grep -c "gdi_surface_bits" "$OUT/nogfx.client.log")
check no_gfx_bitmaps "$([[ $b -ge 10 && $(frames "$OUT/nogfx.client.log") -eq 0 ]] && echo 0 || echo 1)" "$b bitmap update(s) without EGFX"

# 2. Two outputs: one surface per output in a desktop twice as wide
start_server outputs --outputs 2 || exit 1
client "$OUT/outputs.client.log"; rc=$?
stop_server outputs
check two_outputs "$(grep -q "RecvResetGraphicsPdu: width: 3840 height: 1080" "$OUT/outputs.client.log"; echo $?)" "desktop 3840x1080"
s=$(surfaces "$OUT/outputs.client.log")
check two_surfaces "$([[ $s -eq 2 ]] && echo 0 || echo 1)" "$s surface(s) created"
check two_outputs_frames "$([[ $(frames "$OUT/outputs.client.log") -ge 20 ]] && echo 0 || echo 1)" "$(frames "$OUT/outputs.client.log") frames"

# 3. Reconnection: the server keeps serving after a client left
start_server reconnect || exit 1
client "$OUT/reconnect1.client.log"
client "$OUT/reconnect2.client.log"
stop_server reconnect
check reconnect "$([[ $(frames "$OUT/reconnect2.client.log") -ge 20 ]] && echo 0 || echo 1)" "second connection: $(frames "$OUT/reconnect2.client.log") frames"

# 4. NLA (CredSSP): good password accepted, wrong password refused
export OXYDESK_PASSWORD="nla-test-secret"
start_server nla --nla-user test || exit 1
FREERDP_PASSWORD="$OXYDESK_PASSWORD" client "$OUT/nla_ok.client.log" /sec:nla; rc=$?
check nla_accepted "$([[ $rc -eq 124 && $(frames "$OUT/nla_ok.client.log") -ge 20 ]] && echo 0 || echo 1)" "exit $rc, $(frames "$OUT/nla_ok.client.log") frames"
FREERDP_PASSWORD="wrong" client "$OUT/nla_bad.client.log" /sec:nla; rc=$?
check nla_refused "$([[ $rc -ne 124 && $(frames "$OUT/nla_bad.client.log") -eq 0 ]] && echo 0 || echo 1)" "exit $rc, $(frames "$OUT/nla_bad.client.log") frames"
stop_server nla
unset OXYDESK_PASSWORD

# 5. Server health over the whole run
check server_no_error "$(! grep -h -E "ERROR|panicked" "$OUT"/*.server.log | grep -v -E "client loop failure|Connection error" | grep -q .; echo $?)" "no ERROR / panic in the server logs"

echo
[[ $failed -eq 0 ]] && echo "freerdp: all checks passed" || echo "freerdp: failures (logs in $OUT)"
exit $failed
