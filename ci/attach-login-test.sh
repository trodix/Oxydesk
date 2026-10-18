#!/usr/bin/env bash
# session-policy (attach / auto / new) against the login mode of the Debian 13 test VM. The VM has no physical Plasma
# session, so one is imitated: a `kwin_wayland --virtual` on the socket `physical-0` (what a real session would offer to
# the agent) next to a marker process named `kwin_wayland --socket physical-0` without `--virtual`, which is what
# oxydesk-authd looks for. Needs the VM up and in login mode:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep
# usage: RDP_TEST_PASSWORD=... ci/attach-login-test.sh [out-dir]
# Checks: attach without a session is refused; attach and auto join the session (agent started with `--session attach`,
# the compositor stays alive after the client left); auto without a session starts one; new ignores the session.
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 2; }
PORT="${VM_RDP_PORT:-3390}"
ART="$(realpath -m "${1:-artifacts/attach-login-test}")"
VM="bash vm/vm.sh"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the Debian VM first (see the header)" >&2; exit 2; }
rm -rf "$ART"; mkdir -p "$ART"
cargo build -q --release -p rdp-test-client --features ironrdp || exit 2
CLIENT="target/release/rdp-test-client"

fail=0
ok() { echo "ok      $*"; }
ko() { echo "FAILED  $*"; fail=1; }
vm() { $VM ssh "$@"; }

# The password of the account is the one given here, whatever an earlier run chose.
printf 'dev:%s\n' "$RDP_TEST_PASSWORD" | $VM ssh 'sudo chpasswd'

# Sets session-policy of the server (the drop-in of the login mode gets it on the command line) and restarts it.
policy() {
  vm "sudo sed -i -E 's# --session-policy [a-z]+##; s#(ExecStart=/usr/bin/oxydesk --login)#\1 --session-policy $1#' /etc/systemd/system/oxydesk.service.d/login.conf \
      && sudo systemctl daemon-reload && sudo systemctl restart oxydesk && sleep 2" >/dev/null
}

# The user's own session is gone: the agent, our compositors and the imitated physical session.
clean() {
  vm "sudo systemctl stop oxydesk-agent@dev; sudo pkill -u dev -f '[k]win_wayland'; sudo pkill -u dev -f '[p]hysical-0'; sudo pkill -u dev '[p]lasmashell'; sudo pkill -u dev '[o]xydesk-testapp'; sleep 2; rm -f /run/user/1000/physical-0*" >/dev/null 2>&1
}

physical() {
  vm 'export XDG_RUNTIME_DIR=/run/user/$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus XDG_CURRENT_DESKTOP=KDE XDG_SESSION_TYPE=wayland XDG_MENU_PREFIX=plasma-
      kbuildsycoca6 >/dev/null 2>&1
      setsid nohup kwin_wayland --virtual --socket physical-0 --width 1920 --height 1080 --output-count 1 >/tmp/physical-kwin.log 2>&1 </dev/null &
      for _ in $(seq 50); do test -S $XDG_RUNTIME_DIR/physical-0 && break; sleep 0.2; done
      setsid nohup bash -c '"'"'exec -a kwin_wayland python3 -c "import time; time.sleep(100000)" --socket physical-0'"'"' >/dev/null 2>&1 </dev/null &
      WAYLAND_DISPLAY=physical-0 OXYDESK_ECHO_FILES=copy setsid nohup /usr/lib/oxydesk/oxydesk-testapp 1920 1080 --tone --clipboard-echo >/tmp/physical-app.log 2>&1 </dev/null &
      sleep 3; test -S $XDG_RUNTIME_DIR/physical-0'
}

# Connects once, saves what the client sees; prints the exit code of the client.
connect() { # connect <name> [extra client args]
  local name="$1"; shift
  RDP_TEST_PASSWORD="$RDP_TEST_PASSWORD" timeout 120 "$CLIENT" --host 127.0.0.1 --port "$PORT" --no-nla --user dev \
    --out "$ART/$name" --screenshot "$ART/$name.png" --screenshot-wait 8 "$@" >"$ART/$name.log" 2>&1
  echo $?
}

agent_args() { vm 'systemctl show oxydesk-agent@dev -p ExecStart --value; cat /run/oxydesk/agent-dev.env 2>/dev/null' 2>/dev/null; }
alive() { vm "pgrep -u dev -f \"$1\" >/dev/null && echo yes || echo no" 2>/dev/null; }

