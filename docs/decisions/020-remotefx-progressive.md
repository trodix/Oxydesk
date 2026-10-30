# 020. RemoteFX Progressive over EGFX (`OXYDESK_CODEC=rfxp`)

## Status

Accepted. It is the default codec; `rfx` (ADR 019) and `h264` stay available through `OXYDESK_CODEC`.

## Context

Classic RemoteFX (the default) sends every changed area at one fixed quality, so a still desktop keeps the grain of the
quantization. RemoteFX Progressive (MS-RDPEGFX, codec `CAPROGRESSIVE`) sends tiles coarse first and then refines them
with upgrade passes, which converge to a clean picture while keeping the full chroma resolution that H.264 4:2:0 loses.

## Decision

- `crates/rdp/src/progressive.rs` holds a pure encoder (no I/O): 64x64 tiles, change detection against what the client
  was given, a first pass at the quality of classic RemoteFX, then two upgrade passes for the tiles that stay still
  (24 tiles per call, round robin, work spread over the cores). The pump sends each stream with
  `GraphicsPipelineServer::send_remotefx_progressive_frame`, checks the EGFX backpressure before encoding (the encoder is
  stateful) and resets the encoder when the client loses its surfaces or a send is refused.
- The encoder follows FreeRDP's decoder, not the helpers of `ironrdp-graphics` 0.9: its upgrade pass codes one bit count per
  band and restarts the streams per band, whereas decoders share one SRL stream and one raw stream per component.
- `ironrdp-graphics` 0.9.0 grows `kp` by `UP_GR` instead of `UQ_GR` after a zero in Golomb-Rice mode, which no decoder follows
  (fixed upstream in 0.10.0, which needs the whole IronRDP 0.10 stack, and the patched `ironrdp-server` has no release for it).
  A copy of 0.9.0 with the one-line fix lives in `third_party/ironrdp-graphics` (`[patch.crates-io]`); it also fixes the classic
  RemoteFX encoder. A unit test reads the encoder's output back with the decoder.
- Decoders refuse a base quantization below 6 (FreeRDP: `cQuant < 6`), so the last pass keeps whole-pixel precision of the
  11.5 coefficients: close to exact (under 2 levels on smooth content, up to about 15 on hard edges and noise), not lossless.

## Tests

Unit tests decode the streams with a port of FreeRDP's progressive decoder (first pass, upgrades, SRL and raw readers).
FreeRDP (`ci/freerdp-test.sh` section 1d, and `--session login` on the Debian VM) decodes the frames without error; mstsc
(Windows 11 VM, `cargo xtask test --client`) passes the same checks as with `rfx`. The 5 failures of that run (large
clipboard files, connection lost after the audio check, dynamic resize) are identical with `rfx`. The repository's test client
does not decode Progressive, so `vm-test` with `rfxp` cannot see images.

## Consequences

Full-HD first pass: about 36 ms on this host in release (noise-like worst case, 3.5 MB), 1-2 ms per upgrade call.
Refinement state costs about 36 KB per tile until the tile is complete. A client without EGFX falls back to bitmap updates as
before.

## Frames in flight

EGFX lets the server send frames before the client has acknowledged the previous ones; the number it may leave unacknowledged
is the window. The rate regulation made for H.264 (`crates/core/src/rate.rs`) also ran with Progressive and cut the window to
one frame per output as soon as the round trip grew. With Progressive that round trip is mostly the client's decoding time,
not a queue, so the window starved the stream: gaps of about 300 ms between sent frames.

Progressive therefore uses a fixed window of 4 frames per output (8 for two monitors) and leaves the regulation to H.264.
`OXYDESK_MAX_IN_FLIGHT=<n>` sets another size (any codec), `0` gives the regulation back. Measured with `OXYDESK_STATS=1`
on a video over two outputs (medians of 2 s windows): 48 frames/s sent instead of 34, p95 gap between frames 52 ms instead of
140 ms, at the price of a longer round trip (135 ms instead of 63 ms). The client acknowledges about 47 frames/s whatever the
window, so a larger one only adds latency.
