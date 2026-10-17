#!/usr/bin/env bash
# Runs ci/package-build.sh in a clean container (copy of the working tree without target/, build cache in volumes).
# usage: ci/package-container.sh deb|rpm|tar [out-dir]       env: ENGINE (podman|docker), IMAGE (override)
set -euo pipefail
cd "$(dirname "$0")/.."
FORMAT="${1:?deb|rpm|tar}"; OUT="$(realpath -m "${2:-dist-out}")"; mkdir -p "$OUT"
ENGINE="${ENGINE:-$(command -v podman || command -v docker)}"
case "$FORMAT" in
  deb) IMAGE="${IMAGE:-docker.io/library/debian:trixie}"; KEY=debian ;;
  *)   IMAGE="${IMAGE:-docker.io/library/rockylinux:9}"; KEY=rocky ;;
esac
ARCH="$(uname -m)"
"$ENGINE" run --rm --security-opt label=disable -v "$PWD":/src:ro -v "$OUT":/out \
  -v "oxydesk-target-$KEY-$ARCH":/work/target -v "oxydesk-cargo-$KEY-$ARCH":/root/.cargo -v "oxydesk-rustup-$KEY-$ARCH":/root/.rustup \
  "$IMAGE" bash -c '
    set -euo pipefail
    cd /src && tar --exclude=./target --exclude=./artifacts --exclude=./dist-out --exclude=./.git -cf - . | tar -C /work -xf -
    cd /work && ci/package-build.sh '"$FORMAT"' /out'
