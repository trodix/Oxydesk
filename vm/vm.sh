#!/usr/bin/env bash
# VM de test jetable (QEMU/KVM, Debian 13). Voir docs/ et le brief §11.1.
# Usage : bash vm/vm.sh <create|up|down|reset|ssh|deploy|kwin-config|login-config|logs|status> [args]
#
# ATTENTION : les overlays qcow2 référencent leur backing file par chemin ABSOLU.
# Déplacer le dossier vm/ (ou le dépôt) casse les overlays : relancer `create`.
set -euo pipefail

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$VM_DIR/state"

VM_CPUS="${VM_CPUS:-2}"
VM_MEM="${VM_MEM:-4096}"
VM_DISK="${VM_DISK:-20G}"
VM_CPU_MODEL="${VM_CPU_MODEL:-host}"
VM_NIC="${VM_NIC:-vmxnet3}"
VM_VGA="${VM_VGA:-virtio-gl}"
VM_SSH_PORT="${VM_SSH_PORT:-2222}"
VM_RDP_PORT="${VM_RDP_PORT:-3390}"
VM_USER="${VM_USER:-dev}"
VM_SERVICE="${VM_SERVICE:-rdsktp}"
VM_IMAGE_FLAVOR="${VM_IMAGE_FLAVOR:-generic}"
VM_BOOT_TIMEOUT="${VM_BOOT_TIMEOUT:-300}"

IMAGE_BASE_URL="https://cloud.debian.org/images/cloud/trixie/latest"
IMAGE_FILE="debian-13-${VM_IMAGE_FLAVOR}-amd64.qcow2"

BASE="$STATE/base.qcow2"
PROVISIONED="$STATE/provisioned.qcow2"
RUN="$STATE/run.qcow2"
SEED="$STATE/seed.iso"
KEY="$STATE/id_ed25519"
PIDFILE="$STATE/qemu.pid"
CONSOLE="$STATE/console.log"

die() { echo "vm.sh: $*" >&2; exit 1; }
log() { echo "vm.sh: $*" >&2; }

need() { # need <commande> <paquet Fedora>
  command -v "$1" >/dev/null 2>&1 || die "outil manquant : $1 (Fedora : sudo dnf install $2)"
}

check_tools() {
  need qemu-img qemu-img
  need qemu-system-x86_64 qemu-system-x86-core
  need curl curl
  need ssh openssh-clients
  need scp openssh-clients
  need ssh-keygen openssh
  need sha512sum coreutils
  if ! command -v cloud-localds >/dev/null && ! command -v genisoimage >/dev/null && ! command -v xorriso >/dev/null; then
    die "aucun outil ISO (cloud-localds, genisoimage ou xorriso) ; Fedora : sudo dnf install xorriso"
  fi
}

check_kvm() {
  [[ -r /dev/kvm && -w /dev/kvm ]] || die "/dev/kvm inaccessible (KVM obligatoire) : vérifier le groupe kvm / la virtualisation"
}

ssh_opts() {
  echo -i "$KEY" -p "$VM_SSH_PORT" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 -o BatchMode=yes
}
vm_ssh() { # shellcheck disable=SC2046
  ssh $(ssh_opts) "$VM_USER@127.0.0.1" "$@"
}

is_running() {
  [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null
}

make_seed() {
  mkdir -p "$STATE/seed"
  [[ -f "$KEY" ]] || ssh-keygen -q -t ed25519 -N "" -C "rdsktp-vm-test" -f "$KEY"
  local pub; pub="$(cat "$KEY.pub")"
  sed -e "s|@VM_USER@|$VM_USER|g" -e "s|@SSH_PUBKEY@|$pub|g" \
    "$VM_DIR/cloud-init/user-data.tmpl" > "$STATE/seed/user-data"
  # instance-id fixe : cloud-init ne rejoue pas, le seed reste attaché à tous les boots.
  printf 'instance-id: rdsktp-vm-test\nlocal-hostname: rdsktp-test\n' > "$STATE/seed/meta-data"
  rm -f "$SEED"
  if command -v cloud-localds >/dev/null; then
    cloud-localds "$SEED" "$STATE/seed/user-data" "$STATE/seed/meta-data"
  elif command -v genisoimage >/dev/null; then
    genisoimage -quiet -output "$SEED" -volid cidata -joliet -rock "$STATE/seed/user-data" "$STATE/seed/meta-data"
  else
    xorriso -as mkisofs -quiet -output "$SEED" -volid cidata -joliet -rock "$STATE/seed/user-data" "$STATE/seed/meta-data"
  fi
}

new_overlay() { # new_overlay <overlay> <backing>
  rm -f "$1"
  qemu-img create -q -f qcow2 -b "$2" -F qcow2 "$1" >/dev/null
}

start_qemu() { # start_qemu <disque>
  check_kvm
  is_running && die "la VM tourne déjà (pid $(cat "$PIDFILE"))"
  : > "$CONSOLE"
  local -a VGA_ARGS
  case "$VM_VGA" in
    virtio-gl) VGA_ARGS=(-device virtio-vga-gl -display egl-headless) ;; # virgl : rendu 3D via le GPU de l'hôte
    *) VGA_ARGS=(-vga "$VM_VGA" -display none) ;;
  esac
  qemu-system-x86_64 \
    -machine q35,accel=kvm -cpu "$VM_CPU_MODEL" -smp "$VM_CPUS" -m "$VM_MEM" \
    -drive "file=$1,if=virtio,format=qcow2" \
    -drive "file=$SEED,if=virtio,format=raw,readonly=on" \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${VM_SSH_PORT}-:22,hostfwd=tcp:127.0.0.1:${VM_RDP_PORT}-:3389" \
    -device "$VM_NIC,netdev=n0" \
    "${VGA_ARGS[@]}" \
    -serial "file:$CONSOLE" \
    -daemonize -pidfile "$PIDFILE"
}

