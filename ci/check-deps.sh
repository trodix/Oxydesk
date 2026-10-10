#!/usr/bin/env bash
# Règle hexagonale : un crate adaptateur ne dépend que de rdsktp-core, jamais d'un autre crate rdsktp-*.
# `rdsktp-rdp` (assemblage) et les bins sont exemptés.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
for dir in crates/*/; do
  name="$(basename "$dir")"
  case "$name" in core|rdp) continue ;; esac
  if grep -E '^rdsktp-[a-z-]+' "$dir/Cargo.toml" | grep -v '^rdsktp-core' >/dev/null; then
    echo "violation : $name dépend d'un autre adaptateur" >&2; fail=1
  fi
done
if grep -E '^(tokio|pipewire|ironrdp|x264|pam)' crates/core/Cargo.toml >/dev/null; then
  echo "violation : core dépend d'une techno lourde" >&2; fail=1
fi
exit $fail
