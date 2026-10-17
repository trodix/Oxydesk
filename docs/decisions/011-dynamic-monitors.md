# ADR 011 — Multi-monitor with a dynamic number of monitors

Date: 2026-10-10. Complements ADR 006 (multi-monitor EGFX) and ADR 010 (validation).

## 1. Domain
- `Layout::from_monitors(&[MonitorSpec])` normalises a client layout: even sizes, at most 16 monitors, primary monitor as output 0, the others sorted by (x, y), bounding rectangle moved back to the origin (RDP requirement). `OutputId` = rank.
- `CaptureSource::apply_layout`, `place` and `retire` (ports); encoder factory `make_encoder`: the number of outputs can change during a session. The pump (`crates/rdp/src/pump.rs`) creates or destroys encoders, resets EGFX (`ResetGraphics` + surfaces) and triggers a deactivation-reactivation with the bounding size.

## 2. Local IronRDP patch (`third_party/`)
ADR 006 planned not to patch IronRDP. That is no longer tenable: `ironrdp-acceptor` always announced **one** monitor covering the whole desktop in the Monitor Layout PDU, so mstsc (full screen, `use multimon:i:1`) only used its first screen. `ironrdp-server` did not expose the monitors of the client GCC either.
- `third_party/ironrdp-acceptor` and `third_party/ironrdp-server` (0.10 / 0.13) are vendored, declared in `[patch.crates-io]` and excluded from the workspace for linting.
- `Acceptor::set_monitors`: list announced in the Monitor Layout PDU (resumed after reactivation). `RdpServerDisplay::monitors()`.
- `MonitorPolicy` (`set_monitor_policy`): called when the GCC is read, **before** Demand Active, with the client's monitors; it returns the desktop size and the announced list. The server thus adopts the client's layout without a reactivation (a reactivation right after connecting left white screens in mstsc).
- To be reviewed at every IronRDP version bump; ideally both points would be proposed upstream.

## 3. KWin
- `MultiCapture::apply_layout`: adds outputs (factory), removes the last ones (`retire` then drop), resizes all outputs then positions them (`kscreen-doctor output.<name>.position.X,Y`, checked on the `Geometry:` line). `resize` with several outputs goes through `apply_layout` with one output.
- Added outputs: `zkde_screencast.stream_virtual_output` (KWin 6.7, protocol ≥ 2). KWin prefixes the requested name with `Virtual-`; the output disappears with its stream. Start-up outputs (`--output-count`) cannot be destroyed: `retire` disables them (`kscreen-doctor output.X.disable`), `start_named` re-enables them.
- Selection by name (`wl_output.name`) in addition to the left-to-right rank at start-up.
- KWin 6.3: neither custom modes nor virtual outputs. Scaling (`oxydesk-scaling`) stays single-output here unless extended per output (section 6); beyond `--outputs`, the layout is refused and logged.
- Limit observed on a **static** desktop: KWin only sends a frame on damage. Going from an 800×600 output back to 1024×576 (original size) may produce no frame as long as nothing moves. The pump keeps the last frame of an output whose size does not change; the rest is not fixed (real desktops animate the screen).

## 4. Tests
- IronRDP client (`rdp-test-client --multi-resize`): multi-monitor Display Control + client-side reactivation. Steps 2 → 3 → 1 → 2 outputs then back to the configured layout; checks the desktop size and the identity blocks of the synthetic desktop (`--no-identity-blocks` on a real desktop). Mock included. Results: synthetic 14/14, KWin 6.7.5 on the host 5/5 (desktop animated by `oxydesk-testapp`).
- Real mstsc, 2 monitors (Windows 11 VM with two QXL, `bash vm/win11.sh drivers` once, then `WIN_MONITORS=2 cargo xtask test --client`, with `WIN_SESSION=kwin` for KWin; `ci/mstsc-multimon-test.sh` for login mode): connection, second output on the second monitor, layout following the client's monitors.
- Not testable with mstsc: changing the resolution of a local monitor in multimon makes mstsc leave full screen, without sending a layout. Dynamic multi-monitor resizing is therefore covered by the IronRDP client.
- `cargo xtask vm-test --multi-resize --outputs N` runs the scenario against the server deployed in the Debian VM.

## 5. Agent and `--login` mode
- Agent protocol: `SetLayout` (tag 10, list of rectangles) and `LayoutApplied` (tag 7). `AgentLink::set_layout` waits for the answer in the reader thread; video mailboxes are per output and of variable number.
- On the agent side, a `round` lock guarantees that no frame at the old layout is emitted after the answer; encoders are recreated dynamically. The agent session surviving disconnections, the link reconciles the number of outputs on connection.
- Integration test with the real agent binary: `bins/oxydesk-agent/tests/layout.rs`; the agent's `serve` loop is also tested over a socket pair on a synthetic desktop.
- Validated end to end: `cargo xtask vm-test --session login --outputs 2 --multi-resize` (Debian 13 VM, KWin 6.3), with `--max-monitors 2 --no-identity-blocks` added by xtask (no extra output, identity blocks scaled).

## 6. KWin 6.3: per-output scaling
When the source refuses a multi-output layout, `ScalingCapture` remembers the native size of each output (received frames) and sets each output to the requested size (black bars). Native outputs are assumed side by side from left to right; `ScalingInput` finds the pointer's output (the closest) and maps it back into native space. Beyond the number of known outputs, the layout is still refused.

## 7. Pump tests
`pump::tests`: growing/shrinking number of outputs (encoders created on demand), layout refused by the capture, extra outputs without an encoder factory.
