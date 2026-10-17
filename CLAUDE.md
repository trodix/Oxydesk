# RustyDesktop: working rules

## Language

Everything written in the repository (README, ADRs, code comments, scripts, help and log messages) is in **English**.

## Architecture

Hexagonal (see `docs/decisions/001-hexagonal-architecture.md`): `crates/core` holds the domain and the ports; the other
crates are adapters that depend only on `core` (checked by `ci/check-deps.sh`); `rdsktp` and the `bins/*` assemble them.
Logic that can be decided without I/O belongs in pure functions or in `core`, so that it can be tested without a compositor,
a socket or a VM.

## Tests and coverage

A percentage measures the code **executed** by the tests, not the quality of their assertions. The real question is: which
important behaviours still have no test?

| Kind of code | Reference |
|---|---|
| Business logic, calculations, validation rules | 90–100 % |
| Parsers, protocols, error handling | 90–100 %, looking at branches too |
| The application as a whole | 75–85 % (global reference: 80 %) |
| Glue, start-up, external services | according to risk: targeted integration tests |

- The global 80 % is a reference, not a gate: it never blocks a useful fix over a few tenths of a point.
- Critical behaviours need tests for success, errors, limits and resumption (disconnection during a transfer, malformed
  packet, reconnection after a cut).
- Every bug fix comes with a test that reproduces it, when practicable.
- Uncovered zones are reviewed so that the decision to leave them untested is conscious.
- Line coverage (was this line executed?) and branch coverage (was each outcome of a condition exercised?): for code full of
  conditions, branches say more. Even 100 % guarantees neither the combinations nor the boundary values.
- Every feature is also tested in `--login` mode on the Debian VM (`cargo xtask vm-test --session login`), in addition to the
  unit, synthetic and client tests.

Measuring: `ci/coverage.sh` (see the README). Third-party patched crates (`third_party/`) are not workspace members, so tests of
patched code live in `crates/rdp`. Fakes used by the existing tests: an in-memory RDPDR client answering through
`DriveBackend`, a scripted `CaptureSource`, a recording `SendFn` for CLIPRDR, a fake agent over `UnixStream::pair()`, and the
synthetic desktop (`--session synthetic`).

Zones that `cargo test` cannot reach and that the VM scenarios (mstsc, FreeRDP, login mode) cover instead: PAM
(`bins/authd/src/pam.rs`), the Wayland clipboard, PulseAudio capture, DMA-BUF import, the real KWin start-up
(`crates/session`, `--features kwin`), `bins/rdsktp-testapp`, and the `main` functions.
