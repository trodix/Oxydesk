#!/usr/bin/env bash
# Smart card redirection with a real client and a real reader: xfreerdp (container) redirects the reader of this
# machine to `oxydesk --test-mode`; what the applications of the session would see is checked here through the PC/SC
# socket of the desktop (PCSCLITE_CSOCK_NAME), against what this machine's own reader says directly.
#  - the redirected reader is listed with the real reader's name;
#  - the ATR of the card is the same;
#  - an APDU (SELECT of the master file) gets the same answer, byte for byte, through the redirection.
# The host's pcscd is only used as a client (the container sees its socket): nothing is changed, no PIN is sent.
# usage: ci/smartcard-test.sh [out-dir]
# env: CONTAINER_ENGINE (docker|podman) OXYDESK (binary) SMARTCARD_TEST_PORT (34200) SMARTCARD_APDU (default 00:A4:00:00:02:3F:00)
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
command -v opensc-tool >/dev/null || { echo "SKIP smartcard-test: opensc-tool is not installed (OpenSC)"; exit 0; }
ENGINE="${CONTAINER_ENGINE:-}"
for e in podman docker; do [[ -n "$ENGINE" ]] || ! command -v "$e" >/dev/null || ENGINE="$e"; done
[[ -n "$ENGINE" ]] || { echo "neither podman nor docker found (or set CONTAINER_ENGINE)" >&2; exit 2; }
[[ -S /run/pcscd/pcscd.comm ]] || { echo "SKIP smartcard-test: no pcscd socket on this machine"; exit 0; }
APDU="${SMARTCARD_APDU:-00:A4:00:00:02:3F:00}"
PORT="${SMARTCARD_TEST_PORT:-34200}"
ART="$(realpath -m "${1:-artifacts/smartcard-test}")"
rm -rf "$ART" 2>/dev/null || { [[ "$ENGINE" == podman ]] && podman unshare rm -rf "$ART"; } || { echo "cannot clean $ART" >&2; exit 2; }
mkdir -p "$ART/out"
failed=0
check() { if [[ "$2" -eq 0 ]]; then echo "PASS  $1  $3"; else echo "FAIL  $1  $3"; failed=1; fi; }

# What this machine's own reader says (the oracle), through the real pcscd.
unset PCSCLITE_CSOCK_NAME
timeout 20 opensc-tool -l >"$ART/direct.list" 2>&1
timeout 20 opensc-tool -a >"$ART/direct.atr" 2>&1
timeout 20 opensc-tool --send-apdu "$APDU" >"$ART/direct.apdu" 2>&1
READER="$(awk -F'\t' '/^[0-9]+[[:space:]]+(Yes|No)/{ sub(/^[0-9]+[[:space:]]+(Yes|No)[[:space:]]+/,""); print; exit }' "$ART/direct.list" | sed -E 's/[[:space:]]+[0-9]+ [0-9]+$//')"
ATR="$(grep -E '^([0-9a-f]{2}:)+[0-9a-f]{2}$' "$ART/direct.atr" | head -1)"
[[ -n "$READER" && -n "$ATR" ]] || { echo "SKIP smartcard-test: no reader with a card on this machine (see $ART/direct.*)"; exit 0; }
echo "reader: $READER   ATR: $ATR"

OXYDESK="${OXYDESK:-}"
if [[ -z "$OXYDESK" ]]; then cargo build -q -p oxydesk || exit 2; OXYDESK="${CARGO_TARGET_DIR:-target}/debug/oxydesk"; fi
# A relative socket name keeps the path under the 107 bytes a Unix socket allows.
mkdir -p "$ART/pcsc"
SOCK="$ART/pcsc/pcsc.sock"
[[ ${#SOCK} -lt 100 ]] || SOCK="$(mktemp -d /tmp/oxysc.XXXXXX)/pcsc.sock"
RUST_LOG=info "$OXYDESK" --test-mode --listen "127.0.0.1:$PORT" --state-dir "$ART/state" --smartcard "$SOCK" >"$ART/server.log" 2>&1 & srv=$!
trap 'kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null' EXIT
sleep 1

IMAGE=oxydesk-clients:dev
"$ENGINE" build -q -t "$IMAGE" docker/clients >/dev/null || { echo "image build failed" >&2; exit 2; }
"$ENGINE" run --rm --network host --security-opt label=disable -v /run/pcscd:/run/pcscd -e "RDP_PORT=$PORT" -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
  -v "$ART/out:/out:Z" "$IMAGE" xfreerdp smartcard >"$ART/client.txt" 2>&1 & cpid=$!

export PCSCLITE_CSOCK_NAME="$SOCK"
# The reader appears once the client announced its device.
seen=1
for _ in $(seq 120); do
  timeout 10 opensc-tool -l >"$ART/redirected.list" 2>&1 && grep -qF "$READER" "$ART/redirected.list" && { seen=0; break; }
  sleep 0.5
done
check smartcard_reader "$seen" "the redirected reader is listed ($(grep -F "$READER" "$ART/redirected.list" | head -1 | tr -s ' \t' ' '))"
if [[ $seen -eq 0 ]]; then
  timeout 20 opensc-tool -a >"$ART/redirected.atr" 2>&1
  check smartcard_atr "$(grep -qx "$ATR" "$ART/redirected.atr"; echo $?)" "ATR $ATR through the redirection"
  timeout 20 opensc-tool --send-apdu "$APDU" >"$ART/redirected.apdu" 2>&1
  check smartcard_apdu "$(cmp -s "$ART/direct.apdu" "$ART/redirected.apdu"; echo $?)" "APDU $APDU answered like the local reader: $(tr -s '\n' ' ' <"$ART/redirected.apdu" | cut -c1-100)"
fi
touch "$ART/out/smartcard.done"
wait "$cpid" 2>/dev/null
cat "$ART/client.txt"
grep -q FAIL "$ART/client.txt" && failed=1
# The client leaves: the readers are gone.
unset PCSCLITE_CSOCK_NAME; export PCSCLITE_CSOCK_NAME="$SOCK"
sleep 1
if timeout 10 opensc-tool -l 2>&1 | grep -qF "$READER"; then check smartcard_gone 1 "the reader is still listed after the client left"; else check smartcard_gone 0 "no reader after the client left"; fi
[[ $failed -eq 0 ]] && echo "== smartcard: passed" || echo "== smartcard: FAILED (logs in $ART)"
exit $failed
