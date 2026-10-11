#!/usr/bin/env bash
# Drives a real RDP client in a virtual X server against the synthetic desktop of `rdsktp --test-mode` and checks
# what it displays, like a user would: desktop size, live frames, keyboard (marker block colour), clipboard
# round trip and dynamic resizing.
# usage: run.sh CLIENT [SCENARIO...]    CLIENT: xfreerdp | remmina    SCENARIO: screen keyboard clipboard resize
# env: RDP_HOST (127.0.0.1) RDP_PORT (3389) OUT (/out) DESKTOP (1920x1080) HOST_UID HOST_GID
set -uo pipefail
CLIENT="${1:?client: xfreerdp | remmina}"; shift
SCENARIOS=("$@"); [[ ${#SCENARIOS[@]} -gt 0 ]] || SCENARIOS=(screen keyboard clipboard resize)
HOST="${RDP_HOST:-127.0.0.1}"; PORT="${RDP_PORT:-3389}"; OUT="${OUT:-/out}"
DESKTOP="${DESKTOP:-1920x1080}"; DW="${DESKTOP%x*}"; DH="${DESKTOP#*x}"
PROBE=/opt/clients/probe.py
export DISPLAY=:99
mkdir -p "$OUT"
failed=0

# Marker colours of the keys 1..4 (PALETTE of the synthetic desktop; key 6 is magenta like the corner block).
COLORS=("255 0 0" "0 255 0" "0 0 255" "255 255 0")

check() { # name, status (0 = ok), detail
  if [[ "$2" -eq 0 ]]; then echo "PASS  $CLIENT/$1  $3"; else echo "FAIL  $CLIENT/$1  $3"; failed=1; fi
}

start_x() { # width, height
  Xvfb "$DISPLAY" -screen 0 "${1}x${2}x24" -nolisten tcp >"$OUT/$CLIENT.xvfb.log" 2>&1 & XVFB=$!
  for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done
  openbox >"$OUT/$CLIENT.openbox.log" 2>&1 & WM=$!
  sleep 0.5
}

start_client() { # scenario, extra xfreerdp args...
  local scenario="$1"; shift
  case "$CLIENT" in
    xfreerdp)
      local bin; bin="$(command -v xfreerdp3 || command -v xfreerdp)"
      "$bin" "/v:$HOST:$PORT" /u:test /p:x /cert:ignore /gfx:AVC420 +clipboard "$@" \
        >"$OUT/$CLIENT.$scenario.client.log" 2>&1 & ;;
    remmina)
      cat >"$OUT/remmina.remmina" <<R
[remmina]
name=rdsktp
protocol=RDP
server=$HOST:$PORT
username=test
password=
domain=
resolution_mode=2
resolution_width=$DW
resolution_height=$DH
colordepth=32
gfx_h264=1
cert_ignore=1
ignore-tls-errors=1
disableclipboard=0
viewmode=4
scale=0
R
      dbus-run-session -- remmina -c "$OUT/remmina.remmina" >"$OUT/$CLIENT.$scenario.client.log" 2>&1 &
      CLIENT_PID=$!
      # No stored password (Remmina keeps them encrypted): answer the credentials prompt like a user.
      sleep 6; xdotool type --delay 50 x; xdotool key Return ;;
  esac
  CLIENT_PID=${CLIENT_PID:-$!}
}

stop_all() {
  kill "$CLIENT_PID" 2>/dev/null; pkill -x xclip 2>/dev/null
  kill "$WM" "$XVFB" 2>/dev/null; wait 2>/dev/null
  CLIENT_PID=""
}

shot() { import -window root "$OUT/$CLIENT.$1.png" 2>/dev/null && echo "$OUT/$CLIENT.$1.png"; }

# Locates the desktop in the screenshot from its magenta bottom-right block; sets ORIGIN_X/Y and EDGE_X/Y.
locate() { # expected width, expected height, timeout (s)
  local end=$((SECONDS + ${3:-25})) f box
  while [[ $SECONDS -lt $end ]]; do
    f="$(shot locate)" && box="$(python3 -W ignore "$PROBE" corner "$f")" && {
      read -r _ _ EDGE_X EDGE_Y <<<"$box"
      ORIGIN_X=$((EDGE_X + 1 - $1)); ORIGIN_Y=$((EDGE_Y + 1 - $2))
      [[ $ORIGIN_X -ge 0 && $ORIGIN_Y -ge 0 ]] && return 0
    }
    sleep 0.5
  done
  return 1
}

scenario_screen() {
  start_x 2560 1440
  start_client screen /size:"${DW}x${DH}"
  if locate "$DW" "$DH" 30; then
    check desktop_shown 0 "${DW}x${DH} desktop at +$ORIGIN_X+$ORIGIN_Y"
  else
    check desktop_shown 1 "magenta corner block not found (screenshots in $OUT)"; stop_all; return
  fi
  # The heartbeat block (32,0) alternates between white and dark grey: the frames are live.
  local on=0 off=0 f v
  for _ in $(seq 20); do
    f="$(shot beat)" && v="$(python3 -W ignore "$PROBE" pixel "$f" $((ORIGIN_X + 40)) $((ORIGIN_Y + 8)))"
    read -r r _ _ <<<"$v"
    [[ ${r:-0} -gt 200 ]] && on=1; [[ ${r:-255} -lt 90 && ${r:-255} -gt 20 ]] && off=1
    [[ $on -eq 1 && $off -eq 1 ]] && break; sleep 0.2
  done
  check frames_live "$([[ $on -eq 1 && $off -eq 1 ]]; echo $?)" "heartbeat seen on=$on off=$off"
  stop_all
}

scenario_keyboard() {
  start_x 2560 1440
  start_client keyboard /size:"${DW}x${DH}"
  locate "$DW" "$DH" 30 || { check keyboard 1 "desktop not shown"; stop_all; return; }
  xdotool mousemove $((ORIGIN_X + 400)) $((ORIGIN_Y + 300)) click 1 2>/dev/null
  sleep 1
  local i ok=0 f
  for i in 0 1 2 3; do
    xdotool key "$((i + 1))"
    local end=$((SECONDS + 6)) got=1
    while [[ $SECONDS -lt $end ]]; do
      f="$(shot key$i)" && python3 -W ignore "$PROBE" match "$f" $((ORIGIN_X + 12)) $((ORIGIN_Y + 12)) ${COLORS[$i]} >/dev/null && { got=0; break; }
      sleep 0.3
    done
    check "key_$((i + 1))" "$got" "marker colour (${COLORS[$i]})"
  done
  stop_all
}

scenario_clipboard() {
  start_x 2560 1440
  start_client clipboard /size:"${DW}x${DH}"
  locate "$DW" "$DH" 30 || { check clipboard 1 "desktop not shown"; stop_all; return; }
  local text="héllo € rdsktp $RANDOM"
  # Our copy is announced to the server, which echoes it back: the client then owns the clipboard.
  printf '%s' "$text" | xclip -selection clipboard -i
  sleep 3
  pkill -x xclip; sleep 3
  local got; got="$(xclip -selection clipboard -o 2>/dev/null)"
  check clipboard_roundtrip "$([[ "$got" == "$text" ]]; echo $?)" "sent '$text', read back '$got'"
  stop_all
}

scenario_resize() {
  [[ "$CLIENT" == xfreerdp ]] || { echo "SKIP  $CLIENT/resize  (dynamic resolution driven for xfreerdp only)"; return; }
  start_x 2560 1440
  start_client resize /size:1280x720 /dynamic-resolution
  locate 1280 720 30 || { check resize 1 "desktop not shown"; stop_all; return; }
  local win; win="$(xdotool search --onlyvisible --class -- xfreerdp 2>/dev/null | head -1)"
  [[ -n "$win" ]] || win="$(xdotool search --onlyvisible --name FreeRDP 2>/dev/null | head -1)"
  xdotool windowsize "$win" 1000 600
  if locate 1000 600 20; then check resize 0 "desktop follows the window: 1000x600 at +$ORIGIN_X+$ORIGIN_Y"
  else check resize 1 "no 1000x600 desktop after resizing the window"; fi
  stop_all
}

for s in "${SCENARIOS[@]}"; do
  echo "== $CLIENT: $s"
  "scenario_$s" || failed=1
done
[[ -n "${HOST_UID:-}" ]] && chown -R "$HOST_UID:${HOST_GID:-$HOST_UID}" "$OUT" 2>/dev/null
exit $failed
