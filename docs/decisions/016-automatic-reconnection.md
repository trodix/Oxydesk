# ADR 016 — Automatic reconnection after a network drop

Date: 2026-10-17. Complements ADR 008 (login mode: the session belongs to the agent and survives disconnections).

## Problem
After a network drop, the KWin session was intact but the client had to retype its password, and might not even be accepted: the
server handles one connection at a time and a connection cut without a FIN stayed "open" (≈ 15 min of retransmissions), holding
the listener.

## Decision
1. **Auto-Reconnect cookie (MS-RDPBCGR 2.2.4.2 and 5.5).** After each accepted connection, the server sends in a Save Session
   Info PDU (Logon Info Extended) an identifier and 16 random bytes. On a network error, mstsc and FreeRDP (`+auto-reconnect`)
   replay it by themselves in the Client Info, with the proof `HMAC-MD5(secret, 32 zero bytes)` (TLS: no client random).
2. **In-memory registry** (`crates/rdp/src/reconnect.rs`): one cookie per user (a new login replaces the old one), single use
   (a new one is issued on resumption), bound to the client address, valid `reconnect-timeout` (default 10 s, 0 = disabled)
   **counted from the loss**, not from the login. A deliberate disconnection (connection ended without error) forgets it. The
   proof is compared in constant time (`hmac::verify_slice`).
3. **Attach without a password**: `SessionBroker::attach` → `Attach` request to `oxydesk-authd`, which grants it only for an
   account **it authenticated through PAM itself since it started**, above the minimum uid, whose agent is running; it never
   starts a session. A compromised `oxydesk` therefore cannot join a session it did not see opened.
4. **A client retrying without a valid cookie** sends an empty password: refused without counting as a login failure (otherwise
   mstsc's ~20 attempts would lock its address out).
5. **Free the listener quickly**: TCP keepalive and `TCP_USER_TIMEOUT` on accepted connections (`dead-peer-timeout`, default
   8 s) so the drop is noticed before the window ends.
6. **Audio**: packets produced for the old connection while it was frozen are dropped when the next one is accepted, and an audio
   packet sent before the RDPSND negotiation is ignored instead of ending the connection (observed symptom: the resumption was
   accepted then immediately closed, "client format not yet received").

Both settings are in `/etc/oxydesk/oxydesk.conf` (`reconnect-timeout`, `dead-peer-timeout`) and available as options.

## Limits
- Windows are kept, but the monitor layout is that of the returning client.
- The client address must stay the same; otherwise the password is asked again.
- The `third_party/ironrdp-server` patch (cookie, keepalive, audio) has no unit test in the workspace: it is validated by the
  real drop described below.

## Verification
Unit: registry (window, single use, address, proof, replacement, deliberate disconnection), validator (cookie → resumption,
deliberate end, no session to join, empty passwords, disabling), `authd` attach decision, keepalive.
Real (Debian VM, FreeRDP `+auto-reconnect`): the client's flow is cut in the VM with `iptables` and reset on the client side; the
server notices the loss after ≈ 8 s, FreeRDP reconnects by itself, `authd` logs "session rejoined", the same `kwin_wayland`
keeps running (same PID) and frames resume. **mstsc** (Windows 11 VM, ActiveX control): same cut (flow reset by a proxy + packets
of the old flow dropped in the VM); mstsc reconnects by itself, without a password, `kwin_wayland` unchanged
(`ci/mstsc-reconnect-test.sh`).
