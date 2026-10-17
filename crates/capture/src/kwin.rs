//! Capture of a KWin output: `zkde_screencast_unstable_v1` (Wayland) → PipeWire stream (SHM).
//!
//! The process must be authorized by KWin (ADR 004). A dedicated thread owns the Wayland connection (which must
//! stay open as long as the stream lives) and the PipeWire loop; it stores the latest frame in a single-slot
//! mailbox (drop, no queue).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::{properties::properties, spa};
use rdsktp_core::error::CaptureError;
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::CaptureSource;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

mod proto {
    #![allow(clippy::all, non_camel_case_types, non_upper_case_globals, unused)]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}
use proto::zkde_screencast_stream_unstable_v1::{
    self as stream_ev, ZkdeScreencastStreamUnstableV1,
};
use proto::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

/// `zkde_screencast_unstable_v1.pointer`: the cursor is not in the image (the client draws its own, with no
/// latency; embedding it duplicated it with a trail). `metadata`: its bitmap comes next to the frames, as the
/// PipeWire cursor metadata, and goes to the client as the RDP pointer shape.
const POINTER_HIDDEN: u32 = 1;
const POINTER_METADATA: u32 = 4;

/// Pointer mode of the streams: the shape is wanted unless `RDSKTP_POINTER=hidden`.
fn pointer_mode() -> u32 {
    match std::env::var("RDSKTP_POINTER").as_deref() {
        Ok("hidden") => POINTER_HIDDEN,
        _ => POINTER_METADATA,
    }
}

#[derive(Default)]
struct Wl {
    screencast: Option<ZkdeScreencastUnstableV1>,
    outputs: Vec<wl_output::WlOutput>,
    /// Position (x, y) of each output, in registry announcement order.
    positions: Vec<(i32, i32)>,
    /// Name (`Virtual-0`...) of each output, in registry announcement order (empty until announced).
    names: Vec<String>,
    node: Option<u32>,
    failed: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Wl {
    fn event(
        state: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "zkde_screencast_unstable_v1" => {
                    state.screencast = Some(reg.bind(name, version.min(5), qh, ()))
                }
                "wl_output" => {
                    let index = state.outputs.len();
                    state
                        .outputs
                        .push(reg.bind(name, version.min(4), qh, index));
                    state.positions.push((0, 0));
                    state.names.push(String::new());
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, usize> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_output::Event::Geometry { x, y, .. } => state.positions[*index] = (x, y),
            wl_output::Event::Name { name } => state.names[*index] = name,
            _ => {}
        }
    }
}
impl Dispatch<ZkdeScreencastUnstableV1, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: proto::zkde_screencast_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        ev: stream_ev::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            stream_ev::Event::Created { node } => state.node = Some(node),
            stream_ev::Event::Failed { error } => state.failed = Some(error),
            stream_ev::Event::Closed => state.failed = Some("stream closed by KWin".into()),
        }
    }
}

type Mailbox = Arc<Mutex<Option<Frame>>>;
type Cursor = Arc<Mutex<Option<rdsktp_core::cursor::CursorUpdate>>>;
type Waker = Arc<Mutex<Option<Arc<rdsktp_core::wake::Wake>>>>;

/// Source factory of a `MultiCapture` over a KWin session: the outputs the session started with (`base` of them)
/// are captured again by name, beyond that they are virtual outputs that vanish with their capture (KWin 6.7).
pub fn source_factory(socket: String, base: usize, dmabuf: bool) -> crate::multi::SourceFactory {
    Box::new(move |i, o| {
        let name = format!("Virtual-{i}");
        let capture = if i < base {
            KwinCapture::start_named(Some(&socket), &name, dmabuf)
        } else {
            KwinCapture::start_virtual(Some(&socket), &name, o.width, o.height, dmabuf)
        };
        capture.map(|c| Box::new(c) as Box<dyn CaptureSource>)
    })
}

