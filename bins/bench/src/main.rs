//! Encoding bench: time per frame (p50/p95/max) for a given resolution.
//! Usage: bench [--width W --height H --frames N --bitrate-mbps M --fps F --noise]

use std::time::Instant;

use rdsktp_capture::SyntheticSource;
use rdsktp_core::ports::FrameEncoder;
use rdsktp_encode::{OpenH264Config, OpenH264Encoder};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(default)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (w, h): (u32, u32) = (arg(&args, "--width", 1920), arg(&args, "--height", 1080));
    let frames: usize = arg(&args, "--frames", 300);
    let fps: f32 = arg(&args, "--fps", 30.0);
    let cfg = OpenH264Config {
        bitrate_bps: (arg(&args, "--bitrate-mbps", 8.0f64) * 1e6) as u32,
        fps,
        ..Default::default()
    };

    let mut enc = OpenH264Encoder::new(&cfg).unwrap_or_else(|e| {
        eprintln!("bench: {e}");
        std::process::exit(1);
    });
    let mut src = SyntheticSource::new(w, h).with_noise(args.iter().any(|a| a == "--noise"));
    let (mut times, mut bytes, mut keyframes) = (Vec::new(), 0usize, 0usize);
    for i in 0..frames + 10 {
        let frame = src.render();
        let t = Instant::now();
        let out = enc.encode(&frame).expect("encode");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if i >= 10 {
            // First 10 frames ignored (warm-up).
            times.push(ms);
            bytes += out.data.len();
            keyframes += out.keyframe as usize;
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "{w}x{h} frames={} p50={:.1}ms p95={:.1}ms max={:.1}ms mean={:.1}ms => {:.0} fps max; {:.1} KiB/frame avg; {} IDR (budget {:.1} ms @ {fps} fps)",
        times.len(),
        pct(&times, 50.0),
        pct(&times, 95.0),
        pct(&times, 100.0),
        mean,
        1000.0 / mean,
        bytes as f64 / times.len() as f64 / 1024.0,
        keyframes,
        1000.0 / fps as f64
    );
}
