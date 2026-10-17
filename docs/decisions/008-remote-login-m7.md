# ADR 008 — Remote login: authd + one agent per user (M7)

## Decision
Three processes, three identities:

| Process | Identity | Role |
|---|---|---|
| `rdsktp --login` | user `rdsktp` (sysusers), no privilege | RDP server (IronRDP), receives the credentials, assembles the ports |
| `rdsktp-authd` | root | PAM (`pam_authenticate` + `pam_acct_mgmt`), starts `rdsktp-agent@<user>.service`, passes a connected descriptor to the agent |
| `rdsktp-agent` | the logged-in user | KWin session, capture, **H.264 encoding**, input, clipboard, audio, drives, printers |

- **`SessionBroker` port** (core): `login(user, password) -> Result<DesktopPorts, LoginError>`; `attach` was added for the
  automatic reconnection (ADR 016). `DesktopPorts` groups the ports of a desktop. Adapter: `crates/remote` (`RemoteBroker`),
  whose ports are proxies to the agent (`AgentLink`). The direct desktop ports (`--test-mode`, autologin) are unchanged.
- **`rdsktp` has no privilege and no PAM dependency** (`ci/check-deps.sh`: PAM outside `bins/authd` is a violation). It never
  sees a user socket path: authd returns the descriptor through `SCM_RIGHTS`.
- **Protocols**: `authd-proto` (one request = one connection, JSON bounded to 8 KiB, password masked in `Debug`, user names
  validated `[a-z_][a-z0-9_-]{0,31}` before any use in a systemd unit name); `agent-proto` (binary frames
  `[tag][len u32][payload]`, payload bounded to 32 MiB). The `*-proto` crates are contracts, exempt from the "adapter → core
  only" rule.
- **Why encoding is done in the agent**: raw frames (1080p ≈ 8 MB) do not cross the socket; only H.264 (a few tens of KiB)
  does, and the encoder runs under the user's identity, closest to the capture.
- **Gapless resumption**: on the server side, `AgentLink` keeps a short queue (4 frames) per output, delivered in order: only
  "the latest" cannot be kept from an inter-dependent stream. A first attempt with a "last wins" mailbox failed: the jitter
  between the agent's pace and the pump's caused constant gaps, hence keyframes requested in a loop (key→pixel p95 ≈ 203 ms, and
  a broken stream with 2 monitors). Beyond 4 pending frames (EGFX backpressure) the queue is dropped and a keyframe is asked of
  the agent (`RequestKeyframe`); inter frames received meanwhile are ignored. A new EGFX client also triggers a request
  (`PassthroughEncoder::request_keyframe`). The pump never re-sends an inter frame that was already encoded.
- **Life cycle**: the KWin session belongs to the agent and survives RDP disconnections; a new connection of the agent
  replaces the previous one (reconnection). The pump of `rdsktp-rdp` is no longer stopped by a capture error: it waits for the
  next session.
- **Credentials**: IronRDP's `with_credential_validator` (async; the blocking login goes through `spawn_blocking`). Refusal →
  `Reject`; back-end error (authd unreachable, agent dead) → validator error, logged without the password. `authd` waits 1 s
  before any negative answer and answers identically for unknown account / wrong password / forbidden account (`--min-uid`,
  default 1000).
