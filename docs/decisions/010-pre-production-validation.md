# ADR 010 — Validation before production (without the production VMware VM)

Date: 2026-10-10 (sections updated up to 2026-10-17). Scope: everything that could be validated without the production VMware
VM.

## 1. Build and CI on Debian 13 (container `ci/Dockerfile.trixie`)
- Run in the trixie container: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test
  --workspace`, `ci/check-deps.sh`, builds with the `kwin` and `vaapi` features (bindgen of `cros-libva` against the trixie
  `libva-dev` headers), `cargo deb -p oxydesk`.
- Fixes that were needed: `libva-dev` added to the image; `cargo fmt --all` had never been applied (≈39 files); clippy lints
  (`chunks_exact_to_as_chunks`, unused variables outside the `kwin` feature, `field_reassign_with_default`, child process never
  waited for in `oxydesk-testapp`); `exclude = ["third_party"]` in the workspace (the vendored code was linted as a member).
- CI job `features` added (`.github/workflows/ci.yml`), and later a `coverage` job (reference, never a gate; see the README and
  CLAUDE.md). The `vaapi` feature stays off by default in the `.deb` (no documented `libva2` dependency while it is not enabled).

## 2. Real clients and reconnection
- **Graphical xfreerdp 3.32.1** (`/gfx:AVC420 /sound /clipboard +auto-reconnect`) on the QEMU VM in `kwin` session: connection,
  EGFX V8_1 with AVC420, KWin desktop displayed (screenshot in `artifacts/`, not versioned), RDPSND audio open, 693 frames sent in
  30 s of which 1 dropped. Not exercised by hand: interactive typing, `/multimon`, resizing (covered only by the test client at
  the time; see §4 quinquies and ADR 011).
- **Real mstsc.exe (Windows 11 Pro, build 26300, QEMU VM `vm/win11.sh`)** against the server in `--login` mode (Debian 13 + KWin,
  PAM): TLS connection, PAM authentication (`session granted ... user=dev`), EGFX V10, KWin desktop displayed, keyboard input
  proven (three keys sent in the mstsc session change the marker of `oxydesk-testapp`, colour read on the capture). Clipboard,
  audio, resizing and multi-monitor with mstsc: see §4 quater and ADR 011.
- **KRDC**: not run (uses FreeRDP; no difference expected but not verified).
- **`reconnect` scenario** (test client, 8th check): abrupt loss of the link (socket closed without a disconnect PDU) then
  reconnection, then clean disconnection and reconnection; each time a non-black desktop of the right size must come back. Mock
  with a `no_reconnect` fault (test: only `reconnect` fails). Real result: synthetic 3/3 and KWin 8/8, reconnection in ≈ 20 ms.
- **Bug found and fixed by this scenario**: `ironrdp-server` uses **a single event channel for all connections**. The
  `audio-pump` thread of a dead connection kept writing packets into it, which reached the next connection before the RDPSND
  negotiation → `failed to send rdpsnd event ... client format not yet received`, reconnection cut. Fix
  (`crates/rdp/src/sound.rs`): sender captured when the stream starts, pump stopped on the handler's `Drop`, flag rechecked after
  the blocking read before sending.
- **Second connection during the first**: not refused, it stays in the TCP backlog and is only served when the first ends
  (measured: 17 s of waiting with no feedback to the client). Behaviour to document for users; a real policy (refusal or
  replacement) remains to be decided. A cut connection no longer holds the listener for long since ADR 016 (dead-peer
  detection).
- Harmless noise: `Illegal SNI extension` (client connected by IP).

## 3. NLA
xfreerdp measurements against the TLS-only server (`/auth-only`):

| Client option | Result |
|---|---|
| default | success (falls back to TLS) |
| `/sec:tls` | success |
| `/sec:nla` | failure: `Protocol Security Negotiation Failure` (the server does not offer NLA) |
| `/sec:rdp` | failure: the server requires TLS |

- **mstsc 26300 (measured, against the server without NLA)**:

| `.rdp` file | Result |
|---|---|
| default (CredSSP offered) | **success**: the server chooses TLS, mstsc accepts the fallback and sends the credentials (self-signed certificate warning to accept) |
| `enablecredsspsupport:i:0` alone | failure: mstsc only offers protocol `0x0` (legacy RDP), the server requires TLS (`0x904`/`0x7`) |
| `negotiate security layer:i:1` + `enablecredsspsupport:i:0` | TLS established, but mstsc sends **no credentials** (`user=` empty): refused by PAM |

- IronRDP (`ironrdp-server` 0.13.0, latest stable): `CredentialValidator` is only called for TLS mode (credentials of the
  ClientInfoPdu, already used with PAM). For CredSSP/Hybrid, NTLM needs preloaded credentials: no delegation to PAM. No clear
  path, so no code (see ADR 008).
- **Production consequence (corrects the first version of this ADR)**: do **not** set `enablecredsspsupport:i:0`, which prevents
  mstsc from sending the credentials. The default mstsc falls back to TLS by itself and works; the self-signed certificate
  warning just has to be accepted (distribute the certificate or provide a trusted one). Not tested: a group policy imposing NLA
  on the client side, which should prevent that fallback.

## 4. VMware / vSphere (batch 4)
Not done: VMware Workstation is not installed on the machine. Information to collect on the production host: ESXi version,
virtual hardware version of the VM, "3D Support" enabled or not, "3D Renderer" (Automatic / Software / Hardware), allocated VRAM.
Reminder (ADR 004): KWin needs OpenGL and a render DRM node; without them the KWin session does not start.

## 4 bis. Windows test VM
`bash vm/win11.sh install` creates a Windows 11 VM (QEMU/KVM, OVMF Secure Boot, TPM 2.0 emulated by `swtpm`, sparse 80 GB disk
in `vm/state/win11`, git-ignored) from the ISO in the `iso/` folder (project root, git-ignored), with an unattended installation
(`vm/win11/autounattend.xml.tmpl`, throw-away local account with a random password). The data CD (`win11.sh data HOST:PORT USER`)
provides `target.rdp` and the credentials; a fixed script (`mstsc-test.ps1`) registers them and opens `mstsc.exe` at each logon.
There is no remote command agent: the first manual steps were done with the keyboard through the QEMU monitor (`sendkey`,
`screendump`). The QEMU mouse did not work.

## 4 ter. `--login` mode with a real password
`vm-test --session login` (throw-away account, random password): 8/8 on 2 runs out of 3. One run failed on `audio_tone` (ratio
0.38); not reproduced, cause unknown at the time (suspicion: Plasma's start-up sound mixed into the signal).

**Update 2026-10-17 — cause found.** `vm/vm.sh` had been launching the test application without `--tone` since the multi-monitor
change (commit `c529dd7`, "no sound" for manual sessions), so the `audio_tone` check measured silence (ratio 0.00, continuity 1.00)
in every `vm-test` run in `kwin` and `login` mode. `session_test_app` now adds `--tone` when `VM_TONE=1`, which `cargo xtask
vm-test` sets; a manual session stays silent. The intermittent ratio of 0.38 above is a separate, unexplained observation.

## 4 quater. Automatic Windows tests (no human, no LLM)
`RDP_TEST_PASSWORD=... bash vm/win11.sh test HOST:PORT USER` replays a fixed scenario in the Windows VM and returns an exit code
(0 = everything passes).
- **ISO**: if it is missing, `win11.sh install` downloads it into `iso/` (folder overridable with `WIN11_ISO_DIR`, file with
  `WIN11_ISO`) from `https://public.trodix.com/iso/windows/` (variable `WIN11_ISO_URL`) and refuses any file whose SHA-256 differs
  from that of the tested ISO.
