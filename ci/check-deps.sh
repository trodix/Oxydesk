#!/usr/bin/env bash
# Règle hexagonale : un crate adaptateur ne dépend que de rdsktp-core, jamais d'un autre crate rdsktp-*.
# `rdsktp-rdp` (assemblage) et les bins sont exemptés. Les crates `*-proto` sont des contrats de protocole
# (sans logique, sans dépendance rdsktp) que les adaptateurs peuvent partager.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
for dir in crates/*/; do
  name="$(basename "$dir")"
  case "$name" in core|rdp) continue ;; esac
  if grep -E '^rdsktp-[a-z-]+' "$dir/Cargo.toml" | grep -Ev '^rdsktp-(core|[a-z]+-proto)\b' >/dev/null; then
    echo "violation : $name dépend d'un autre adaptateur" >&2; fail=1
  fi
done
if grep -E '^(tokio|pipewire|ironrdp|x264|pam)' crates/core/Cargo.toml >/dev/null; then
  echo "violation : core dépend d'une techno lourde" >&2; fail=1
fi
for proto in crates/*-proto; do
  if grep -E '^rdsktp-' "$proto/Cargo.toml" | grep -v '^name' | grep -v '^description' >/dev/null; then
    echo "violation : $(basename "$proto") ne doit dépendre d'aucun crate rdsktp" >&2; fail=1
  fi
done
# Seul `authd` (privilégié) charge PAM.
if grep -rEl 'libpam|pam_start' crates bins --include=*.rs 2>/dev/null | grep -v '^bins/authd/' >/dev/null; then
  echo "violation : PAM utilisé hors de bins/authd" >&2; fail=1
fi
exit $fail
