use std::net::SocketAddr;
use std::path::PathBuf;
use std::ops::Not;
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
use rdsktp_core::error::{AudioError, LoginError};
use rdsktp_core::ports::{AudioParams, AudioProvider, AudioSource, ClipboardBackend, DesktopPorts, InputSink, SessionBroker};
use tokio::sync::mpsc::UnboundedSender;

use crate::clipboard::{announce, LocalText, TextClipboard};
use crate::pump::{self, PumpParams, PumpPorts, Shared};
use crate::sound::SoundFactory;

pub struct ServerConfig {
    pub addr: SocketAddr,
    /// Screen layout; the RDP desktop is their bounding rectangle.
    pub layout: Layout,
    pub fps: f32,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    /// NLA/CredSSP with preloaded credentials; otherwise TLS only.
    pub nla: Option<(String, String)>,
}

/// Outgoing adapters of a desktop, provided by the composition root (fixed mode) or by a `SessionBroker`.
pub type Ports = DesktopPorts;

type Slot<T> = Arc<Mutex<Option<T>>>;

/// Port slots of the current session, filled when a session is installed (at startup
/// in fixed mode, after a successful login in broker mode) and read by the IronRDP handlers.
#[derive(Clone)]
struct SessionSlot {
    shared: Arc<Shared>,
    input: Slot<Box<dyn InputSink>>,
    clipboard: Slot<Box<dyn ClipboardBackend>>,
    audio: Slot<Arc<dyn AudioProvider>>,
}

impl SessionSlot {
    fn new(shared: Arc<Shared>) -> Self {
        Self { shared, input: Default::default(), clipboard: Default::default(), audio: Default::default() }
    }

    fn install(&self, ports: Ports) {
        // `shared.gfx` is left untouched: the EGFX channel of the current connection may already exist (it is created
        // before credential validation). The pump restarts from scratch on every new session.
        *self.input.lock().expect("input mutex") = Some(ports.input);
        *self.clipboard.lock().expect("clipboard mutex") = Some(ports.clipboard);
        *self.audio.lock().expect("audio mutex") = ports.audio;
        *self.shared.ports.lock().expect("ports mutex") = Some(PumpPorts { capture: ports.capture, encoders: ports.encoders });
    }
}

/// Audio provider delegating to the current session.
struct SlotAudio(Slot<Arc<dyn AudioProvider>>);

impl AudioProvider for SlotAudio {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        let provider = self.0.lock().expect("audio mutex").clone();
        provider.ok_or_else(|| AudioError::Backend("no audio session".into()))?.open(params)
    }
}

/// Validates the received credentials (TLS mode) by opening the user's session through the broker.
struct BrokerValidator {
    broker: Arc<dyn SessionBroker>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl ironrdp_server::CredentialValidator for BrokerValidator {
    async fn validate(&self, credentials: &Credentials) -> Result<ironrdp_server::CredentialDecision, ironrdp_server::CredentialValidationError> {
        use ironrdp_server::CredentialDecision;
        let (user, pass) = (credentials.username.clone(), credentials.password.clone());
        let broker = self.broker.clone();
        let outcome = tokio::task::spawn_blocking(move || broker.login(&user, &pass))
            .await
            .map_err(ironrdp_server::CredentialValidationError::new)?;
        match outcome {
            Ok(ports) => {
                self.slot.install(ports);
                Ok(CredentialDecision::Accept)
            }
            Err(LoginError::Denied) => {
                tracing::warn!(user = %credentials.username, "connection refused");
                Ok(CredentialDecision::Reject)
            }
            Err(LoginError::Backend(e)) => {
                tracing::error!("login: {e}");
                Err(ironrdp_server::CredentialValidationError::new(std::io::Error::other(e)))
            }
        }
    }
}

struct GfxHandler;

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        tracing::debug!("egfx caps client: {pdu:?}");
    }
    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        tracing::info!("egfx ready: {negotiated:?}");
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
    clipboard: Slot<Box<dyn ClipboardBackend>>,
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
            if let Some(backend) = clipboard.lock().expect("clipboard mutex").as_mut() {
                if let Err(e) = backend.set_text(&text) {
                    tracing::warn!("clipboard: {e}");
                }
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

struct Input(Slot<Box<dyn InputSink>>);

impl Input {
    /// Applies `f` to the input port of the current session (ignored while no session is open).
    fn with<R>(&self, f: impl FnOnce(&mut dyn InputSink) -> R) -> Option<R> {
        self.0.lock().expect("input mutex").as_mut().map(|i| f(i.as_mut()))
    }
}

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let (code, extended, pressed) = match event {
            KeyboardEvent::Pressed { code, extended } => (code, extended, true),
            KeyboardEvent::Released { code, extended } => (code, extended, false),
            _ => return,
        };
        let scancode = u16::from(code) | if extended { 0xE000 } else { 0 };
        if let Some(Err(e)) = self.with(|i| i.key(scancode, pressed)) {
            tracing::warn!("input: {e}");
        }
    }
    fn mouse(&mut self, event: MouseEvent) {
        use rdsktp_core::ports::PointerButton as B;
        let r = self.with(|i| match event {
            MouseEvent::Move { x, y } => i.pointer_move(i32::from(x), i32::from(y)),
            MouseEvent::LeftPressed => i.pointer_button(B::Left, true),
            MouseEvent::LeftReleased => i.pointer_button(B::Left, false),
            MouseEvent::RightPressed => i.pointer_button(B::Right, true),
            MouseEvent::RightReleased => i.pointer_button(B::Right, false),
            MouseEvent::MiddlePressed => i.pointer_button(B::Middle, true),
            MouseEvent::MiddleReleased => i.pointer_button(B::Middle, false),
            MouseEvent::Button4Pressed => i.pointer_button(B::Back, true),
            MouseEvent::Button4Released => i.pointer_button(B::Back, false),
            MouseEvent::Button5Pressed => i.pointer_button(B::Forward, true),
            MouseEvent::Button5Released => i.pointer_button(B::Forward, false),
            MouseEvent::VerticalScroll { value } => i.wheel(0, i32::from(value)),
            MouseEvent::Scroll { x, y } => i.wheel(x, y),
            MouseEvent::RelMove { .. } => Ok(()),
        });
        if let Some(Err(e)) = r {
            tracing::warn!("input: {e}");
        }
    }
}

