//! Capture d'une sortie KWin : `zkde_screencast_unstable_v1` (Wayland) → flux PipeWire (SHM).
//!
//! Le processus doit être autorisé par KWin (ADR 004). Un thread dédié possède la connexion Wayland (qui doit
//! rester ouverte tant que le flux vit) et la boucle PipeWire ; il dépose la dernière frame dans une boîte aux
//! lettres à un seul élément (drop, pas de file).

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
use proto::zkde_screencast_stream_unstable_v1::{self as stream_ev, ZkdeScreencastStreamUnstableV1};
use proto::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

/// `zkde_screencast_unstable_v1.pointer` : curseur absent de l'image. Le client affiche son curseur
/// local (sans latence) ; l'incruster le dupliquait avec une traînée. La forme du curseur (mode
/// metadata + pointer PDU RDP) reste à faire.
const POINTER_HIDDEN: u32 = 1;

#[derive(Default)]
struct Wl {
    screencast: Option<ZkdeScreencastUnstableV1>,
    outputs: Vec<wl_output::WlOutput>,
    node: Option<u32>,
    failed: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Wl {
    fn event(state: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "zkde_screencast_unstable_v1" => state.screencast = Some(reg.bind(name, version.min(5), qh, ())),
                "wl_output" => state.outputs.push(reg.bind(name, version.min(4), qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Wl {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZkdeScreencastUnstableV1, ()> for Wl {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1, _: proto::zkde_screencast_unstable_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for Wl {
    fn event(state: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: stream_ev::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            stream_ev::Event::Created { node } => state.node = Some(node),
            stream_ev::Event::Failed { error } => state.failed = Some(error),
            stream_ev::Event::Closed => state.failed = Some("flux fermé par KWin".into()),
        }
    }
}

type Mailbox = Arc<Mutex<Option<Frame>>>;

pub struct KwinCapture {
    mailbox: Mailbox,
    stop: Arc<AtomicBool>,
    dead: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl KwinCapture {
    /// `socket` : nom du socket Wayland (relatif à `XDG_RUNTIME_DIR`) ou `None` pour l'environnement.
    /// `output` : index de la sortie `wl_output` à capturer.
    pub fn start(socket: Option<&str>, output: usize) -> Result<Self, CaptureError> {
        let conn = connect(socket)?;
        let mailbox: Mailbox = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let dead: Arc<Mutex<Option<String>>> = Arc::default();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let worker = {
            let (mailbox, stop, dead) = (mailbox.clone(), stop.clone(), dead.clone());
            std::thread::Builder::new()
                .name("kwin-capture".into())
                .spawn(move || {
                    if let Err(e) = run(conn, output, mailbox, stop, &ready_tx) {
                        let _ = ready_tx.send(Err(e.clone()));
                        *dead.lock().expect("dead") = Some(e);
                    }
                })
                .map_err(|e| CaptureError::Unavailable(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self { mailbox, stop, dead, worker: Some(worker) }),
            Ok(Err(e)) => Err(CaptureError::Unavailable(e)),
            Err(_) => Err(CaptureError::Unavailable("KWin n'a pas fourni de flux dans le délai".into())),
        }
    }
}

impl CaptureSource for KwinCapture {
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
            let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| err("XDG_RUNTIME_DIR absent".into()))?;
            let stream = UnixStream::connect(Path::new(&dir).join(name)).map_err(|e| err(format!("socket Wayland {name} : {e}")))?;
            Connection::from_socket(stream)
        }
        None => Connection::connect_to_env(),
    }
    .map_err(|e| err(format!("connexion Wayland : {e}")))
}

struct PwData {
    format: VideoInfoRaw,
    mailbox: Mailbox,
}

fn run(conn: Connection, output: usize, mailbox: Mailbox, stop: Arc<AtomicBool>, ready: &mpsc::Sender<Result<(), String>>) -> Result<(), String> {
    // 1. Wayland : demander le flux et obtenir le nœud PipeWire.
    let mut queue = conn.new_event_queue::<Wl>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut wl = Wl::default();
    queue.roundtrip(&mut wl).map_err(|e| format!("registre Wayland : {e}"))?;
    let screencast = wl
        .screencast
        .clone()
        .ok_or("zkde_screencast_unstable_v1 non annoncé : exécutable non autorisé par KWin (ADR 004)")?;
    let wl_output = wl.outputs.get(output).ok_or_else(|| format!("sortie {output} introuvable ({} sortie(s))", wl.outputs.len()))?.clone();
    let _stream = screencast.stream_output(&wl_output, POINTER_HIDDEN, &qh, ());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while wl.node.is_none() && wl.failed.is_none() {
        if std::time::Instant::now() > deadline {
            return Err("délai dépassé en attendant le nœud PipeWire".into());
        }
        queue.roundtrip(&mut wl).map_err(|e| format!("Wayland : {e}"))?;
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(e) = wl.failed.take() {
        return Err(format!("KWin a refusé le flux : {e}"));
    }
    let node = wl.node.expect("node");

    // 2. PipeWire : consommer le flux (SHM mappé).
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("pipewire mainloop : {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("pipewire context : {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connexion PipeWire : {e}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "rdsktp-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("flux PipeWire : {e}"))?;

    let ready_ok = ready.clone();
    let announced = std::cell::Cell::new(false);
    let _listener = stream
        .add_local_listener_with_user_data(PwData { format: Default::default(), mailbox })
        .state_changed(|_, _, old, new| tracing_log(format_args!("pipewire: {old:?} -> {new:?}")))
        .param_changed(|_, d, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((mt, st)) = spa::param::format_utils::parse_format(param) {
                if mt == spa::param::format::MediaType::Video && st == spa::param::format::MediaSubtype::Raw {
                    let _ = d.format.parse(param);
                }
            }
        })
        .process(move |stream, d| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
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
            // Tampon vide = mise à jour du curseur seule : on garde la dernière image.
            if size == 0 || stride <= 0 || sz.width == 0 || sz.height == 0 {
                return;
            }
            let Some(src) = data.data() else { return };
            let need = stride as usize * sz.height as usize;
            if src.len() < offset + need {
                return;
            }
            let frame = Frame {
                output: OutputId(0),
                width: sz.width,
                height: sz.height,
                stride: stride as u32,
                format: pixel,
                data: FrameData::Shm(src[offset..offset + need].to_vec()),
            };
            *d.mailbox.lock().expect("mailbox") = Some(frame);
            if !announced.replace(true) {
                let _ = ready_ok.send(Ok(()));
            }
        })
        .register()
        .map_err(|e| format!("listener PipeWire : {e}"))?;

    let formats = [VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA];
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
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
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .map_err(|e| format!("pod : {e:?}"))?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or("pod invalide")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connexion au nœud {node} : {e}"))?;

    // Arrêt coopératif : un timer vérifie l'indicateur toutes les 100 ms.
    let ml = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100)));
    mainloop.run();
    drop(conn);
    Ok(())
}

fn tracing_log(args: std::fmt::Arguments<'_>) {
    eprintln!("{args}");
}
