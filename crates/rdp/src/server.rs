use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{CliprdrBackend, CliprdrBackendFactory};
use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    CliprdrServerFactory, Credentials, DesktopSize, DisplayUpdate, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent, ServerEventSender, TlsIdentityCtx,
};
use rdsktp_core::layout::Layout;
use rdsktp_core::ports::{AudioProvider, CaptureSource, ClipboardBackend, FrameEncoder, InputSink};
use tokio::sync::mpsc::UnboundedSender;

use crate::clipboard::{announce, LocalText, TextClipboard};
use crate::pump::{self, PumpParams, Shared};
use crate::sound::SoundFactory;

pub struct ServerConfig {
    pub addr: SocketAddr,
    /// Disposition des écrans ; le bureau RDP est leur rectangle englobant.
    pub layout: Layout,
    pub fps: f32,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    /// NLA/CredSSP avec identifiants préchargés ; sinon TLS seul.
    pub nla: Option<(String, String)>,
}

/// Adaptateurs sortants fournis par le composition root.
pub struct Ports {
    pub capture: Box<dyn CaptureSource>,
    /// Un encodeur par sortie, dans l'ordre de `ServerConfig::layout`.
    pub encoders: Vec<Box<dyn FrameEncoder>>,
    pub input: Box<dyn InputSink>,
    pub clipboard: Box<dyn ClipboardBackend>,
    /// Audio sortant (RDPSND) ; `None` = pas de canal audio.
    pub audio: Option<Arc<dyn AudioProvider>>,
}

struct GfxHandler;

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        tracing::debug!("egfx caps client: {pdu:?}");
    }
    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        tracing::info!("egfx prêt: {negotiated:?}");
    }
}

struct GfxFactory(Arc<Shared>);

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0.sender.lock().expect("sender mutex") = Some(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler)
    }
    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(
            GfxHandler,
        ))));
        *self.0.gfx.lock().expect("gfx mutex") = Some(server.clone());
        Some((GfxDvcBridge::new(server.clone()), server))
    }
}

struct ClipFactory {
    shared: Arc<Shared>,
    local: LocalText,
    clipboard: Arc<Mutex<Box<dyn ClipboardBackend>>>,
}

impl ClipFactory {
    fn send_fn(&self) -> crate::clipboard::SendFn {
        let shared = self.shared.clone();
        Arc::new(move |msg| {
            if let Some(tx) = shared.sender.lock().expect("sender mutex").as_ref() {
                let _ = tx.send(ServerEvent::Clipboard(msg));
            }
        })
    }
}

impl ServerEventSender for ClipFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.shared.sender.lock().expect("sender mutex") = Some(sender);
    }
}

impl CliprdrBackendFactory for ClipFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let clipboard = self.clipboard.clone();
        let on_text = Arc::new(move |text: String| {
            if let Err(e) = clipboard.lock().expect("clipboard mutex").set_text(&text) {
                tracing::warn!("presse-papiers: {e}");
            }
        });
        Box::new(TextClipboard::new(
            self.send_fn(),
            self.local.clone(),
            on_text,
        ))
    }
}

impl CliprdrServerFactory for ClipFactory {}

struct Input(Box<dyn InputSink>);

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let (code, extended, pressed) = match event {
            KeyboardEvent::Pressed { code, extended } => (code, extended, true),
            KeyboardEvent::Released { code, extended } => (code, extended, false),
            _ => return,
        };
        let scancode = u16::from(code) | if extended { 0xE000 } else { 0 };
        if let Err(e) = self.0.key(scancode, pressed) {
            tracing::warn!("entrée: {e}");
        }
    }
    fn mouse(&mut self, event: MouseEvent) {
        use rdsktp_core::ports::PointerButton as B;
        let r = match event {
            MouseEvent::Move { x, y } => self.0.pointer_move(i32::from(x), i32::from(y)),
            MouseEvent::LeftPressed => self.0.pointer_button(B::Left, true),
            MouseEvent::LeftReleased => self.0.pointer_button(B::Left, false),
            MouseEvent::RightPressed => self.0.pointer_button(B::Right, true),
            MouseEvent::RightReleased => self.0.pointer_button(B::Right, false),
            MouseEvent::MiddlePressed => self.0.pointer_button(B::Middle, true),
            MouseEvent::MiddleReleased => self.0.pointer_button(B::Middle, false),
            MouseEvent::Button4Pressed => self.0.pointer_button(B::Back, true),
            MouseEvent::Button4Released => self.0.pointer_button(B::Back, false),
            MouseEvent::Button5Pressed => self.0.pointer_button(B::Forward, true),
            MouseEvent::Button5Released => self.0.pointer_button(B::Forward, false),
            MouseEvent::VerticalScroll { value } => self.0.wheel(0, i32::from(value)),
            MouseEvent::Scroll { x, y } => self.0.wheel(x, y),
            MouseEvent::RelMove { .. } => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!("entrée: {e}");
        }
    }
}

struct Display(DesktopSize);
struct NoUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for NoUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        // Tout passe par EGFX : aucune mise à jour bitmap classique.
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

/// Lance le serveur RDP jusqu'à son arrêt.
pub async fn run(config: ServerConfig, ports: Ports) -> anyhow::Result<()> {
    // ring ET aws-lc-rs sont dans l'arbre de dépendances : choisir explicitement (déjà installé = ok).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let shared = Arc::new(Shared::default());
    let local: LocalText = Arc::new(Mutex::new(None));
    let clipboard = Arc::new(Mutex::new(ports.clipboard));

    let desktop = config.layout.desktop_size();
    let ctx = TlsIdentityCtx::init_from_paths(&config.tls_cert, &config.tls_key)?;
    let tls = ctx.make_acceptor()?;
    let builder = RdpServer::builder().with_addr(config.addr);
    let builder = match &config.nla {
        Some(_) => builder.with_hybrid(tls, ctx.pub_key.clone()),
        None => builder.with_tls(tls),
    };
    let clip_factory = ClipFactory {
        shared: shared.clone(),
        local: local.clone(),
        clipboard: clipboard.clone(),
    };
    let send = clip_factory.send_fn();
    let mut server = builder
        .with_input_handler(Input(ports.input))
        .with_display_handler(Display(DesktopSize {
            width: desktop.0 as u16,
            height: desktop.1 as u16,
        }))
        .with_gfx_factory(Some(Box::new(GfxFactory(shared.clone()))))
        .with_cliprdr_factory(Some(Box::new(clip_factory)))
        .with_sound_factory(ports.audio.map(|provider| {
            Box::new(SoundFactory { shared: shared.clone(), provider }) as Box<dyn ironrdp_server::SoundServerFactory>
        }))
        .build();
    if let Some((username, password)) = config.nla.clone() {
        server.set_credentials(Some(Credentials {
            username,
            password,
            domain: None,
        }));
    }

    let pump = pump::spawn(
        shared.clone(),
        ports.capture,
        ports.encoders,
        PumpParams {
            layout: config.layout.clone(),
            fps: config.fps,
        },
    );
    // Texte local -> distant : on interroge le port, on annonce la copie différée.
    let poller_shared = shared.clone();
    let poller = std::thread::Builder::new()
        .name("clipboard-poll".into())
        .spawn(move || {
            while !poller_shared.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let text = clipboard.lock().expect("clipboard mutex").poll_text();
                match text {
                    Ok(Some(t)) => announce(&send, &local, t),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("presse-papiers: {e}"),
                }
            }
        })?;

    tracing::info!(addr = %config.addr, "écoute RDP");
    let result = server.run().await;
    shared.stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    let _ = poller.join();
    result
}
