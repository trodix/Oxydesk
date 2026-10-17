# ADR 015: per-printer print modes (AUTO, RAW, XPS), PostScript/PDF to XPS path

## Context
ADR 014 sends the client **raw** jobs: the language (PostScript, PCL, text) was deduced from the Windows driver name, with PostScript as the default. A Windows printer is not bound to satisfy that guess: a GDI-only driver, "Microsoft Print to PDF" or "XPS Document Writer" refuses a raw job.

The protocol has an XPS path ([MS-RDPEPC]): the client announces `RDPDR_PRINTER_ANNOUNCE_FLAG_XPSFORMAT` (0x10); the server sends `DR_PRN_USING_XPS` (component `RDPDR_CTYP_PRN` 0x5052, packet 0x5543, `PrinterId`, `Flags` ignored; not an IRP, no reply) **before** the first `Write`; jobs are then XPS documents that the client's Windows driver renders for the printer. Full "Easy Print" ([MS-RDPEXPS]: dynamic channels `TSVCTKT` and `XPSRD`, print tickets and driver interface) is a layer above, not needed to carry a job.

## Decision

### Three modes, chosen per printer
`--printer-mode SPEC` (default `auto`); `SPEC` is a comma-separated list of `MODE` (default) or `PATTERN=MODE` entries (`*` matches any sequence, case-insensitive; the first matching rule wins). Example: `--printer-mode "xps,HP*=raw:pcl,Zebra=raw:text"`.

| Mode | Rule |
|---|---|
| `raw` | Raw jobs. **Requires an explicitly known language**: the one the driver name states (`PostScript`, word `PS`, `Imagesetter`, `LaserWriter`, `PCL`, `Text Only`), or the one given in the configuration (`raw:ps`, `raw:pcl`, `raw:text`). Refused for a driver that produces documents (PDF, XPS, OneNote, fax). |
| `xps` | XPS jobs. Requires the client to have announced the XPS flag; otherwise the printer is not redirected (explicit message). |
| `auto` | Language stated by the driver name and driver not "document" → `raw` (the behaviour validated so far, kept); otherwise, if the client announces XPS → `xps`; otherwise **no guessing**: the printer is not redirected and the log says what to configure (`raw:ps`, `xps`...). |

The format is decided **once, at announcement** (pure function `resolve_format`, `crates/core/src/printer.rs`). In `xps` mode, `DR_PRN_USING_XPS` is sent before the printer is offered to the desktop, hence before any `Write`.

**No fallback after a job was sent.** A job refused or failed by the client is never resent in the other format (risk of printing twice). For an XPS job, the document is converted **entirely before** starting the job on the client: a conversion failure sends nothing to it.

