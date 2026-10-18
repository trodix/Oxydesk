//! `oxydesk-agent`: runs as the logged-in user (unit `oxydesk-agent@<user>.service`).
//! Owns the KWin session, capture, H.264 encoding, input, clipboard and audio, and
//! exposes them to `oxydesk` through the `oxydesk-agent-proto` protocol on a Unix socket. One connection at a time:
//! the most recent one replaces the previous one (reconnection); the desktop session survives disconnections.

mod convert;
mod drives;
mod printers;
mod settings;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use convert::{button, from_info, held, hold_stamp, now_ms, to_info};
use oxydesk_agent_proto::{
    read_msg, write_msg, CursorImage, FileRange, FromAgent, ToAgent, MAX_OUTPUTS,
};
use oxydesk_core::layout::OutputId;
use oxydesk_core::ports::{
    AudioParams, AudioProvider, CaptureSource, ClipboardBackend, FrameEncoder, InputSink,
};
use oxydesk_core::{FileExchange, RangeRequest, RangeSender, DEFAULT_BITRATE_BPS};
use oxydesk_encode::{AutoEncoder, EncoderChoice};
use settings::{arg, AudioMode, Settings};

const AUDIO_PACKET_MS: u32 = 20;

struct Desktop {
    capture: Mutex<Box<dyn CaptureSource>>,
    input: Mutex<Box<dyn InputSink>>,
    clipboard: Mutex<Box<dyn ClipboardBackend>>,
    audio: Option<Arc<dyn AudioProvider>>,
    /// Outputs captured now (changes with `SetLayout`).
    outputs: AtomicU32,
    /// Outputs the desktop has when no client is connected, for a desktop that is given back as it was (a session
    /// joined rather than started); `None` when a client's layout is kept for the next one.
    resting_outputs: Option<u32>,
    width: u32,
    height: u32,
    fps: f32,
    encoder: EncoderChoice,
    /// Wake-up by the capture (`event`) or fixed rate of `poll_ms` (or 1/fps when absent).
    event: bool,
    poll_ms: Option<f32>,
    /// Minimum delay between two encoding rounds (caps the rate in `event` mode).
    min_interval_ms: f32,
    _session: Mutex<Box<dyn std::any::Any + Send>>,
    /// Set after a resize: the video loop drops its last (old size) frame and restarts from keyframes.
    resized: AtomicBool,
    /// A client that cannot decode H.264: the frames go out as uncompressed pixels.
    raw: AtomicBool,
    /// Target bitrate of each encoder, set by the server from what the client's link carries.
    bitrate: AtomicU32,
    /// Held by the video loop during a round and by a layout change: after the answer, no frame of the old
    /// layout is sent any more.
    round: Mutex<()>,
}

fn runtime_dir() -> PathBuf {
    // `%U` cannot be used in the unit: derive the directory from the effective uid.
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })))
}

