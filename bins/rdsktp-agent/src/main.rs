//! `rdsktp-agent`: runs as the logged-in user (unit `rdsktp-agent@<user>.service`).
//! Owns the KWin session, capture, H.264 encoding, input, clipboard and audio, and
//! exposes them to `rdsktp` through the `rdsktp-agent-proto` protocol on a Unix socket. One connection at a time:
//! the most recent one replaces the previous one (reconnection); the desktop session survives disconnections.

mod drives;
mod printers;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdsktp_agent_proto::{
    read_msg, write_msg, CursorImage, FileInfo, FileRange, FromAgent, ToAgent, MAX_OUTPUTS,
};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::{
    AudioParams, AudioProvider, CaptureSource, ClipboardBackend, FrameEncoder, InputSink,
    PointerButton,
};
use rdsktp_core::{FileEntry, FileExchange, RangeRequest, RangeSender};
use rdsktp_encode::{AutoEncoder, EncoderChoice};

const AUDIO_PACKET_MS: u32 = 20;

struct Desktop {
    capture: Mutex<Box<dyn CaptureSource>>,
    input: Mutex<Box<dyn InputSink>>,
    clipboard: Mutex<Box<dyn ClipboardBackend>>,
    audio: Option<Arc<dyn AudioProvider>>,
    /// Outputs captured now (changes with `SetLayout`).
    outputs: AtomicU32,
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
    /// Held by the video loop during a round and by a layout change: after the answer, no frame of the old
    /// layout is sent any more.
    round: Mutex<()>,
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
}

fn runtime_dir() -> PathBuf {
    // `%U` cannot be used in the unit: derive the directory from the effective uid.
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })))
}

