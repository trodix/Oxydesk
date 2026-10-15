//! VA-API backend oracle: encode synthetic frames on the GPU, decode them with libopenh264 (an
//! independent decoder) and compare the pixels to the source. Skipped when there is no usable
//! hardware encoder or no libopenh264.
#![cfg(all(feature = "vaapi", feature = "openh264"))]

use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use rdsktp_core::frame::{DmaBuf, Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::FrameEncoder;
use rdsktp_encode::{VaapiConfig, VaapiEncoder};

const W: u32 = 1920;
const H: u32 = 1080;

fn frame(n: u32) -> Frame {
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let o = ((y * W + x) * 4) as usize;
            // BGRA: smooth gradient that scrolls, so every frame differs.
            let g = ((x + y / 2 + n * 8) % 256) as u8;
            px[o..o + 4].copy_from_slice(&[g / 2 + 40, g / 3 + 60, g / 4 + 80, 255]);
        }
    }
    // Red 32x32 marker at (0,0), blue 32x32 block at (64,0) (BGRA).
    for y in 0..32 {
        for x in 0..32 {
            let o = ((y * W + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[0, 0, 255, 255]);
            let o = ((y * W + x + 64) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[255, 0, 0, 255]);
        }
    }
    Frame {
        output: OutputId(0),
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra8,
        data: FrameData::Shm(px),
    }
}

fn decoder() -> Option<Decoder> {
    let path = std::env::var("RDSKTP_OPENH264_LIB").ok().or_else(|| {
        [
            "/usr/lib64/libopenh264.so.8",
            "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
    })?;
    // SAFETY: test-only, system library.
    let api = unsafe { OpenH264API::from_blob_path_unchecked(path) }.ok()?;
    Decoder::with_api_config(api, DecoderConfig::new()).ok()
}

fn nal_types(annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i..i + 3] == [0, 0, 1] {
            out.push(annex_b[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

fn f_clone(f: &Frame) -> Frame {
    Frame {
        output: f.output,
        width: f.width,
        height: f.height,
        stride: f.stride,
        format: f.format,
        data: FrameData::Shm(Vec::new()),
    }
}

fn rgb_at(rgba: &[u8], w: usize, x: usize, y: usize) -> [i32; 3] {
    let o = (y * w + x) * 4;
    [rgba[o] as i32, rgba[o + 1] as i32, rgba[o + 2] as i32]
}

/// Moves the pixels of `f` into a linear DMA-BUF created from a memfd through /dev/udmabuf
/// (independent of VA-API). `None` when udmabuf is not available.
fn to_dmabuf(f: &Frame) -> Option<Frame> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let FrameData::Shm(px) = &f.data else {
        return None;
    };
    let size = px.len().div_ceil(4096) * 4096;
    // SAFETY: plain syscalls on descriptors created here; every return value is checked.
    unsafe {
        let mem = libc::memfd_create(c"rdsktp-test".as_ptr(), libc::MFD_ALLOW_SEALING);
        if mem < 0 || libc::ftruncate(mem, size as i64) != 0 {
            return None;
        }
        if libc::write(mem, px.as_ptr().cast(), px.len()) != px.len() as isize {
            return None;
        }
        libc::fcntl(mem, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK);
        let dev = libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR);
        if dev < 0 {
            return None;
        }
        #[repr(C)]
        struct Create {
            memfd: u32,
            flags: u32,
            offset: u64,
            size: u64,
        }
        let req = Create {
            memfd: mem as u32,
            flags: 1,
            offset: 0,
            size: size as u64,
        };
        let fd = libc::ioctl(dev, 0x4018_7542, &req as *const Create);
        libc::close(dev);
        libc::close(mem);
        if fd < 0 {
            return None;
        }
        Some(Frame {
            output: f.output,
            width: f.width,
            height: f.height,
            stride: f.stride,
            format: f.format,
            data: FrameData::DmaBuf(DmaBuf {
                fd: OwnedFd::from_raw_fd(fd),
                offset: 0,
                modifier: 0,
            }),
        })
    }
}

#[test]
fn gpu_stream_decodes_to_the_source_pixels() {
    run(false);
}

/// Same oracle, but the source is a DMA-BUF imported and converted to NV12 by the GPU.
#[test]
fn dmabuf_import_decodes_to_the_source_pixels() {
    run(true);
}

fn run(dmabuf: bool) {
    let Ok(mut enc) = VaapiEncoder::new(&VaapiConfig::default(), W, H) else {
        eprintln!("SKIPPED: no usable VA-API H.264 encoder");
        return;
    };
    let Some(mut dec) = decoder() else {
        eprintln!("SKIPPED: libopenh264 missing");
        return;
    };

    let mut max_err = 0.0f64;
    for n in 0..30u32 {
        if n == 15 {
            enc.request_keyframe();
        }
        let f = frame(n);
        let sent = if dmabuf {
            let Some(d) = to_dmabuf(&f) else {
                eprintln!("SKIPPED: /dev/udmabuf not usable");
                return;
            };
            d
        } else {
            Frame {
                data: FrameData::Shm(match &f.data {
                    FrameData::Shm(v) => v.clone(),
                    _ => unreachable!(),
                }),
                ..f_clone(&f)
            }
        };
        let out = enc.encode(&sent).expect("encode");
        let types = nal_types(&out.data);
        if n == 0 || n == 15 {
            assert!(out.keyframe, "frame {n} must be a keyframe");
            assert!(
                types.contains(&7) && types.contains(&8) && types.contains(&5),
                "frame {n}: SPS/PPS/IDR expected, got {types:?}"
            );
        } else {
            assert!(!out.keyframe, "frame {n} must be inter");
            assert!(
                types.contains(&1) && !types.contains(&5),
                "frame {n}: P slice expected, got {types:?}"
            );
        }

        let yuv = dec.decode(&out.data).expect("decode").expect("picture");
        let (w, h) = yuv.dimensions();
        assert_eq!(
            (w as u32, h as u32),
            (W, H),
            "cropping must give the visible size"
        );
        let mut rgba = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut rgba);

        // Markers survive lossy coding within a tolerance.
        let red = rgb_at(&rgba, w, 16, 16);
        let blue = rgb_at(&rgba, w, 80, 16);
        assert!(
            red[0] > 200 && red[1] < 60 && red[2] < 60,
            "frame {n}: red marker {red:?}"
        );
        assert!(
            blue[2] > 200 && blue[0] < 60 && blue[1] < 60,
            "frame {n}: blue marker {blue:?}"
        );

        // Mean absolute error over a sampled grid (source is BGRA, decoded is RGBA).
        let FrameData::Shm(src) = &f.data else {
            unreachable!()
        };
        let (mut sum, mut cnt) = (0i64, 0i64);
        for y in (0..h).step_by(7) {
            for x in (0..w).step_by(7) {
                let s = (y * w + x) * 4;
                let d = rgb_at(&rgba, w, x, y);
                sum += (d[0] - src[s + 2] as i32).abs() as i64
                    + (d[1] - src[s + 1] as i32).abs() as i64
                    + (d[2] - src[s] as i32).abs() as i64;
                cnt += 3;
            }
        }
        max_err = max_err.max(sum as f64 / cnt as f64);
    }
    if dmabuf {
        assert_eq!(
            enc.gpu_frames(),
            30,
            "every frame must go through the GPU import path"
        );
    }
    eprintln!("max mean absolute error over 30 frames: {max_err:.2}");
    assert!(
        max_err < 12.0,
        "decoded picture too far from the source: {max_err:.2}"
    );
}

/// Detailed content that changes at every frame, so that the rate control has something to limit.
fn grainy(n: u32) -> Frame {
    let mut state = n.wrapping_mul(2_654_435_761).wrapping_add(12345);
    let mut px = vec![255u8; (W * H * 4) as usize];
    #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks_mut` needs a newer MSRV
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let base = ((i as u32 % W) / 3 + (i as u32 / W) / 2 + n) as u8;
        let grain = (state >> 29) as u8;
        p[..3].copy_from_slice(&[
            base.wrapping_add(grain),
            (base / 2).wrapping_add(grain),
            (255 - base).wrapping_add(grain),
        ]);
    }
    Frame {
        output: OutputId(0),
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra8,
        data: FrameData::Shm(px),
    }
}

