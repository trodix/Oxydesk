#!/usr/bin/env bash
# Installs the packages of a folder in clean containers, checks them (ci/package-check.sh), removes them and checks nothing remains.
#   .deb -> debian:trixie              .rpm -> fedora:latest, rockylinux:9, rockylinux:10        .tar.gz -> the three families
# usage: ci/package-test.sh [packages-dir]          env: ENGINE (podman|docker), ONLY=deb|rpm|tar
set -uo pipefail
cd "$(dirname "$0")/.."
DIR="$(realpath -m "${1:-dist-out}")"
ENGINE="${ENGINE:-$(command -v podman || command -v docker)}"
DEB=docker.io/library/debian:trixie; FED=registry.fedoraproject.org/fedora:latest
ROCKY9=docker.io/library/rockylinux:9; ROCKY10=quay.io/rockylinux/rockylinux:10
failed=0

# $1 image, $2 label, $3 shell snippet installing the package, $4 snippet removing it, $5 format given to the check
run() {
  local image="$1" label="$2" install="$3" remove="$4" fmt="$5"
  echo "=== $label"
  "$ENGINE" run --rm --security-opt label=disable -v "$DIR":/pkg:ro -v "$PWD/ci":/ci:ro "$image" bash -c "
    set -e
    $install
    set +e
    /ci/package-check.sh $fmt || exit 1
    $remove
    for f in /usr/bin/oxydesk /usr/bin/oxydesk-authd /usr/lib/systemd/system/oxydesk-authd.service /lib/systemd/system/oxydesk-authd.service /usr/lib/cups/backend/oxydesk; do
      [ ! -e \$f ] || { echo \"FAIL  \$f remains after removal\"; exit 1; }
    done
    echo 'PASS  removal leaves no binary, unit or backend'
  " || { echo "== $label: FAILED"; failed=1; }
}

DEBS_DEPS="apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends systemd binutils file libc-bin >/dev/null"
RPM_ROCKY="dnf -y -q install binutils systemd >/dev/null"
for t in deb rpm tar; do [ -z "${ONLY:-}" ] || [ "$ONLY" = "$t" ] || continue
  case $t in
  deb) for f in "$DIR"/*.deb; do [ -e "$f" ] || continue
         run $DEB "deb on Debian 13" "$DEBS_DEPS && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq /pkg/$(basename "$f") >/dev/null" \
           "DEBIAN_FRONTEND=noninteractive apt-get remove -y -qq oxydesk >/dev/null" deb; done ;;
  rpm) for f in "$DIR"/*.rpm; do [ -e "$f" ] || continue
         for img in $FED $ROCKY9 $ROCKY10; do
           run $img "rpm on ${img##*/}" "$RPM_ROCKY; dnf -y -q install /pkg/$(basename "$f") >/dev/null" "dnf -y -q remove oxydesk >/dev/null" rpm
         done; done ;;
  tar) for f in "$DIR"/*.tar.gz; do [ -e "$f" ] || continue
         b=$(basename "$f")
         run $DEB "tar.gz on Debian 13" "$DEBS_DEPS && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends fuse3 libopenh264-8 libpipewire-0.3-0t64 >/dev/null; mkdir /t && tar -C /t -xzf /pkg/$b && /t/*/install.sh >/dev/null && systemd-sysusers /usr/lib/sysusers.d/oxydesk.conf" "/t/*/uninstall.sh >/dev/null" tar
         for img in $FED $ROCKY9 $ROCKY10; do
           run $img "tar.gz on ${img##*/}" "$RPM_ROCKY; dnf -y -q install fuse3 pipewire-libs tar gzip >/dev/null; mkdir /t && tar -C /t -xzf /pkg/$b && /t/*/install.sh >/dev/null && systemd-sysusers /usr/lib/sysusers.d/oxydesk.conf" "/t/*/uninstall.sh >/dev/null" tar
         done; done ;;
  esac
done
[ $failed -eq 0 ] && echo "== packages: all passed" || { echo "== packages: FAILED"; exit 1; }
