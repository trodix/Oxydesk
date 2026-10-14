//! Changing the target bitrate of a running software encoder: the frames get smaller, and the stream stays valid
//! (a decoder takes every frame, no new keyframe needed). Skipped without libopenh264.
#![cfg(feature = "openh264")]

use openh264::decoder::{Decoder, DecoderConfig};
use openh264::OpenH264API;
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::FrameEncoder;
use rdsktp_encode::{OpenH264Config, OpenH264Encoder};

const W: u32 = 640;
const H: u32 = 360;

/// Detailed content that changes at every frame, so that the rate control has something to limit.
fn frame(n: u32) -> Frame {
    let mut state = n.wrapping_mul(2_654_435_761).wrapping_add(12345);
    let mut px = vec![255u8; (W * H * 4) as usize];
    #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks_mut` needs a newer MSRV
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // Gradient plus a fine grain: a frame size the rate control can bring down by raising the quantizer.
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

#[test]
fn a_lower_bitrate_makes_smaller_frames_without_breaking_the_stream() {
    let cfg = OpenH264Config {
        fps: 30.0,
        bitrate_bps: 6_000_000,
        ..Default::default()
    };
    let Ok(mut enc) = OpenH264Encoder::new(&cfg) else {
        eprintln!("SKIPPED: libopenh264 missing");
        return;
    };
    let Some(mut dec) = decoder() else {
        eprintln!("SKIPPED: libopenh264 missing");
        return;
    };
    let mut sizes = Vec::new();
    for n in 0..120u32 {
        if n == 60 {
            enc.set_bitrate(600_000);
        }
        let out = enc.encode(&frame(n)).expect("encode");
        // The rate control skips frames when the bitrate is short: they come out empty and are not sent.
        if !out.data.is_empty() {
            dec.decode(&out.data)
                .unwrap_or_else(|e| panic!("frame {n} does not decode: {e}"));
        }
        sizes.push(out.data.len() as f64);
    }
    // Median: a keyframe now and then (scene change on noise) must not weigh on the comparison.
    let median = |r: std::ops::Range<usize>| {
        let mut v = sizes[r].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (before, after) = (median(30..60), median(90..120));
    assert!(
        after < before * 0.5,
        "frames of {before:.0} bytes before, {after:.0} after the drop to a tenth of the bitrate"
    );
}