struct Display(DesktopSize);
struct NoUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for NoUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        // Everything goes through EGFX: no classic bitmap update.
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

/// Runs the RDP server with a fixed desktop (test mode, autologin) until it stops.
pub async fn run(config: ServerConfig, ports: Ports) -> anyhow::Result<()> {
    serve(config, Some(ports), None).await
}

/// Runs the RDP server in login mode: each connection is authenticated (TLS mode, credentials from the
/// ClientInfoPdu) and opens its session through the `SessionBroker`. Incompatible with NLA (see ADR 008).
pub async fn run_with_broker(config: ServerConfig, broker: Arc<dyn SessionBroker>) -> anyhow::Result<()> {
    anyhow::ensure!(config.nla.is_none(), "login mode is incompatible with NLA");
    serve(config, None, Some(broker)).await
}

async fn serve(config: ServerConfig, initial: Option<Ports>, broker: Option<Arc<dyn SessionBroker>>) -> anyhow::Result<()> {
    // ring AND aws-lc-rs are in the dependency tree: choose explicitly (already installed = ok).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let shared = Arc::new(Shared::default());
    let local: LocalText = Arc::new(Mutex::new(None));
    let slot = SessionSlot::new(shared.clone());
    let clipboard = slot.clipboard.clone();
    // The audio channel is announced as soon as the server can offer one: always in login mode, otherwise depending on the port.
    let audio_enabled = broker.is_some() || initial.as_ref().is_some_and(|p| p.audio.is_some());
    if let Some(ports) = initial {
        slot.install(ports);
    }

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
        .with_input_handler(Input(slot.input.clone()))
        .with_display_handler(Display(DesktopSize {
            width: desktop.0 as u16,
            height: desktop.1 as u16,
        }))
        .with_gfx_factory(Some(Box::new(GfxFactory(shared.clone()))))
        .with_cliprdr_factory(Some(Box::new(clip_factory)))
        .with_sound_factory(audio_enabled.then(|| {
            Box::new(SoundFactory { shared: shared.clone(), provider: Arc::new(SlotAudio(slot.audio.clone())) })
                as Box<dyn ironrdp_server::SoundServerFactory>
        }))
        .with_credential_validator(broker.map(|broker| Arc::new(BrokerValidator { broker, slot: slot.clone() }) as Arc<dyn ironrdp_server::CredentialValidator>))
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
        PumpParams {
            layout: config.layout.clone(),
            fps: config.fps,
            event: std::env::var("RDSKTP_PUMP").is_ok_and(|v| v == "tick").not(),
        },
    );
    // Local -> remote text: poll the port, announce the delayed copy.
    let poller_shared = shared.clone();
    let poller = std::thread::Builder::new()
        .name("clipboard-poll".into())
        .spawn(move || {
            while !poller_shared.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let text = match clipboard.lock().expect("clipboard mutex").as_mut() {
                    Some(c) => c.poll_text(),
                    None => continue,
                };
                match text {
                    Ok(Some(t)) => announce(&send, &local, t),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
            }
        })?;

    tracing::info!(addr = %config.addr, "RDP listening");
    let result = server.run().await;
    shared.stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    let _ = poller.join();
    result
}
