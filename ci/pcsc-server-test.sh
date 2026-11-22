#!/usr/bin/env bash
# PC/SC server (crates/pcsc) against the real libpcsclite: the `fake_reader` example serves a fake card reader on a
# private socket and the clients of this machine use it through PCSCLITE_CSOCK_NAME. The host's own pcscd is never
# touched.
#  1. opensc-tool: the reader is listed, the ATR printed, APDUs answered.
#  2. a C probe linked to libpcsclite.so.1 (needs cc): connect, status, transmit, transactions, attributes,
#     SCardCancel, time-outs, and a card removal / insertion waking SCardGetStatusChange.
#  3. the device detached (no reader any more), then attached again; the socket goes away with the server.
# usage: ci/pcsc-server-test.sh [out-dir]
set -uo pipefail
cd "$(dirname "$0")/.."
if ! command -v opensc-tool >/dev/null; then
  echo "SKIP pcsc-server-test: opensc-tool is not installed (OpenSC), nothing to test the PC/SC server with"
  exit 0
fi
OUT="$(realpath -m "${1:-artifacts/pcsc-server-test}")"
cargo build -q -p oxydesk-pcsc --example fake_reader || exit 1
EXE="$PWD/target/debug/examples/fake_reader"
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$OUT" || exit 1
# The socket path is relative to $OUT (a socket path is limited to 107 bytes); libpcsclite takes it as is.
SOCK=pcscd.comm
export PCSCLITE_CSOCK_NAME="$SOCK"
rc=0
srv=""; probe=""
cleanup() { [[ -n $probe ]] && kill "$probe" 2>/dev/null; [[ -n $srv ]] && kill "$srv" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

check() { # name, command that must succeed
  local name="$1"; shift
  if "$@"; then echo "PASS  $name"; else echo "FAIL  $name"; rc=1; fi
}
wait_for() { # file pattern [tenths of a second]
  for _ in $(seq "${3:-100}"); do grep -q "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done; return 1
}
command_server() { echo "$1" >&3; }

mkfifo cmd
exec 3<>cmd
RUST_LOG=debug "$EXE" "$SOCK" <cmd >server.log 2>&1 & srv=$!
wait_for server.log "serving" || { echo "FAIL  server did not start (see $OUT/server.log)"; exit 1; }

# --- 1. opensc-tool --------------------------------------------------------------------------------------------
timeout 20 opensc-tool -l >list.log 2>&1
check "opensc_list     the reader is listed" grep -q "Fake Reader 0" list.log
timeout 20 opensc-tool -a >atr.log 2>&1
check "opensc_atr      the ATR is printed" grep -q "3b:75:13:00:00:45:09:ea:90:00" atr.log
timeout 20 opensc-tool --send-apdu 00:A4:04:00 >select.log 2>&1
check "opensc_select   SELECT answered 90 00" grep -q "SW1=0x90, SW2=0x00" select.log
timeout 20 opensc-tool --send-apdu 00:B0:00:00:04 >read.log 2>&1
check "opensc_read     READ BINARY data comes back" grep -q "00 01 02 03" read.log

# --- 2. the C probe on libpcsclite -----------------------------------------------------------------------------
if command -v cc >/dev/null && cc -Wall -o probe "$OLDPWD/crates/pcsc/tests/libpcsclite_probe.c" -l:libpcsclite.so.1 -lpthread 2>probe-build.log; then
  ./probe >probe.log 2>&1 & probe=$!
  if wait_for probe.log "WAITING removal" 100; then
    command_server remove
    wait_for probe.log "WAITING insertion" 100 && command_server insert
  fi
  for _ in $(seq 100); do kill -0 "$probe" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$probe" 2>/dev/null; then echo "FAIL  probe did not finish (see $OUT/probe.log)"; rc=1; kill "$probe"; fi
  probe=""
  p() { grep -q "$1" probe.log; }
  check "probe_context   context established, reader listed" p "^LIST 0 Fake Reader 0"
  check "probe_state     state, ATR and counter read" p "^STATE 0 .* atr=3b751300004509ea9000 count=0"
  check "probe_connect   connected with T=1" p "^CONNECT 0 proto=2"
  check "probe_status    SCardStatus gives name, ATR, protocol" p "^STATUS 0 name=Fake Reader 0 .* proto=2 atr=3b751300004509ea9000"
  check "probe_transmit  SELECT answered 9000" p "^TRANSMIT_SELECT 0 9000"
  check "probe_big       258-byte answer" p "^TRANSMIT_256 0 len=258"
  check "probe_small     small buffer: INSUFFICIENT_BUFFER with the size" p "^TRANSMIT_SMALL_BUFFER 80100008 need=258"
  check "probe_attrib    ATR attribute" p "^ATTRIB_ATR 0 3b751300004509ea9000"
  check "probe_trans     begin/end transaction, reconnect" bash -c 'grep -q "^BEGIN 0" probe.log && grep -q "^END 0" probe.log && grep -q "^RECONNECT 0" probe.log'
  check "probe_timeout   SCARD_E_TIMEOUT without change" p "^TIMEOUT_RV 8010000a"
  check "probe_cancel    SCardCancel ends the wait (SCARD_E_CANCELLED)" bash -c 'grep -q "^CANCEL_RV 0" probe.log && grep -q "^CANCELLED_RV 80100002" probe.log'
  check "probe_removal   removal wakes SCardGetStatusChange (EMPTY, counter 1)" p "^REMOVAL 0 event=12 count=1"
  check "probe_removed   transmit after removal: SCARD_W_REMOVED_CARD" p "^TRANSMIT_AFTER_REMOVAL 80100069"
  check "probe_insertion insertion wakes it again, with the ATR" p "^INSERTION 0 event=22 atr=3b751300004509ea9000"
  check "probe_release   disconnect and release" bash -c 'grep -q "^DISCONNECT 0" probe.log && grep -q "^RELEASE 0" probe.log && grep -q "^DONE" probe.log'
else
  echo "SKIP  libpcsclite probe: no C compiler or libpcsclite.so.1 (see $OUT/probe-build.log)"
fi

# --- 3. detach / attach ----------------------------------------------------------------------------------------
command_server detach
sleep 0.5
timeout 20 opensc-tool -l >list-detached.log 2>&1
check "detach          no reader once the device is detached" grep -q "No smart card readers" list-detached.log
command_server attach
for _ in $(seq 50); do timeout 20 opensc-tool -l 2>&1 | grep -q "Fake Reader 0" && break; sleep 0.2; done
timeout 20 opensc-tool -l >list-attached.log 2>&1
check "attach          the reader is back" grep -q "Fake Reader 0" list-attached.log

# --- the socket goes away with the server ---------------------------------------------------------------------
command_server quit
wait "$srv" 2>/dev/null; srv=""
check "cleanup         no socket left" test ! -e "$SOCK"
exit $rc
