# ADR 006 — Multi-monitor (M5)

Status: accepted; the number of monitors became dynamic and follows the client in ADR 011.

## Decision
- N monitors = N outputs of `kwin_wayland --virtual --output-count N` (same size, side by side: Virtual-0, Virtual-1...), **one
  capture stream and one H.264 encoder per output**, **one EGFX surface per output** mapped at the origin of the output.
- The RDP desktop is the bounding rectangle (`Layout::desktop_size`); `Display::size()` returns it. For N > 1,
  `resize_with_monitors` emits a `ResetGraphics` carrying the list of monitors (the first automatic `ResetGraphics` of
  ironrdp-egfx 0.3.0 carries none), then the surfaces are created.
- Logical index of an output = left→right rank (`wl_output.geometry` position), not the registry announcement order.
- The mouse stays in absolute desktop coordinates: the outputs being contiguous from (0,0), they coincide with KWin's global
  coordinates.

## Findings
- The IronRDP 0.13 server announces a single `DesktopSize`; multi-monitor therefore goes through EGFX (surfaces + monitors of
  the ResetGraphics). Announcing the client's real monitors in the Monitor Layout PDU later required a local patch of
  `ironrdp-acceptor` (ADR 011).
- `ironrdp-egfx` on the **client** side: a single `H264Decoder`, which does not receive the surface identifier although each
  surface has its own P stream. Two interleaved streams in one decoder give "PDU error". Worked around in the test client
  (`SurfaceTap` + `PerSurfaceDecoder`). FreeRDP/mstsc have one decoder per surface.
- KWin's `--output-count` imposes identical outputs on KWin 6.3: heterogeneous sizes are not covered there (ADR 011 §6 for the
  scaling fallback).

## Verified (Debian 13 VM, virgl)
`xtask vm-test --session kwin --outputs 2`: 7/7 (3840x1080 desktop, identity blocks at the right abscissas, key→pixel p95 ≈
71 ms, ~43 fps). `--outputs 3`: 7/7 (p95 ≈ 104 ms). One monitor and synthetic: unchanged.

## Manual test
xfreerdp 3.31.1 (`/size:3840x1080 /smart-sizing`): 2 monitors displayed side by side, then Plasma on 2 monitors without the
testapp, mouse movement between the monitors checked by eye on the host.

## Since then
mstsc with 2 monitors and Display Control (dynamic layout) are covered by ADR 011. Still not verified: KRDC, negative offsets,
CPU budget shared between encoders. Note for tests with mstsc: the multimon recipe of the Windows VM needs two QXL displays.
