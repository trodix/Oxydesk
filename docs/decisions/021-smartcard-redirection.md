# ADR 021: smart card redirection (MS-RDPESC), the client's readers seen as PC/SC readers

## Context
An RDP client can share its smart card readers (mstsc: "Smart cards", xfreerdp: `/smartcard[:NAME]`, Remmina: "Share smart cards"). Goal: a card in the reader of the user's workstation is usable by the applications of the Linux session (OpenSC, `pkcs11-tool`, Firefox and Thunderbird through p11-kit, Java, `pcsc_scan`, French health insurance (Vitale) and ID card middleware...), as if the reader were plugged into the server.

The protocol is [MS-RDPESC], carried by the RDPDR channel of ADR 013:
- the client announces one `RDPDR_DTYP_SMARTCARD` device named `SCARD`, and the server must announce the smart card capability (`CAP_SMARTCARD_TYPE`) in its capability set, otherwise clients such as mstsc do not redirect anything;
- every operation is an `IRP_MJ_DEVICE_CONTROL` whose IOCTL is a `SCARD_IOCTL_*` (establish context, list readers, get status change, connect, transmit...). Input and output buffers are NDR (MS-RPCE type serialization 1): an 8-byte stream header, an 8-byte private header, pointer referents, 4-byte alignment;
- the client executes the call with its own winscard (PC/SC) and returns the result, so the server sees exactly the API of `SCardEstablishContext`, `SCardTransmit`...

On the Linux side the applications speak the **pcsc-lite client protocol** to a daemon over a Unix socket (`libpcsclite` connects to `/run/pcscd/pcscd.comm`, or to `$PCSCLITE_CSOCK_NAME`). The same shape as the smart card support of xrdp (`chansrv`), which is the model followed here.

## Decision

### Port (`crates/core/src/smartcard.rs`, hexagonal)
- `RemoteSmartcard` (blocking, like `RemoteDrive`): the winscard calls of the client's subsystem: `establish_context`, `release_context`, `list_readers`, `get_status_change`, `cancel`, `connect`, `reconnect`, `disconnect`, `begin_transaction`, `end_transaction`, `transmit`, `status`, `control`, `get_attrib`. Contexts and cards are opaque numbers (`ScContext`, `ScCard`) minted by the adapter of the protocol. No protocol dependency.
- The values (result codes, `SCARD_STATE_*`, protocols) are the winscard ones: MS-RDPESC and pcsc-lite share them, so they cross the port unchanged. The two exceptions are translated in the adapters that own them: the protocol `RAW` (0x10000 on the Windows side, 0x4 in pcsc-lite) and the card state numbering of `SCardStatus` (1..6 on Windows, bit flags in pcsc-lite).
- `SmartcardHost`: `attached(Arc<dyn RemoteSmartcard>)` / `detached()`, what the RDP adapter calls when the client announces or removes its device (like `DriveHost`). `SmartcardError` is `Code(u32)` (the client answered with that `SCARD_*` code) or `Link(String)` (the client is gone, which the PC/SC side reports as `SCARD_E_NO_SERVICE`). `DesktopPorts` gains `smartcards`.

### RDP adapter (`crates/rdp/src/smartcard/`)
- `ndr.rs`: the codec of the server direction, **pure** (bytes in, bytes out), because `ironrdp-rdpdr` only implements the client direction (it decodes the calls and encodes the answers). The calls are built here and the answers decoded here. It is checked against the client side of `ironrdp-rdpdr` (`reference_tests.rs`): every call built here must decode there, and every answer built there must decode here. Only the W (UTF-16) variants of the IOCTLs are used.
- `mod.rs`: `RdpSmartcard` implements `RemoteSmartcard` on top of the `Link` request/response tagging that drives and printers already use: an IOCTL is a `DeviceControl` IRP, its answer is the raw output buffer (`Body::Control`). A wait for a card (`get_status_change`) can last as long as the application allows, so the link call takes its own time limit (`call_with`) instead of the 30 s of the others; `cancel` is another IOCTL that reaches the client while the first is blocked, and the client ends the first one with `SCARD_E_CANCELLED`.
- The device is never opened: Windows servers send their IOCTLs to the device without a create, and the clients ignore the file id. (The first version sent a `Create` with an empty path like printers do; the IronRDP client does not handle a create on a smart card device, and the real clients do not need it.)
- `SmartcardRegistry` carries the host to the connections. In broker (`--login`) mode the device is announced **before** the user's session, and thus the host, exist: the registry remembers the device and attaches it when the host appears (same problem and same answer as drives and printers, see ADR 013).
- The server capability set now includes `CAP_SMARTCARD_TYPE` (`third_party/ironrdp-rdpdr`).