wait_ssh() {
  local deadline=$((SECONDS + VM_BOOT_TIMEOUT))
  until vm_ssh true 2>/dev/null; do
    is_running || die "QEMU est mort pendant le boot (voir $CONSOLE)"
    ((SECONDS < deadline)) || die "SSH injoignable après ${VM_BOOT_TIMEOUT}s (voir $CONSOLE)"
    sleep 3
  done
}

cmd_down() {
  if ! is_running; then rm -f "$PIDFILE"; return 0; fi
  local pid; pid="$(cat "$PIDFILE")"
  vm_ssh 'sudo poweroff' 2>/dev/null || true
  for _ in $(seq 1 60); do
    kill -0 "$pid" 2>/dev/null || { rm -f "$PIDFILE"; return 0; }
    sleep 1
  done
  log "arrêt propre impossible, kill de $pid"
  kill "$pid" 2>/dev/null || true; sleep 2; kill -9 "$pid" 2>/dev/null || true
  rm -f "$PIDFILE"
}

cmd_create() {
  check_tools
  check_kvm
  mkdir -p "$STATE"
  if [[ ! -f "$BASE" ]]; then
    log "téléchargement de $IMAGE_FILE"
    curl -fL --retry 3 -o "$BASE.part" "$IMAGE_BASE_URL/$IMAGE_FILE"
    local want got
    want="$(curl -fsSL "$IMAGE_BASE_URL/SHA512SUMS" | awk -v f="$IMAGE_FILE" '$2==f {print $1}')"
    [[ -n "$want" ]] || die "SHA512 introuvable pour $IMAGE_FILE"
    got="$(sha512sum "$BASE.part" | awk '{print $1}')"
    [[ "$want" == "$got" ]] || { rm -f "$BASE.part"; die "SHA512 incorrect pour $IMAGE_FILE"; }
    mv "$BASE.part" "$BASE"; chmod a-w "$BASE"
  fi
  make_seed
  local tmp="$STATE/provisioning.qcow2"
  new_overlay "$tmp" "$BASE"
  qemu-img resize -q "$tmp" "$VM_DISK"
  log "premier boot : provisioning (plusieurs minutes)"
  start_qemu "$tmp"
  wait_ssh
  vm_ssh 'sudo cloud-init status --wait' >/dev/null || log "cloud-init a signalé une erreur (marqueur vérifié ensuite)"
  vm_ssh 'test -f /var/lib/rdsktp-provisioned' || { vm_ssh 'sudo tail -n 40 /var/log/provision-rdsktp.log' || true; die "provisioning incomplet (marqueur absent)"; }
  cmd_down
  chmod u+w "$PROVISIONED" 2>/dev/null || true
  mv "$tmp" "$PROVISIONED"; chmod a-w "$PROVISIONED"
  cmd_reset
  log "create terminé"
}

cmd_reset() {
  [[ -f "$PROVISIONED" ]] || die "image provisionnée absente : lancer 'create' d'abord"
  is_running && cmd_down
  new_overlay "$RUN" "$PROVISIONED"
}

cmd_up() {
  check_tools
  [[ -f "$RUN" ]] || cmd_reset
  [[ -f "$SEED" ]] || make_seed
  start_qemu "$RUN"
  wait_ssh
  log "VM prête (ssh :$VM_SSH_PORT, rdp :$VM_RDP_PORT)"
}

cmd_ssh() { vm_ssh "$@"; }

cmd_deploy() {
  local deb="${1:-}"
  if [[ -z "$deb" ]]; then
    deb="$(ls -t "$VM_DIR"/../target/debian/*.deb 2>/dev/null | head -n1 || true)"
  fi
  [[ -n "$deb" && -f "$deb" ]] || die "aucun .deb à déployer (passer un fichier ou lancer cargo deb)"
  log "déploiement de $deb"
  # shellcheck disable=SC2046
  scp $(ssh_opts | sed 's/-p /-P /') "$deb" "$VM_USER@127.0.0.1:/tmp/deploy.deb"
  vm_ssh 'sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall /tmp/deploy.deb'
  vm_ssh "if systemctl cat $VM_SERVICE >/dev/null 2>&1; then sudo systemctl restart $VM_SERVICE; sudo journalctl -u $VM_SERVICE -n 30 --no-pager; fi"
}

