#!/usr/bin/env bash
# Disposable Windows 11 test VM (QEMU/KVM, UEFI + emulated TPM 2.0) used to run the stock mstsc.exe
# against the RDP server. Usage: bash vm/win11.sh <install|up|down|status|shot FILE|data HOST:PORT USER|test HOST:PORT USER>
# The install ISO is read in place (never copied) from iso/ (git-ignored); the rest lives in vm/state/win11 (git-ignored).
set -euo pipefail

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
W="$VM_DIR/win11"
S="$VM_DIR/state/win11"
# ISO: $WIN11_ISO, else <project root>/iso (git-ignored; $WIN11_ISO_DIR overrides the folder).
ISO_URL="${WIN11_ISO_URL:-https://public.trodix.com/iso/windows/Windows11_Client_x64_en-us_26300_9457.iso}"
ISO="${WIN11_ISO:-${WIN11_ISO_DIR:-$VM_DIR/../iso}/$(basename "$ISO_URL")}"
# Missing ISO: downloaded to that path, then checked against the SHA-256 of the ISO used for the tests.
ISO_SHA256="bd4307df32bc8af33b39ccecb1174aeb345386630f89a2b86c7a4e36b55ea650"
CPUS="${WIN_CPUS:-4}"; MEM="${WIN_MEM:-8192}"; DISK="${WIN_DISK:-80G}"
MON="$S/monitor.sock"; PIDFILE="$S/qemu.pid"; TPMDIR="$S/tpm"

