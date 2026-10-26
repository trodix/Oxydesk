# ADR 019 — RemoteFX as the default picture codec

Date: 2026-10-26. Complements ADR 009 (GPU encoding) and ADR 018 (login screen).

## Problem
H.264 4:2:0 over EGFX is light on bandwidth but blurs thin coloured text, which is what a desktop is mostly made of.

## Decision
1. **`OXYDESK_CODEC` chooses the codec.** `rfx` (default) sends bitmap updates, RemoteFX when the client supports it and plain
   bitmaps otherwise, for the changed area only (the picture is diffed against the last one sent), with no chroma subsampling.
   `h264` offers the EGFX channel as before. An unknown value logs a warning and uses `rfx`.
2. **Quantization.** The tiles use IronRDP's default table (LL3, LH3, HL3, HH3 = 6; LH2, HL2 = 7; HH2, LH1, HL1 = 8; HH1 = 9).
   6 is the minimum the specification allows, so the fine grain seen on flat areas is the codec itself, not a quality we gave
   up. xrdp's classic RemoteFX path uses the same table (`librfxcodec`, `g_rfx_default_quantization_values`) and has no quality
   setting there; its Progressive path varies the table with the connection type but is not coarser for luminance. No quality
   knob is offered: going lower needs lossless RLGR, which costs far more bandwidth and CPU.
3. **Backpressure.** The software RemoteFX encoder takes 50-140 ms per full frame, slower than a video produces them. The pump
   (`crates/rdp/src/pump.rs`) therefore keeps at most one bitmap update in flight (`Shared::bitmaps_queued`, decremented when the
   server takes the update). While the client is behind, only the newest picture is kept (`OutputState::pending`) and sent once
   the previous one has been taken. Without it the unbounded queue grew during a video and the picture and the effect of clicks
   lagged further and further behind. Measured with `mstsc` on the neon VM with a windowed 1080p video: 3 of 10 checks failed
   before (two clicks), none of the click checks after; encodes 45-90 ms instead of up to 140 ms.
4. **Pointer inputs.** The middle button, the side buttons (back, forward) and the horizontal wheel of the pointer PDUs are
   decoded in `third_party/ironrdp-server/src/handler.rs` and forwarded.

## Limits
- The raw frames still travel from the agent to the server at the full rate (about 8 MB per 1920x1080 output); the encode
  cost itself is unchanged.
- 15 and 16-bit colour depths are untested with a real `mstsc`.
