# 002 — IronRDP spike (M1): verdict and pinned version

Status: **provisional GO for IronRDP**; the three open points (multi-monitor, clipboard, mstsc) were settled later (ADR 006,
011, 005, 010).
Date: 2026-10-10. Machine: Ryzen 5 5600X, Fedora 44. Tools: `bins/spike`, `bins/bench`.

## Pinned versions
`ironrdp-server =0.13.0` (features `egfx`, `helper`), `ironrdp-egfx =0.3.0`, `ironrdp-dvc =0.8.0`, `ironrdp-cliprdr =0.7.0`,
`ironrdp-displaycontrol =0.8.0`, `ironrdp-rdpsnd 0.9`. `ironrdp-server` and `ironrdp-acceptor` are vendored and patched in
`third_party/` (ADR 011, 012, 016), as is `ironrdp-rdpdr` (ADR 013). MSRV of ironrdp-server: 1.89 (project toolchain: 1.99).
`openh264 0.9.8` with the `libloading` feature (libopenh264 loaded at run time).

## Measured results
| Criterion (§9) | Result |
|---|---|
| 1. H.264 AVC420 over EGFX | **OK with xfreerdp 3.31** (TLS): EGFX V8.1 + AVC420 negotiated, ~30 fps sustained, 1 frame dropped by backpressure out of ~430. Not checked visually (no virtual display); KRDC not tested. |
| 2. Encoding 1080p / 720p | Desktop-like content (synthetic): **4.2 ms / 1.6 ms** (p50, 1 thread). Worst case, full noise: **32.6 ms / 13.9 ms** (1080p = the whole 30 fps budget). Key-to-pixel latency measured later with the IronRDP client (ADR 003). |
| 5. NLA/CredSSP | **OK with xfreerdp**: right password → session, wrong → `AUTHENTICATION_FAILED`. Measured later with mstsc (ADR 010). |

## Findings from reading the code (settled later)
- **NLA needs preloaded credentials** (NTLM): incompatible with an arbitrary PAM login. Settled in M7: TLS mode +
  `CredentialValidator` (PAM validation through `oxydesk-authd`), see ADR 008 and 010.
- **Multi-monitor**: `RdpServerDisplay::size()` exposes a single `DesktopSize` and the acceptor sent a `MonitorLayout` with
  **one** monitor. `ironrdp-egfx` handles several surfaces, `map_surface_to_output(x, y)` and `resize_with_monitors` (≤16).
  Outcome: EGFX surfaces (ADR 006) plus a local patch of the acceptor (ADR 011).
- **Clipboard**: `CliprdrBackend`/`CliprdrBackendFactory` exist on the server side (ADR 005, 012).
- **Audio**: `RdpsndServerHandler` (formats, choice, start/stop); generic `AudioFormat` (PCM, and Opus possible depending on
  `WaveFormat`). Implemented in ADR 007.
- **UDP**: not supported. **Auto-reconnect**: nothing in the stock `ironrdp-server`; added by a local patch (ADR 016).

## Encoder
- `openh264` loaded dynamically: no g++/nasm required. libopenh264 is **2.6.0 on Fedora 44 and Debian 13**
  (`libopenh264-8`). The crate normally checks a Cisco hash; we use `from_blob_path_unchecked` (distribution builds).
  H.264 licence/patents: the Debian/Fedora package is a source build; to be validated by legal if distributed.
- x264 (crate `x264 0.5.0`, last release 2022) set aside: absent from Fedora, old crate.

## Open points at the time
1. Key-to-pixel latency and clipboard: measure in M2 with the IronRDP back end of the test client (done, ADR 003).
2. Test on the VM with the CPU flags of production: the test VM has AVX2, not AVX-512.
3. Provisioning: `libpipewire-0.3-0` does not exist under that name on Debian 13 (probably `...-0t64`); fix the package list.