kwin_minor=$(vm 'kwin_wayland --version' 2>/dev/null | sed -E 's/.* [0-9]+\.([0-9]+)\..*/\1/')
[[ "$kwin_minor" =~ ^[0-9]+$ ]] || kwin_minor=0
echo "KWin 6.$kwin_minor"

# 1. attach, no session: refused.
clean; policy attach
rc=$(connect attach-none)
if [[ "$rc" != 0 ]] && [[ ! -s "$ART/attach-none.png" ]]; then ok "attach without a session is refused (rc $rc)"; else ko "attach without a session was accepted (rc $rc)"; fi
vm 'sudo journalctl -u oxydesk-authd --no-pager -n 20' 2>/dev/null | grep -q "no Plasma session to join" \
  && ok "authd logged the refusal" || ko "no refusal in the authd journal"

# 2. attach, with a session: joined.
clean; physical || ko "the imitated physical session did not start"
rc=$(connect attach-joined --width 1280 --height 720)
[[ "$rc" == 0 && -s "$ART/attach-joined.png" ]] && ok "attach joins the session (rc 0, screenshot taken)" || ko "attach with a session failed (rc $rc)"
agent_args > "$ART/attach-joined.agent.txt"
grep -q -- "--session attach" "$ART/attach-joined.agent.txt" && grep -q "physical-0" "$ART/attach-joined.agent.txt" \
  && ok "the agent was started with --session attach on physical-0" || ko "the agent was not started in attach mode ($(cat "$ART/attach-joined.agent.txt"))"
[[ "$(alive '[s]ocket oxydesk-0')" == no ]] && ok "no compositor of ours was started" || ko "a private compositor was started"
sleep 3
[[ "$(alive '[s]ocket physical-0 --width')" == yes ]] && ok "the session is still alive after the client left" || ko "the session was killed"

# 2b. The whole client scenario in the joined session (input, clipboard, audio, reconnection, dynamic monitors).
vm 'sudo systemctl stop oxydesk-agent@dev' >/dev/null 2>&1
for variant in plain multi; do
  extra=(); [[ "$variant" == multi ]] && extra=(--multi-resize --max-monitors 2 --no-identity-blocks)
  mkdir -p "$ART/suite-$variant"
  RDP_TEST_PASSWORD="$RDP_TEST_PASSWORD" timeout 300 "$CLIENT" --host 127.0.0.1 --port "$PORT" --no-nla --user dev \
    --out "$ART/suite-$variant" "${extra[@]}" --keyboard-layout 0x40C --no-layout-check >"$ART/suite-$variant.log" 2>&1
  bad=$(python3 -I -c 'import json,sys; c=json.load(open(sys.argv[1]))["checks"]; print(",".join(x["name"] for x in c if not x["passed"]))' "$ART/suite-$variant/client.json" 2>&1)
  # Before KWin 6.7 an extra monitor cannot be created: the layouts with more monitors than the desktop are refused
  # (documented limit), the other ones must work.
  if [[ "$kwin_minor" -lt 7 ]]; then
    bad=$(tr ',' '\n' <<<"$bad" | grep -v -E '^(multi_resize_two|multi_resize_add_third|multi_resize_add_again)$' | paste -sd, -)
  fi
  [[ -z "$bad" ]] && ok "client scenario ($variant) in the joined session" || ko "client scenario ($variant): $bad"
done

# 3. auto, with a session: joined too (the agent of step 2 is stopped so that authd decides again).
vm 'sudo systemctl stop oxydesk-agent@dev' >/dev/null 2>&1; policy auto
rc=$(connect auto-joined)
[[ "$rc" == 0 ]] && ok "auto with a session: connected" || ko "auto with a session failed (rc $rc)"
agent_args | grep -q -- "--session attach" && ok "auto joined the session" || ko "auto did not join the session"

# 4. auto, no session: a session of ours.
clean; policy auto
rc=$(connect auto-new)
[[ "$rc" == 0 ]] && ok "auto without a session: connected" || ko "auto without a session failed (rc $rc)"
[[ "$(alive '[s]ocket oxydesk-0')" == yes ]] && ok "auto without a session started its own compositor" || ko "no own compositor"

# 5. new, with a session: ignored.
clean; physical; policy new
rc=$(connect new-ignores)
[[ "$rc" == 0 ]] && ok "new with a session: connected" || ko "new with a session failed (rc $rc)"
[[ "$(alive '[s]ocket oxydesk-0')" == yes ]] && ok "new started its own compositor" || ko "new did not start its own compositor"

# Back to the login mode as configured.
clean; policy new
echo "artifacts: $ART"
exit $fail