- **Agent unit**: template `rdsktp-agent@.service` (`User=%i`, `PAMName=rdsktp-session` for a real logind session →
  `XDG_RUNTIME_DIR` and session bus). `%U` cannot be used in that context: the agent derives `/run/user/<uid>` itself and waits
  for the bus before starting KWin. Socket: `<runtime>/rdsktp-agent.sock`, mode 0600. The agent leads its process group and ends
  it on SIGTERM (KWin, plasmashell and applications live in the logind scope, not the service's cgroup).
- **PAM through `dlopen`** (`libloading`, `libpam.so.0`): no headers nor `pam` crate on the development machine; the PAM
  conversation answers any hidden prompt with the password, the replies are allocated with `calloc`/`strdup` as PAM requires.
- **Testable core of the agent**: the command line (`settings.rs`) and the pure decisions of the connection loop (`convert.rs`)
  are separate modules; `serve` is tested against the synthetic desktop over a socket pair.

## IronRDP constraints
The validator is only called in **TLS mode** (credentials of the ClientInfoPdu). With CredSSP/NLA, NTLM needs preloaded
credentials: impossible for dynamic accounts. Consequences: `--login` is incompatible with `--nla-user`; a client that imposes
NLA (mstsc "require NLA") cannot connect. Credentials travel inside the TLS tunnel (self-signed certificate unless one is
provided in `--state-dir`). To reassess: NLA with a server-side validator if IronRDP ever exposes it (ADR 010 §3).

## Oracles
- Unit tests: `agent-proto` (round trip of every message, EOF, corrupted input), `authd-proto` (round trip, password absent
  from `Debug`, bounded sizes, usable passed descriptor, name validation), `remote` (gap → resynchronisation on a keyframe,
  gapless stream, inconsistent Hello, clipboard/audio/agent death), `xtask` (`--session login`), `rdsktp-agent` (settings,
  conversions, `serve` against the synthetic desktop).
- VM test `cargo xtask vm-test --session login` (account `dev`, password taken from `RDP_TEST_PASSWORD`, never an argument):
  the usual scenarios through the real PAM login, plus a **negative control** (`deny-control`): a random wrong password must
  make the `connect` check fail. The test application plays the 440 Hz tone only when the VM is configured with `VM_TONE=1`,
  which `vm-test` sets.

## Verified (Debian 13 VM, real PAM, account `dev`)
| Mode | Result | key→pixel p95 |
|---|---|---|
| `--session login` | 7/7 + negative control | 123.6 ms |
| `--session login --outputs 2` | 8/8 (multi_screen) + negative control | 101.7 ms |
| `--session kwin` (regression, direct path) | 7/7 | 68.5 ms |
| `--session synthetic` (regression) | 7/7 | 37.9 ms |

The login path costs ≈ 50 ms more: two 30 Hz paced loops in series (agent, pump) plus the socket hop. Audio with 2 monitors:
ratio 0.77 / continuity 0.91 (thresholds passed but lower than with 1 monitor: VM CPU load).

Bugs found by these tests (fixed): `ProtectHome=yes` on authd also hides `/run/user` (agent socket unreachable); KWin grants the
capture only to an executable with its `.desktop` file (ADR 004): `rdsktp-agent.desktop` added; `Requires=`/`After=` of the test
drop-in in the wrong section; the EGFX channel is created *before* the credentials are validated (do not reset it when the
session is installed); a "refusal" is observed on the client side by the absence of images (the RDP handshake succeeds before
validation).

## Latency optimisation (measured on the VM, p95 key→pixel, 60 samples, 2 passes)
Cause of the extra cost: two 30 Hz paced stages in series (agent, pump). Port `CaptureSource::set_waker` +
`core::wake::Wake`: the capture (KWin or remote agent) wakes the consumer on every frame.

| Scenario | p95 |
|---|---|
| direct, paced pump | 68 / 69 ms |
| direct, woken pump | 54 / 54 ms |
| login, paced agent + pump | 102 / 102 ms |
| login, woken pump only | 73 / 74 ms |
| login, woken agent without cap + woken pump | 88 / 60 ms (unstable) |
| **login, woken agent, 16 ms cap + woken pump (chosen)** | 60 / 66 ms |
| login, agent polled at 4 ms + woken pump | 66 / 66 ms |

Defaults kept: pump driven by the capture (`RDSKTP_PUMP=tick` goes back to the fixed pace), agent `--loop event
--min-interval-ms 16` (≤ 60 frames/s). Final check: kwin 53.7 ms; synthetic 26.0 ms; login 55.5 ms; login 2 monitors
77.9 / 74.2 / 69.5 ms, 7/7 (8/8 with 2 monitors). Audio with 2 monitors: one run at ratio 0.73 (threshold passed), the others at
1.00.

## Not verified / next
Production VMware, NLA, several simultaneous users (the RDP server handles one connection at a time), stopping the agent on
disconnection/inactivity (the session persists as long as the unit runs; automatic reconnection relies on it, ADR 016), SELinux
(Fedora ≠ Debian), systemd hardening of the agent (`ProtectSystem`... not applied so as not to break KWin), rate limiting beyond
the 1 s delay (login throttling per address and per account was added later), no dedicated audit log.
