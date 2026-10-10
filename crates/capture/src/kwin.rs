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
use wayland_client::{Connection, Dispatch, QueueHandle};

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

/// `zkde_screencast_unstable_v1.pointer`: cursor absent from the image. The client displays its
/// local (no latency); embedding it duplicated it with a trail. The cursor shape (metadata
/// mode + RDP pointer PDU) remains to be done.
const POINTER_HIDDEN: u32 = 1;

#[derive(Default)]
struct Wl {
    screencast: Option<ZkdeScreencastUnstableV1>,
    outputs: Vec<wl_output::WlOutput>,
    /// Position (x, y) of each output, in registry announcement order.
    positions: Vec<(i32, i32)>,
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
        if let wl_output::Event::Geometry { x, y, .. } = ev {
            state.positions[*index] = (x, y);
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
type Waker = Arc<Mutex<Option<Arc<rdsktp_core::wake::Wake>>>>;

pub struct KwinCapture {
    mailbox: Mailbox,
    waker: Waker,
    stop: Arc<AtomicBool>,
    dead: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
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
        let conn = connect(socket)?;
        let mailbox: Mailbox = Arc::default();
        let waker: Waker = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let dead: Arc<Mutex<Option<String>>> = Arc::default();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let worker = {
            let (mailbox, waker, stop, dead) =
                (mailbox.clone(), waker.clone(), stop.clone(), dead.clone());
            std::thread::Builder::new()
                .name("kwin-capture".into())
                .spawn(move || {
                    if let Err(e) = run(conn, output, dmabuf, mailbox, waker, stop, &ready_tx) {
                        let _ = ready_tx.send(Err(e.clone()));
                        *dead.lock().expect("dead") = Some(e);
                    }
                })
                .map_err(|e| CaptureError::Unavailable(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                mailbox,
                waker,
                stop,
                dead,
                worker: Some(worker),
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
    waker: Waker,
}

fn run(
    conn: Connection,
    output: usize,
    dmabuf: bool,
    mailbox: Mailbox,
    waker: Waker,
    stop: Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
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
    // The logical index of an output is its left-to-right then top-to-bottom rank, not the registry order.
    let mut order: Vec<usize> = (0..wl.outputs.len()).collect();
    order.sort_by_key(|&i| (wl.positions[i].0, wl.positions[i].1, i));
    let registry_index = *order
        .get(output)
        .ok_or_else(|| format!("output {output} not found ({} output(s))", wl.outputs.len()))?;
    let wl_output = wl.outputs[registry_index].clone();
    let _stream = screencast.stream_output(&wl_output, POINTER_HIDDEN, &qh, ());
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

    let ready_ok = ready.clone();
    let announced = std::cell::Cell::new(false);
    let _listener = stream
        .add_local_listener_with_user_data(PwData {
            format: Default::default(),
            mailbox,
            waker,
        })
        .state_changed(|_, _, old, new| tracing_log(format_args!("pipewire: {old:?} -> {new:?}")))
        .param_changed(|_, d, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((mt, st)) = spa::param::format_utils::parse_format(param) {
                if mt == spa::param::format::MediaType::Video
                    && st == spa::param::format::MediaSubtype::Raw
                {
                    let _ = d.format.parse(param);
                }
            }
        })
        .process(move |stream, d| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
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
                    let _ = ready_ok.send(Ok(()));
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
                let _ = ready_ok.send(Ok(()));
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