- **Engine**: Microsoft's RDP ActiveX control (`mstscax.dll`, the engine of `mstsc.exe`) hosted in a WinForms form by
  `vm/win11/rdp-activex-test.ps1`: no confirmation window, password injected through the non-scriptable COM interface, default
  CredSSP policy (TLS fallback). No COM events under PowerShell: the state is read by polling (`Connected`).
- **Without reinstalling or rebooting**: `win11.sh snapshot` starts Windows once on a qcow2 overlay of the base disk (shut down
  cleanly), waits for the script to write `READY` on the serial port, then `savevm ready`. Each `test` does `-loadvm ready`,
  replaces the data CD (`change dat`, parameters only) and the script, which was polling a `params.txt`, starts at once. The
  snapshot is rebuilt automatically if the script or `win11.sh` change. For QEMU, the disk is AHCI (NVMe blocks `savevm`) and the
  OVMF NVRAM is qcow2. Measured duration: 24 s per test (38 s with a Windows boot), including about 8 s of deliberate waits.
- **Launching**: `cargo xtask test` only runs the unit tests; `cargo xtask test --client` adds this client test (`--server`:
  server test on the Debian 13 VM; `--integration`: both, independent) (compiles and starts a local `--test-mode` server, calls
  `win11.sh test`, checks the prerequisites and stops with the list of what is missing). It is not in the CI (no KVM nor ISO).