/// What a capture stream is bound to.
#[derive(Debug, Clone)]
enum Target {
    /// The output of this left-to-right rank (startup).
    Rank(usize),
    /// The existing output of this name.
    Named(String),
    /// A new virtual output, created with the stream and destroyed with it.
    Virtual {
        name: String,
        width: u32,
        height: u32,
    },
}

pub struct KwinCapture {
    mailbox: Mailbox,
    cursor: Cursor,
    waker: Waker,
    stop: Arc<AtomicBool>,
    dead: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
    socket: Option<String>,
    /// Name of the output in KWin (`kscreen-doctor`).
    name: String,
    target: Target,
}

impl KwinCapture {
    /// `socket`: Wayland socket name (relative to `XDG_RUNTIME_DIR`) or `None` for the environment.
    /// `output`: index of the `wl_output` to capture.
    pub fn start(socket: Option<&str>, output: usize) -> Result<Self, CaptureError> {
        Self::start_with(socket, output, false)
    }

    /// `dmabuf`: also offer linear DMA-BUF buffers (delivered as `FrameData::DmaBuf` when KWin accepts;
    /// otherwise the stream stays on shared memory). Only useful with a GPU encoder that imports them.
    pub fn start_with(
        socket: Option<&str>,
        output: usize,
        dmabuf: bool,
    ) -> Result<Self, CaptureError> {
        Self::start_target(
            socket,
            Target::Rank(output),
            format!("Virtual-{output}"),
            dmabuf,
        )
    }

    /// Captures the existing output `name` (enabled again if it had been disabled).
    pub fn start_named(
        socket: Option<&str>,
        name: &str,
        dmabuf: bool,
    ) -> Result<Self, CaptureError> {
        let _ = run_kscreen_doctor(socket, &[&format!("output.{name}.enable")]);
        Self::start_target(
            socket,
            Target::Named(name.to_owned()),
            name.to_owned(),
            dmabuf,
        )
    }

    /// Creates a virtual output of this size (KWin 6.7: `stream_virtual_output`), which exists as long as
    /// the capture. KWin prefixes the requested name with `Virtual-`: `name` is the resulting one.
    pub fn start_virtual(
        socket: Option<&str>,
        name: &str,
        width: u32,
        height: u32,
        dmabuf: bool,
    ) -> Result<Self, CaptureError> {
        let target = Target::Virtual {
            name: name.strip_prefix("Virtual-").unwrap_or(name).to_owned(),
            width,
            height,
        };
        Self::start_target(socket, target, name.to_owned(), dmabuf)
    }

    fn start_target(
        socket: Option<&str>,
        target: Target,
        name: String,
        dmabuf: bool,
    ) -> Result<Self, CaptureError> {
        let conn = connect(socket)?;
        let mailbox: Mailbox = Arc::default();
        let cursor: Cursor = Arc::default();
        let waker: Waker = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let dead: Arc<Mutex<Option<String>>> = Arc::default();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Option<String>, String>>();
        let worker = {
            let (mailbox, cursor, waker, stop, dead) = (
                mailbox.clone(),
                cursor.clone(),
                waker.clone(),
                stop.clone(),
                dead.clone(),
            );
            let target = target.clone();
            std::thread::Builder::new()
                .name("kwin-capture".into())
                .spawn(move || {
                    if let Err(e) = run(
                        conn, &target, dmabuf, mailbox, cursor, waker, stop, &ready_tx,
                    ) {
                        let _ = ready_tx.send(Err(e.clone()));
                        *dead.lock().expect("dead") = Some(e);
                    }
                })
                .map_err(|e| CaptureError::Unavailable(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(found)) => Ok(Self {
                mailbox,
                cursor,
                waker,
                stop,
                dead,
                worker: Some(worker),
                socket: socket.map(str::to_owned),
                // A stream bound by rank captures the output of that rank, whatever its name: kscreen-doctor must
                // be told the name of the output really captured.
                name: found.unwrap_or(name),
                target,
            }),
            Ok(Err(e)) => Err(CaptureError::Unavailable(e)),
            Err(_) => Err(CaptureError::Unavailable(
                "KWin did not provide a stream in time".into(),
            )),
        }
    }
}

impl CaptureSource for KwinCapture {
    fn set_waker(&mut self, wake: Arc<rdsktp_core::wake::Wake>) {
        *self.waker.lock().expect("waker") = Some(wake);
    }

