#!/usr/bin/env bash
# Hexagonal rule: an adapter crate only depends on rdsktp-core, never on another rdsktp-* crate.
# `rdsktp-rdp` (assembly) and the bins are exempt. The `*-proto` crates are protocol contracts
# (no logic, no rdsktp dependency) that adapters may share.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
for dir in crates/*/; do
  name="$(basename "$dir")"
  case "$name" in core|rdp) continue ;; esac
  if grep -E '^rdsktp-[a-z-]+' "$dir/Cargo.toml" | grep -Ev '^rdsktp-(core|[a-z]+-proto)\b' >/dev/null; then
    echo "violation: $name depends on another adapter" >&2; fail=1
  fi
done
if grep -E '^(tokio|pipewire|ironrdp|x264|pam)' crates/core/Cargo.toml >/dev/null; then
  echo "violation: core depends on a heavy technology" >&2; fail=1
fi
for proto in crates/*-proto; do
  if grep -E '^rdsktp-' "$proto/Cargo.toml" | grep -v '^name' | grep -v '^description' >/dev/null; then
    echo "violation: $(basename "$proto") must not depend on any rdsktp crate" >&2; fail=1
  fi
done
# Only `authd` (privileged) loads PAM.
if grep -rEl 'libpam|pam_start' crates bins --include=*.rs 2>/dev/null | grep -v '^bins/authd/' >/dev/null; then
  echo "violation: PAM used outside bins/authd" >&2; fail=1
fi
exit $fail
