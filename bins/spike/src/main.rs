//! Spike M1 : source synthétique -> openh264 (AVC420) -> ironrdp-server (EGFX) -> client RDP.
//! Usage : spike --cert cert.pem --key key.pem [--addr 127.0.0.1:3390] [--width W --height H --fps F --seconds S]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    Credentials, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent, ServerEventSender, TlsIdentityCtx,
};
use rdsktp_capture::SyntheticSource;
use rdsktp_core::ports::FrameEncoder;
use rdsktp_encode::{OpenH264Config, OpenH264Encoder};
use tokio::sync::mpsc::UnboundedSender;

const NO_KEY: usize = usize::MAX;

#[derive(Default)]
struct Shared {
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    gfx: Mutex<Option<GfxServerHandle>>,
    key: AtomicUsize,
}

struct Handler;

impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu) {
        eprintln!("[egfx] caps client : {pdu:?}");
    }
    fn on_ready(&mut self, negotiated: &ironrdp_egfx::pdu::CapabilitySet) {
        eprintln!("[egfx] prêt, caps négociées : {negotiated:?}");
    }
}

struct Factory(Arc<Shared>);

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0.sender.lock().unwrap() = Some(sender);
    }
}

impl GfxServerFactory for Factory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler)
    }
    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(Handler))));
        *self.0.gfx.lock().unwrap() = Some(server.clone());
        Some((GfxDvcBridge::new(server.clone()), server))
    }
}

struct Input(Arc<Shared>);

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if let KeyboardEvent::Pressed {
            code,
            extended: false,
        } = event
        {
            if (0x02..=0x09).contains(&code) {
                self.0.key.store((code - 2) as usize, Ordering::SeqCst);
            }
        }
    }
    fn mouse(&mut self, _event: MouseEvent) {}
}

struct Display(DesktopSize);

struct NoUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for NoUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        self.0
    }
    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(NoUpdates))
    }
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
}

/// Producteur : capture -> encodage -> EGFX, à cadence fixe, avec abandon si backpressure.
async fn produce(shared: Arc<Shared>, w: u16, h: u16, fps: f32, seconds: u64) {
    let mut src = SyntheticSource::new(w as u32, h as u32);
    let mut enc = OpenH264Encoder::new(&OpenH264Config {
        fps,
        ..Default::default()
    })
    .expect("encodeur");
    let mut ticker = tokio::time::interval(Duration::from_secs_f32(1.0 / fps));
    let (mut sent, mut dropped, mut bytes) = (0u64, 0u64, 0usize);
    let mut enc_ms: Vec<f64> = Vec::new();
    let mut current: Option<(GfxServerHandle, Option<u16>)> = None;
    let t0 = Instant::now();
    let mut last_report = Instant::now();
    loop {
        ticker.tick().await;
        if seconds > 0 && t0.elapsed().as_secs() >= seconds {
            break;
        }
        let Some(handle) = shared.gfx.lock().unwrap().clone() else {
            continue;
        };
        if !current
            .as_ref()
            .is_some_and(|(h, _)| Arc::ptr_eq(h, &handle))
        {
            current = Some((handle.clone(), None)); // nouvelle connexion
            enc.force_keyframe();
        }
        let (handle, surface) = current.as_mut().unwrap();
        {
            let mut g = handle.lock().unwrap();
            if !g.is_ready() || !g.supports_avc420() {
                continue;
            }
            if surface.is_none() {
                let id = g.create_surface(w, h).expect("surface");
                g.map_surface_to_output(id, 0, 0);
                *surface = Some(id);
            }
            if g.should_backpressure() {
                dropped += 1;
                continue;
            }
        }
        let key = shared.key.swap(NO_KEY, Ordering::SeqCst);
        if key != NO_KEY {
            src.set_marker(key);
        }
        let frame = src.render();
        let t = Instant::now();
        let out = enc.encode(&frame).expect("encode");
        enc_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        bytes += out.data.len();
        let (msgs, chan) = {
            let mut g = handle.lock().unwrap();
            let region = Avc420Region::full_frame(w, h, 22);
            if g.send_avc420_frame(
                surface.unwrap(),
                &out.data,
                &[region],
                t0.elapsed().as_millis() as u32,
            )
            .is_none()
            {
                dropped += 1;
                continue;
            }
            (g.drain_output(), g.channel_id())
        };
        sent += 1;
        let Some(chan) = chan else { continue };
        let svc = encode_dvc_messages(chan, msgs, ironrdp_svc::ChannelFlags::empty())
            .expect("encode dvc");
        if let Some(tx) = shared.sender.lock().unwrap().as_ref() {
            let _ = tx.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages: svc,
            }));
        }
        if last_report.elapsed() >= Duration::from_secs(2) {
            enc_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| {
                enc_ms[((q * enc_ms.len() as f64).ceil() as usize).clamp(1, enc_ms.len()) - 1]
            };
            eprintln!(
                "[stats] {:.0}s envoyées={sent} abandonnées(backpressure)={dropped} {:.0} Kio/s enc p50={:.1}ms p95={:.1}ms",
                t0.elapsed().as_secs_f64(), bytes as f64 / 1024.0 / 2.0, p(0.5), p(0.95)
            );
            (bytes, enc_ms, last_report) = (0, Vec::new(), Instant::now());
        }
    }
    eprintln!("[spike] fin : {sent} frames envoyées, {dropped} abandonnées");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cert: PathBuf = arg(&args, "--cert").expect("--cert requis");
    let key: PathBuf = arg(&args, "--key").expect("--key requis");
    let addr: SocketAddr =
        arg(&args, "--addr").unwrap_or_else(|| "127.0.0.1:3390".parse().unwrap());
    let (w, h): (u16, u16) = (
        arg(&args, "--width").unwrap_or(1920),
        arg(&args, "--height").unwrap_or(1080),
    );
    let fps: f32 = arg(&args, "--fps").unwrap_or(30.0);
    let seconds: u64 = arg(&args, "--seconds").unwrap_or(0);

    let shared = Arc::new(Shared::default());
    shared.key.store(NO_KEY, Ordering::SeqCst);
    let ctx = TlsIdentityCtx::init_from_paths(&cert, &key)?;
    let tls = ctx.make_acceptor()?;
    let nla_user: Option<String> = arg(&args, "--nla-user");
    let builder = RdpServer::builder().with_addr(addr);
    let builder = match &nla_user {
        // NLA/CredSSP : identifiants préchargés (le mot de passe ne passe que par l'environnement).
        Some(_) => builder.with_hybrid(tls, ctx.pub_key.clone()),
        None => builder.with_tls(tls),
    };
    let mut server = builder
        .with_input_handler(Input(shared.clone()))
        .with_display_handler(Display(DesktopSize {
            width: w,
            height: h,
        }))
        .with_gfx_factory(Some(Box::new(Factory(shared.clone()))))
        .build();
    if let Some(username) = nla_user {
        let password = std::env::var("RDSKTP_SPIKE_PASSWORD")
            .expect("RDSKTP_SPIKE_PASSWORD requis avec --nla-user");
        server.set_credentials(Some(Credentials {
            username,
            password,
            domain: None,
        }));
    }
    eprintln!("[spike] écoute sur {addr}, {w}x{h}@{fps}");
    tokio::spawn(produce(shared, w, h, fps, seconds));
    server.run().await
}
