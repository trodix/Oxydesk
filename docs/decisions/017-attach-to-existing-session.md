# ADR 017 — Joining the Plasma session the user already has (`session-policy`)

Date: 2026-10-18. Complements ADR 008 (login mode) and ADR 004 (KWin authorization of capture and input).

## Problem
In `login` mode every login ends in a private virtual KWin owned by `oxydesk-agent@<user>`. The desktop that is open on the
physical screen of the same user can therefore not be reached from an RDP client.

## Decision
1. **A server option, `session-policy = new | attach | auto`** (default `auto`; `new` is the previous behaviour). `attach` refuses a login
   when the user has no Plasma session; `auto` joins it if there is one and starts a session of our own otherwise. The password
   is still checked by PAM.
2. **`oxydesk-authd` decides**, being root and the only component that can look at other processes. After PAM it looks for a
   `kwin_wayland` of the user that names its `--socket`, is not `--virtual` (ours always are) and whose socket exists in
   `/run/user/<uid>`. `plan(policy, existing)` is a pure function (start, attach to this socket, refuse). A user whose agent
   already runs keeps that session whatever the policy. The policy travels in `Request::Login` (`#[serde(default)]`, so an old
   `oxydesk` still works); the choice reaches the unit through `/run/oxydesk/agent-<user>.env`
   (`OXYDESK_AGENT_SESSION=--session attach --kwin-socket NAME`).
3. **The agent has a third session kind, `attach`.** Nothing is started and so nothing is stopped with the agent. Every enabled
   monitor of the compositor is captured (`kscreen-doctor -o`, left to right) through `ExistingOutput`, which lets frames and
   cursor through and refuses everything that would act on a monitor (resize, disabling, moving, primary). The client's window
   sizes are reached by the scaling fallback already used for KWin 6.3. Input and clipboard connect to the same socket as before.
   The agent announces as many outputs as the desktop has; `AgentLink` then asks for the client's number through `SetLayout`.
4. **The client's monitors and the desktop's monitors need not match.**
   - The client has **more** monitors: the extra ones are virtual outputs (`stream_virtual_output`, KWin 6.7), created with their
     capture and gone with it. The real monitors are untouched.
   - The client has **fewer** monitors: the first ones (left to right) stay shown, and `ExistingDesktop` has the windows of the
     others brought onto them (`WindowMover`, implemented by a KWin script loaded on the session bus: every normal window or
     dialog found on a hidden output goes to the first shown one). The script stays loaded with the original places and a global
     shortcut.
   - **When the client leaves with `virtual-outputs = remove`**, the desktop is given back as it was: windows left on the client's extra (virtual) monitors are
     brought onto a real monitor, the virtual monitors are removed (the agent waits until KWin shows the real ones only), then
     the shortcut is invoked (`kglobalaccel`) and each window still on the monitor it was brought to goes back where it was
     (one the user moved elsewhere meanwhile is left alone); the script removes itself. The order matters: KWin remembers
     window places per set of monitors and re-applies them when the set changes, which undid a restore made before the
     virtual monitors were removed.
   - **`virtual-outputs = keep`** (the default; `remove` gives the behaviour above) skips the restore: the client's extra monitors stay, as virtual displays
     of the Plasma session, because the agent keeps their capture stream open (KWin removes a virtual output with its stream).
     They go when the agent stops.
   - **A virtual desktop stands for each extra monitor** (`Oxydesk Virtual-N`, created by KWin script on the session bus:
     `WindowMover::claim` / `park`). Desktops cannot be shown side by side, so they are the place where the windows of
     an extra monitor wait while the monitor is not there. When the client arrives with an extra monitor, the windows parked
     on its desktop (and only on it) come onto the monitor, on the current desktop, so a client with two monitors in front of
     a one-monitor session with a kept virtual display uses that display as its second screen. When the client leaves (or
     drops the monitor mid-session), the windows of the monitor go to the first real one and are parked on that desktop: with
     `keep` the desktop stays like the monitor (it is also what the user sees in System Settings > Virtual Desktops), with
     `remove` the desktop is removed with the monitor. This replaces "no window is moved" in `keep` mode: windows would
     otherwise sit on a monitor nobody looks at. The choice travels like the policy
     (`Request::Login.keep_virtual_outputs`, then `--virtual-outputs` in the unit's environment file) and is read when the
     session is joined.
5. **Sound** is the monitor of the default output (`parec -d @DEFAULT_MONITOR@`), without the private sink and without changing
   the default sink: the local speakers keep playing.
6. **The keyboard layout of the client is not written** into the user's `kxkbrc` (`OXYDESK_KEYBOARD_LAYOUT=off`).

## Limits
- Extra monitors for the client need KWin 6.7; with an older KWin the layout is refused and the client keeps the desktop's monitors.
- A panel or a widget on a hidden monitor is not moved. If the agent is killed rather than stopped, the windows stay where they were brought.
- The real monitors are never resized: the image is scaled, with bars when the aspect ratio differs.
- KWin must accept `oxydesk-agent` for `zkde_screencast_unstable_v1` and `org_kde_kwin_fake_input` (`oxydesk-agent.desktop`, ADR 004).
  A KWin that was running before the package was installed may need the user to log out and in once.
- A locked screen is captured as is; whether the password can be typed through RDP is to be checked on each Plasma version.
- One client at a time, as everywhere: the local user and the remote one share the same desktop.

## Verification
Unit tests (policy table, `/proc` lookup with a fake `/proc`, environment file, `ExistingOutput`, agent settings, `parec`
command, request round-trip, `ExistingDesktop` with a fake `WindowMover`, the gather script, the list of enabled outputs, scaling of an added output) and, against a private KWin with two outputs on a private D-Bus session and a real `konsole`
window, `bins/oxydesk-agent/tests/attach_kwin.rs` (skipped when KWin is not installed) and, by hand, mstsc against a physical session.

## A desktop without a monitor
With no real screen the client's monitors are virtual outputs of the agent (no `Oxydesk Virtual-N` desktop: they are the monitors,
not extra ones). The agent ends 60 s after the last client, and at once when a real screen is plugged in (it polls the outputs
every 2 s): the outputs go with it, KWin moves their windows onto the screens and the client reconnects to the desktop as it is.
Virtual desktops left over from an earlier run whose monitor no longer exists are removed (`WindowMover::sweep`) when the client
first sets its layout and when it leaves. With fewer physical screens than the client has (`virtual-outputs = keep`), the extra
monitors and their desktops stay, with their windows, until the agent stops.
