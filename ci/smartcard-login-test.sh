#!/usr/bin/env bash
# Smart card redirection in the login mode (PAM + per-user agent) of the Debian 13 test VM, with a real client and
# the real reader of this machine: xfreerdp (container) redirects the reader, the agent of the VM serves it to the
# session of the user as a PC/SC reader, and what an application of the session sees is checked over SSH, against what
# this machine's own reader says directly. The VM must be up and deployed in login mode, with OpenSC:
#   RDP_TEST_PASSWORD=... cargo xtask vm-test --session login --keep
#   bash vm/vm.sh ssh 'sudo apt-get install -y opensc'
# The host's pcscd is only used as a client (the container sees its socket): nothing is changed, no PIN is sent.
# usage: RDP_TEST_PASSWORD=... ci/smartcard-login-test.sh
# env: CONTAINER_ENGINE VM_RDP_PORT (3390) VM_USER (dev) SMARTCARD_APDU (default 00:A4:00:00:02:3F:00)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
command -v opensc-tool >/dev/null || { echo "SKIP smartcard-login-test: opensc-tool is not installed on this machine"; exit 0; }
ENGINE="${CONTAINER_ENGINE:-}"
for e in podman docker; do [[ -n "$ENGINE" ]] || ! command -v "$e" >/dev/null || ENGINE="$e"; done
[[ -n "$ENGINE" ]] || { echo "neither podman nor docker found (or set CONTAINER_ENGINE)" >&2; exit 2; }
[[ -n "${RDP_TEST_PASSWORD:-}" ]] || { echo "RDP_TEST_PASSWORD required (password set on the VM account)" >&2; exit 2; }
[[ -S /run/pcscd/pcscd.comm ]] || { echo "SKIP smartcard-login-test: no pcscd socket on this machine"; exit 0; }
APDU="${SMARTCARD_APDU:-00:A4:00:00:02:3F:00}"
PORT="${VM_RDP_PORT:-3390}"; VUSER="${VM_USER:-dev}"
ART="$(realpath -m artifacts/smartcard-login-test)"
IMAGE=oxydesk-clients:dev
VM="bash vm/vm.sh"
(exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null || { echo "no server on 127.0.0.1:$PORT: start the VM first (see the header)" >&2; exit 2; }
rm -rf "$ART" 2>/dev/null || { [[ "$ENGINE" == podman ]] && podman unshare rm -rf "$ART"; } || { echo "cannot clean $ART" >&2; exit 2; }
mkdir -p "$ART/out"
failed=0
check() { if [[ "$2" -eq 0 ]]; then echo "PASS  login/$1  $3"; else echo "FAIL  login/$1  $3"; failed=1; fi; }
$VM ssh 'command -v opensc-tool' >/dev/null 2>&1 || { echo "opensc-tool missing in the VM: bash vm/vm.sh ssh 'sudo apt-get install -y opensc'" >&2; exit 2; }
RUNDIR="$($VM ssh 'echo /run/user/$(id -u)')"
SOCK="$RUNDIR/oxydesk-pcsc/pcsc.sock"

# What this machine's own reader says (the oracle), through the real pcscd.
unset PCSCLITE_CSOCK_NAME
timeout 20 opensc-tool -l >"$ART/direct.list" 2>&1
timeout 20 opensc-tool -a >"$ART/direct.atr" 2>&1
timeout 20 opensc-tool --send-apdu "$APDU" >"$ART/direct.apdu" 2>&1
READER="$(awk '/^[0-9]+[[:space:]]+(Yes|No)/{ sub(/^[0-9]+[[:space:]]+(Yes|No)[[:space:]]+/,""); print; exit }' "$ART/direct.list" | sed -E 's/[[:space:]]+[0-9]+ [0-9]+$//')"
ATR="$(grep -E '^([0-9a-f]{2}:)+[0-9a-f]{2}$' "$ART/direct.atr" | head -1)"
[[ -n "$READER" && -n "$ATR" ]] || { echo "SKIP smartcard-login-test: no reader with a card on this machine"; exit 0; }
echo "reader: $READER   ATR: $ATR"

"$ENGINE" build -q -t "$IMAGE" docker/clients >/dev/null || { echo "image build failed" >&2; exit 2; }
# Nothing of a previous connection must be left.
$VM ssh "PCSCLITE_CSOCK_NAME=$SOCK timeout 10 opensc-tool -l 2>&1 | grep -qF '$READER'" >/dev/null 2>&1 && echo "note: a reader was already served before the client connected"
"$ENGINE" run --rm --network host --security-opt label=disable -v /run/pcscd:/run/pcscd \
  -e "RDP_PORT=$PORT" -e "RDP_USER=$VUSER" -e "RDP_PASS=$RDP_TEST_PASSWORD" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
  -v "$ART/out:/out:Z" "$IMAGE" xfreerdp smartcard >"$ART/client.txt" 2>&1 & cpid=$!

seen=1
for _ in $(seq 180); do
  $VM ssh "PCSCLITE_CSOCK_NAME=$SOCK timeout 10 opensc-tool -l" >"$ART/redirected.list" 2>&1 && grep -qF "$READER" "$ART/redirected.list" && { seen=0; break; }
  sleep 1
done
check smartcard_reader "$seen" "the redirected reader is listed in the session of $VUSER ($(grep -F "$READER" "$ART/redirected.list" | head -1 | tr -s ' \t' ' '))"
if [[ $seen -eq 0 ]]; then
  $VM ssh "PCSCLITE_CSOCK_NAME=$SOCK timeout 20 opensc-tool -a" >"$ART/redirected.atr" 2>&1
  check smartcard_atr "$(grep -qx "$ATR" "$ART/redirected.atr"; echo $?)" "ATR $ATR through the agent relay"
  $VM ssh "PCSCLITE_CSOCK_NAME=$SOCK timeout 20 opensc-tool --send-apdu $APDU" >"$ART/redirected.apdu" 2>&1
  check smartcard_apdu "$(cmp -s "$ART/direct.apdu" "$ART/redirected.apdu"; echo $?)" "APDU $APDU answered like the local reader: $(tr -s '\n' ' ' <"$ART/redirected.apdu" | cut -c1-100)"
  # The programs of the session were started with the variable (KWin itself is not readable: it has capabilities).
  $VM ssh "for p in \$(pgrep -u \$(id -u) -x plasmashell); do tr '\\0' '\\n' </proc/\$p/environ | grep -x PCSCLITE_CSOCK_NAME=$SOCK && exit 0; done; exit 1" >"$ART/session-env.txt" 2>&1
  check smartcard_session_env "$?" "plasmashell has $(head -1 "$ART/session-env.txt")"
fi
touch "$ART/out/smartcard.done"
wait "$cpid" 2>/dev/null
cat "$ART/client.txt"
grep -q FAIL "$ART/client.txt" && failed=1
# The client left: the readers go away with the connection.
gone=1
for _ in $(seq 30); do
  if ! $VM ssh "PCSCLITE_CSOCK_NAME=$SOCK timeout 10 opensc-tool -l 2>&1 | grep -qF '$READER'" >/dev/null 2>&1; then gone=0; break; fi
  sleep 1
done
check smartcard_cleanup "$gone" "no reader left after the client disconnected"
[[ $failed -eq 0 ]] && echo "== smartcard login: passed" || echo "== smartcard login: FAILED (logs in $ART)"
exit $failed
