# ADR 009 — GPU encoding: VA-API behind `FrameEncoder` (M8)

## Decision
- **`VaapiEncoder` adapter** (`crates/encode`, Cargo feature `vaapi`, off by default): hardware H.264 through `cros-libva`. It
  adds no dependency on other adapters; it implements the existing `FrameEncoder` port.
- **`AutoEncoder`** (same crate): the back end is chosen at the **first frame** (VA-API needs the size). `--encoder
  auto|vaapi|openh264` in `rdsktp`, `rdsktp-agent` and `bench`. `auto` (default) falls back to openh264 if VA-API is unavailable
  (no DRM node, no driver, no H.264 encoder); `vaapi` and `openh264` never fall back.
- **Stream produced**: Constrained Baseline/Main/High (depending on the driver), CBR, no B-frames, 1 reference frame, POC type 2,
  one slice, CAVLC, cropping (surfaces aligned to 16).
- **Packed headers** (`h264_headers.rs`): radeonsi writes neither SPS/PPS nor the slice header (it announces packed headers
  0x1f). Without them the stream starts with a start code followed by invalid slice data. The application therefore writes
  SPS+PPS (at each IDR) and the slice header (every frame): bit writer, Exp-Golomb, emulation prevention, unit tested.
- **`cros-libva` 0.0.13 vendored** in `third_party/cros-libva` (two fixes: missing VP9 fields with libva 1.23;
  `EncPackedHeaderParameter`/`EncPackedHeaderData` types added, bindgen allowlist extended). To re-synchronise if the upstream
  crate takes them.
- BGRA→NV12 conversion on the CPU (BT.601 limited range, same matrix as openh264) and upload to the VA surface.

## Oracles
- `crates/encode/tests/vaapi.rs`: 30 1080p frames encoded by the GPU and **decoded by libopenh264**, checking NAL types
  (SPS/PPS/IDR at frames 0 and 15, P otherwise), red/blue markers at the expected positions, mean absolute error < 12 (measured:
  0.98). Ignored without a VA-API encoder or without libopenh264.
- Unit tests of `h264_headers`; unit tests of `AutoEncoder` (choice, lazy creation, keyframe requests made early).
- Local end to end (`rdsktp --test-mode --encoder vaapi` + IronRDP `rdp-test-client`, release builds): **7/7**, key→pixel p95
  34.1 ms (openh264: 25.4 ms).
- Fallback: `LIBVA_DRIVER_NAME=bogus` + `--encoder auto` → openh264; `--encoder vaapi` → explicit error.

## Measurements (RX 7800 XT, 1080p, release, `bench`)
| Source | VA-API | openh264 |
|---|---|---|
| synthetic (little motion) | 13.7 ms/frame (p95 14.4) | 4.2 ms |
| noise (worst case) | 13.2 ms | 32.6 ms |

GPU time is essentially constant, dominated by the CPU conversion and by waiting for the encoding to finish. The GPU only wins
on heavy content; on a nearly static desktop openh264 is faster. The benefit of the GPU is **CPU load** and worst-case behaviour,
not latency on simple content.

## DMA-BUF path (GPU import, no CPU conversion)
- **Core**: `FrameData::DmaBuf(DmaBuf { fd: OwnedFd, offset, modifier })` (the fd belongs to the frame).
- **Capture** (`KwinCapture::start_with(.., dmabuf)`): besides the SHM format, offers the same format with the **linear**
  modifier only (`MANDATORY | DONT_FIXATE`). If KWin accepts, frames are delivered as `DmaBuf` (duplicated fd); otherwise the
  stream stays SHM. Linear only: the CPU can always read the buffer back as a fallback.
- **VA-API encoder**: `DRM_PRIME_2` import (XRGB8888/XBGR8888), then BGRx→NV12 conversion by the video engine (VPP,
  `VAProfileNone`/`VideoProc`, BT.601 limited range, same coefficients as the CPU) into the encoder's source surface. The VPP
  context is created at the first DmaBuf frame. If the import fails: one message, then CPU read of the buffer (mmap +
  `DMA_BUF_IOCTL_SYNC`) and the usual path. `openh264` also reads the DmaBuf by mmap (correct, but see the measurements).
- **Choice**: `--dmabuf auto|on|off` (`rdsktp`, `rdsktp-agent`, `bench --kwin`). `auto` = true only if the encoder can use the
  GPU (`--encoder vaapi`, or `auto` with usable VA-API). Without a GPU: SHM offer only, behaviour unchanged.
- `cros-libva` vendored: `ProcPipelineParameter` type and `va_vpp.h` added.

### Oracles and measurements of the DmaBuf path
- Test `dmabuf_import_decodes_to_the_source_pixels`: the 30 frames are placed in a real DMA-BUF (memfd + `/dev/udmabuf`,
  independent of VA-API), imported, converted by the GPU, encoded, decoded by libopenh264; mean error 0.98 (same as the CPU
  path) and `gpu_frames() == 30` required. Negative control: a "full" output range instead of "limited" raises the error to
  3.97, so the oracle detects a wrong conversion.
- Real virtual KWin on the RX 7800 XT (`bench --kwin`, 1080p, release, continuously scrolling desktop):

| Configuration | Transport received | Time/frame (p50) |
|---|---|---|
| VA-API + DmaBuf (GPU import) | 210/210 DmaBuf | **7.3 ms** (p95 7.9) |
| VA-API, SHM + CPU conversion | 210/210 SHM | 14.4 ms |
| openh264, SHM | SHM | 12.2 ms |
| openh264 forced on DmaBuf (CPU read back) | DmaBuf | 268 ms (p95 399) |

- Visual check: the H.264 stream produced from KWin DmaBuf frames (`bench --kwin --out`) is decoded by ffmpeg; the image (Konsole
  window, text, colours) is correct.
- The last row justifies `--dmabuf auto`: reading a GPU buffer back from the CPU is ~20× slower than SHM, so never offer DmaBuf
  without a GPU encoder.
- `core::dmabuf` and the VA-API paths need a GPU and libva, so `cargo test` does not execute them (CLAUDE.md lists the zone).

## Known risks of DmaBuf
- The buffer can be rewritten by KWin as soon as it is queued again; the frame is consumed later from the mailbox. The kernel
  applies implicit synchronisation (fences) for GPU reads, but slight tearing remains possible if the encoder falls several
  frames behind.
- Padding lines (1080 → 1088) are not replicated by the VPP (the CPU replicates them); they are cropped by the SPS cropping.
- Non-linear modifiers (better for KWin) are not offered.

## Not verified / next
- **Production**: VMware without a GPU → SHM + openh264 remains the real path; neither VA-API nor DmaBuf can be tested there.
  It remains to confirm that KWin runs there at all (ADR 004: OpenGL + DRM node required).
- NVENC: not implemented (no NVIDIA GPU available, nothing to run).
- Not replayed: `vm-test` (virgl VM, build without `vaapi`) after the capture change; a full RDP session with the GPU encoder on
  a real KWin (the `wlr-data-control` clipboard is not granted to a KWin session launched on the development machine);
  KRDC/mstsc; a build on Debian 13 is done by the CI `features` job (libva headers of trixie vs vendored `cros-libva`).
- GPU passthrough in the test VM: not done (optional).
- Packaging: the `vaapi` feature is not enabled in the packages (`libva` as a dependency to be decided).