- **Results**: the script writes one JSON line per check on COM1 (`-serial file`), read by the host.
- **Checks** (8/8 + 2 host-side against `--test-mode`): `connect`, `heartbeat` (real pixels read on the guest screen),
  `clipboard_roundtrip` and `clipboard_roundtrip_large` (23 and 65,556 characters, € / emoji / CJK; the server's echo is detected
  by the clipboard sequence number, then the text is read back from a child process), `guest_audio_device`,
  `still_connected_after_audio`, `resize_request_survives`, `desktop_resized`, `disconnect`. Host side: QEMU records the guest's
  audio output (`wav` back end, 48 kHz) and `rdp-test-client --analyze-wav` (Goertzel) checks the 440 Hz; a local 1000 Hz
  reference sound proves that the recording chain works. Later additions: drives, printers, file clipboard, reconnection
  (`ci/mstsc-reconnect-test.sh`, ADR 016), multi-monitor (`ci/mstsc-multimon-test.sh`).
- **Bug found and fixed**: with the 48 kHz format first, mstsc played the sound at 405 Hz (pitch × 44.1/48) on a 44.1 kHz device;
  at 44.1 kHz the 440 Hz is exact. `crates/rdp/src/sound.rs` now offers 44.1 kHz first. The test would have given a false positive
  without the separate reference sound (1000 Hz), hence its distinct frequency.

## 5. Left for production
Test on the real VMware VM (KWin without GPU, `vmwgfx`/llvmpipe); mstsc with a group policy imposing NLA; KRDC; policy for a
concurrent connection; NVENC; USB redirection.

## 4 quinquies. Dynamic resizing (Display Control)

- **Path**: `RdpServerDisplay::request_layout` (primary monitor, size adjusted to even) → `Shared.resize` → the pump calls
  `CaptureSource::resize` (new port, `ResizeUnsupported` by default), recomputes the `Layout`, sends `DisplayUpdate::Resize`
  (deactivation-reactivation) and **waits** for the client loop to restart before announcing `ResetGraphics` + the new EGFX
  surfaces (otherwise mstsc discards the announcement, measured). `AutoEncoder` recreates its codec when the frame size changes.
  `Display::size()` returns the current size, so a reconnection restarts at the right size.
- **Covered**:
  - synthetic desktop (`--test-mode`): `cargo xtask test --client`, mstsc check `desktop_resized` (a magenta block drawn at the
    bottom-right corner of the desktop must appear at the right position, the check centring a desktop smaller than its window);
  - **real KWin 6.7** (Fedora machine): `WIN_SESSION=kwin cargo xtask test --client` starts `oxydesk --session kwin` with
    `oxydesk-testapp` (which redraws its window and its magenta block at the new size); `desktop_resized` passes, the virtual output
    gets a custom mode (`kscreen-doctor output.Virtual-0.addCustomMode...` then `mode`, then the geometry is read back because
    `kscreen-doctor` returns 0 even on failure). A `.desktop` authorising `target/debug/oxydesk` and `oxydesk-testapp` is needed
    (`X-KDE-Wayland-Interfaces`, see ADR 004); the `.deb` now depends on `libkscreen-bin`;
  - **login mode**: agent protocol `Resize` / `Resized` (round-trip test and test of the `AgentLink::resize` link with a fake
    agent: answer, refusal, silent agent), `MultiCapture` delegates when there is a single output;
  - mstsc of the Windows VM against the **login server of the Debian 13 VM** (`vm-test --session login --keep`, then
    `RDP_TEST_PASSWORD=... WIN_EXPECT_RESIZE=0 bash vm/win11.sh test 10.0.2.2:3390 dev`, the Windows VM reaching the Debian VM's
    `hostfwd` through 10.0.2.2): connection, image, clipboard, audio pass (8/8).
