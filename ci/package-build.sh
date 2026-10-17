#!/usr/bin/env bash
# Builds the release binaries and one package: deb (Debian 13), rpm (Fedora / RHEL / Rocky) or tar (generic tar.gz).
# Meant to run as root inside a clean container of the matching family (see .github/workflows/package.yml and ci/package-test.sh):
#   deb: debian:trixie     rpm and tar: rockylinux:9 (oldest glibc supported: the result runs on Rocky 9/10 and recent Fedora)
# usage: ci/package-build.sh deb|rpm|tar [out-dir]       env: SKIP_DEPS=1 (toolchain and libraries already installed)
set -euo pipefail
cd "$(dirname "$0")/.."
FORMAT="${1:?usage: package-build.sh deb|rpm|tar [out-dir]}"
OUT="$(realpath -m "${2:-dist-out}")"
ARCH="$(uname -m)"          # x86_64 | aarch64
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' Cargo.toml | head -1)"
mkdir -p "$OUT"

if [[ -z "${SKIP_DEPS:-}" ]]; then
  if command -v apt-get >/dev/null; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends build-essential pkg-config clang libclang-dev \
      libpipewire-0.3-dev curl ca-certificates git >/dev/null
  else
    dnf -y -q install --enablerepo=crb gcc gcc-c++ make pkgconf-pkg-config clang clang-devel pipewire-devel git ca-certificates \
      rpm-build tar gzip >/dev/null
    command -v curl >/dev/null || dnf -y -q install curl >/dev/null
  fi
  command -v cargo >/dev/null || { curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1; }
fi
[[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release --locked -p oxydesk-testapp -p oxydesk-authd -p oxydesk-cups-backend
cargo build --release --locked -p oxydesk --features kwin
cargo build --release --locked -p oxydesk-agent --features kwin

case "$FORMAT" in
  deb)
    command -v cargo-deb >/dev/null || cargo install cargo-deb --locked >/dev/null 2>&1
    cargo deb --no-build -p oxydesk --output "$OUT/" >/dev/null
    ;;
  rpm)
    command -v cargo-generate-rpm >/dev/null || cargo install cargo-generate-rpm --locked >/dev/null 2>&1
    cargo generate-rpm -p bins/oxydesk -o "$OUT/"
    ;;
  tar)
    NAME="oxydesk-$VERSION-linux-$ARCH"
    STAGE="$(mktemp -d)/$NAME"
    install -d "$STAGE/bin" "$STAGE/lib/oxydesk" "$STAGE/lib/cups-backend" "$STAGE/systemd" "$STAGE/pam" "$STAGE/share"
    for b in oxydesk oxydesk-authd oxydesk-agent; do install -m755 "target/release/$b" "$STAGE/bin/"; done
    install -m755 target/release/oxydesk-testapp "$STAGE/lib/oxydesk/"
    install -m700 target/release/oxydesk-cups-backend "$STAGE/lib/cups-backend/oxydesk"
    install -m644 dist/oxydesk.service dist/oxydesk-authd.service dist/oxydesk-agent@.service "$STAGE/systemd/"
    install -d "$STAGE/pam/debian" "$STAGE/pam/rpm"
    install -m644 dist/pam/oxydesk dist/pam/oxydesk-session "$STAGE/pam/debian/"
    install -m644 dist/pam/rpm/oxydesk dist/pam/rpm/oxydesk-session "$STAGE/pam/rpm/"
    install -m644 dist/oxydesk.desktop dist/oxydesk-agent.desktop dist/sysusers/oxydesk.conf "$STAGE/share/"
    install -m644 dist/oxydesk.conf "$STAGE/share/oxydesk-server.conf"
    install -m755 dist/tar/install.sh "$STAGE/install.sh"
    install -m755 dist/tar/uninstall.sh "$STAGE/uninstall.sh"
    install -m644 README.md LICENSE "$STAGE/"
    tar -C "$(dirname "$STAGE")" -czf "$OUT/$NAME.tar.gz" "$NAME"
    ;;
  *) echo "unknown format: $FORMAT" >&2; exit 2 ;;
esac
( cd "$OUT" && sha256sum $(ls | grep -v SHA256SUMS) > SHA256SUMS-$FORMAT-$ARCH.txt )
ls -l "$OUT"
