//! Colour oracle of the software backend: encode known pictures, decode them with libopenh264 (an independent decoder)
//! reading the planes as BT.709 full range, as RDP clients do. Skipped when libopenh264 is missing.
#![cfg(feature = "openh264")]

use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use oxydesk_core::frame::{Frame, FrameData, PixelFormat};
use oxydesk_core::layout::OutputId;
use oxydesk_core::ports::FrameEncoder;
use oxydesk_core::ColorDepth;
use oxydesk_encode::{OpenH264Config, OpenH264Encoder};

const W: usize = 256;
const H: usize = 64;

fn decoder() -> Option<Decoder> {
    let path = std::env::var("OXYDESK_OPENH264_LIB").ok().or_else(|| {
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

fn frame(px: impl Fn(usize) -> [u8; 3]) -> Frame {
    let mut data = Vec::with_capacity(W * H * 4);
    for _ in 0..H {
        for x in 0..W {
            let [r, g, b] = px(x);
            data.extend_from_slice(&[b, g, r, 255]);
        }
    }
    Frame {
        output: OutputId(0),
        width: W as u32,
        height: H as u32,
        stride: (W * 4) as u32,
        format: PixelFormat::Bgra8,
        data: FrameData::Shm(data),
    }
}

/// Luma of the first row of the decoded picture.
fn decode_luma(depth: ColorDepth, frame: &Frame) -> Option<Vec<u8>> {
    let mut dec = decoder()?;
    let mut enc = OpenH264Encoder::new(&OpenH264Config {
        bitrate_bps: 50_000_000,
        ..Default::default()
    })
    .ok()?;
    enc.set_color_depth(depth);
    let yuv = dec.decode(&enc.encode(frame).ok()?.data).ok()??;
    let (y, _, _) = (yuv.y(), yuv.u(), yuv.v());
    Some(y[..W].to_vec())
}

#[test]
fn black_and_white_keep_the_full_range() {
    let f = frame(|x| {
        if x < W / 2 {
            [0, 0, 0]
        } else {
            [255, 255, 255]
        }
    });
    let Some(y) = decode_luma(ColorDepth::TrueColor, &f) else {
        eprintln!("libopenh264 missing: test skipped");
        return;
    };
    // Limited range would give 16 and 235.
    assert!(y[8] <= 4, "black luma {}", y[8]);
    assert!(y[W - 8] >= 251, "white luma {}", y[W - 8]);
}

#[test]
fn a_reduced_depth_flattens_a_gradient() {
    let f = frame(|x| [x as u8, x as u8, x as u8]);
    let Some(full) = decode_luma(ColorDepth::TrueColor, &f) else {
        eprintln!("libopenh264 missing: test skipped");
        return;
    };
    let reduced = decode_luma(ColorDepth::Rgb332, &f).expect("same library");
    let distinct = |v: &[u8]| {
        let mut v = v.to_vec();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    // 256 grey levels at 24 bits (a few merge in the lossy coding), at most 8 red levels at 8 bits.
    assert!(distinct(&full) > 100, "true colour: {}", distinct(&full));
    assert!(distinct(&reduced) <= 16, "8 bits: {}", distinct(&reduced));
}
