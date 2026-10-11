#!/usr/bin/env bash
# Printer redirection (RDPDR) against the synthetic desktop.
#  1. The IronRDP test client redirects a printer and records the jobs; the `printers` scenario sends jobs through the
#     Unix socket of the printer and compares the recorded bytes.
#  2. Same with a real CUPS: a private cupsd (user mode, own spool) with the `rdsktp` backend; `lp -o raw` must come out
#     byte for byte, and a text job through the CUPS filters must come out as the text printer language.
# usage: ci/printers-test.sh [out-dir]   env: PRINTERS_TEST_KIB (default 512)
set -uo pipefail
cd "$(dirname "$0")/.."
OUT="$(realpath -m "${1:-artifacts/printers-test}")"
PORT="${PRINTERS_TEST_PORT:-33892}"
cargo build -q --release -p rdsktp -p rdp-test-client -p rdsktp-cups-backend --features rdp-test-client/ironrdp || exit 1
rm -rf "$OUT"; mkdir -p "$OUT/state" "$OUT/rdsktp-printers" "$OUT/record"
rc=0
srv=""; cups=""
cleanup() { [[ -n $srv ]] && kill "$srv" 2>/dev/null; [[ -n $cups ]] && kill "$cups" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

wait_port() { for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && return 0; sleep 0.1; done; return 1; }

# --- private cupsd -----------------------------------------------------------------------------------------------
HAVE_CUPS=0
if command -v cupsd >/dev/null && command -v lp >/dev/null && command -v lpadmin >/dev/null; then
  C="$OUT/cups"; mkdir -p "$C"/{bin/backend,bin/filter,bin/daemon,spool,cache,state,log,tmp}
  for f in /usr/lib/cups/filter/* ; do ln -sf "$f" "$C/bin/filter/"; done
  for f in /usr/lib/cups/daemon/* ; do ln -sf "$f" "$C/bin/daemon/"; done
  for f in /usr/lib/cups/backend/*; do [[ $(basename "$f") != rdsktp ]] && ln -sf "$f" "$C/bin/backend/"; done
  install -m 0755 target/release/rdsktp-cups-backend "$C/bin/backend/rdsktp"
  cat >"$C/cupsd.conf" <<CONF
LogLevel debug
Listen $C/cups.sock
Browsing No
DefaultAuthType None
<Location />
  Order allow,deny
  Allow all
</Location>
<Policy default>
  <Limit All>
    Order allow,deny
    Allow all
  </Limit>
</Policy>
CONF
  cat >"$C/cups-files.conf" <<CONF
SystemGroup $(id -gn)
ServerRoot $C
ServerBin $C/bin
RequestRoot $C/spool
CacheDir $C/cache
StateDir $C/state
TempDir $C/tmp
AccessLog $C/log/access
ErrorLog $C/log/error
PageLog $C/log/page
CONF
  CUPS_SERVERROOT="$C" cupsd -f -c "$C/cupsd.conf" -s "$C/cups-files.conf" >"$OUT/cupsd.log" 2>&1 & cups=$!
  for _ in $(seq 50); do [[ -S $C/cups.sock ]] && break; sleep 0.1; done
  if [[ -S $C/cups.sock ]]; then HAVE_CUPS=1; export CUPS_SERVER="$C/cups.sock"; else echo "SKIP cups: private cupsd did not start (see $OUT/cupsd.log)"; fi
else
  echo "SKIP cups: cupsd/lp/lpadmin missing"
fi

RUST_LOG=info target/release/rdsktp --test-mode --listen "127.0.0.1:$PORT" --state-dir "$OUT/state" --printers "$OUT/rdsktp-printers" \
  $([[ $HAVE_CUPS = 1 ]] && echo --printers-cups on || echo --printers-cups off) >"$OUT/server.log" 2>&1 & srv=$!
wait_port || { echo "FAIL server did not start"; exit 1; }

timeout 180 target/release/rdp-test-client --out "$OUT" --port "$PORT" --no-nla --printer-socket "$OUT/rdsktp-printers" \
  --printer-record "$OUT/record" --printer-name Bureau --printer-kib "${PRINTERS_TEST_KIB:-512}" >"$OUT/client.log" 2>&1 &
cli=$!

if [[ $HAVE_CUPS = 1 ]]; then
  user=$(id -un)
  q="$user-Bureau"
  for _ in $(seq 100); do lpstat -p "$q" >/dev/null 2>&1 && break; sleep 0.2; done
  if lpstat -p "$q" >/dev/null 2>&1; then
    printf 'RAW-BYTES \x00\x01\x02 \xff fin\n' >"$OUT/raw.bin"
    lp -d "$q" -o raw "$OUT/raw.bin" >"$OUT/lp-raw.log" 2>&1
    printf 'Bonjour depuis CUPS\nligne deux\n' >"$OUT/text.txt"
    lp -d "$q" "$OUT/text.txt" >"$OUT/lp-text.log" 2>&1
    for _ in $(seq 150); do
      [[ -z $(lpstat -o "$q" 2>/dev/null) ]] && break; sleep 0.2
    done
    sleep 1
    raw_ok=0; text_ok=0
    for f in "$OUT"/record/*; do
      cmp -s "$f" "$OUT/raw.bin" && raw_ok=1
      grep -q "Bonjour depuis CUPS" "$f" 2>/dev/null && text_ok=1
    done
    [[ $raw_ok = 1 ]] && echo "PASS  cups_raw   lp -o raw came out byte for byte" || { echo "FAIL  cups_raw   job missing or altered (see $OUT/cups/log/error)"; rc=1; }
    [[ $text_ok = 1 ]] && echo "PASS  cups_text  text job through the CUPS filters came out" || { echo "FAIL  cups_text  text job missing (see $OUT/cups/log/error)"; rc=1; }
    # The queue goes away with the client.
  else
    echo "FAIL  cups_queue  queue $q never appeared"; rc=1
  fi
fi

wait $cli
python3 - "$OUT/client.json" <<'P'
import json, sys
checks = {c["name"]: c for c in json.load(open(sys.argv[1]))["checks"]}
c = checks.get("printers")
ok = bool(c and c["passed"])
print(("PASS" if ok else "FAIL"), " printers ", c["detail"] if c else "scenario missing")
sys.exit(0 if ok else 1)
P
[[ $? = 0 ]] || rc=1

# Once the client has gone, neither the socket nor the queue must stay.
sleep 1.5
if [[ -e "$OUT/rdsktp-printers/Bureau.sock" ]]; then echo "FAIL  cleanup  the socket stays"; rc=1; else echo "PASS  cleanup  no socket left"; fi
if [[ $HAVE_CUPS = 1 ]]; then
  if lpstat -p 2>/dev/null | grep -q Bureau; then echo "FAIL  cleanup  the CUPS queue stays"; rc=1; else echo "PASS  cleanup  no queue left"; fi
fi
exit $rc
