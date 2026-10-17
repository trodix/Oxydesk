#!/usr/bin/env bash
# mstsc (VM Windows 11) with two monitors against the login mode of the Debian 13 test VM, the primary monitor at the
# right of the other one (negative coordinates, like a laptop with an external screen on its left): each monitor must
# show its own output, a click must land on the output and at the place of the monitor that got it, and the compositor's
# primary output (where the shell puts its panel) must be the client's primary monitor. Needs:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep      (Debian VM up, in login mode, with the test app)
#   a Windows VM image prepared as for `cargo xtask test --client` (vm/win11.sh)
# usage: RDP_TEST_PASSWORD=... ci/mstsc-multimon-test.sh [out-dir]
# env: VM_RDP_PORT (3390) VM_USER (dev) WIN_MONITORS_LEFT (1; 0: the other monitor at the right)
set -uo pipefail
cd "$(dirname "$0")/.."
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 2; }
PORT="${VM_RDP_PORT:-3390}"; VUSER="${VM_USER:-dev}"; LEFT="${WIN_MONITORS_LEFT:-1}"
ART="$(realpath -m "${1:-artifacts/mstsc-multimon-test}")"
VM="bash vm/vm.sh"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the Debian VM first (see the header)" >&2; exit 2; }
rm -rf "$ART"; mkdir -p "$ART"
# A session that exists keeps its outputs: the agent must start with the monitors of this client.
$VM ssh "sudo systemctl stop oxydesk-agent@$VUSER; sudo systemctl restart oxydesk" >/dev/null 2>&1
WIN_MONITORS=2 WIN_MONITORS_LEFT="$LEFT" WIN_EXPECT_RESIZE=0 WIN_LAYOUT_CHECK=0 WIN_EXPECT_TONE=0 \
  bash vm/win11.sh test "10.0.2.2:$PORT" "$VUSER" 2>&1 | tee "$ART/win11.log"
rc=${PIPESTATUS[0]}
# The panel of the shell goes to the primary output of the compositor, which is the first output of the layout: the
# client's primary monitor, at (0,0) in the client's own space (1920 pixels from the left of the desktop when the other
# monitor is at its left).
listing="$($VM ssh "sudo -u $VUSER env XDG_RUNTIME_DIR=/run/user/\$(id -u $VUSER) WAYLAND_DISPLAY=oxydesk-0 kscreen-doctor -o" 2>&1 | sed 's/\x1b\[[0-9;]*m//g')"
echo "$listing" > "$ART/kscreen.txt"
primary_x="$(echo "$listing" | awk '/^Output:/{n=$3} /priority 1$/{p=n} /Geometry:/{split($2,a,","); g[n]=a[1]} END{print g[p]}')"
want=0; [[ "$LEFT" == 1 ]] && want=1920
if [[ "$primary_x" == "$want" ]]; then echo "PASS kwin_primary_is_the_client_primary primary output at x=$primary_x"
else echo "FAIL kwin_primary_is_the_client_primary primary output at x=$primary_x, expected $want (see $ART/kscreen.txt)"; rc=1; fi
# The panel of the shell itself (a "dock" window) must be on the primary output, not on the other monitor.
panel="$($VM ssh "cat > /tmp/oxydesk-panel.js <<'JS'
for (const w of workspace.windowList()) if (w.dock) print('OXYDESK_PANEL ' + w.output.name);
JS
sudo -u $VUSER env XDG_RUNTIME_DIR=/run/user/\$(id -u $VUSER) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/\$(id -u $VUSER)/bus bash -c 'id=\$(qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript /tmp/oxydesk-panel.js oxydesk-panel); qdbus6 org.kde.KWin /Scripting/Script\$id org.kde.kwin.Script.run; sleep 1'
sudo journalctl --since -20sec --no-pager | grep -o 'OXYDESK_PANEL .*' | sort -u" 2>&1)"
echo "$panel" > "$ART/panel.txt"
primary_name="$(echo "$listing" | awk '/^Output:/{n=$3} /priority 1$/{print n}')"
if [[ "$panel" == "OXYDESK_PANEL $primary_name" ]]; then echo "PASS panel_on_the_primary_output panel on $primary_name"
else echo "FAIL panel_on_the_primary_output panels: [$panel], primary: $primary_name"; rc=1; fi
exit "$rc"
