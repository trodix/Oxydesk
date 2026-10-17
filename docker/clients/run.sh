#!/usr/bin/env bash
# Drives a real RDP client in a virtual X server against the synthetic desktop of `oxydesk --test-mode` and checks
# what it displays, like a user would: desktop size, live frames, keyboard (marker block colour), clipboard
# round trip and dynamic resizing.
# usage: run.sh CLIENT [SCENARIO...]    CLIENT: xfreerdp | remmina    SCENARIO: screen keyboard clipboard mouse resize drives printers
#   fallback_nohh264 / fallback_nogfx: client without H.264 (uncompressed EGFX) / without EGFX (bitmap updates): pixels
#   compared with what the synthetic desktop draws (probe.py integrity). clipboard_image / clipboard_files: round trips.
# env: RDP_HOST (127.0.0.1) RDP_PORT (3389) RDP_USER (test) RDP_PASS (x) OUT (/out) DESKTOP (1920x1080) HOST_UID HOST_GID
set -uo pipefail
CLIENT="${1:?client: xfreerdp | remmina}"; shift
SCENARIOS=("$@"); [[ ${#SCENARIOS[@]} -gt 0 ]] || SCENARIOS=(screen keyboard clipboard clipboard_image clipboard_files mouse fallback_nohh264 fallback_nogfx resize)
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
      "$bin" "/v:$HOST:$PORT" "/u:${RDP_USER:-test}" "/p:${RDP_PASS:-x}" /cert:ignore ${GFX_ARG-/gfx:AVC420} +clipboard "$@" \
        >"$OUT/$CLIENT.$scenario.client.log" 2>&1 & ;;
    remmina)
      cat >"$OUT/remmina.remmina" <<R
[remmina]
name=oxydesk
protocol=RDP
server=$HOST:$PORT
username=${RDP_USER:-test}
password=
domain=
resolution_mode=${RES_MODE:-2}
resolution_width=$DW
resolution_height=$DH
colordepth=32
gfx_h264=${GFX_H264:-1}
network=${REMMINA_NETWORK:-7}
cert_ignore=1
ignore-tls-errors=1
disableclipboard=0
${SHARE_DIR:+sharefolder=$SHARE_DIR}
${SHARE_PRINTER:+shareprinter=1}
viewmode=${VIEWMODE:-4}
scale=${SCALE:-0}
R
      dbus-run-session -- remmina -c "$OUT/remmina.remmina" >"$OUT/$CLIENT.$scenario.client.log" 2>&1 &
      CLIENT_PID=$!
      # No stored password (Remmina keeps them encrypted): answer the credentials prompt like a user.
      sleep 6; xdotool type --delay 50 "${RDP_PASS:-x}"; xdotool key Return ;;
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
    local end=$((SECONDS + 8)) got=1
    while [[ $SECONDS -lt $end ]]; do
      xdotool key "$((i + 1))"   # pressing again is harmless: the marker only follows the last key
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
  local text="héllo € oxydesk $RANDOM"
  # Our copy is announced to the server, which echoes it back: the client then owns the clipboard.
  printf '%s' "$text" | xclip -selection clipboard -i
  sleep 3
  pkill -x xclip; sleep 3
  local got; got="$(xclip -selection clipboard -o 2>/dev/null)"
  check clipboard_roundtrip "$([[ "$got" == "$text" ]]; echo $?)" "sent '$text', read back '$got'"
  stop_all
}

scenario_mouse() {
  start_x 2560 1440
  start_client mouse /size:"${DW}x${DH}"
  locate "$DW" "$DH" 30 || { check mouse 1 "desktop not shown"; stop_all; return; }
  # The synthetic desktop draws a 32x32 block centred on the pointer: orange, cyan with the left button, purple with the right.
  local px=$((ORIGIN_X + 600)) py=$((ORIGIN_Y + 400)) f
  xdotool mousemove "$px" "$py"; sleep 0.5; xdotool mousemove $((px + 3)) $((py + 2)); sleep 0.3
  expect_pixel() { # name, colour, X, Y, [wiggle]: the first motion events can be lost while the client settles
    local end=$((SECONDS + 8)) got=1 i=0
    while [[ $SECONDS -lt $end ]]; do
      i=$((i + 1)); [[ -z "${5:-}" ]] || xdotool mousemove $(($3 + i % 2)) "$4"
      f="$(shot mouse)" && python3 -W ignore "$PROBE" match "$f" "$3" "$4" $2 >/dev/null && { got=0; break; }
      sleep 0.3
    done
    check "$1" "$got" "pointer block ($2) at +$(($3 - ORIGIN_X))+$(($4 - ORIGIN_Y))"
  }
  expect_pixel pointer_move "255 128 0" "$px" "$py" w
  xdotool mousedown 1; expect_pixel button_left "0 255 255" "$px" "$py"; xdotool mouseup 1
  xdotool mousedown 3; expect_pixel button_right "128 0 255" "$px" "$py"; xdotool mouseup 3
  px=$((ORIGIN_X + 1500)); py=$((ORIGIN_Y + 900)); xdotool mousemove $((px - 5)) $((py - 5)); sleep 0.3; xdotool mousemove "$px" "$py"
  expect_pixel pointer_far "255 128 0" "$px" "$py" w
  stop_all
}

scenario_resize() {
  local w=1280 h=720 target_w=1000 target_h=600
  start_x 2560 1440
  if [[ "$CLIENT" == xfreerdp ]]; then
    start_client resize /size:${w}x${h} /dynamic-resolution
  else
    # Scale mode 2 = "dynamic resolution update": the desktop follows the Remmina window.
    RES_MODE=0 VIEWMODE=1 SCALE=2 start_client resize
  fi
  if [[ "$CLIENT" == xfreerdp ]]; then locate $w $h 30 || { check resize 1 "desktop not shown"; stop_all; return; }
  else sleep 8; fi
  local win
  if [[ "$CLIENT" == xfreerdp ]]; then
    win="$(xdotool search --onlyvisible --class -- xfreerdp 2>/dev/null | head -1)"
    [[ -n "$win" ]] || win="$(xdotool search --onlyvisible --name FreeRDP 2>/dev/null | head -1)"
  else
    win="$(xdotool search --onlyvisible --name oxydesk 2>/dev/null | head -1)"
  fi
  [[ -n "$win" ]] || { check resize 1 "client window not found"; stop_all; return; }
  xdotool windowsize "$win" $target_w $target_h
  if [[ "$CLIENT" == remmina ]]; then
    # Remmina's frame (toolbar, tabs) shrinks the viewport: the desktop must be resized to it, so its magenta
    # corner sits in the window's bottom-right corner (with a desktop that stayed 1920x1080 it would be scrolled out).
    local f box ex ey X Y WIDTH HEIGHT ok=1
    eval "$(xdotool getwindowgeometry --shell "$win")"
    local end=$((SECONDS + 15))
    while [[ $SECONDS -lt $end ]]; do
      f="$(shot locate)" && box="$(python3 -W ignore "$PROBE" corner "$f")" && {
        read -r _ _ ex ey <<<"$box"
        (( ex + 1 <= X + WIDTH + 4 && ex + 1 >= X + WIDTH - 40 && ey + 1 <= Y + HEIGHT + 4 && ey + 1 >= Y + HEIGHT - 40 )) && { ok=0; break; }
      }
      sleep 0.5
    done
    check resize "$ok" "desktop corner at ${ex:-?},${ey:-?} for a ${WIDTH}x${HEIGHT} window at +$X+$Y"
    stop_all; return
  fi
  if locate $target_w $target_h 20; then check resize 0 "desktop follows the window: 1000x600 at +$ORIGIN_X+$ORIGIN_Y"
  else check resize 1 "no 1000x600 desktop after resizing the window"; fi
  stop_all
}

# Uncompressed fallbacks: what the client shows must be what the desktop draws, pixel for pixel (the background obeys
# an invariant that holds in every frame, see probe.py integrity), not just "frames arrived".
fallback_pixels() { # name, description
  locate "$DW" "$DH" 40 || { check "$1" 1 "desktop not shown (screenshots in $OUT)"; stop_all; return; }
  check "$1_shown" 0 "${DW}x${DH} desktop at +$ORIGIN_X+$ORIGIN_Y ($2)"
  xdotool mousemove $((ORIGIN_X + 8)) $((ORIGIN_Y + 8)); sleep 1
  local f res
  f="$(shot "$1")"
  res="$(python3 -W ignore "$PROBE" integrity "$f" "$ORIGIN_X" "$ORIGIN_Y" "$DW" "$DH")"
  check "$1_pixels_exact" "$?" "$res"
  # The corner block is exactly magenta (a lossy codec would shift it).
  python3 -W ignore "$PROBE" match "$f" $((ORIGIN_X + DW - 16)) $((ORIGIN_Y + DH - 16)) 255 0 255 2 >/dev/null
  check "$1_corner_exact" "$?" "corner block (255,0,255) +-2"
  # Live frames, keyboard marker and heartbeat colours keep working on the fallback.
  local on=0 off=0 v r
  for _ in $(seq 20); do
    f="$(shot beat)" && v="$(python3 -W ignore "$PROBE" pixel "$f" $((ORIGIN_X + 40)) $((ORIGIN_Y + 8)))"
    read -r r _ _ <<<"$v"
    [[ ${r:-0} -gt 250 ]] && on=1; [[ ${r:-255} -eq 48 ]] && off=1
    [[ $on -eq 1 && $off -eq 1 ]] && break; sleep 0.2
  done
  check "$1_heartbeat_exact" "$([[ $on -eq 1 && $off -eq 1 ]]; echo $?)" "heartbeat blocks exactly white / grey 48 (on=$on off=$off)"
  xdotool mousemove $((ORIGIN_X + 400)) $((ORIGIN_Y + 300)) click 1 2>/dev/null; sleep 1
  local end=$((SECONDS + 8)) got=1
  while [[ $SECONDS -lt $end ]]; do
    xdotool key 2; f="$(shot key)" && python3 -W ignore "$PROBE" match "$f" $((ORIGIN_X + 12)) $((ORIGIN_Y + 12)) 0 255 0 2 >/dev/null && { got=0; break; }
    sleep 0.3
  done
  check "$1_marker_exact" "$got" "marker block (0,255,0) +-2 after key 2"
  stop_all
}

scenario_fallback_nohh264() {
  start_x 2560 1440
  GFX_ARG="/gfx:AVC420:off,AVC444:off" GFX_H264=0 start_client fallback_nohh264 /size:"${DW}x${DH}"
  fallback_pixels fallback_nohh264 "EGFX without H.264, uncompressed"
}

scenario_fallback_nogfx() {
  start_x 2560 1440
  GFX_ARG="" REMMINA_NETWORK=1 GFX_H264=0 start_client fallback_nogfx /network:modem /size:"${DW}x${DH}"
  fallback_pixels fallback_nogfx "no EGFX, bitmap updates"
}

png() { python3 -W ignore - "$@" <<'PY'
import sys
from PIL import Image
w, h, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
im = Image.new("RGB", (w, h))
im.putdata([((x * 255) // (w - 1), (y * 255) // (h - 1), (x * 7 + y * 13) % 256) for y in range(h) for x in range(w)])
im.save(out)
PY
}

scenario_clipboard_image() {
  start_x 2560 1440
  start_client clipboard_image /size:"${DW}x${DH}"
  locate "$DW" "$DH" 30 || { check clipboard_image 1 "desktop not shown"; stop_all; return; }
  # FreeRDP's X11 client only exposes the DIB-based image targets (image/bmp...), not image/png.
  png 301 77 "$OUT/$CLIENT.image.sent.png"   # odd width: row padding of the DIB
  python3 -W ignore -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])' "$OUT/$CLIENT.image.sent.png" "$OUT/$CLIENT.image.sent.bmp"
  xclip -selection clipboard -t image/bmp -i "$OUT/$CLIENT.image.sent.bmp"
  sleep 3
  pkill -x xclip; sleep 3
  xclip -selection clipboard -t image/bmp -o >"$OUT/$CLIENT.image.got.bmp" 2>/dev/null
  local d; d="$(python3 -W ignore "$PROBE" imgdiff "$OUT/$CLIENT.image.sent.png" "$OUT/$CLIENT.image.got.bmp" 2>&1 | tail -1)"
  check clipboard_image_roundtrip "$([[ "$d" == 0 ]]; echo $?)" "301x77 image sent and read back, max channel difference '${d}' (0 = identical)"
  stop_all
}

scenario_clipboard_files() {
  # Remmina 1.4.39 (rdp plugin) has no file clipboard: no FileGroupDescriptorW/FileContents in the plugin.
  [[ "$CLIENT" == remmina ]] && { echo "SKIP  $CLIENT/clipboard_files_roundtrip  not supported by this client (no file clipboard in the rdp plugin)"; return; }
  start_x 2560 1440
  start_client clipboard_files /size:"${DW}x${DH}"
  locate "$DW" "$DH" 30 || { check clipboard_files 1 "desktop not shown"; stop_all; return; }
  local src="$OUT/files-sent/oxydesk-files"
  rm -rf "$OUT/files-sent" "$OUT/files-got"; mkdir -p "$src/sub/deeper"
  : >"$src/empty.txt"; printf 'hello\n' >"$src/hello.txt"
  head -c 1500001 /dev/urandom >"$src/sub/big.bin"; head -c 524288 /dev/urandom >"$src/sub/deeper/block.bin"
  printf 'caf\xc3\xa9' >"$src/sub/café.txt"
  (cd "$OUT/files-sent" && sha256sum $(find oxydesk-files -type f | sort)) >"$OUT/files.sent.sha"
  local uris="file://$PWD/$src"
  uris="file://$(realpath "$src")"
  # One owner only (a second xclip would take the selection from the first); FreeRDP reads the list from
  # text/uri-list (empty answer otherwise). -loops: xclip otherwise exits after a few requests (TARGETS counts).
  printf '%s\r\n' "$uris" | xclip -selection clipboard -loops 100 -t text/uri-list -i &
  sleep 3
  local targets; targets="$(xclip -selection clipboard -o -t TARGETS 2>/dev/null | tr '\n' ' ')"
  echo "targets after our copy: $targets"
  pkill -x xclip; sleep 4
  targets="$(xclip -selection clipboard -o -t TARGETS 2>/dev/null | tr '\n' ' ')"
  echo "targets after the echo: $targets"
  local list; list="$(xclip -selection clipboard -o -t text/uri-list 2>/dev/null | tr -d '\r')"
  echo "$list" >"$OUT/files.uris"
  local root; root="$(head -1 <<<"$list" | sed 's|^file://||')"
  if [[ -z "$root" || ! -e "$root" ]]; then
    check clipboard_files_roundtrip 1 "no file list echoed back (targets: $targets)"; stop_all; return
  fi
  mkdir -p "$OUT/files-got"
  timeout 120 cp -r "$root" "$OUT/files-got/" 2>"$OUT/files.cp.log"
  local cprc=$?
  (cd "$OUT/files-got" && sha256sum $(find "$(basename "$root")" -type f | sort | sed 's|^|./|') 2>/dev/null | sed 's| \./| |') >"$OUT/files.got.sha"
  sed 's|oxydesk-files|X|' "$OUT/files.sent.sha" >"$OUT/files.sent.norm"; sed "s|$(basename "$root")|X|" "$OUT/files.got.sha" >"$OUT/files.got.norm"
  check clipboard_files_roundtrip "$([[ $cprc -eq 0 ]] && diff -q "$OUT/files.sent.norm" "$OUT/files.got.norm" >/dev/null; echo $?)" "$(wc -l <"$OUT/files.sent.sha") files incl. folders, empty file, 1.5 MB file, accent; copy exit $cprc"
  stop_all
}

# The server mounts the drive on the host, out of this container's sight: the host checks the files while the client
# stays connected, then creates $OUT/drives.done (see ci/clients-test.sh).
scenario_drives() {
  mkdir -p "$OUT/share"; rm -f "$OUT/drives.done"
  start_x 2560 1440
  if [[ "$CLIENT" == xfreerdp ]]; then
    start_client drives /size:"${DW}x${DH}" "/drive:Partage,$OUT/share"
  else
    SHARE_DIR="$OUT/share" start_client drives
  fi
  locate "$DW" "$DH" 30 || { check drives_desktop 1 "desktop not shown"; stop_all; return; }
  echo "drives: waiting for the host to check $OUT/share"
  local end=$((SECONDS + 150))
  while [[ $SECONDS -lt $end && ! -e "$OUT/drives.done" ]]; do sleep 0.5; done
  check drives_checked "$([[ -e "$OUT/drives.done" ]]; echo $?)" "host-side check finished"
  stop_all
}

# A CUPS queue "Recorder" that writes each job it receives to $OUT/printed: the client redirects it, the host sends a
# job to the socket of the desktop and checks what came out here (ci/clients-test.sh creates $OUT/printers.done).
scenario_printers() {
  mkdir -p "$OUT/printed" /usr/lib/cups/backend; rm -f "$OUT/printers.done"
  cat >/usr/lib/cups/backend/record <<'B'
#!/bin/sh
if [ $# -eq 0 ]; then echo 'direct record "Unknown" "Recorder"'; exit 0; fi
if [ -n "$6" ]; then cat "$6"; else cat; fi >"/out/printed/job-$1-$$.bin"
exit 0
B
  chmod 700 /usr/lib/cups/backend/record
  cupsd; for _ in $(seq 50); do lpstat -r >/dev/null 2>&1 && break; sleep 0.2; done
  lpadmin -p Recorder -E -v record:/ -m raw >"$OUT/lpadmin.log" 2>&1 || { check printers_queue 1 "lpadmin: $(cat "$OUT/lpadmin.log")"; return; }
  start_x 2560 1440
  if [[ "$CLIENT" == xfreerdp ]]; then
    start_client printers /size:"${DW}x${DH}" "/printer:Recorder,MS Publisher Imagesetter"
  else
    SHARE_PRINTER=1 start_client printers
  fi
  locate "$DW" "$DH" 30 || { check printers_desktop 1 "desktop not shown"; stop_all; return; }
  echo "printers: waiting for the host to check $OUT/printed"
  local end=$((SECONDS + 150))
  while [[ $SECONDS -lt $end && ! -e "$OUT/printers.done" ]]; do sleep 0.5; done
  check printers_checked "$([[ -e "$OUT/printers.done" ]]; echo $?)" "host-side check finished"
  stop_all
  pkill -x cupsd
}

for s in "${SCENARIOS[@]}"; do
  echo "== $CLIENT: $s"
  "scenario_$s" || failed=1
done
# Rootless Podman maps the container's root to the host user already: chown would hand the files to a sub-uid.
[[ -n "${HOST_UID:-}" && "$(awk 'NR==1{print $2}' /proc/self/uid_map)" == 0 ]] && chown -R "$HOST_UID:${HOST_GID:-$HOST_UID}" "$OUT" 2>/dev/null
exit $failed
