# ADR 018 — Login screen for a client without a password

Date: 2026-10-18. Complements ADR 008 (login mode) and ADR 016 (automatic reconnection).

## Problem
Login mode is TLS only: the credentials are those of the Client Info PDU. `mstsc` without NLA does not ask for a password, so a
connection without saved credentials arrives with an empty one and was refused, with no way to type it.

## Decision
1. **A connection with an empty password is accepted into a login screen** (like xrdp's), with the user name the client sent
   already filled in and the cursor on the password; without a name the cursor starts on the name. Enter moves on, then submits;
   Tab and the arrows change field, Escape clears the password.
2. **Hexagonal split.** `oxydesk_core::greeter` is the pure form (state, key handling) and `oxydesk_core::keymap` turns the
   scancodes into characters for the client's layout (US, GB, FR, DE tables, others type as US; dead keys type nothing).
   `crates/rdp/src/greeter.rs` draws it (anti-aliased vector text with `ab_glyph`, Noto Sans subset embedded from `crates/rdp/assets/`, license in `NotoSans-OFL.txt`; rounded shapes by distance field) and presents it as the capture, input and clipboard of a desktop, so the pump,
   the EGFX/H.264 path and the monitor layout work unchanged. The encoder comes from the composition root
   (`ServerConfig::greeter_encoder`, openh264 in `oxydesk`).
3. **Same login path.** The password typed goes through `LoginCore`, shared with the credentials of the Client Info: failure
   limits per address and per account first, then `SessionBroker::login`. Blocked: the broker is not asked and the screen says so.
   On success the keyboard layout announced at connection and the monitors of the client are handed to the real session, which
   replaces the screen.
4. **A client that never signs in must not hold the server** (one connection at a time): after 2 minutes on the screen the
   connection is ended.
5. **Reconnection cookie.** The cookie cannot be handed over at connection (the user is not known, and one issued for a name that
   was not proven would let anyone resume that user's session). Once the typed login succeeded, the server sends it through a
   patched `ServerEvent::ReconnectCookie` of `third_party/ironrdp-server`; a cookie still queued when the connection ended is
   dropped, never given to the next client.
6. **Pointer.** The screen offers an arrow cursor (the client draws it itself, like a desktop pointer).
7. **Limits.** No clipboard, drives, audio or printers before the login.
