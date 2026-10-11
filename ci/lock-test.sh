#!/usr/bin/env bash
# Session lock in login mode, on the Debian 13 test VM, with a real client (xfreerdp in a Docker/Podman container):
# the lock screen (kscreenlocker) shows up in the remote image, survives a disconnection and a reconnection
# (same KWin, nothing lost), refuses a wrong password and unlocks with the right one typed through RDP.
# The VM must be up in login mode:  RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep
# usage: RDP_TEST_PASSWORD=... ci/lock-test.sh
# env: CONTAINER_ENGINE VM_RDP_PORT (3390) VM_USER (dev)
set -uo pipefail
cd "$(dirname "$0")/.."
ENGINE="${CONTAINER_ENGINE:-}"
for e in podman docker; do [[ -n "$ENGINE" ]] || ! command -v "$e" >/dev/null || ENGINE="$e"; done
[[ -n "$ENGINE" ]] || { echo "neither podman nor docker found (or set CONTAINER_ENGINE)" >&2; exit 2; }
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 1; }
PORT="${VM_RDP_PORT:-3390}"; USER_NAME="${VM_USER:-dev}"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the VM first (see the header)" >&2; exit 1; }
ART=artifacts/lock-test
IMAGE=rdsktp-clients:dev
rm -rf "$ART" 2>/dev/null || { [[ "$ENGINE" == podman ]] && podman unshare rm -rf "$ART"; }
mkdir -p "$ART"
"$ENGINE" build -q -t "$IMAGE" docker/clients >/dev/null || { echo "image build failed" >&2; exit 2; }
NAME=rdsktp-lock-test
failed=0
check() { if [[ "$2" -eq 0 ]]; then echo "PASS  $1  $3"; else echo "FAIL  $1  $3"; failed=1; fi; }

# As the VM user, on its session bus.
vmuser() { bash vm/vm.sh ssh "sudo -u $USER_NAME env DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus $*" 2>/dev/null; }
greeter() { bash vm/vm.sh ssh "pgrep -u $USER_NAME -x kscreenlocker_g >/dev/null && echo yes || echo no" 2>/dev/null | tail -1; }
kwin_pid() { bash vm/vm.sh ssh "pgrep -u $USER_NAME -x kwin_wayland" 2>/dev/null | tail -1; }

connect() {
  "$ENGINE" rm -f "$NAME" >/dev/null 2>&1
  "$ENGINE" run -d --name "$NAME" --network host -v "$PWD/$ART:/o:Z" --entrypoint bash "$IMAGE" -c "
    export DISPLAY=:99; Xvfb :99 -screen 0 1280x720x24 -nolisten tcp & sleep 1; openbox & sleep 1
    xfreerdp3 /v:127.0.0.1:$PORT /u:$USER_NAME /p:'$RDP_TEST_PASSWORD' /cert:ignore /gfx:AVC420 /size:1280x720 >/o/client.log 2>&1 &
    sleep 3000" >/dev/null
  sleep 30
}
shot() { "$ENGINE" exec -e DISPLAY=:99 "$NAME" import -window root "/o/$1.png"; }
# Mean absolute difference of two screenshots (coarse grid): large when the lock screen replaces the desktop.
imgdiff() { "$ENGINE" exec "$NAME" python3 -I -c "
from PIL import Image, ImageChops
a=Image.open('/o/$1.png').convert('RGB').resize((64,36)); b=Image.open('/o/$2.png').convert('RGB').resize((64,36))
d=ImageChops.difference(a,b).convert('L'); print(sum(d.getdata())//len(d.getdata()))"; }
type_password() { "$ENGINE" exec -e DISPLAY=:99 "$NAME" bash -c "xdotool mousemove 600 400; sleep 1; xdotool click 1; sleep 1; xdotool type --delay 60 '$1'; sleep 0.5; xdotool key Return; sleep 5"; }

connect
pid=$(kwin_pid)
check session_up "$([[ -n "$pid" && "$(greeter)" == no ]] && echo 0 || echo 1)" "KWin $pid, not locked"
shot unlocked
vmuser dbus-send --session --type=method_call --dest=org.freedesktop.ScreenSaver /ScreenSaver org.freedesktop.ScreenSaver.Lock >/dev/null
sleep 6; shot locked
d=$(imgdiff unlocked locked)
check lock_screen_shown "$([[ "$(greeter)" == yes && "${d:-0}" -ge 15 ]] && echo 0 || echo 1)" "greeter running, image changed by $d"

# Disconnection, then reconnection: the lock outlives the connection, the session is the same.
"$ENGINE" rm -f "$NAME" >/dev/null 2>&1; sleep 3
connect
shot reconnected
d=$(imgdiff unlocked reconnected)
check lock_survives_reconnect "$([[ "$(greeter)" == yes && "$(kwin_pid)" == "$pid" && "${d:-0}" -ge 15 ]] && echo 0 || echo 1)" "same KWin ($pid), still locked, image changed by $d"

type_password "wrong-$RANDOM"
check wrong_password_stays_locked "$([[ "$(greeter)" == yes ]] && echo 0 || echo 1)" "greeter still running"
type_password "$RDP_TEST_PASSWORD"
check unlock "$([[ "$(greeter)" == no && "$(kwin_pid)" == "$pid" ]] && echo 0 || echo 1)" "greeter gone, same KWin"
shot unlocked_again
"$ENGINE" rm -f "$NAME" >/dev/null 2>&1

echo
[[ $failed -eq 0 ]] && echo "lock: all checks passed" || echo "lock: failures (screenshots in $ART)"
exit $failed