### Linux side (`crates/pcsc`, crate `oxydesk-pcsc`)
- `PcscServer` listens on a Unix socket (directory `0700`, socket `0600`, a stale socket replaced, a symlink refused; clients of another user are refused) and speaks the client/server protocol of pcsc-lite 4.x (`winscard_msg.h`): version negotiation, `SCardEstablishContext`..`SCardGetAttrib`, `CMD_GET_READERS_STATE` and `CMD_WAIT_READER_STATE_CHANGE` (the table of up to 16 readers that `libpcsclite` reads for `SCardGetStatusChange`), `SCardCancel` from another thread. It implements `SmartcardHost`: the readers exist while a device is attached, and every request becomes a call on `RemoteSmartcard`.
- The reader table, the wire format and the translation of states are pure functions (`table`, `wire`), tested without a socket. A scripted `fake` subsystem and the `fake_reader` example allow testing with the real `libpcsclite` (`ci/pcsc-server-test.sh`: `opensc-tool`, a C probe linked to `libpcsclite.so.1`).
- The socket is at a **fixed place**, `$XDG_RUNTIME_DIR/oxydesk-pcsc/pcsc.sock` (`default_socket_path`), and `PCSCLITE_CSOCK_NAME` points to it in the environment of KWin, of the session programs and, through `dbus-update-activation-environment`, of the services activated through D-Bus. The place is fixed so that the environment can name the socket before the service exists (applications connect lazily and report `SCARD_E_NO_SERVICE` until it does). A shell opened outside the session has to export the variable itself. The host's real `pcscd` is never touched: it keeps `/run/pcscd`, and the session simply looks elsewhere.

