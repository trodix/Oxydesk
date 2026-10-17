#!/usr/bin/env bash
# Video bitrate regulation against a slow link: sdl-freerdp (offscreen) connects to `oxydesk --test-mode` through
# ci/throttle-proxy.py (RATE_KBIT, DELAY_MS), once with --adaptive-bitrate off and once with on. The server's own
# "frame-pump" lines give the round trip measured from the client's frame acknowledgements.
# The check: with regulation, the last round trips are lower than without, and the client keeps receiving frames.
# usage: ci/rate-test.sh [out-dir]
# env: FREERDP (sdl-freerdp) OXYDESK RATE_KBIT (3000) DELAY_MS (20) RATE_SECONDS (45)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
FREERDP="${FREERDP:-sdl-freerdp}"
OXYDESK="${OXYDESK:-target/debug/oxydesk}"
OUT="$(realpath -m "${1:-artifacts/rate-test}")"
RATE="${RATE_KBIT:-3000}"; DELAY="${DELAY_MS:-20}"; SECS="${RATE_SECONDS:-45}"
SRV=33895; PROXY=33896
command -v "$FREERDP" >/dev/null || { echo "missing client: $FREERDP" >&2; exit 2; }
cargo build -q -p oxydesk || exit 2
rm -rf "$OUT"; mkdir -p "$OUT"
export SDL_VIDEODRIVER=offscreen
pids=()
trap 'kill "${pids[@]}" 2>/dev/null' EXIT

run() { # mode
  local mode="$1"
  RUST_LOG=info "$OXYDESK" --test-mode --listen "127.0.0.1:$SRV" --state-dir "$OUT/state-$mode" --adaptive-bitrate "$mode" >"$OUT/$mode.raw" 2>&1 & local s=$!
  python3 ci/throttle-proxy.py "127.0.0.1:$PROXY" "127.0.0.1:$SRV" "$RATE" "$DELAY" >"$OUT/$mode.proxy" 2>&1 & local p=$!
  for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PROXY") 2>/dev/null && break; sleep 0.1; done
  timeout "$SECS" "$FREERDP" /v:127.0.0.1:$PROXY /u:test /p:x /cert:ignore /log-level:WARN \
    "/log-filters:com.freerdp.channels.rdpgfx.client:DEBUG" >"$OUT/$mode.client" 2>&1
  kill $s $p 2>/dev/null; wait $s $p 2>/dev/null
  sed 's/\x1b\[[0-9;]*m//g' "$OUT/$mode.raw" >"$OUT/$mode.log"; rm -f "$OUT/$mode.raw"
}

# mean of the rtt_ms field over the lines "frame-pump" after the first ones (the link has settled)
rtt() { grep "frame-pump" "$OUT/$1.log" | tail -n +2 | sed -n 's/.* rtt_ms=\([0-9.]*\).*/\1/p' | awk '{s+=$1;n++} END{if(n)printf "%.1f",s/n; else print "nan"}'; }

run off; run on
off=$(rtt off); on=$(rtt on)
fo=$(grep -c "Got GFX RDPGFX_CODECID_AVC420" "$OUT/off.client"); fn=$(grep -c "Got GFX RDPGFX_CODECID_AVC420" "$OUT/on.client")
echo "link ${RATE} kbit/s +${DELAY} ms: mean RTT off=${off} ms (${fo} frames), on=${on} ms (${fn} frames)"
grep "rate regulation" "$OUT/on.log" | tail -3
ok=0
awk -v a="$off" -v b="$on" 'BEGIN{exit !(a!="nan" && b!="nan" && b < a)}' || { echo "FAIL regulated RTT is not lower"; ok=1; }
[[ "$fn" -gt 50 ]] || { echo "FAIL too few frames with regulation"; ok=1; }
[[ $ok -eq 0 ]] && echo "PASS rate regulation" || exit 1
