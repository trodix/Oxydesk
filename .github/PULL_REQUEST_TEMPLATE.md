## What and why

<!-- What does this change, and why? Link the issue or ADR (Fixes #123, docs/decisions/0xx-...). -->

## Bug fixes: how to reproduce (before the fix)

<!-- Delete this section if the PR is not a bug fix. -->

- **rdsktp version / commit**: <!-- `rdsktp --version`, `git rev-parse --short HEAD`, or the .deb version -->
- **Server OS and KWin version**: <!-- e.g. Debian 13, KWin 6.3.6 (`kwin_wayland --version`) -->
- **Session mode**: <!-- `--login` (PAM + agent), `--session kwin`, `--session synthetic` / `--test-mode` -->
- **Client and version**: <!-- mstsc (Windows build), xfreerdp 3.x, Remmina... -->
- **Hardware / virtualisation**: <!-- bare metal, QEMU, VMware (3D support on/off), GPU, encoder (openh264 / vaapi) -->
- **Steps**:
  1.
  2.
- **Expected / actual**:
- **Logs**: <!-- `journalctl -u rdsktp -u rdsktp-authd`, agent log, client log (trim secrets) -->

## Tests

- [ ] A test reproduces the bug / covers the feature (unit, synthetic or loopback), see "Tests and coverage" in `CLAUDE.md`
- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- [ ] Tested in `--login` mode on the Debian VM (`cargo xtask vm-test --session login`), or not applicable: <!-- why -->
- [ ] Tested with a real client when the change touches the protocol: <!-- mstsc / xfreerdp / Remmina, versions -->

## Documentation

- [ ] README / ADR updated when behaviour, options or limits changed
- [ ] Everything written in English
