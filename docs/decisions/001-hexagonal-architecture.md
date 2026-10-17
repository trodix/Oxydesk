# 001 — Hexagonal architecture of RustyDesktop (Cargo workspace)

Status: accepted (M0)

- `crates/core`: domain + ports (traits). Light dependencies only.
- The other crates are adapters that depend only on `core`, never on each other (checked by `ci/check-deps.sh`). `rdsktp`
  (the primary adapter) and the `bins/*` assemble them. The `*-proto` crates are wire contracts and are exempt from the rule
  (see ADR 008).
- Technical back ends are Cargo features; the test adapters (synthetic capture, in-memory clipboard, ...) implement the same
  ports.
- A port is created only if it has at least two implementations (real or test).
- Security boundary = crate boundary: only `rdsktp-authd` depends on PAM.
- Logic that needs no I/O lives in pure functions (or in `core`) so that it is tested without a compositor, a socket or a VM
  (for example `bins/rdsktp-agent/src/{settings,convert}.rs`).

IronRDP version/commit pinned in M1: see ADR 002.