- **Measured limit: KWin 6.3.6 (Debian 13) cannot resize its `--virtual` back end**, by any route:
  - custom modes absent (`kscreen-doctor` 6.3: "Unable to parse arguments" for `addCustomMode`, protocol
    `kde_output_management_v2` v12 without `set_custom_mode`);
  - virtual outputs: `zkde_screencast.stream_virtual_output` is refused ("Could not find output",
    `QObject::connect(KWin::ScreencastStreamV1Interface, ...) invalid nullptr`) because the 6.3 virtual back end cannot create
    them. A virtual-output fallback was written then removed: it only worked on KWin 6.7 where custom modes already suffice
    (ADR 011 later uses `stream_virtual_output` for additional outputs on 6.7);
  - `trixie-backports` offers no newer KWin (6.3.6 only);
  - restarting KWin at the new size would destroy the user's applications; scaling on the server side would do what the client's
    smart sizing already does.
  Research: custom modes of the `--virtual` back end only arrive with Plasma 6.6 (KWin MR 8766, on top of MR 8534); KWin 6.6
  needs Qt 6.10 and KF 6.22 (Debian 13: Qt 6.8.2, KF 6.13), so no simple backport. All 6.3 `--virtual` outputs have the same size
  (`--output-count`), which rules out a set of outputs of different sizes.
  **Fallback chosen: scaling on the session side** (`crates/scaling`). `ScalingCapture` first tries the real resize; if refused,
  the desktop keeps its native size (1920x1080), each frame is fitted to the size the client asked for (aspect ratio kept, black
  bars, bilinear resampling, `ScaleMap` in `oxydesk-core`) and `ScalingInput` maps the mouse back. The fallback is installed in
  the agent (`--login` mode, where H.264 encoding is done in the agent) and in `oxydesk --session kwin`. Measured on the Debian 13
  VM (KWin 6.3.6) with mstsc: 9/9 without `WIN_EXPECT_RESIZE=0` (`desktop_resized`, `resize_request_survives`); agent log:
  `desktop 1920x1080 scaled to 1024x576`. On KWin 6.7.5 (host) the real resize still passes. Limits: resampled image (possible
  blur below a factor of 2), extra agent CPU, text less sharp than with a real resize. For a sharp rendering in production: KWin
  ≥ 6.6.
- **Clipboard on KWin 6.7.5**: the compositor no longer announces `zwlr_data_control_manager_v1` but `ext_data_control_manager_v1`
  (visible without authorisation). `oxydesk-clipboard` instantiates both protocols (same shape of requests and events,
  `data_control!` macro) and tries `ext` then `wlr` (KWin 6.3: `wlr`). Measured with the mstsc test: `clipboard_roundtrip` and
  `clipboard_roundtrip_large` pass both ways on the KWin 6.7.5 host and on the Debian 13 VM.
- **Size remembered by KWin**: KWin ≥ 6.6 keeps custom modes in `~/.config/kwinoutputconfig.json`; a later session therefore starts
  at the last size, not the configured one, and the server ignored all the frames (black screen). The pump now asks again for the
  configured size if the gap lasts more than a second, otherwise it adopts the compositor's size and announces it to the client.
- **Several monitors**: handled since ADR 011 (the request used to be ignored). Mouse mapping needs no change with a single
  output at the origin; DMA-BUF capture after a resize is untested.