    fn take_cursor(&mut self) -> Option<rdsktp_core::cursor::CursorUpdate> {
        let update = self.cursor.lock().expect("cursor").take()?;
        rdsktp_core::cursor::resolve(update)
    }

    /// The virtual output gets a custom mode of the requested size (`kscreen-doctor`, libkscreen); the
    /// PipeWire stream then renegotiates its format and delivers frames of the new size. Custom modes
    /// need a recent KWin (measured: absent from Plasma 6.3.6, present in 6.7.5); `kscreen-doctor` reports
    /// failures on its output with a zero exit status, so the result is checked on the output geometry.
    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        let name = self.name.clone();
        // Adding an existing mode fails harmlessly; selecting the mode is what decides.
        let _ = self.kscreen_doctor(&[&format!(
            "output.{name}.addCustomMode.{width}.{height}.60000.reduced"
        )]);
        self.kscreen_doctor(&[&format!("output.{name}.mode.{width}x{height}@60")])?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let listing = self.kscreen_doctor(&["-o"])?;
            let size = parse_output_size(&listing, &name);
            if size == Some((width, height)) {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(CaptureError::Unavailable(format!(
                    "KWin cannot set {width}x{height} on {name} (now {size:?}, outputs {:?}): custom modes are missing in Plasma 6.3 (present in 6.7)",
                    output_names(&listing)
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Old-size frame: dropped. A frame of the new size may already be there (a static desktop sends no other).
        let mut mailbox = self.mailbox.lock().expect("mailbox");
        if mailbox
            .as_ref()
            .is_some_and(|f| (f.width, f.height) != (width, height))
        {
            mailbox.take();
        }
        Ok(())
    }

    /// An output of the session stays in KWin when its capture stops: it is disabled. A virtual output
    /// vanishes with its stream.
    fn retire(&mut self) {
        if !matches!(self.target, Target::Virtual { .. }) {
            if let Err(e) = self.kscreen_doctor(&[&format!("output.{}.disable", self.name)]) {
                tracing_log(format_args!("cannot disable {}: {e}", self.name));
            }
        }
    }

    fn place(&mut self, x: i32, y: i32) -> Result<(), CaptureError> {
        let name = self.name.clone();
        tracing_log(format_args!("placing {name} at {x},{y}"));
        let command = format!("output.{name}.position.{x},{y}");
        // The session is busy while the shell starts: the order is given again when KWin has not acted on it.
        let start = std::time::Instant::now();
        let mut resent = false;
        self.kscreen_doctor(&[&command])?;
        loop {
            let listing = self.kscreen_doctor(&["-o"])?;
            if parse_output_position(&listing, &name) == Some((x, y)) {
                return Ok(());
            }
            let waited = start.elapsed();
            if waited > Duration::from_secs(10) {
                tracing_log(format_args!(
                    "{name} not at {x},{y}: {:?}",
                    parse_output_position(&listing, &name)
                ));
                return Err(CaptureError::Unavailable(format!(
                    "KWin did not move {name} to {x},{y}"
                )));
            }
            if !resent && waited > Duration::from_secs(3) {
                resent = true;
                self.kscreen_doctor(&[&command])?;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn output_name(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn place_group(&mut self, moves: &[(String, i32, i32)]) -> Result<(), CaptureError> {
        let commands: Vec<String> = moves
            .iter()
            .map(|(name, x, y)| format!("output.{name}.position.{x},{y}"))
            .collect();
        tracing_log(format_args!("placing {moves:?}"));
        let args: Vec<&str> = commands.iter().map(String::as_str).collect();
        self.kscreen_doctor(&args)?;
        let start = std::time::Instant::now();
        let mut resent = false;
        loop {
            let listing = self.kscreen_doctor(&["-o"])?;
            let wrong: Vec<_> = moves
                .iter()
                .filter(|(name, x, y)| parse_output_position(&listing, name) != Some((*x, *y)))
                .map(|(name, _, _)| name.clone())
                .collect();
            if wrong.is_empty() {
                return Ok(());
            }
            let waited = start.elapsed();
            if waited > Duration::from_secs(10) {
                return Err(CaptureError::Unavailable(format!(
                    "KWin did not move {wrong:?} to {moves:?}"
                )));
            }
            if !resent && waited > Duration::from_secs(3) {
                resent = true;
                self.kscreen_doctor(&args)?;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn set_primary(&mut self) -> Result<(), CaptureError> {
        let name = self.name.clone();
        tracing_log(format_args!("{name} becomes the primary output"));
        self.kscreen_doctor(&[&format!("output.{name}.primary")])
            .map(|_| ())
    }

    fn recv_latest(&mut self, _output: OutputId) -> Result<Option<Frame>, CaptureError> {
        if let Some(frame) = self.mailbox.lock().expect("mailbox").take() {
            return Ok(Some(frame));
        }
        match self.dead.lock().expect("dead").as_ref() {
            Some(e) => Err(CaptureError::Unavailable(e.clone())),
            None => Ok(None),
        }
    }
}

impl KwinCapture {
    /// Runs `kscreen-doctor` against the captured session; returns its standard output (colors removed).
    fn kscreen_doctor(&self, args: &[&str]) -> Result<String, CaptureError> {
        run_kscreen_doctor(self.socket.as_deref(), args)
    }
}

fn run_kscreen_doctor(socket: Option<&str>, args: &[&str]) -> Result<String, CaptureError> {
    let mut cmd = std::process::Command::new("kscreen-doctor");
    if let Some(socket) = socket {
        cmd.env("WAYLAND_DISPLAY", socket);
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| CaptureError::Unavailable(format!("kscreen-doctor: {e}")))?;
    if !out.status.success() {
        return Err(CaptureError::Unavailable(format!(
            "kscreen-doctor: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(strip_ansi(&String::from_utf8_lossy(&out.stdout)))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Names of the outputs in a `kscreen-doctor -o` listing.
/// Registry index of the output of this left-to-right then top-to-bottom rank.
fn output_of_rank(positions: &[(i32, i32)], rank: usize) -> Option<usize> {
    let mut order: Vec<usize> = (0..positions.len()).collect();
    order.sort_by_key(|&i| (positions[i].0, positions[i].1, i));
    order.get(rank).copied()
}

fn output_names(listing: &str) -> Vec<&str> {
    listing
        .lines()
        .filter_map(|l| l.strip_prefix("Output:"))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect()
}

/// Size read from the `Geometry: X,Y WxH` line of output `name` in a `kscreen-doctor -o` listing.
fn parse_output_size(listing: &str, name: &str) -> Option<(u32, u32)> {
    let block = listing
        .split("Output:")
        .find(|b| b.split_whitespace().nth(1) == Some(name))?;
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with("Geometry:"))?;
    let (w, h) = line.split_whitespace().last()?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Position read from the `Geometry: X,Y WxH` line of output `name` in a `kscreen-doctor -o` listing.
fn parse_output_position(listing: &str, name: &str) -> Option<(i32, i32)> {
    let block = listing
        .split("Output:")
        .find(|b| b.split_whitespace().nth(1) == Some(name))?;
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with("Geometry:"))?;
    let (x, y) = line.split_whitespace().nth(1)?.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

impl Drop for KwinCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn connect(socket: Option<&str>) -> Result<Connection, CaptureError> {
    let err = |m: String| CaptureError::Unavailable(m);
    match socket {
        Some(name) => {
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .ok_or_else(|| err("XDG_RUNTIME_DIR missing".into()))?;
            let stream = UnixStream::connect(Path::new(&dir).join(name))
                .map_err(|e| err(format!("Wayland socket {name}: {e}")))?;
            Connection::from_socket(stream)
        }
        None => Connection::connect_to_env(),
    }
    .map_err(|e| err(format!("Wayland connection: {e}")))
}

struct PwData {
    format: VideoInfoRaw,
    mailbox: Mailbox,
    cursor: Cursor,
    waker: Waker,
    /// Buffer metadata request (cursor), sent once the format is known.
    meta_pod: Vec<u8>,
}

/// Gives the buffer back to the stream when dropped.
struct Requeue<'a> {
    stream: &'a pw::stream::Stream,
    raw: *mut pw::sys::pw_buffer,
}

impl Drop for Requeue<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` was dequeued from `stream` and is requeued exactly once.
        unsafe { self.stream.queue_raw_buffer(self.raw) };
    }
}

/// The cursor metadata of a buffer, decoded; `None` without metadata or when it only moved the pointer.
///
/// # Safety
/// `buffer` must point to a valid `spa_buffer` whose metadata stay valid for the call.
unsafe fn cursor_update(
    buffer: *const spa::sys::spa_buffer,
) -> Option<rdsktp_core::cursor::CursorUpdate> {
    let b = &*buffer;
    if b.metas.is_null() {
        return None;
    }
    for meta in std::slice::from_raw_parts(b.metas, b.n_metas as usize) {
        if meta.type_ == spa::sys::SPA_META_Cursor && !meta.data.is_null() {
            let bytes = std::slice::from_raw_parts(meta.data.cast::<u8>(), meta.size as usize);
            return crate::cursor_meta::parse_cursor_meta(bytes);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn run(
    conn: Connection,
    target: &Target,
    dmabuf: bool,
    mailbox: Mailbox,
    cursor: Cursor,
    waker: Waker,
    stop: Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<Option<String>, String>>,
) -> Result<(), String> {
    // 1. Wayland: request the stream and obtain the PipeWire node.
    let mut queue = conn.new_event_queue::<Wl>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut wl = Wl::default();
    queue
        .roundtrip(&mut wl)
        .map_err(|e| format!("Wayland registry: {e}"))?;
    let screencast = wl.screencast.clone().ok_or(
        "zkde_screencast_unstable_v1 not announced: executable not authorized by KWin (ADR 004)",
    )?;
    // `geometry` events arrive after binding: a second roundtrip fetches them.
    queue
        .roundtrip(&mut wl)
        .map_err(|e| format!("output geometry: {e}"))?;
    let mut resolved: Option<String> = None;
    let _stream = match target {
        Target::Virtual {
            name,
            width,
            height,
        } => {
            if screencast.version() < 2 {
                return Err("virtual outputs need zkde_screencast version 2 (KWin 6.7)".into());
            }
            screencast.stream_virtual_output(
                name.clone(),
                *width as i32,
                *height as i32,
                1.0,
                pointer_mode(),
                &qh,
                (),
            )
        }
        Target::Named(name) => {
            let i = wl
                .names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| format!("output {name} not found ({:?})", wl.names))?;
            screencast.stream_output(&wl.outputs[i], pointer_mode(), &qh, ())
        }
        Target::Rank(output) => {
            // The logical index of an output is its left-to-right then top-to-bottom rank, not the registry order.
            let registry_index = output_of_rank(&wl.positions, *output).ok_or_else(|| {
                format!("output {output} not found ({} output(s))", wl.outputs.len())
            })?;
            resolved = wl
                .names
                .get(registry_index)
                .filter(|n| !n.is_empty())
                .cloned();
            screencast.stream_output(&wl.outputs[registry_index], pointer_mode(), &qh, ())
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while wl.node.is_none() && wl.failed.is_none() {
        if std::time::Instant::now() > deadline {
            return Err("timed out waiting for the PipeWire node".into());
        }
        queue
            .roundtrip(&mut wl)
            .map_err(|e| format!("Wayland: {e}"))?;
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(e) = wl.failed.take() {
        return Err(format!("KWin refused the stream: {e}"));
    }
    let node = wl.node.expect("node");

    // 2. PipeWire: consume the stream (mapped SHM).
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("pipewire mainloop : {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| format!("pipewire context : {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("PipeWire connection: {e}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "rdsktp-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("PipeWire stream: {e}"))?;

    // Room for the cursor bitmap in the buffers (KWin offers 256 px; larger ones than RDP's 96 px are dropped).
    let meta_bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
            id: spa::param::ParamType::Meta.as_raw(),
            properties: vec![
                spa::pod::Property::new(
                    spa::sys::SPA_PARAM_META_type,
                    spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor)),
                ),
                spa::pod::Property::new(
                    spa::sys::SPA_PARAM_META_size,
                    spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                        spa::utils::ChoiceFlags::empty(),
                        spa::utils::ChoiceEnum::Range {
                            default: crate::cursor_meta::meta_size(256) as i32,
                            min: crate::cursor_meta::meta_size(1) as i32,
                            max: crate::cursor_meta::meta_size(512) as i32,
                        },
                    ))),
                ),
            ],
        }),
    )
    .map_err(|e| format!("pod : {e:?}"))?
    .0
    .into_inner();
    let ready_ok = ready.clone();
    let announced = std::cell::Cell::new(false);
    let _listener = stream
        .add_local_listener_with_user_data(PwData {
            format: Default::default(),
            mailbox,
            cursor,
            waker,
            meta_pod: meta_bytes,
        })
        .state_changed(|_, _, old, new| tracing_log(format_args!("pipewire: {old:?} -> {new:?}")))
        .param_changed(|stream, d, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((mt, st)) = spa::param::format_utils::parse_format(param) {
                if mt == spa::param::format::MediaType::Video
                    && st == spa::param::format::MediaSubtype::Raw
                {
                    let _ = d.format.parse(param);
                    let size = d.format.size();
                    let buffers_bytes = buffers_pod(size.width, size.height);
                    if let (Some(meta), Some(buffers)) = (
                        Pod::from_bytes(&d.meta_pod),
                        Pod::from_bytes(&buffers_bytes),
                    ) {
                        stream.update_params(&mut [buffers, meta]).ok();
                    }
                }
            }
        })
        .process(move |stream, d| {
            // SAFETY: the buffer comes from this stream and goes back to it when `_requeue` drops.
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() {
                return;
            }
            let _requeue = Requeue { stream, raw };
            // SAFETY: `raw` is a valid buffer until it is requeued; its `spa_buffer` is checked for null.
            let spa_buffer = unsafe { (*raw).buffer };
            if spa_buffer.is_null() {
                return;
            }
            if let Some(update) = unsafe { cursor_update(spa_buffer) } {
                *d.cursor.lock().expect("cursor") = Some(update);
                if let Some(w) = d.waker.lock().expect("waker").as_ref() {
                    w.notify();
                }
            }
            // SAFETY: `datas` holds `n_datas` entries, valid like the buffer; `Data` wraps `spa_data`.
            let datas: &mut [spa::buffer::Data] = unsafe {
                if (*spa_buffer).n_datas == 0 || (*spa_buffer).datas.is_null() {
                    &mut []
                } else {
                    std::slice::from_raw_parts_mut(
                        (*spa_buffer).datas.cast(),
                        (*spa_buffer).n_datas as usize,
                    )
                }
            };
            let Some(data) = datas.first_mut() else {
                return;
            };
            let (size, stride, offset) = {
                let c = data.chunk();
                (c.size() as usize, c.stride(), c.offset() as usize)
            };
            let sz = d.format.size();
            let pixel = match d.format.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => PixelFormat::Bgra8,
                VideoFormat::RGBx | VideoFormat::RGBA => PixelFormat::Rgba8,
                _ => return,
            };
            // Empty buffer = cursor-only update: keep the last image.
            if size == 0 || stride <= 0 || sz.width == 0 || sz.height == 0 {
                return;
            }
            if data.type_() == spa::buffer::DataType::DmaBuf {
                // The descriptor is duplicated; the producer may reuse the buffer once it is requeued.
                let raw = data.as_raw();
                // SAFETY: `fd` of a DmaBuf-typed data is a valid descriptor owned by PipeWire for this buffer.
                let Ok(fd) = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw.fd as i32) }
                    .try_clone_to_owned()
                else {
                    return;
                };
                let frame = Frame {
                    output: OutputId(0),
                    width: sz.width,
                    height: sz.height,
                    stride: stride as u32,
                    format: pixel,
                    data: FrameData::DmaBuf(rdsktp_core::DmaBuf {
                        fd,
                        offset: offset as u32,
                        modifier: d.format.modifier(),
                    }),
                };
                *d.mailbox.lock().expect("mailbox") = Some(frame);
                if let Some(w) = d.waker.lock().expect("waker").as_ref() {
                    w.notify();
                }
                if !announced.replace(true) {
                    let _ = ready_ok.send(Ok(resolved.clone()));
                }
                return;
            }
            let Some(src) = data.data() else { return };
            let need = stride as usize * sz.height as usize;
            if src.len() < offset + need {
                return;
            }
            dump_debug_frame(
                &src[offset..offset + need],
                sz.width,
                sz.height,
                stride as usize,
                pixel,
            );
            let frame = Frame {
                output: OutputId(0),
                width: sz.width,
                height: sz.height,
                stride: stride as u32,
                format: pixel,
                data: FrameData::Shm(src[offset..offset + need].to_vec()),
            };
            *d.mailbox.lock().expect("mailbox") = Some(frame);
            if let Some(w) = d.waker.lock().expect("waker").as_ref() {
                w.notify();
            }
            if !announced.replace(true) {
                let _ = ready_ok.send(Ok(resolved.clone()));
            }
        })
        .register()
        .map_err(|e| format!("PipeWire listener: {e}"))?;

    let formats = [
        VideoFormat::BGRx,
        VideoFormat::BGRA,
        VideoFormat::RGBx,
        VideoFormat::RGBA,
    ];
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            formats[0],
            formats[0],
            formats[1],
            formats[2],
            formats[3]
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj.clone()),
    )
    .map_err(|e| format!("pod : {e:?}"))?
    .0
    .into_inner();
    // With DMA-BUF enabled, the linear-modifier variant comes first (preferred), SHM stays as a fallback.
    let mut obj_dma = obj.clone();
    obj_dma.properties.push(spa::pod::Property {
        key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
        // MANDATORY | DONT_FIXATE (the latter is behind a libspa feature flag).
        flags: spa::pod::PropertyFlags::from_bits_retain(
            spa::pod::PropertyFlags::MANDATORY.bits() | (1 << 4),
        ),
        value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            // DRM_FORMAT_MOD_LINEAR only: the CPU can still read it if the GPU import is unavailable.
            spa::utils::ChoiceEnum::Enum {
                default: 0i64,
                alternatives: vec![0i64],
            },
        ))),
    });
    let bytes_dma: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj_dma),
    )
    .map_err(|e| format!("pod : {e:?}"))?
    .0
    .into_inner();
    let mut params = vec![Pod::from_bytes(&bytes).ok_or("invalid pod")?];
    if dmabuf {
        params.insert(0, Pod::from_bytes(&bytes_dma).ok_or("invalid pod")?);
    }
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connecting to node {node}: {e}"))?;

    // Cooperative stop: a timer checks the flag every 100 ms.
    let ml = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
        }
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(100)),
        Some(Duration::from_millis(100)),
    );
    mainloop.run();
    drop(conn);
    Ok(())
}

/// Buffer requirements for CPU-readable frames (shared memory), repeated with the cursor metadata request.
fn buffers_pod(width: u32, height: u32) -> Vec<u8> {
    let (stride, size) = (width as i32 * 4, width as i32 * height as i32 * 4);
    let int = |k, v| spa::pod::Property::new(k, spa::pod::Value::Int(v));
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![
                spa::pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_buffers,
                    spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                        spa::utils::ChoiceFlags::empty(),
                        spa::utils::ChoiceEnum::Range {
                            default: 4,
                            min: 2,
                            max: 16,
                        },
                    ))),
                ),
                int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
                int(spa::sys::SPA_PARAM_BUFFERS_size, size),
                int(spa::sys::SPA_PARAM_BUFFERS_stride, stride),
                int(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr),
                ),
            ],
        }),
    )
    .map(|(c, _)| c.into_inner())
    .unwrap_or_default()
}

fn tracing_log(args: std::fmt::Arguments<'_>) {
    eprintln!("{args}");
}

/// Diagnostic: `RDSKTP_DUMP_FRAME=/path/img.ppm` writes the 20th received image (PPM) then goes quiet.
fn dump_debug_frame(src: &[u8], w: u32, h: u32, stride: usize, format: PixelFormat) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let Ok(path) = std::env::var("RDSKTP_DUMP_FRAME") else {
        return;
    };
    if COUNT.fetch_add(1, Ordering::Relaxed) != 20 {
        return;
    }
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for row in src.chunks_exact(stride).take(h as usize) {
        #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` is too recent for the MSRV
        for px in row[..w as usize * 4].chunks_exact(4) {
            match format {
                PixelFormat::Bgra8 => out.extend_from_slice(&[px[2], px[1], px[0]]),
                PixelFormat::Rgba8 => out.extend_from_slice(&px[..3]),
            }
        }
    }
    let _ = std::fs::write(path, out);
}

#[cfg(test)]
mod tests {
    #[test]
    fn rank_follows_positions_not_registry_order() {
        // Virtual-1 announced second but at the left: rank 0 is registry index 1 (its name, not "Virtual-0", is the one
        // of the captured output).
        let positions = [(1920, 0), (0, 0), (3840, 0)];
        assert_eq!(output_of_rank(&positions, 0), Some(1));
        assert_eq!(output_of_rank(&positions, 1), Some(0));
        assert_eq!(output_of_rank(&positions, 3), None);
    }

    use super::*;

    #[test]
    fn parses_output_size_from_kscreen_doctor_listing() {
        let listing = "Output: 1 Virtual-0 4611fd0e\n\tenabled\n\tModes:  1:1920x1080@60*!\n\tGeometry: 0,0 1024x576\n\tScale: 1\nOutput: 2 Virtual-1 abc\n\tGeometry: 1024,0 1920x1080\n";
        assert_eq!(parse_output_size(listing, "Virtual-0"), Some((1024, 576)));
        assert_eq!(parse_output_size(listing, "Virtual-1"), Some((1920, 1080)));
        assert_eq!(parse_output_size(listing, "Virtual-2"), None);
        assert_eq!(parse_output_position(listing, "Virtual-1"), Some((1024, 0)));
        assert_eq!(parse_output_position(listing, "Virtual-2"), None);
        assert_eq!(
            strip_ansi("\u{1b}[01;33mGeometry: \u{1b}[0;0m0,0 1x2"),
            "Geometry: 0,0 1x2"
        );
    }
}