### `--login` mode
As for drives and printers: the server keeps the RDP connection, the agent (running as the user) hosts the PC/SC service. Agent protocol: `ToAgent::SmartcardAttached`/`SmartcardDetached`/`SmartcardReply`, `FromAgent::SmartcardCall` (operation `ScOp`, answer `ScResult`; contexts and cards are the server's numbers and cross the link unchanged). `AgentLink` runs each call on its own thread (a wait for a card must not block the `cancel` that ends it, nor the reader thread) and the agent proxy waits without a time limit on a status change, only the end of the link ends it. Buffers are bounded (`MAX_SC_BLOB`, `MAX_SC_READERS`). The agent starts the service for the whole connection and exports `PCSCLITE_CSOCK_NAME` when it starts the user's KWin session (`KwinSessionConfig.env`).

### Options (`bins/oxydesk`)
`--smartcard PATH|off` (default with `--session kwin`: `$XDG_RUNTIME_DIR/oxydesk-pcsc/pcsc.sock`; nothing outside a KWin session unless a path is given). In `--login` mode the agent applies the default as the user.

## Tests
- Unit: NDR codec (round trip against `ironrdp-rdpdr`, padding, 32/64-bit contexts, truncated and lying buffers), protocol renumbering, `RdpSmartcard` against a scripted link, pcsc-lite wire format (every command, sizes, unknown commands), reader table (states, event counters, removal and insertion), agent protocol (round trip, bounded counts), `AgentLink` with a fake agent, full agent → server → reader relay in one process (including a wait ended by a cancel and a link loss during a wait).
- Transport: `bins/rdp-test-client/tests/smartcard_loopback.rs` wires `RdpdrServer` and a fake client (`smartcard_client.rs`, a reader with a card of known ATR) back to back: a whole PC/SC session (context, list, status change, connect, transmit, status, transactions, reconnect, control, disconnect, release), an empty reader, an unknown reader.
- Service: `ci/pcsc-server-test.sh` (the real `libpcsclite`).
- Real clients: xfreerdp `/smartcard` and a physical reader (a Vitale card), then mstsc; both in `--login` mode on the Debian VM (see Results).

## Limits
- Only the readers are redirected, not the card contents: what the application sees is what the client's subsystem answers. A PIN pad or a reader with a secure channel works only as far as `SCardControl` can carry it.
- One device per connection, which is how the clients announce it (all their readers behind one `SCARD` device).
- The application sees the client's reader names. Two users on one server never share a service (one socket per session).
- No `SCARD_IOCTL_LOCATECARDS`, `GETREADERICON`, cache calls or `SETATTRIB`: the pcsc-lite protocol has no use for them (they are answered as unsupported if a client ever asked).
- A shell started outside the session does not have `PCSCLITE_CSOCK_NAME` unless the user exports it.

## Results
Reader: Precise Biometrics Sense MC (USB), card: a test Vitale card (ATR `3b:75:13:00:00:45:09:ea:90:00`). The oracle is the reader read directly through the machine's own `pcscd`.

- **Real `libpcsclite`** (`ci/pcsc-server-test.sh`): `opensc-tool -l`, `-a` and `--send-apdu` and a C probe linked to `libpcsclite.so.1` against `PcscServer` with the scripted reader: pass.
- **xfreerdp 3.32 (host) and 3.15 (container) → server in `--session kwin`** (`ci/smartcard-test.sh`): the reader is listed under the client's name, the ATR equals the direct one, `SELECT MF` (`00 A4 00 00 02 3F 00`) is answered like the local reader (`90 00`), the readers disappear with the connection: pass.
- **Debian 13 VM, `--login` mode** (`ci/smartcard-login-test.sh`, PAM → authd → per-user agent → KWin session): same four checks over SSH as the user, plus `PCSCLITE_CSOCK_NAME` found in the environment of `plasmashell`, plus the reader gone after the client leaves: pass.
- Workspace: 668 tests pass, clippy is clean, `ci/check-deps.sh` is clean. Line coverage of the new code: `ndr` 94 %, `table` 99.5 %, `wire` 99 %, `service` 97 %, `conn` 92 %, `rdp/smartcard/mod.rs` 87 % (with the loopback tests, which need `--features rdp-test-client/ironrdp`; `ci/coverage.sh` does not enable it and shows 18 % for that file), agent proxy 85 %, `remote/smartcard.rs` 85 %, `pcsc/monitor.rs` 80 % (the retry paths of the monitor thread are the least covered).
- Not measured: mstsc (the Windows VM has no USB passthrough of the reader yet).

### What the real clients and the VM taught
- **FreeRDP announces its single `SCARD` device several times** (same id). Declining the repeats made it drop the device (`irp_new failed`), so a repeat of the same id is accepted idempotently and only a *second, different* device is declined (`DriveBackend::announce_smartcard`).
- **A detach in `--login` mode never completed** (found by the VM scenario: the reader stayed listed after the client left). The reader monitor ends a detach with a `cancel` sent through the agent proxy; the server had already dropped the device and nobody answered, and the proxy waits without a limit for a status change. The proxy now refuses every call between a detach and the next attach (`Pending::fail_all` closes, `reopen` on attach), covered by `calls_after_the_device_went_fail_at_once_until_the_next_one`.
- The image checks of `cargo xtask vm-test --session login` (`first_frame`, `frames_flow`, `key_to_pixel`, `key_sync_release`, `reconnect`) fail on this VM at revisions that predate the feature too (`06182f5`, `1d4506c`); the clipboard and audio checks pass. Not related to this change, not investigated here.
