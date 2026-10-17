# ADR 005: real clipboard (M4)

## Decision
- `oxydesk-clipboard` adapter (feature `wayland`) of the `ClipboardBackend` port, through the Wayland data-control protocols:
  `zwlr_data_control_manager_v1` (KWin 6.3 exposes it to an unprivileged client, as found in the spike, ADR 004: no `.desktop`
  authorisation needed) and, since KWin 6.4+, `ext_data_control_manager_v1`, which is tried first (ADR 010).
- One Wayland connection and one dedicated thread; `set_text` / `poll_text` do not block; the last text wins.
- **Recognising our own selection** by a private MIME type specific to the instance
  (`application/x-oxydesk-instance-<pid>-<nonce>`), never by comparing text: another application that copies the same text is a
  real change (otherwise the echo oracle would be masked).
- Reading a remote selection: pipe + `receive`, read in a thread (cap 32 MiB), lossy UTF-8.
- RDP → desktop: `set_selection` with the usual text MIME types. Desktop → RDP: `poll_text`.
- **Line endings**: CF_UNICODETEXT uses CRLF, Linux LF. Converted in `TextClipboard` (`rdp` crate, protocol side), not in the
  Wayland adapter. The test client, which shares `TextClipboard`, therefore behaves like a Windows client.
- Images (CF_DIB ↔ PNG) and files (file lists of any size, ADR 012) were added later.

## Test oracle
`oxydesk-testapp --clipboard-echo` copies back any text selection it receives (like an application). The `clipboard_roundtrip`
(short Unicode) and `clipboard_roundtrip_large` (64 KiB+, with newlines) checks therefore go through: RDP client → oxydesk →
Wayland selection → testapp → selection → oxydesk → RDP client. Negative control: without `--clipboard-echo`, both checks fail
("no text received").

## Known limits
- No primary selection (middle click).
- The selection already present at start-up is sent to the first client.
- The Wayland thread is not stopped explicitly (it ends with the process).
- Not tested with Klipper.

## Manual test
xfreerdp 3.31 (+clipboard) to the VM, Plasma + KWrite: copy-paste host → VM and VM → host checked by hand on the host, with accents
and `€`. Not tried by hand: emoji, newlines, text > 64 KiB (covered only by the automated checks with the echo agent; mstsc
covers them since ADR 010).
