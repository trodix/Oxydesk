# ADR 014: printer redirection (RDPDR), client printers usable from the session

## Context
An RDP client can share its printers (mstsc: "Printers", xfreerdp: `/printer:NAME[,DRIVER]`, Remmina: "Share printers"). Goal: see them in the Linux session as ordinary CUPS queues (LibreOffice, Firefox, `lp`...), with the result coming out of the printer at the client's workstation.

The protocol ([MS-RDPEPC]) uses the RDPDR channel already used for drives (ADR 013):
- the client announces each printer as an `RDPDR_DTYP_PRINT` device whose data carries flags (default, network, ASCII), the Windows driver name, the printer name and "cached fields";
- a print job is `IRP_MJ_CREATE` (job start), a series of `IRP_MJ_WRITE` (the bytes), then `IRP_MJ_CLOSE` (job end);
- the client forwards the bytes **as is** to its printer (a "RAW" job on Windows, `cupsPrintFile` in raw mode on FreeRDP): the server must produce the language the printer understands.

## Decision

### Ports (`crates/core/src/printer.rs`, hexagonal)
- `PrinterInfo { name, driver, is_default, format }`: what the client announces, plus the **format** that will really be sent (`PrintFormat`: raw `PostScript`/`Pcl`/`Text`, or XPS). How it is resolved from the driver name and the configured mode is ADR 015, which **supersedes** the first version of this ADR (language guessed from the driver name, PostScript by default).
- `RemotePrinter` (blocking, like `RemoteDrive`): `begin(title) -> Box<dyn PrintJob>`; `PrintJob`: `write(&mut self, &[u8])`, `finish(self)`, `abort(self)`. No protocol dependency.
- `PrinterHost`: `added(name, info, Arc<dyn RemotePrinter>)` / `removed(name)`, what the RDP adapter calls (like `DriveHost`). `DesktopPorts` gains `printers`.

### RDP adapter (`crates/rdp/src/printer.rs`)
- The RDPDR backend of ADR 013 also accepts `Print` devices (the `CAP_PRINTER_TYPE` capability is announced to the client). Announcement data is decoded (UTF-16, or ASCII when the flag is set: FreeRDP).
- A job = `Create` (empty path), `Write` in 64 KiB blocks with the cumulative position, `Close`. A refused `Write` aborts the job (`abort`). `Create` is sent with a zero-length path (clients refuse any other path), and FreeRDP's reply to a failed `Create` lacks the "Information" byte: the decoder tolerates it. Blocks are sent **one after the other**: reading the source slows down with the client (the whole job is never buffered).
- Drivers that refuse raw jobs ("Print to PDF", XPS, GDI-only) are announced, but the client refuses the job; ADR 015 decides what is exposed and how.

### Linux side (`crates/spool`, crate `oxydesk-spool`)
- For each printer, a **Unix socket** `<directory>/<name>.sock` (mode `0600`, directory `0700` named `oxydesk-printers`, by default under `$XDG_RUNTIME_DIR`). **One connection = one job**: the bytes read are written to `PrintJob::write`, end of stream (EOF) ends the job (`finish`), and the last line sent back is `OK` or `ERROR: <reason>`. The job is aborted (`abort`) only if the RDP client or the link fails midway. One job at a time per printer (lock), 120 s read inactivity timeout. This interface suits any script (`socat - UNIX-CONNECT:...`) and is the test seam.
- **CUPS queue** (`CupsQueues`, optional, `--printers`): `lpadmin -p <user>-<name> -E -v oxydesk:<socket path> -m <model> -D "<name> (RDP client)" -o printer-is-shared=false -u allow:<user>`, then `lpadmin -x` on removal; queues left by a previous session are removed at start (`lpstat -v`). The model follows the format: `drv:///cupsfilters.drv/...` (PostScript, PCL), `drv:///cupsfilters.drv/textonly.ppd` (text), or the generic PostScript driver for XPS (ADR 015). `lpadmin` is only accepted by `cupsd` from members of its `SystemGroup` (`lpadmin`, `wheel` or `sudo` depending on the distribution); otherwise a warning is logged and only the sockets exist. CUPS therefore converts the application's job into the language wanted by the client's printer before handing it to the backend. The queue is visible only to the session user (`-u allow:`), and the name is never interpreted by a shell (arguments go straight to `lpadmin`).
- **CUPS backend** (`bins/oxydesk-cups-backend`, installed as `/usr/lib/cups/backend/oxydesk`, owner `root`, mode `0700`: CUPS runs it as `root`): reads the job on standard input (or the file in argument 6), connects to the socket in the `oxydesk:/path` URI and copies. It refuses any path that is not a socket in an `oxydesk-printers` directory owned by the job's user (argument 2), so a queue administrator cannot aim at an arbitrary socket with the backend's rights.
- If CUPS is absent or `lpadmin` is refused, the sockets remain available and a warning says so: a missing CUPS does not prevent the session.