# Configure le service rdsktp pour une session KWin réelle (autologin de $VM_USER) : drop-in systemd + restart.
cmd_kwin_config() {
  local uid_run="/run/user/$(vm_ssh 'id -u')"
  vm_ssh 'sudo mkdir -p /etc/systemd/system/rdsktp.service.d && sudo tee /etc/systemd/system/rdsktp.service.d/kwin.conf >/dev/null' <<EOF
[Service]
User=$VM_USER
SupplementaryGroups=video render kvm
DynamicUser=no
ProtectHome=no
ProtectSystem=full
PrivateTmp=no
Environment=XDG_RUNTIME_DIR=$uid_run
Environment=${VM_SERVER_ENV:-RDSKTP_PUMP=event}
ExecStart=
ExecStart=/usr/bin/rdsktp --session kwin --outputs ${VM_OUTPUTS:-1} --session-app "plasmashell" --session-app "/usr/lib/rdsktp/rdsktp-testapp 1920 1080 --clipboard-echo --tone" --listen 0.0.0.0:3389 --state-dir /var/lib/rdsktp
EOF
  vm_ssh 'sudo systemctl daemon-reload && sudo systemctl restart rdsktp && sleep 5 && systemctl is-active rdsktp && sudo journalctl -u rdsktp -n 20 --no-pager'
}

# Mode login distant : rdsktp (utilisateur rdsktp, --login) + rdsktp-authd (root) + rdsktp-agent@dev.
# Le mot de passe du compte de test vient de RDP_TEST_PASSWORD et transite par l'entrée standard (jamais par argv).
cmd_login_config() {
  [[ -n "${RDP_TEST_PASSWORD:-}" ]] || die "RDP_TEST_PASSWORD requis"
  printf '%s:%s\n' "$VM_USER" "$RDP_TEST_PASSWORD" | vm_ssh 'sudo chpasswd'
  vm_ssh 'sudo mkdir -p /etc/rdsktp /etc/systemd/system/rdsktp.service.d && sudo rm -f /etc/systemd/system/rdsktp.service.d/kwin.conf'
  vm_ssh 'sudo tee /etc/rdsktp/agent.conf >/dev/null' <<CONF
RDSKTP_AGENT_ARGS=--outputs ${VM_OUTPUTS:-1} ${VM_AGENT_EXTRA:-} --session-app plasmashell --session-app "/usr/lib/rdsktp/rdsktp-testapp 1920 1080 --clipboard-echo --tone"
CONF
  vm_ssh 'sudo tee /etc/systemd/system/rdsktp.service.d/login.conf >/dev/null' <<CONF
[Unit]
Requires=rdsktp-authd.service
After=rdsktp-authd.service

[Service]
User=rdsktp
DynamicUser=no
Environment=${VM_SERVER_ENV:-RDSKTP_PUMP=event}
ExecStart=
ExecStart=/usr/bin/rdsktp --login --outputs ${VM_OUTPUTS:-1} --listen 0.0.0.0:3389 --state-dir /var/lib/rdsktp
CONF
  vm_ssh 'sudo systemd-sysusers /usr/lib/sysusers.d/rdsktp.conf; sudo systemctl daemon-reload && sudo systemctl restart rdsktp-authd rdsktp && sleep 3 && systemctl is-active rdsktp-authd rdsktp && sudo journalctl -u rdsktp-authd -u rdsktp -n 20 --no-pager'
}

cmd_logs() {
  case "${1:-console}" in
    console) [[ -f "$CONSOLE" ]] && cat "$CONSOLE" || die "pas de console ($CONSOLE)" ;;
    journal) vm_ssh "sudo journalctl --no-pager ${2:+-u $2}" ;;
    *) die "usage : logs [console|journal [unité]]" ;;
  esac
}

cmd_status() {
  if is_running; then echo "VM: running (pid $(cat "$PIDFILE"))"; else echo "VM: stopped"; fi
  for f in "$BASE" "$PROVISIONED" "$RUN" "$SEED" "$KEY"; do
    [[ -e "$f" ]] && echo "ok      $f" || echo "absent  $f"
  done
}

main() {
  local c="${1:-}"; shift || true
  case "$c" in
    create) cmd_create ;;
    up) cmd_up ;;
    down) cmd_down ;;
    reset) cmd_reset ;;
    ssh) cmd_ssh "$@" ;;
    deploy) cmd_deploy "$@" ;;
    kwin-config) cmd_kwin_config ;;
    login-config) cmd_login_config ;;
    logs) cmd_logs "$@" ;;
    status) cmd_status ;;
    *) die "usage : bash vm/vm.sh <create|up|down|reset|ssh|deploy|kwin-config|login-config|logs|status>" ;;
  esac
}
main "$@"