fn build_desktop(args: &[String]) -> anyhow::Result<Desktop> {
    let session: String = arg(args, "--session").unwrap_or_else(|| "kwin".into());
    let width: u32 = arg(args, "--width").unwrap_or(1920);
    let height: u32 = arg(args, "--height").unwrap_or(1080);
    let outputs: u32 = arg(args, "--outputs").unwrap_or(1);
    anyhow::ensure!(
        (1..=16).contains(&outputs),
        "--outputs must be between 1 and 16"
    );
    let fps: f32 = arg(args, "--fps").unwrap_or(30.0);
    let audio_mode: String = arg(args, "--audio")
        .unwrap_or_else(|| if session == "kwin" { "pulse" } else { "tone" }.into());
    let audio: Option<Arc<dyn AudioProvider>> = match audio_mode.as_str() {
        "off" => None,
        "tone" => Some(Arc::new(rdsktp_audio::ToneProvider)),
        "pulse" => Some(Arc::new(rdsktp_audio::PulseCapture)),
        other => anyhow::bail!("unknown --audio: {other}"),
    };
    let encoder: EncoderChoice = arg::<String>(args, "--encoder")
        .map_or(Ok(EncoderChoice::Auto), |v| v.parse())
        .map_err(|e: String| anyhow::anyhow!(e))?;
    #[cfg_attr(not(feature = "kwin"), allow(unused_variables))]
    let dmabuf = match arg::<String>(args, "--dmabuf").as_deref() {
        Some("on") => true,
        Some("off") => false,
        None | Some("auto") => encoder.wants_dmabuf(),
        Some(other) => anyhow::bail!("unknown --dmabuf: {other} (auto|on|off)"),
    };
    let make = |capture: Box<dyn CaptureSource>,
                input: Box<dyn InputSink>,
                clipboard: Box<dyn ClipboardBackend>,
                keep: Box<dyn std::any::Any + Send>| Desktop {
        capture: Mutex::new(capture),
        input: Mutex::new(input),
        clipboard: Mutex::new(clipboard),
        audio: audio.clone(),
        outputs: AtomicU32::new(outputs),
        width,
        height,
        fps,
        encoder,
        event: arg::<String>(args, "--loop").is_none_or(|v| v != "tick"),
        poll_ms: arg(args, "--poll-ms"),
        min_interval_ms: arg(args, "--min-interval-ms").unwrap_or(16.0),
        _session: Mutex::new(keep),
        resized: AtomicBool::new(false),
        raw: AtomicBool::new(false),
        round: Mutex::new(()),
    };
    match session.as_str() {
        "synthetic" => {
            let d = rdsktp_capture::SyntheticDesktop::with_outputs(outputs, width, height);
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
                use rdsktp_core::ports::SessionProvider;
                if audio.is_some() {
                    if let Err(e) = rdsktp_audio::PulseCapture::ensure_sink() {
                        tracing::warn!("audio unavailable: {e}");
                    }
                }
                let socket: String =
                    arg(args, "--kwin-socket").unwrap_or_else(|| "rdsktp-0".into());
                let programs = args
                    .windows(2)
                    .filter(|w| w[0] == "--session-app")
                    .map(|w| w[1].split_whitespace().map(str::to_owned).collect())
                    .collect();
                let mut kwin =
                    rdsktp_session::KwinSession::new(rdsktp_session::KwinSessionConfig {
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
                        rdsktp_capture::KwinCapture::start_with(Some(&socket), i, dmabuf)
                            .map(|c| Box::new(c) as Box<dyn CaptureSource>)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let capture = rdsktp_capture::MultiCapture::new(sources).with_factory(
                    rdsktp_capture::source_factory(socket.clone(), outputs as usize, dmabuf),
                );
                let input = rdsktp_input::KwinFakeInput::connect(Some(&socket))?;
                let clipboard = rdsktp_clipboard::WaylandClipboard::connect(Some(&socket))?;
                // KWin that cannot resize its desktop (6.3): the client's size is obtained by scaling.
                let (capture, input) = rdsktp_scaling::wrap(Box::new(capture), Box::new(input));
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
        other => anyhow::bail!("unknown --session: {other}"),
    }
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(w: &Writer, msg: &FromAgent) -> bool {
    let mut guard = w.lock().expect("writer mutex");
    write_msg(&mut *guard, msg).is_ok() && guard.flush().is_ok()
}

fn button(b: u8) -> Option<PointerButton> {
    Some(match b {
        0 => PointerButton::Left,
        1 => PointerButton::Right,
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        _ => return None,
    })
}

/// Capture -> encode -> send, at a fixed rate. A keyframe is produced on connection and on request.
fn video_loop(d: Arc<Desktop>, w: Writer, stop: Arc<AtomicBool>, keyframes: Arc<Vec<AtomicBool>>) {
    let fps = d.fps;
    let mut encoders: Vec<Box<dyn FrameEncoder>> = Vec::new();
    let mut last: Vec<Option<rdsktp_core::Frame>> = Vec::new();
    let period = d.poll_ms.map_or_else(
        || Duration::from_secs_f32(1.0 / fps),
        |ms| Duration::from_secs_f32(ms / 1000.0),
    );
    let min_interval = Duration::from_secs_f32(d.min_interval_ms / 1000.0);
    let wake = Arc::new(rdsktp_core::wake::Wake::default());
    d.capture
        .lock()
        .expect("capture mutex")
        .set_waker(wake.clone());
    let mut next = Instant::now();
    let mut first = true;
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
                rdsktp_core::CursorUpdate::Hidden => None,
                rdsktp_core::CursorUpdate::Shape(s) => Some(CursorImage {
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
            encoders.push(Box::new(AutoEncoder::new(d.encoder, fps, 8_000_000)));
            keyframes[encoders.len() - 1].store(true, Ordering::Relaxed);
        }
        encoders.truncate(outputs);
        last.resize_with(outputs, || None);
        if d.resized.swap(false, Ordering::SeqCst) {
            last.iter_mut().for_each(|l| *l = None);
            keyframes
                .iter()
                .for_each(|k| k.store(true, Ordering::Relaxed));
        }
        for i in 0..outputs {
            let wanted_key = first || keyframes[i].swap(false, Ordering::Relaxed);
            if wanted_key {
                encoders[i].request_keyframe();
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
                let msg = match rdsktp_core::pixels::to_bgra(frame) {
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

fn to_info(e: &FileEntry) -> FileInfo {
    FileInfo {
        path: e.path.clone(),
        size: e.size,
        is_dir: e.is_dir,
        modified: e.modified,
        read_only: e.read_only,
    }
}

fn from_info(i: FileInfo) -> FileEntry {
    FileEntry {
        path: i.path,
        size: i.size,
        is_dir: i.is_dir,
        modified: i.modified,
        read_only: i.read_only,
    }
}

fn serve(d: Arc<Desktop>, stream: UnixStream) {
    let writer: Writer = match stream.try_clone() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            tracing::error!("socket: {e}");
            return;
        }
    };
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
    // The agent outlives the connections: every server starts with the H.264 stream.
    d.raw.store(false, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    // Ranges of the file lists exchanged with the server (the lists die with the connection).
    let files = Arc::new(FileExchange::default());
    let keyframes: Arc<Vec<AtomicBool>> =
        Arc::new((0..MAX_OUTPUTS).map(|_| AtomicBool::new(true)).collect());
    let mut threads = vec![{
        let (d, w, stop, kf) = (d.clone(), writer.clone(), stop.clone(), keyframes.clone());
        std::thread::spawn(move || video_loop(d, w, stop, kf))
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
    let drives = match rdsktp_drivefs::DriveMounts::new(runtime_dir().join("rdsktp-drives")) {
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
    let printers = match rdsktp_spool::PrinterSpool::new(
        runtime_dir().join(rdsktp_spool::SOCKET_DIR),
        rdsktp_spool::Cups::detect(&rdsktp_spool::current_user()),
    ) {
        Ok(spool) => {
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
                let layout = rdsktp_core::XkbLayout { layout, variant };
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
                let layout = rdsktp_core::layout::Layout {
                    outputs: outputs
                        .iter()
                        .enumerate()
                        .map(|(i, r)| rdsktp_core::layout::OutputInfo {
                            id: OutputId(i as u32),
                            x: r.x,
                            y: r.y,
                            width: r.width,
                            height: r.height,
                            primary: i == 0,
                        })
                        .collect(),
                };
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
                language,
            } => {
                if let Some(printers) = &printers {
                    printers.added(printer_id, name, driver, is_default, language);
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

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        arg(&args, "--socket").unwrap_or_else(|| runtime.join("rdsktp-agent.sock"));
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