/// A new target bitrate without a keyframe: the frames get smaller and an independent decoder takes the whole stream.
#[test]
fn a_lower_bitrate_makes_smaller_frames_without_a_keyframe() {
    let cfg = VaapiConfig {
        bitrate_bps: 12_000_000,
        fps: 30.0,
        ..VaapiConfig::default()
    };
    let Ok(mut enc) = VaapiEncoder::new(&cfg, W, H) else {
        eprintln!("SKIPPED: no usable VA-API H.264 encoder");
        return;
    };
    let Some(mut dec) = decoder() else {
        eprintln!("SKIPPED: libopenh264 missing");
        return;
    };
    let mut sizes = Vec::new();
    for n in 0..120u32 {
        if n == 60 {
            enc.set_bitrate(1_000_000);
        }
        let out = enc.encode(&grainy(n)).expect("encode");
        if n > 0 {
            assert!(
                !out.keyframe,
                "frame {n}: the new bitrate needs no keyframe"
            );
        }
        dec.decode(&out.data)
            .unwrap_or_else(|e| panic!("frame {n} does not decode: {e}"));
        sizes.push(out.data.len() as f64);
    }
    let median = |r: std::ops::Range<usize>| {
        let mut v = sizes[r].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (before, after) = (median(30..60), median(90..120));
    assert!(
        after < before * 0.5,
        "frames of {before:.0} bytes before, {after:.0} after the drop to a twelfth of the bitrate"
    );
}