### XPS chain
- `XpsConverter` port (domain); `GhostscriptXps` adapter (`crates/spool/src/xps.rs`): `gs -dSAFER -sDEVICE=xpswrite`, 120 s timeout, detected at start (`gs -h` must mention `xpswrite`). Without it, XPS printers are not served (warning); raw printers are unaffected.
- CUPS queue of an XPS printer: generic PostScript model (`drv:///sample.drv/generic.ppd`); the printer **socket** reads the document (PostScript or PDF, `gs` detects it), converts it in the private (0700) directory, then sends it in blocks. A script can therefore also write a PDF or PostScript straight into the socket. Document limited to 512 MiB.
- `--login` mode: same decision on the server side (the server's `--printer-mode`); the format travels in `ToAgent::PrinterAdded.format` (0 PostScript, 1 PCL, 2 text, **3 XPS**) and the agent converts in the user's session (`ghostscript` is recommended by the `.deb` and installed by `vm/vm.sh deploy`).

### System CUPS, ACL and session binding
The system CUPS is not modified; existing protections are kept: queue `<user>-<printer>`, `lpadmin -u allow:<user>`, `printer-is-shared=false`, socket `0600` in a `0700` directory of the user's runtime dir, backend `root:0700` that refuses any socket not owned by the job's user. In addition:
- a queue is tied to **its RDP connection**: one spool per connection, removed on disconnection, and its location (`-L`) carries the session id `<pid>-<n>`;
- fix: a printer with the same name as a previous one first stops the old one (previously the new queue was created and then deleted by the old entry's cleanup, which had the same queue name).

## Levels of evidence
| | Implemented | Checked with a software driver | Validated on a physical printer |
|---|---|---|---|
| Mode choice (3 modes, pattern rules, explicit refusal) | yes | unit tests (decision table) | no |
| `DR_PRN_USING_XPS` before any `Write`, once, only in xps mode | yes | server end-to-end test against the test client (packet order); mstsc (Windows 11 VM) | no |
| PostScript → XPS (`gs xpswrite`) | yes | conversion tests (2 pages); `gs` 10.06 on the host | no |
| mstsc renders the XPS | yes | "Microsoft PS Class Driver": the PostScript produced contains 2 pages | no |
| Conversion failure: no job, no fallback | yes | unit test | no |
| Raw (text, PostScript, PCL) | yes (ADR 014) | "Generic / Text Only" with mstsc; xfreerdp and Remmina with PostScript | no |
| `--login` mode (Debian 13 VM) | yes | see "Results" | no |
| GDI-only printer | yes (xps mode) | **no** (no GDI-only driver in the VM) | **no** |
| Full Easy Print (MS-RDPEXPS) | **no** | no | no |

mstsc accepting an XPS with a software driver is encouraging but proves neither compatibility with physical printers nor full Easy Print.

## Settings (tray, duplex, colour)
Without MS-RDPEXPS, no print ticket is negotiated with the client and nothing goes through `TSVCTKT`/`XPSRD`. What is sent is the page content. We therefore **do not promise** that tray, duplex or colour choices made in the Linux session (CUPS options) reach the printer: the Windows driver applies its default settings. What is really applied is what CUPS and `gs` put into the document: content, page size, orientation, page order, copies that CUPS expands. Finishing options (duplex, tray, colour/greyscale) are **to be checked on a physical printer** (protocol below); until then they are considered not transmitted. In raw mode, what the queue's generic PPD puts in the PostScript or PCL goes to the printer without the Windows driver being involved.

## Dependencies and environments
- `gs` with `xpswrite`: checked on the development host (Ghostscript 10.06.0) and in the Debian 13 VM (10.05.1, see "Results"). `ghostscript` package recommended by the `.deb`.
- FreeRDP (xfreerdp, Remmina) does not announce the XPS flag (`xps=false` in the server log): for them `xps` mode is refused and `auto` picks raw if the driver name states a language (FreeRDP announces `MS Publisher Imagesetter`, PostScript). A FreeRDP driver with any other name must be configured (`raw:ps`).
- Change from ADR 014: a driver whose name states no language is no longer taken for PostScript; with mstsc it goes XPS (`auto`), with FreeRDP it must be configured.
- Double rendering with XPS (CUPS/`gs`, then the Windows driver); fidelity on real documents (fonts, transparency) is not measured beyond the protocol below.

## Validation protocol on a physical printer
To be done with a Windows workstation (mstsc) and at least two printers: a PostScript or PCL one (`raw` mode) and a **GDI-only** one (`xps` mode, no PostScript/PCL). For each, from a Linux session (LibreOffice and `lp`):
1. **Text**: a text page with accents (é è ç ñ œ), symbols and digits.
2. **Fonts**: regular, bold, italic, sizes 6 to 48 pt, a non-Latin font (Cyrillic, CJK), fonts embedded in a PDF.
3. **Images**: colour JPEG, PNG with transparency, gradient, greyscale image, 0.1 pt line (thinness).
4. **Several pages**: 5 pages with a landscape page in the middle; check the order, no missing or duplicated page; a 50-page job.
5. **Finishing requested on the Linux side** (duplex, tray, colour/greyscale, 3 copies): note what is applied (expected: copies yes, the rest to be observed).
6. **Robustness**: client disconnecting during a job; printer offline; two consecutive jobs.
Success criterion: no missing page or double printing, readable and correctly encoded text, faithful images. Record for each line: printer, mode, result (passed / failed / deviation with description).

## Results (tests run)
- Unit and integration tests (`cargo test --workspace`, clippy, fmt, `check-deps`): green. They cover the mode decision table, `--printer-mode` parsing, the real `gs` converter (2 pages, skipped without `xpswrite`), no `begin()` after a conversion failure, `DR_PRN_USING_XPS` before any job, an undecidable printer not redirected, format 3 of the agent protocol and replacement of a same-name queue.
- mstsc, host (`cargo xtask test --client`): raw 16/16; XPS (`WIN_PRINTERS=2`) 16/16, the "Microsoft PS Class Driver" renders the XPS as a 2-page PostScript. "Microsoft Print To PDF" is redirected as XPS by `auto` ("document" driver).
- mstsc, `--login` mode (Debian 13 VM, Ghostscript 10.05.1 with `xpswrite`): raw 14/14; XPS 14/14 with the job sent by `lp` to the user's CUPS queue (3 pages rendered). `resize_request_survives` can fail from the second mstsc session of the same VM start (unrelated to printing, absent after a fresh `vm-test`): run `ci/mstsc-login-test.sh` once per `vm-test`.
- xfreerdp and Remmina (host, `ci/clients-test.sh`, and `--login` mode, `ci/clients-login-test.sh`): green for printers (raw, `auto`); `ci/freerdp-login-test.sh` green. They do not announce the XPS flag.
- `clipboard_files_large` test (mstsc): intermittent failure also measured on the previous commit (the server gets `CB_RESPONSE_FAIL` on the first range of the freshly written 128 MiB file); the guest script now excludes `C:\oxydesk` from Defender scanning and waits for the file to be unlocked, which stabilised it.