### `--login` mode
As for drives: the server keeps the RDP connection, the agent (running as the user) hosts the sockets and CUPS queues. Agent protocol: `ToAgent::PrinterAdded`/`PrinterRemoved`/`PrintReply`, `FromAgent::PrintCall` (operation `PrintOp`: `Begin`, `Write`, `Finish`, `Abort`). `AgentLink` runs each call on a thread and refuses blocks over 1 MiB; the agent exposes a `ProxyPrinter` to the same `PrinterMounts`.

### Options (`bins/oxydesk`)
`--printers DIR|off` (default with `--session kwin`: `$XDG_RUNTIME_DIR/oxydesk-printers`); `--printers-cups on|off` (default: CUPS queues if `lpadmin` answers); `off` creates sockets only. In `--login` mode the agent applies these defaults as the user. `--printer-mode` is described in ADR 015.

## Tests
- Unit: announcement decoding (UTF-16, ASCII, empty fields, truncated), safe queue name, job through the socket (exact bytes, several jobs, disconnection midway), agent protocol (frame round trip), `AgentLink` with a fake agent.
- End to end: IronRDP test client (announces a printer, records the job bytes), `bash ci/printers-test.sh` (job through the socket, then through a private real `cupsd`, `lp` and the backend).
- Real clients: xfreerdp (container with a `cupsd` whose backend records the job), mstsc (Windows VM, "Generic / Text Only" printer, the bytes received are compared to what the host sent).

## Limits
- One direction only: Linux session to the client's printer. No printer status (paper, errors), no job read-back.
- No job options (duplex, tray, colour): what CUPS puts in the stream is all the printer receives (ADR 015 lists what is measured for XPS).
- Printers added during the session are taken into account at the client's next announcement (mstsc re-announces them).

## Results
- Unit tests: announcement (UTF-16, ASCII, cached fields, truncated), formats, spool (exact bytes, several jobs, disconnection), CUPS (fake `lpadmin`/`lpstat`), agent protocol, server link with a fake agent, full agent → server → printer relay (2.5 MB) in one process.
- `ci/printers-test.sh`: IronRDP client (5 jobs including 512 KiB), then a private `cupsd` with the real backend: `lp -o raw` byte for byte, text job filtered by CUPS, socket and queue removed after disconnection. The backend does refuse a socket outside an `oxydesk-printers` directory.
- xfreerdp and Remmina (container, recording CUPS queue): 5 jobs including 256 KiB, identical bytes.
- mstsc (Windows 11 VM, `cargo xtask test --client`): "Generic / Text Only" printer on a local TCP/IP port; the 100019-byte job sent by the host arrives identical. mstsc does not redirect a file port, and the driver must be added (`Add-PrinterDriver`) on Windows 11.
- Findings: `Create` must have a zero-length path; FreeRDP answers a failed `Create` without the "Information" byte.
- **`--login` mode (Debian 13 VM)**: `cargo xtask vm-test --session login --keep`, then `ci/clients-login-test.sh` (xfreerdp and Remmina: folder mounted and read/written, 5 jobs through the socket, 300,000 bytes through the `dev-Recorder` queue with `lp -o raw`, queue removed on disconnection) and `ci/mstsc-login-test.sh` (mstsc, 14/14 checks including drives, a 100019-byte printer job and a 128 MiB clipboard file; host-side checks over SSH). This validates the agent relay of ADR 013 too.
- Findings from the login test: (1) the server read the drives and printers host when the channel was built, before the password was validated: without NLA it saw the previous connection's session, or none; the host is now resolved when the device is announced. (2) On Debian 13, `lpadmin -m drv:///...` needs `cups-driverd`, which is only in the `cups` package (the .deb recommends it, with `cups-filters`); the user must be in the `lpadmin` group. (3) The .deb installs the backend as `/usr/lib/cups/backend/oxydesk` (root, 0700).