fn build_desktop(args: &[String]) -> anyhow::Result<Desktop> {
    let cfg = Settings::parse(args)?;
    let (width, height, outputs) = (cfg.width, cfg.height, cfg.outputs);
    let audio: Option<Arc<dyn AudioProvider>> = match cfg.audio {
        AudioMode::Off => None,
        AudioMode::Tone => Some(Arc::new(oxydesk_audio::ToneProvider)),
        AudioMode::Pulse => Some(Arc::new(oxydesk_audio::PulseCapture)),
        AudioMode::Monitor => Some(Arc::new(oxydesk_audio::PulseMonitor)),
    };
    #[cfg_attr(not(feature = "kwin"), allow(unused_variables))]
    let dmabuf = cfg.use_dmabuf();
    let make = |capture: Box<dyn CaptureSource>,
                input: Box<dyn InputSink>,
                clipboard: Box<dyn ClipboardBackend>,
                keep: Box<dyn std::any::Any + Send>| Desktop {
        capture: Mutex::new(capture),
        input: Mutex::new(input),
        clipboard: Mutex::new(clipboard),
        audio: audio.clone(),
        outputs: AtomicU32::new(outputs),
        resting_outputs: None,
        width,
        height,
        fps: cfg.fps,
        encoder: cfg.encoder,
        event: cfg.event,
        poll_ms: cfg.poll_ms,
        min_interval_ms: cfg.min_interval_ms,
        _session: Mutex::new(keep),
        resized: AtomicBool::new(false),
        raw: AtomicBool::new(false),
        bitrate: AtomicU32::new(DEFAULT_BITRATE_BPS),
        round: Mutex::new(()),
    };
    match cfg.session.as_str() {
        "synthetic" => {
            let d = oxydesk_capture::SyntheticDesktop::with_outputs(outputs, width, height);
            Ok(make(
                Box::new(d.clone()),
                Box::new(d.clone()),
                Box::new(d),
                Box::new(()),
            ))
        }
        "kwin" => {
            #[cfg(feature = "kwin")]
            {
                use oxydesk_core::ports::SessionProvider;
                if audio.is_some() {
                    if let Err(e) = oxydesk_audio::PulseCapture::ensure_sink() {
                        tracing::warn!("audio unavailable: {e}");
                    }
                }
                let socket = cfg.kwin_socket.clone();
                let programs = cfg.programs.clone();
                let mut kwin =
                    oxydesk_session::KwinSession::new(oxydesk_session::KwinSessionConfig {
                        socket: socket.clone(),
                        width,
                        height,
                        outputs,
                        programs,
                    });
                kwin.start()
                    .map_err(|e| anyhow::anyhow!("KWin session: {e}"))?;
                let sources = (0..outputs as usize)
                    .map(|i| {
                        oxydesk_capture::KwinCapture::start_with(Some(&socket), i, dmabuf)
                            .map(|c| Box::new(c) as Box<dyn CaptureSource>)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let capture = oxydesk_capture::MultiCapture::new(sources).with_factory(
                    oxydesk_capture::source_factory(socket.clone(), outputs as usize, dmabuf),
                );
                let input = oxydesk_input::KwinFakeInput::connect(Some(&socket))?;
                let clipboard = oxydesk_clipboard::WaylandClipboard::connect(Some(&socket))?;
                // KWin that cannot resize its desktop (6.3): the client's size is obtained by scaling.
                let (capture, input) = oxydesk_scaling::wrap(Box::new(capture), Box::new(input));
                Ok(make(
                    Box::new(capture),
                    Box::new(input),
                    Box::new(clipboard),
                    Box::new(kwin),
                ))
            }
            #[cfg(not(feature = "kwin"))]
            anyhow::bail!("binary built without the `kwin` feature");
        }
        "attach" => {
            #[cfg(feature = "kwin")]
            {
                // The desktop of the user, already running: nothing is started, so nothing is stopped with the agent.
                // Its monitors are theirs and are captured as they are (the client's sizes come from scaling), the
                // client's extra monitors are virtual outputs, and when the client has fewer monitors the windows
                // of the others are brought onto the ones it shows. The keyboard layout of the client is not
                // written into their settings.
                if std::env::var_os("OXYDESK_KEYBOARD_LAYOUT").is_none() {
                    std::env::set_var("OXYDESK_KEYBOARD_LAYOUT", "off");
                }
                let socket = cfg.kwin_socket.clone();
                // Fails at once rather than waiting on tools that would wait for a compositor to appear.
                anyhow::ensure!(
                    runtime_dir().join(&socket).exists(),
                    "no Wayland socket {socket} in {}: the session to join is not running",
                    runtime_dir().display()
                );
                let mut physical = oxydesk_capture::enabled_outputs(Some(&socket))?;
                physical.truncate(oxydesk_core::layout::MAX_OUTPUTS);
                anyhow::ensure!(
                    !physical.is_empty(),
                    "the session being joined has no monitor"
                );
                let sources = (0..physical.len())
                    .map(|rank| {
                        oxydesk_capture::KwinCapture::start_with(Some(&socket), rank, dmabuf).map(
                            |c| {
                                Box::new(oxydesk_capture::ExistingOutput(c))
                                    as Box<dyn CaptureSource>
                            },
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let multi = oxydesk_capture::MultiCapture::new(sources).with_factory(
                    oxydesk_capture::existing_factory(socket.clone(), physical.clone(), dmabuf),
                );
                let outputs = physical.len() as u32;
                let mut source = oxydesk_capture::ExistingDesktop::new(
                    multi,
                    physical,
                    Box::new(oxydesk_capture::KwinWindowMover::default()),
                )
                .keep_virtual_outputs(cfg.keep_virtual_outputs)
                .with_settle({
                    let socket = socket.clone();
                    Box::new(move |count| {
                        oxydesk_capture::enabled_outputs(Some(&socket))
                            .is_ok_and(|names| names.len() == count)
                    })
                });
                let sizes = first_frame_sizes(&mut source, outputs as usize)?;
                let (width, height) = sizes[0];
                let input = oxydesk_input::KwinFakeInput::connect(Some(&socket))?;
                let clipboard = oxydesk_clipboard::WaylandClipboard::connect(Some(&socket))?;
                let (capture, input) = oxydesk_scaling::wrap(Box::new(source), Box::new(input));
                for (i, size) in sizes.iter().enumerate() {
                    capture.learn_native(OutputId(i as u32), *size);
                }
                let mut desktop = make(
                    Box::new(capture),
                    Box::new(input),
                    Box::new(clipboard),
                    Box::new(()),
                );
                desktop.width = width;
                desktop.height = height;
                desktop.outputs = AtomicU32::new(outputs);
                desktop.resting_outputs = (!cfg.keep_virtual_outputs).then_some(outputs);
                Ok(desktop)
            }
            #[cfg(not(feature = "kwin"))]
            anyhow::bail!("binary built without the `kwin` feature");
        }
        other => anyhow::bail!("unknown --session: {other}"),
    }
}

/// Size of each of the `count` outputs of the desktop being joined, from their first frames (monitors of the
/// user: they are not ours to choose). The first output must answer; the others are left unknown after the delay.
#[cfg(feature = "kwin")]
fn first_frame_sizes(
    source: &mut dyn CaptureSource,
    count: usize,
) -> anyhow::Result<Vec<(u32, u32)>> {
    let mut sizes: Vec<Option<(u32, u32)>> = vec![None; count];
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && sizes.iter().any(Option::is_none) {
        for (i, size) in sizes.iter_mut().enumerate().filter(|(_, s)| s.is_none()) {
            if let Some(frame) = source.recv_latest(OutputId(i as u32))? {
                *size = Some((frame.width, frame.height));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let first =
        sizes[0].ok_or_else(|| anyhow::anyhow!("the session being joined sent no image"))?;
    Ok(sizes.into_iter().map(|s| s.unwrap_or(first)).collect())
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(w: &Writer, msg: &FromAgent) -> bool {
    let mut guard = w.lock().expect("writer mutex");
    write_msg(&mut *guard, msg).is_ok() && guard.flush().is_ok()
}

/// Capture -> encode -> send, at a fixed rate. A keyframe is produced on connection and on request.
fn video_loop(
    d: Arc<Desktop>,
    w: Writer,
    stop: Arc<AtomicBool>,
    keyframes: Arc<Vec<AtomicBool>>,
    holds: Arc<Vec<AtomicU64>>,
) {
    let fps = d.fps;
    let mut encoders: Vec<Box<dyn FrameEncoder>> = Vec::new();
    let mut last: Vec<Option<oxydesk_core::Frame>> = Vec::new();
    let period = d.poll_ms.map_or_else(
        || Duration::from_secs_f32(1.0 / fps),
        |ms| Duration::from_secs_f32(ms / 1000.0),
    );
    let min_interval = Duration::from_secs_f32(d.min_interval_ms / 1000.0);
    let wake = Arc::new(oxydesk_core::wake::Wake::default());
    d.capture
        .lock()
        .expect("capture mutex")
        .set_waker(wake.clone());
    let mut next = Instant::now();
    let mut first = true;
    let mut applied_bitrate = d.bitrate.load(Ordering::Relaxed);
    let mut seen = 0u64;
    while !stop.load(Ordering::Relaxed) {
        if d.event {
            if !first {
                wake.wait(seen, period.max(Duration::from_millis(1)));
            }
            seen = wake.generation();
        } else {
            next += period;
            match next.checked_duration_since(Instant::now()) {
                Some(wait) => std::thread::sleep(wait),
                None => next = Instant::now(),
            }
        }
        let round_start = Instant::now();
        let _round = d.round.lock().expect("round mutex");
        let cursor = d.capture.lock().expect("capture mutex").take_cursor();
        if let Some(update) = cursor {
            let msg = FromAgent::Cursor(match update {
                oxydesk_core::CursorUpdate::Hidden => None,
                oxydesk_core::CursorUpdate::Shape(s) => Some(CursorImage {
                    width: s.width,
                    height: s.height,
                    hot_x: s.hot_x,
                    hot_y: s.hot_y,
                    rgba: s.rgba,
                }),
            });
            if !send(&w, &msg) {
                stop.store(true, Ordering::Relaxed);
                return;
            }
        }
        let outputs = d.outputs.load(Ordering::SeqCst) as usize;
        // Encoders follow the number of outputs (a new one starts with a keyframe).
        while encoders.len() < outputs {
            encoders.push(Box::new(AutoEncoder::new(
                d.encoder,
                fps,
                d.bitrate.load(Ordering::Relaxed),
            )));
            keyframes[encoders.len() - 1].store(true, Ordering::Relaxed);
        }
        encoders.truncate(outputs);
        let bitrate = d.bitrate.load(Ordering::Relaxed);
        if bitrate != applied_bitrate {
            applied_bitrate = bitrate;
            encoders.iter_mut().for_each(|e| e.set_bitrate(bitrate));
        }
        last.resize_with(outputs, || None);
        if d.resized.swap(false, Ordering::SeqCst) {
            last.iter_mut().for_each(|l| *l = None);
            holds.iter().for_each(|h| h.store(0, Ordering::Relaxed));
            keyframes
                .iter()
                .for_each(|k| k.store(true, Ordering::Relaxed));
        }
        for i in 0..outputs {
            let wanted_key = first || keyframes[i].swap(false, Ordering::Relaxed);
            if wanted_key {
                encoders[i].request_keyframe();
            } else if held(&holds[i]) {
                continue; // the server has frames enough waiting: the next ones would only be thrown away
            }
            let fresh = match d
                .capture
                .lock()
                .expect("capture mutex")
                .recv_latest(OutputId(i as u32))
            {
                Ok(f) => f,
                Err(e) => {
                    send(&w, &FromAgent::Fatal(format!("capture: {e}")));
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            match fresh {
                Some(f) => last[i] = Some(f),
                // Static desktop: re-encode the last frame only if a keyframe is requested.
                None if wanted_key => {}
                None => continue,
            }
            let Some(frame) = last[i].as_ref() else {
                if wanted_key {
                    keyframes[i].store(true, Ordering::Relaxed); // nothing to encode yet: retry
                }
                continue;
            };
            if d.raw.load(Ordering::Relaxed) {
                let msg = match oxydesk_core::pixels::to_bgra(frame) {
                    Ok(Some(data)) => FromAgent::VideoRaw {
                        output: i as u32,
                        width: frame.width,
                        height: frame.height,
                        data,
                    },
                    Ok(None) => continue,
                    Err(e) => {
                        send(&w, &FromAgent::Fatal(format!("pixels: {e}")));
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                };
                if !send(&w, &msg) {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                continue;
            }
            let encoded = match encoders[i].encode(frame) {
                Ok(e) => e,
                Err(e) => {
                    send(&w, &FromAgent::Fatal(format!("encoding: {e}")));
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            if encoded.data.is_empty() {
                continue; // frame skipped by the rate control
            }
            let msg = FromAgent::Video {
                output: i as u32,
                keyframe: encoded.keyframe,
                width: frame.width,
                height: frame.height,
                data: encoded.data,
            };
            if !send(&w, &msg) {
                stop.store(true, Ordering::Relaxed);
                return;
            }
        }
        first = false;
        drop(_round);
        if let Some(rest) = min_interval.checked_sub(round_start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

fn audio_loop(
    provider: Arc<dyn AudioProvider>,
    params: AudioParams,
    w: Writer,
    stop: Arc<AtomicBool>,
) {
    let mut source = match provider.open(params) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("audio: {e}");
            return;
        }
    };
    let mut buf = vec![0u8; params.bytes_for_ms(AUDIO_PACKET_MS)];
    while !stop.load(Ordering::Relaxed) {
        if source.read_exact(&mut buf).is_err() || !send(&w, &FromAgent::Audio(buf.clone())) {
            break;
        }
    }
}

fn serve(d: Arc<Desktop>, stream: UnixStream) {
    serve_connection(&d, stream);
    // The client is gone: a desktop it had changed (windows brought onto its monitors) is given back.
    d.capture.lock().expect("capture mutex").release();
    if let Some(outputs) = d.resting_outputs {
        d.outputs.store(outputs, Ordering::SeqCst);
    }
}

fn serve_connection(d: &Arc<Desktop>, stream: UnixStream) {
    let writer: Writer = match stream.try_clone() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            tracing::error!("socket: {e}");
            return;
        }
    };
    // The agent outlives the connections: every server (before the Hello, which a server may answer at once) starts with the H.264 stream.
    d.raw.store(false, Ordering::SeqCst);
    d.bitrate.store(DEFAULT_BITRATE_BPS, Ordering::SeqCst);
    if !send(
        &writer,
        &FromAgent::Hello {
            outputs: d.outputs.load(Ordering::SeqCst),
            width: d.width,
            height: d.height,
        },
    ) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    // Ranges of the file lists exchanged with the server (the lists die with the connection).
    let files = Arc::new(FileExchange::default());
    let keyframes: Arc<Vec<AtomicBool>> =
        Arc::new((0..MAX_OUTPUTS).map(|_| AtomicBool::new(true)).collect());
    let holds: Arc<Vec<AtomicU64>> =
        Arc::new((0..MAX_OUTPUTS).map(|_| AtomicU64::new(0)).collect());
    let mut threads = vec![{
        let (d, w, stop, kf) = (d.clone(), writer.clone(), stop.clone(), keyframes.clone());
        let holds = holds.clone();
        std::thread::spawn(move || video_loop(d, w, stop, kf, holds))
    }];
    threads.push({
        let (d, w, stop, files) = (d.clone(), writer.clone(), stop.clone(), files.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let (text, image, selection) = {
                    let mut c = d.clipboard.lock().expect("clipboard mutex");
                    (c.poll_text(), c.poll_image(), c.poll_files())
                };
                match selection {
                    Ok(Some(list)) => {
                        let infos = list.entries.iter().map(to_info).collect();
                        let list_id = files.serve(list);
                        if !send(
                            &w,
                            &FromAgent::Files {
                                list_id,
                                files: infos,
                            },
                        ) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
                match text {
                    Ok(Some(t)) => {
                        if !send(&w, &FromAgent::ClipboardText(t)) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
                match image {
                    Ok(Some(png)) => {
                        if !send(&w, &FromAgent::ClipboardImage(png)) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
            }
        })
    });
    // Drives of the client: mounted in the session for the duration of the connection.
    let drives = match oxydesk_drivefs::DriveMounts::new(runtime_dir().join("oxydesk-drives")) {
        Ok(m) => {
            let w = writer.clone();
            Some(drives::Drives::new(
                Arc::new(m),
                Arc::new(move |msg: &FromAgent| send(&w, msg)),
            ))
        }
        Err(e) => {
            tracing::warn!("drive redirection unavailable: {e}");
            None
        }
    };
    // Printers of the client: a socket and a CUPS queue in the session for the duration of the connection.
    let printers = match oxydesk_spool::PrinterSpool::new(
        runtime_dir().join(oxydesk_spool::SOCKET_DIR),
        oxydesk_spool::Cups::detect(&oxydesk_spool::current_user()),
    ) {
        Ok(spool) => {
            // The printers whose jobs are XPS need Ghostscript in the session of the user.
            let spool = spool.with_xps(
                oxydesk_spool::GhostscriptXps::detect()
                    .map(|gs| Arc::new(gs) as Arc<dyn oxydesk_core::XpsConverter>),
            );
            let w = writer.clone();
            Some(printers::Printers::new(
                Arc::new(spool),
                Arc::new(move |msg: &FromAgent| send(&w, msg)),
            ))
        }
        Err(e) => {
            tracing::warn!("printer redirection unavailable: {e}");
            None
        }
    };
    // Audio: one stream at a time, stopped by a flag specific to each open.
    let mut audio_stop: Option<Arc<AtomicBool>> = None;
    let mut reader = stream;
    loop {
        let msg: ToAgent = match read_msg(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("protocol: {e}");
                break;
            }
        };
        match msg {
            ToAgent::Key { scancode, pressed } => {
                let _ = d.input.lock().expect("input mutex").key(scancode, pressed);
            }
            ToAgent::KeyboardLayout { layout, variant } => {
                let layout = oxydesk_core::XkbLayout { layout, variant };
                if let Err(e) = d
                    .input
                    .lock()
                    .expect("input mutex")
                    .set_keyboard_layout(&layout)
                {
                    eprintln!("keyboard layout: {e}");
                }
            }
            ToAgent::PointerMove { x, y } => {
                let _ = d.input.lock().expect("input mutex").pointer_move(x, y);
            }
            ToAgent::PointerButton { button: b, pressed } => {
                if let Some(b) = button(b) {
                    let _ = d
                        .input
                        .lock()
                        .expect("input mutex")
                        .pointer_button(b, pressed);
                }
            }
            ToAgent::Wheel { dx, dy } => {
                let _ = d.input.lock().expect("input mutex").wheel(dx, dy);
            }
            ToAgent::SetLayout { outputs } => {
                let layout = convert::layout_of(&outputs);
                let _round = d.round.lock().expect("round mutex");
                let result = d
                    .capture
                    .lock()
                    .expect("capture mutex")
                    .apply_layout(&layout);
                let ok = result.is_ok();
                if let Err(e) = &result {
                    tracing::warn!("layout of {} output(s): {e}", outputs.len());
                } else {
                    d.outputs.store(outputs.len() as u32, Ordering::SeqCst);
                    d.resized.store(true, Ordering::SeqCst);
                }
                send(
                    &writer,
                    &FromAgent::LayoutApplied {
                        ok,
                        outputs: d.outputs.load(Ordering::SeqCst),
                    },
                );
            }
            ToAgent::Resize { width, height } => {
                let _round = d.round.lock().expect("round mutex");
                let result = d
                    .capture
                    .lock()
                    .expect("capture mutex")
                    .resize(width, height);
                if let Err(e) = &result {
                    tracing::warn!("resize {width}x{height}: {e}");
                } else {
                    d.resized.store(true, Ordering::SeqCst);
                }
                send(&writer, &FromAgent::Resized { ok: result.is_ok() });
            }
            ToAgent::SetPixels { raw } => {
                if d.raw.swap(raw, Ordering::SeqCst) != raw {
                    tracing::info!(raw, "pixel mode");
                    // Every output sends its last frame again in the new form (a static desktop sends nothing otherwise).
                    keyframes
                        .iter()
                        .for_each(|k| k.store(true, Ordering::Relaxed));
                }
            }
            ToAgent::SetBitrate { bitrate_bps } => {
                d.bitrate.store(bitrate_bps, Ordering::Relaxed);
            }
            ToAgent::Hold { output, held } => {
                if let Some(h) = holds.get(output as usize) {
                    h.store(hold_stamp(held, now_ms()), Ordering::Relaxed);
                }
            }
            ToAgent::RequestKeyframe { output } => {
                if let Some(k) = keyframes.get(output as usize) {
                    k.store(true, Ordering::Relaxed);
                }
            }
            ToAgent::ClipboardSet(text) => {
                if let Err(e) = d.clipboard.lock().expect("clipboard mutex").set_text(&text) {
                    tracing::warn!("clipboard: {e}");
                }
            }
            ToAgent::ClipboardImageSet(png) => {
                if let Err(e) = d.clipboard.lock().expect("clipboard mutex").set_image(&png) {
                    tracing::warn!("clipboard: {e}");
                }
            }
            ToAgent::FilesSet {
                list_id,
                files: infos,
            } => {
                let w = writer.clone();
                let send_range: RangeSender = Arc::new(move |r: RangeRequest| {
                    send(
                        &w,
                        &FromAgent::FileRead(FileRange {
                            req_id: r.req_id,
                            list_id: r.list_id,
                            index: r.index,
                            offset: r.offset,
                            len: r.len,
                        }),
                    )
                });
                let list = files.remote_list(
                    list_id,
                    infos.into_iter().map(from_info).collect(),
                    send_range,
                );
                if let Err(e) = d.clipboard.lock().expect("clipboard mutex").set_files(list) {
                    tracing::warn!("clipboard: {e}");
                }
            }
            ToAgent::FileRead(r) => {
                // Off the reader thread: the answer may take a while (disk), and the answers to our own
                // requests arrive through this thread.
                let (files, w) = (files.clone(), writer.clone());
                std::thread::spawn(move || {
                    let data = files.read_served(r.list_id, r.index, r.offset, r.len);
                    send(
                        &w,
                        &FromAgent::FileData {
                            req_id: r.req_id,
                            ok: data.is_some(),
                            data: data.unwrap_or_default(),
                        },
                    );
                });
            }
            ToAgent::FileData { req_id, ok, data } => files.complete(req_id, ok.then_some(data)),
            ToAgent::AudioOpen {
                sample_rate,
                channels,
            } => {
                if let Some(s) = audio_stop.take() {
                    s.store(true, Ordering::Relaxed);
                }
                if let Some(provider) = d.audio.clone() {
                    let flag = Arc::new(AtomicBool::new(false));
                    audio_stop = Some(flag.clone());
                    let w = writer.clone();
                    let params = AudioParams {
                        sample_rate,
                        channels,
                    };
                    // Stop if the connection drops (`stop`) or if this stream is closed (`flag`).
                    let global = stop.clone();
                    let both = Arc::new(AtomicBool::new(false));
                    let both2 = both.clone();
                    std::thread::spawn(move || {
                        while !global.load(Ordering::Relaxed) && !flag.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        both2.store(true, Ordering::Relaxed);
                    });
                    threads.push(std::thread::spawn(move || {
                        audio_loop(provider, params, w, both)
                    }));
                }
            }
            ToAgent::DriveAdded { drive_id, name } => {
                if let Some(drives) = &drives {
                    drives.added(drive_id, name);
                }
            }
            ToAgent::DriveRemoved { drive_id } => {
                if let Some(drives) = &drives {
                    drives.removed(drive_id);
                }
            }
            ToAgent::DriveReply { req_id, result } => {
                if let Some(drives) = &drives {
                    drives.pending.complete(req_id, result);
                }
            }
            ToAgent::PrinterAdded {
                printer_id,
                name,
                driver,
                is_default,
                format,
            } => {
                if let Some(printers) = &printers {
                    printers.added(printer_id, name, driver, is_default, format);
                }
            }
            ToAgent::PrinterRemoved { printer_id } => {
                if let Some(printers) = &printers {
                    printers.removed(printer_id);
                }
            }
            ToAgent::PrintReply { req_id, result } => {
                if let Some(printers) = &printers {
                    printers.pending.complete(req_id, result);
                }
            }
            ToAgent::AudioClose => {
                if let Some(s) = audio_stop.take() {
                    s.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    files.fail_all();
    if let Some(drives) = &drives {
        drives.close();
    }
    if let Some(printers) = &printers {
        printers.close();
    }
    if let Some(s) = audio_stop {
        s.store(true, Ordering::Relaxed);
    }
    let _ = writer.lock().map(|s| s.shutdown(std::net::Shutdown::Both));
    for t in threads {
        let _ = t.join();
    }
}

/// systemd stops the agent with SIGTERM, which skips destructors, and `PAMName` puts the session's processes in
/// the logind scope rather than in the service's cgroup: `KillMode=control-group` does not reach them. Led by the
/// agent, the whole process group (KWin, plasmashell, applications) is therefore ended with it.
fn end_group_on_sigterm() {
    extern "C" fn handler(_: libc::c_int) {
        // SAFETY: only async-signal-safe calls. Default action restored first: the signal sent to the group
        // also ends this process.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::kill(0, libc::SIGTERM);
            libc::_exit(0);
        }
    }
    // SAFETY: getpid/getpgrp have no preconditions; the handler is async-signal-safe.
    unsafe {
        // Not leader (started from a shell or a test): the group is not ours to end.
        if libc::getpgrp() == libc::getpid() {
            libc::signal(
                libc::SIGTERM,
                handler as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    end_group_on_sigterm();
    let runtime = runtime_dir();
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        // SAFETY: called at startup, before any other thread is created.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime) };
    }
    // The systemd user manager (session bus) starts in parallel with the logind session.
    let bus = runtime.join("bus");
    let deadline = Instant::now() + Duration::from_secs(30);
    while arg::<String>(&args, "--session")
        .as_deref()
        .unwrap_or("kwin")
        == "kwin"
        && !bus.exists()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(200));
    }
    let desktop = Arc::new(build_desktop(&args)?);
    let socket: PathBuf =
        arg(&args, "--socket").unwrap_or_else(|| runtime.join("oxydesk-agent.sock"));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    // Only the user (owner) and root can connect; authd (root) passes the descriptor.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %socket.display(), "agent ready");
    // Current connection: the new one replaces the old one.
    let mut current: Option<(UnixStream, std::thread::JoinHandle<()>)> = None;
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        if let Some((old, handle)) = current.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
            let _ = handle.join();
        }
        let Ok(handle_stream) = stream.try_clone() else {
            continue;
        };
        let d = desktop.clone();
        tracing::info!("client connected");
        current = Some((handle_stream, std::thread::spawn(move || serve(d, stream))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_agent_proto::{FileInfo, Rect};

    /// A synthetic desktop served over a socket pair: the server side is the test.
    struct Conn {
        server: UnixStream,
        agent: Option<std::thread::JoinHandle<()>>,
    }

    impl Conn {
        fn start(extra: &[&str]) -> Self {
            let mut args: Vec<String> = [
                "--session",
                "synthetic",
                "--width",
                "320",
                "--height",
                "240",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
            args.extend(extra.iter().map(|s| (*s).to_owned()));
            let desktop = Arc::new(build_desktop(&args).expect("synthetic desktop"));
            let (server, agent_side) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let agent = std::thread::spawn(move || serve(desktop, agent_side));
            Self {
                server,
                agent: Some(agent),
            }
        }

        fn say(&mut self, msg: ToAgent) {
            write_msg(&mut self.server, &msg).unwrap();
        }

        fn next(&mut self) -> FromAgent {
            let msg = read_msg::<_, FromAgent>(&mut self.server)
                .expect("readable")
                .expect("connection still open");
            // The agent stops after a fatal error: say why instead of timing out on the next read.
            if let FromAgent::Fatal(why) = &msg {
                panic!("the agent failed: {why}");
            }
            msg
        }

        /// The first message the predicate picks, skipping the others (the video never stops).
        fn until<T>(&mut self, mut pick: impl FnMut(&FromAgent) -> Option<T>) -> T {
            for _ in 0..2000 {
                let msg = self.next();
                if let Some(v) = pick(&msg) {
                    return v;
                }
            }
            panic!("the expected message never came");
        }

        /// Closes the connection and waits for the agent to wind down.
        fn finish(mut self) {
            self.server.shutdown(std::net::Shutdown::Both).unwrap();
            self.agent.take().unwrap().join().expect("serve returned");
        }
    }

    fn rect(x: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn a_connection_starts_with_the_size_of_the_desktop() {
        let mut c = Conn::start(&["--outputs", "2"]);
        assert_eq!(
            c.next(),
            FromAgent::Hello {
                outputs: 2,
                width: 320,
                height: 240
            }
        );
        c.finish();
    }

    #[test]
    fn video_starts_with_a_keyframe_on_every_output() {
        let mut c = Conn::start(&["--outputs", "2", "--encoder", "openh264"]);
        let mut seen = [false; 2];
        c.until(|m| {
            if let FromAgent::Video {
                output, keyframe, ..
            } = m
            {
                if *keyframe {
                    seen[*output as usize] = true;
                }
            }
            seen.iter().all(|s| *s).then_some(())
        });
        c.finish();
    }

    #[test]
    fn asking_for_raw_pixels_switches_the_stream_and_back() {
        let mut c = Conn::start(&["--encoder", "openh264"]);
        c.say(ToAgent::SetPixels { raw: true });
        let (w, h, len) = c.until(|m| match m {
            FromAgent::VideoRaw {
                width,
                height,
                data,
                ..
            } => Some((*width, *height, data.len())),
            _ => None,
        });
        assert_eq!(len, (w * h * 4) as usize);
        c.say(ToAgent::SetPixels { raw: false });
        c.until(|m| matches!(m, FromAgent::Video { keyframe: true, .. }).then_some(()));
        c.finish();
    }

    #[test]
    fn a_resize_is_answered_and_a_refused_one_says_so() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::Resize {
            width: 640,
            height: 480,
        });
        assert_eq!(
            c.until(|m| matches!(m, FromAgent::Resized { .. }).then(|| m.clone())),
            FromAgent::Resized { ok: true }
        );
        c.finish();

        // With two outputs a single size makes no sense: refused, and the connection goes on.
        let mut c = Conn::start(&["--outputs", "2"]);
        c.say(ToAgent::Resize {
            width: 640,
            height: 480,
        });
        assert_eq!(
            c.until(|m| matches!(m, FromAgent::Resized { .. }).then(|| m.clone())),
            FromAgent::Resized { ok: false }
        );
        c.finish();
    }

    #[test]
    fn a_new_layout_changes_the_number_of_outputs_and_a_bad_one_changes_nothing() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::SetLayout {
            outputs: vec![rect(0, 320, 240), rect(320, 320, 240)],
        });
        assert_eq!(
            c.until(|m| matches!(m, FromAgent::LayoutApplied { .. }).then(|| m.clone())),
            FromAgent::LayoutApplied {
                ok: true,
                outputs: 2
            }
        );
        // The second output now delivers frames.
        c.until(|m| matches!(m, FromAgent::Video { output: 1, .. }).then_some(()));
        c.say(ToAgent::SetLayout { outputs: vec![] });
        assert_eq!(
            c.until(|m| matches!(m, FromAgent::LayoutApplied { .. }).then(|| m.clone())),
            FromAgent::LayoutApplied {
                ok: false,
                outputs: 2
            }
        );
        c.finish();
    }

    #[test]
    fn the_clipboard_goes_to_the_desktop_and_comes_back() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::ClipboardSet("héllo".into()));
        let text = c.until(|m| match m {
            FromAgent::ClipboardText(t) => Some(t.clone()),
            _ => None,
        });
        assert_eq!(text, "héllo");
        c.say(ToAgent::ClipboardImageSet(vec![1, 2, 3]));
        let png = c.until(|m| match m {
            FromAgent::ClipboardImage(p) => Some(p.clone()),
            _ => None,
        });
        assert_eq!(png, [1, 2, 3]);
        c.finish();
    }

    #[test]
    fn a_file_selection_of_the_server_is_announced_to_the_desktop_without_failing() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::FilesSet {
            list_id: 3,
            files: vec![FileInfo {
                path: "a.txt".into(),
                size: 4,
                is_dir: false,
                modified: 0,
                read_only: false,
            }],
        });
        // A read of a list the agent does not serve is answered negatively, not ignored.
        c.say(ToAgent::FileRead(FileRange {
            req_id: 9,
            list_id: 99,
            index: 0,
            offset: 0,
            len: 4,
        }));
        let (ok, data) = c.until(|m| match m {
            FromAgent::FileData {
                req_id: 9,
                ok,
                data,
            } => Some((*ok, data.clone())),
            _ => None,
        });
        assert!(!ok && data.is_empty());
        c.finish();
    }

    #[test]
    fn audio_flows_after_open_and_stops_after_close() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::AudioOpen {
            sample_rate: 44_100,
            channels: 2,
        });
        let len = c.until(|m| match m {
            FromAgent::Audio(b) => Some(b.len()),
            _ => None,
        });
        assert_eq!(
            len,
            AudioParams {
                sample_rate: 44_100,
                channels: 2
            }
            .bytes_for_ms(AUDIO_PACKET_MS)
        );
        c.say(ToAgent::AudioClose);
        c.finish();
    }

    #[test]
    fn without_audio_an_open_request_is_ignored() {
        let mut c = Conn::start(&["--audio", "off"]);
        c.say(ToAgent::AudioOpen {
            sample_rate: 44_100,
            channels: 2,
        });
        c.say(ToAgent::RequestKeyframe { output: 0 });
        // The keyframe request is still served, and nothing audio comes.
        for _ in 0..50 {
            assert!(!matches!(c.next(), FromAgent::Audio(_)));
        }
        c.finish();
    }

    #[test]
    fn input_and_unknown_requests_do_not_disturb_the_connection() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::Key {
            scancode: 0x1E,
            pressed: true,
        });
        c.say(ToAgent::PointerMove { x: 5, y: 6 });
        c.say(ToAgent::PointerButton {
            button: 9,
            pressed: true,
        });
        c.say(ToAgent::PointerButton {
            button: 0,
            pressed: false,
        });
        c.say(ToAgent::Wheel { dx: 0, dy: -120 });
        c.say(ToAgent::Hold {
            output: 0,
            held: true,
        });
        // Out-of-range outputs are ignored rather than indexing out of bounds.
        c.say(ToAgent::Hold {
            output: 99,
            held: true,
        });
        c.say(ToAgent::RequestKeyframe { output: 99 });
        c.say(ToAgent::SetBitrate {
            bitrate_bps: 2_000_000,
        });
        c.say(ToAgent::Hold {
            output: 0,
            held: false,
        });
        c.say(ToAgent::RequestKeyframe { output: 0 });
        c.until(|m| matches!(m, FromAgent::Video { keyframe: true, .. }).then_some(()));
        c.finish();
    }

    #[test]
    fn drive_and_printer_messages_for_nothing_registered_are_harmless() {
        let mut c = Conn::start(&[]);
        c.say(ToAgent::DriveRemoved { drive_id: 5 });
        c.say(ToAgent::PrinterRemoved { printer_id: 5 });
        c.say(ToAgent::RequestKeyframe { output: 0 });
        c.until(|m| matches!(m, FromAgent::Video { .. }).then_some(()));
        c.finish();
    }

    #[test]
    fn a_new_server_replaces_the_previous_one_on_the_same_desktop() {
        let args: Vec<String> = [
            "--session",
            "synthetic",
            "--width",
            "320",
            "--height",
            "240",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let desktop = Arc::new(build_desktop(&args).unwrap());
        for _ in 0..2 {
            let (mut server, agent_side) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let d = desktop.clone();
            let handle = std::thread::spawn(move || serve(d, agent_side));
            // Whatever the previous server asked for, this one starts from the H.264 stream.
            assert!(matches!(
                read_msg::<_, FromAgent>(&mut server).unwrap(),
                Some(FromAgent::Hello { .. })
            ));
            write_msg(&mut server, &ToAgent::SetPixels { raw: true }).unwrap();
            server.shutdown(std::net::Shutdown::Both).unwrap();
            handle.join().unwrap();
            assert!(desktop.raw.load(Ordering::SeqCst));
        }
        let (mut server, agent_side) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let d = desktop.clone();
        let handle = std::thread::spawn(move || serve(d, agent_side));
        let _ = read_msg::<_, FromAgent>(&mut server).unwrap();
        assert!(!desktop.raw.load(Ordering::SeqCst));
        server.shutdown(std::net::Shutdown::Both).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_broken_stream_ends_the_connection() {
        let mut c = Conn::start(&[]);
        use std::io::Write as _;
        // A frame announcing an absurd length.
        c.server.write_all(&[0xFF; 16]).unwrap();
        c.until(|m| matches!(m, FromAgent::Hello { .. }).then_some(()));
        c.agent.take().unwrap().join().expect("serve returned");
    }

    #[test]
    fn unknown_sessions_and_missing_kwin_are_refused_at_startup() {
        let err = build_desktop(&["--session".into(), "x11".into()])
            .err()
            .unwrap();
        assert!(err.to_string().contains("unknown --session: x11"));
        assert!(build_desktop(&["--outputs".into(), "0".into()]).is_err());
    }
}
