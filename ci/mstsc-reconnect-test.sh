#!/usr/bin/env bash
# Automatic reconnection (RDP auto-reconnect cookie) with mstsc (VM Windows 11) against the login mode of the Debian 13 test
# VM: mstsc connects through a proxy, the flow is cut (proxy reset + packets of the old flow dropped in the VM) and mstsc must
# come back by itself, without password, in the same KWin session.
#   RDP_TEST_PASSWORD=... ci/mstsc-reconnect-test.sh      (Debian VM up in login mode, Windows VM prepared as for vm/win11.sh)
# env: VM_USER (dev) PROXY_PORT (3391)
set -uo pipefail
cd "$(dirname "$0")/.."
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required" >&2; exit 2; }
VUSER="${VM_USER:-dev}"; PP="${PROXY_PORT:-3391}"
VM="bash vm/vm.sh"; SERIAL=vm/state/win11/serial.log
OUT="$(mktemp -d)"
$VM ssh 'sudo iptables -F' >/dev/null 2>&1
sed "s/3391/$PP/" ci/cutproxy.py > "$OUT/proxy.py"; python3 "$OUT/proxy.py" > "$OUT/proxy.log" 2>&1 & PROXY=$!
cleanup() { kill "$PROXY" "${WATCH:-}" 2>/dev/null; $VM ssh 'sudo iptables -F' >/dev/null 2>&1; }
trap cleanup EXIT
sleep 1
: > "$SERIAL"
MARK="$($VM ssh 'date +"%Y-%m-%d %H:%M:%S"')"; KW="$($VM ssh "pgrep -u $VUSER -x kwin_wayland | head -1")"
(
  for _ in $(seq 900); do grep -q 'STEP cut-now' "$SERIAL" 2>/dev/null && break; sleep 0.5; done
  PORT="$($VM ssh "ss -tn state established '( sport = :3389 )' | awk 'NR>1{split(\$4,a,\":\"); print a[length(a)]}' | head -1")"
  $VM ssh "sudo iptables -I INPUT -p tcp --sport $PORT --dport 3389 -j DROP; sudo iptables -I OUTPUT -p tcp --sport 3389 --dport $PORT -j DROP"
  kill -USR1 "$PROXY"; echo "cut (flow port $PORT)"
) & WATCH=$!
WIN_RECONNECT=1 WIN_EXPECT_RESIZE=0 WIN_LAYOUT_CHECK=0 WIN_EXPECT_TONE=0 bash vm/win11.sh test "10.0.2.2:$PP" "$VUSER" 300 | tee "$OUT/win11.log"
rc=${PIPESTATUS[0]}
KW2="$($VM ssh "pgrep -u $VUSER -x kwin_wayland | head -1")"
$VM ssh "sudo journalctl -u rdsktp -u rdsktp-authd --since '$MARK' --no-pager -o cat | grep -iE 'reconnect|rejoined' | cut -c1-200"
$VM ssh "sudo journalctl -u rdsktp --since '$MARK' --no-pager -o cat | grep -q 'automatic reconnection accepted'" && echo "PASS cookie_used" || { echo "FAIL cookie_used"; rc=1; }
[[ -n "$KW" && "$KW" == "$KW2" ]] && echo "PASS same_kwin $KW" || { echo "FAIL same_kwin $KW -> $KW2"; rc=1; }
exit "$rc"