die() { echo "win11.sh: $*" >&2; exit 1; }
mon() { printf '%s\n' "$1" | socat - "UNIX-CONNECT:$MON" >/dev/null; }
# mon_wait CMD: sends one HMP command and waits for its completion (the "(qemu)" prompt), for slow ones (savevm).
mon_wait() {
  python3 -I - "$MON" "$1" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(900)
def until_prompt():
    buf = b""
    while not buf.rstrip().endswith(b"(qemu)"):
        d = s.recv(65536)
        if not d: break
        buf += d
    return buf
until_prompt()
s.sendall(sys.argv[2].encode() + b"\n")
out = until_prompt().decode(errors="replace")
print(out.replace("\r", ""))
sys.exit(1 if "rror" in out or "annot" in out else 0)
PY
}
running() { [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; }

make_iso() { # make_iso <out.iso> <label> <files...>
  local out="$1" label="$2"; shift 2
  local tmp; tmp="$(mktemp -d)"; cp "$@" "$tmp/"
  xorriso -as mkisofs -quiet -J -r -V "$label" -o "$out" "$tmp"; rm -rf "$tmp"
}

start_tpm() {
  mkdir -p "$TPMDIR"
  swtpm socket --tpm2 --tpmstate dir="$TPMDIR" --ctrl type=unixio,path="$S/swtpm.sock" \
    --pid file="$S/swtpm.pid" --daemon
}

launch() { # launch <with-install-iso: 0|1> [extra qemu args...]
  local with_iso="$1"; shift
  running && die "already running"
  start_tpm
  local boot=()
  if [[ "$with_iso" == 1 ]]; then
    boot=(-drive "if=none,id=win,media=cdrom,file=$ISO,readonly=on" -device "ide-cd,drive=win,bus=ide.0,bootindex=1")
  fi
  qemu-system-x86_64 -machine q35,smm=on,accel=kvm -cpu host -smp "$CPUS" -m "$MEM" \
    -global driver=cfi.pflash01,property=secure,value=on \
    -drive "if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE.secboot.fd,readonly=on" \
    -drive "if=pflash,format=qcow2,unit=1,file=$S/OVMF_VARS.qcow2" \
    -chardev "socket,id=chrtpm,path=$S/swtpm.sock" -tpmdev emulator,id=tpm0,chardev=chrtpm \
    -device tpm-crb,tpmdev=tpm0 \
    -drive "if=none,id=sys,file=${DISK_IMG:-$S/base.qcow2},format=qcow2" -device ide-hd,drive=sys,bus=ide.3,bootindex=2 \
    "${boot[@]}" \
    -drive "if=none,id=ans,media=cdrom,file=$S/answer.iso,readonly=on" -device ide-cd,drive=ans,bus=ide.1 \
    -drive "if=none,id=dat,media=cdrom,file=$S/data.iso,readonly=on" -device ide-cd,drive=dat,bus=ide.2,id=datcd \
    -device e1000e,netdev=n0 -netdev user,id=n0 \
    -audiodev "wav,id=snd,path=$S/audio.wav,out.frequency=48000" -device ich9-intel-hda -device hda-output,audiodev=snd \
    -vga std -device qemu-xhci -device usb-tablet -device usb-kbd \
    -serial "file:$S/serial.log" -display none -monitor "unix:$MON,server,nowait" -pidfile "$PIDFILE" "$@" -daemonize
}

stop_vm() { # quit QEMU (lets the wav backend finalize its file), stop swtpm
  if running; then mon quit; for _ in $(seq 1 30); do running || break; sleep 0.5; done; fi
  [[ -f "$S/swtpm.pid" ]] && kill "$(cat "$S/swtpm.pid")" 2>/dev/null || true
}

# The snapshot depends on the guest script and on the virtual hardware defined in this file.
snapshot_key() { cat "$W/rdp-activex-test.ps1" "${BASH_SOURCE[0]}" | sha256sum | cut -d' ' -f1; }

take_snapshot() { # boot, wait for the guest script to wait for its parameters (READY), savevm
  running && die "already running"
  [[ -f "$S/OVMF_VARS.qcow2" ]] || qemu-img convert -q -f raw -O qcow2 "$S/OVMF_VARS.fd" "$S/OVMF_VARS.qcow2"
  # Fresh overlay on the pristine base (cleanly shut down), so a snapshot never builds on a killed session.
  rm -f "$S/overlay.qcow2"; qemu-img create -q -f qcow2 -b "$S/base.qcow2" -F qcow2 "$S/overlay.qcow2"
  export DISK_IMG="$S/overlay.qcow2"
  echo "no target yet" > "$S/README.txt"; make_iso "$S/data.iso" RDPDATA "$S/README.txt" "$W/rdp-activex-test.ps1"
  : > "$S/serial.log"; launch 0
  for _ in $(seq 1 300); do grep -q '^READY' "$S/serial.log" 2>/dev/null && break; running || die "VM stopped"; sleep 1; done
  grep -q '^READY' "$S/serial.log" || { stop_vm; die "guest never reported READY"; }
  sleep 10   # let the desktop settle
  mon_wait "savevm ready" >/dev/null || { stop_vm; die "savevm failed"; }
  stop_vm
  snapshot_key > "$S/snapshot.sha"
  echo "snapshot 'ready' saved"
}

case "${1:-}" in
  install)
    if [[ ! -r "$ISO" ]]; then
      mkdir -p "$(dirname "$ISO")"
      echo "ISO not found, downloading $ISO_URL"; curl -fL --retry 3 -C - -o "$ISO" "$ISO_URL" || die "download failed"
      [[ "$(sha256sum "$ISO" | cut -d' ' -f1)" == "$ISO_SHA256" ]] || { rm -f "$ISO"; die "downloaded ISO does not match the expected SHA-256"; }
    fi
    mkdir -p "$S"
    [[ -f "$S/password" ]] || { head -c 18 /dev/urandom | base64 | tr -d '/+=' > "$S/password"; chmod 600 "$S/password"; }
    sed "s|@PASSWORD@|$(cat "$S/password")|g" "$W/autounattend.xml.tmpl" > "$S/autounattend.xml"
    cp "$W/first-logon.ps1" "$S/"
    make_iso "$S/answer.iso" ANSWER "$S/autounattend.xml" "$S/first-logon.ps1"
    echo "no target yet" > "$S/README.txt"; make_iso "$S/data.iso" RDPDATA "$S/README.txt" "$W/rdp-activex-test.ps1"
    rm -rf "$TPMDIR" "$S/snapshot.sha"; qemu-img convert -q -f raw -O qcow2 /usr/share/OVMF/OVMF_VARS.secboot.fd "$S/OVMF_VARS.qcow2"
    qemu-img create -q -f qcow2 "$S/base.qcow2" "$DISK"
    rm -f "$S/audio.wav"; launch 1
    # "Press any key to boot from CD": a few Enter presses during the first seconds.
    for _ in $(seq 1 25); do sleep 1; mon "sendkey ret" || true; done
    echo "install started (pid $(cat "$PIDFILE")); follow with: bash vm/win11.sh shot /path/x.ppm" ;;
  up) launch 0 ;;
  down) running && { mon "system_powerdown"; for _ in $(seq 1 60); do running || break; sleep 1; done; running && mon quit; }
        [[ -f "$S/swtpm.pid" ]] && kill "$(cat "$S/swtpm.pid")" 2>/dev/null || true ;;
  status) running && echo "running pid $(cat "$PIDFILE")" || echo stopped ;;
  shot) mon "screendump ${2:?output file (.ppm)}" ;;
  data) # data HOST:PORT USER  (password from RDP_TEST_PASSWORD; goes to the data CD, never argv)
    [[ -n "${RDP_TEST_PASSWORD:-}" ]] || die "RDP_TEST_PASSWORD required"
    running && die "stop the VM first (down), the data CD is rebuilt"
    printf 'full address:s:%s\nauthentication level:i:0\nprompt for credentials:i:0\nredirectclipboard:i:1\nscreen mode id:i:1\ndesktopwidth:i:1280\ndesktopheight:i:720\nsmart sizing:i:1\n' "${2:?HOST:PORT}" > "$S/target.rdp"
    printf '%s\t%s' "${3:?USER}" "$RDP_TEST_PASSWORD" > "$S/cred.txt"
    make_iso "$S/data.iso" RDPDATA "$S/target.rdp" "$S/cred.txt" "$W/mstsc-test.ps1"; rm -f "$S/cred.txt" ;;
  snapshot) take_snapshot ;;
  test) # test HOST:PORT USER [TIMEOUT_S]: restores the snapshot, swaps the data CD (params only), waits for the
        # guest script to report END on the serial port, checks the tone recorded from the guest audio output.
        # Exit 0 iff the guest RESULT passed and (unless WIN_EXPECT_TONE=0) the 440 Hz tone was heard.
    [[ -n "${RDP_TEST_PASSWORD:-}" ]] || die "RDP_TEST_PASSWORD required"
    running && die "already running"
    addr="${2:?HOST:PORT}"; : "${3:?USER}"
    [[ -f "$S/overlay.qcow2" && "$(snapshot_key)" == "$(cat "$S/snapshot.sha" 2>/dev/null)" ]] || take_snapshot
    printf 'server=%s\nport=%s\nuser=%s\npassword=%s\nwidth=1280\nheight=720\n' \
      "${addr%%:*}" "${addr##*:}" "$3" "$RDP_TEST_PASSWORD" > "$S/params.txt"
    make_iso "$S/test-data.iso" RDPDATA "$S/params.txt"; rm -f "$S/params.txt" "$S/audio.wav"
    export DISK_IMG="$S/overlay.qcow2"
    : > "$S/serial.log"; launch 0 -loadvm ready
    for _ in $(seq 1 50); do [[ -S "$MON" ]] && break; sleep 0.2; done
    mon_wait "change dat $S/test-data.iso" >/dev/null || { stop_vm; die "cannot swap the data CD"; }
    for _ in $(seq 1 "${4:-180}"); do grep -q '^END' "$S/serial.log" 2>/dev/null && break; running || break; sleep 1; done
    grep -q '^END' "$S/serial.log" 2>/dev/null || echo "timeout or VM stopped before END" >&2
    stop_vm
    tr -d '\r' < "$S/serial.log" | grep -E '^(BEGIN|CHECK|RESULT|END|ERROR)' || true
    ok=1
    tr -d '\r' < "$S/serial.log" | grep -q '^RESULT {.*"passed":true' || ok=0
    if [[ "${WIN_EXPECT_TONE:-1}" == 1 ]]; then
      bin="$VM_DIR/../target/debug/rdp-test-client"
      [[ -x "$bin" ]] || (cd "$VM_DIR/.." && cargo build -q -p rdp-test-client --features ironrdp)
      "$bin" --analyze-wav "$S/audio.wav" 1000 2 guest_audio_chain_reference || ok=0
      "$bin" --analyze-wav "$S/audio.wav" || ok=0
    fi
    [[ $ok == 1 ]] ;;
  *) die "usage: win11.sh <install|up|down|status|shot FILE|data HOST:PORT USER|snapshot|test HOST:PORT USER>" ;;
esac
