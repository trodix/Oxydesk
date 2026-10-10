//! Encoding bench: time per frame (p50/p95/max) for a given resolution.
//! Usage: bench [--width W --height H --frames N --bitrate-mbps M --fps F --noise --encoder auto|vaapi|openh264 --out stream.h264]
//!        bench --kwin [--dmabuf auto|on|off]   (needs the `kwin` feature and an authorized executable)

use std::time::Instant;

use rdsktp_capture::SyntheticSource;
use rdsktp_core::ports::FrameEncoder;
use rdsktp_encode::{AutoEncoder, EncoderChoice};

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

enum Source {
    Synthetic(SyntheticSource),
    #[cfg(feature = "kwin")]
    Kwin { capture: Box<dyn rdsktp_core::ports::CaptureSource>, _session: Box<dyn std::any::Any>, shm: usize, dmabuf: usize },
}

impl Source {
    fn new(args: &[String], w: u32, h: u32, choice: EncoderChoice) -> Self {
        if args.iter().any(|a| a == "--kwin") {
            #[cfg(feature = "kwin")]
            {
                use rdsktp_core::ports::SessionProvider;
                let socket = format!("rdsktp-bench-{}", std::process::id());
                let mut session = rdsktp_session::KwinSession::new(rdsktp_session::KwinSessionConfig {
                    socket: socket.clone(),
                    width: w,
                    height: h,
                    outputs: 1,
                    // Continuous damage so the compositor keeps producing frames.
                    programs: vec![["konsole", "-e", "sh", "-c", "while :; do date +%N; done"].map(String::from).to_vec()],
                });
                session.start().unwrap_or_else(|e| fail(&format!("KWin session: {e}")));
                let dmabuf = match args.windows(2).find(|a| a[0] == "--dmabuf").map(|a| a[1].as_str()) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => choice.wants_dmabuf(),
                };
                let capture = rdsktp_capture::KwinCapture::start_with(Some(&socket), 0, dmabuf).unwrap_or_else(|e| fail(&format!("capture: {e}")));
                return Source::Kwin { capture: Box::new(capture), _session: Box::new(session), shm: 0, dmabuf: 0 };
            }
            #[cfg(not(feature = "kwin"))]
            fail("built without the `kwin` feature");
        }
        Source::Synthetic(SyntheticSource::new(w, h).with_noise(args.iter().any(|a| a == "--noise")))
    }

    fn next(&mut self) -> rdsktp_core::Frame {
        match self {
            Source::Synthetic(s) => s.render(),
            #[cfg(feature = "kwin")]
            Source::Kwin { capture, shm, dmabuf, .. } => loop {
                match capture.recv_latest(rdsktp_core::OutputId(0)) {
                    Ok(Some(f)) => {
                        match f.data {
                            rdsktp_core::FrameData::DmaBuf(_) => *dmabuf += 1,
                            _ => *shm += 1,
                        }
                        return f;
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Err(e) => fail(&format!("capture: {e}")),
                }
            },
        }
    }

    fn transports(&self) -> String {
        match self {
            Source::Synthetic(_) => "synthetic (shm)".into(),
            #[cfg(feature = "kwin")]
            Source::Kwin { shm, dmabuf, .. } => format!("shm={shm} dmabuf={dmabuf}"),
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("bench: {msg}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (w, h): (u32, u32) = (arg(&args, "--width", 1920), arg(&args, "--height", 1080));
    let frames: usize = arg(&args, "--frames", 300);
    let fps: f32 = arg(&args, "--fps", 30.0);
    let bitrate_bps = (arg(&args, "--bitrate-mbps", 8.0f64) * 1e6) as u32;
    let choice: EncoderChoice = args
        .windows(2)
        .find(|w| w[0] == "--encoder")
        .map_or(Ok(EncoderChoice::OpenH264), |w| w[1].parse())
        .unwrap_or_else(|e| {
            eprintln!("bench: {e}");
            std::process::exit(1);
        });
    let mut enc = AutoEncoder::new(choice, fps, bitrate_bps);
    let mut dump = args.windows(2).find(|a| a[0] == "--out").map(|a| std::fs::File::create(&a[1]).unwrap_or_else(|e| fail(&format!("{}: {e}", a[1]))));
    let mut src = Source::new(&args, w, h, choice);
    let (mut times, mut bytes, mut keyframes) = (Vec::new(), 0usize, 0usize);
    for i in 0..frames + 10 {
        let frame = src.next();
        let t = Instant::now();
        let out = enc.encode(&frame).unwrap_or_else(|e| {
            eprintln!("bench: {e}");
            std::process::exit(1);
        });
        if let Some(f) = dump.as_mut() {
            std::io::Write::write_all(f, &out.data).expect("write --out");
        }
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
    println!("encoder: {}; frames by transport: {}", enc.name(), src.transports());
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
