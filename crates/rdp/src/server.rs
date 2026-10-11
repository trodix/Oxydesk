use std::net::SocketAddr;
use std::ops::Not;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{CliprdrBackend, CliprdrBackendFactory};
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitiesV107Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitiesV8Flags, CapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    CliprdrServerFactory, Credentials, DesktopSize, DisplayUpdate, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent, ServerEventSender, TlsIdentityCtx,
};
use rdsktp_core::error::{AudioError, LoginError};
use rdsktp_core::layout::{Layout, MonitorSpec};
use rdsktp_core::ports::{
    AudioParams, AudioProvider, AudioSource, ClipboardBackend, DesktopPorts, InputSink,
    SessionBroker,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::clipboard::{announce, Content, LocalContent, RdpClipboard};
use crate::drive::DriveFactory;
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
    /// Failed-login limits (login mode).
    pub throttle: rdsktp_core::ThrottleConfig,
    /// Format of the jobs of each redirected printer (ADR 015).
    pub printer_modes: rdsktp_core::PrinterModes,
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
    drives: Slot<Arc<dyn rdsktp_core::drive::DriveHost>>,
    printers: Slot<Arc<dyn rdsktp_core::printer::PrinterHost>>,
}

impl SessionSlot {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            input: Default::default(),
            clipboard: Default::default(),
            audio: Default::default(),
            drives: Default::default(),
            printers: Default::default(),
        }
    }

    fn install(&self, ports: Ports) {
        // `shared.gfx` is left untouched: the EGFX channel of the current connection may already exist (it is created
        // before credential validation). The pump restarts from scratch on every new session.
        *self.input.lock().expect("input mutex") = Some(ports.input);
        *self.clipboard.lock().expect("clipboard mutex") = Some(ports.clipboard);
        *self.audio.lock().expect("audio mutex") = ports.audio;
        *self.drives.lock().expect("drives mutex") = ports.drives;
        *self.printers.lock().expect("printers mutex") = ports.printers;
        *self.shared.ports.lock().expect("ports mutex") = Some(PumpPorts {
            capture: ports.capture,
            encoders: ports.encoders,
            make_encoder: ports.make_encoder,
        });
    }
}

/// Audio provider delegating to the current session.
struct SlotAudio(Slot<Arc<dyn AudioProvider>>);

impl AudioProvider for SlotAudio {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        let provider = self.0.lock().expect("audio mutex").clone();
        provider
            .ok_or_else(|| AudioError::Backend("no audio session".into()))?
            .open(params)
    }
}

/// Failed-login limits shared by the connection filter (which knows the peer) and the credential validator
/// (which does not). Connections are served one at a time, so the peer of the last accepted connection is the
/// one being validated.
#[derive(Clone)]
struct LoginGuard {
    throttle: Arc<Mutex<rdsktp_core::LoginThrottle>>,
    peer: Arc<Mutex<Option<std::net::IpAddr>>>,
}

impl LoginGuard {
    fn new(cfg: rdsktp_core::ThrottleConfig) -> Self {
        Self {
            throttle: Arc::new(Mutex::new(rdsktp_core::LoginThrottle::new(cfg))),
            peer: Arc::default(),
        }
    }

    fn peer(&self) -> Option<std::net::IpAddr> {
        *self.peer.lock().expect("peer mutex")
    }
}

/// Drops the connections of blocked addresses before any protocol exchange.
struct GuardHandler(LoginGuard);

impl ironrdp_server::ConnectionHandler for GuardHandler {
    fn on_accept(&mut self, peer: std::net::SocketAddr) -> bool {
        let ip = peer.ip();
        *self.0.peer.lock().expect("peer mutex") = Some(ip);
        let now = std::time::Instant::now();
        match self
            .0
            .throttle
            .lock()
            .expect("throttle mutex")
            .ip_blocked(ip, now)
        {
            Some(left) => {
                tracing::warn!(%ip, retry_in_secs = left.as_secs(), "connection dropped: too many failed logins");
                false
            }
            None => true,
        }
    }
}

/// Validates the received credentials (TLS mode) by opening the user's session through the broker.
struct BrokerValidator {
    broker: Arc<dyn SessionBroker>,
    slot: SessionSlot,
    guard: LoginGuard,
}

#[async_trait::async_trait]
impl ironrdp_server::CredentialValidator for BrokerValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<ironrdp_server::CredentialDecision, ironrdp_server::CredentialValidationError> {
        use ironrdp_server::CredentialDecision;
        let (user, pass) = (credentials.username.clone(), credentials.password.clone());
        let ip = self.guard.peer();
        let blocked = self.guard.throttle.lock().expect("throttle mutex").blocked(
            ip,
            &user,
            std::time::Instant::now(),
        );
        if let Some(left) = blocked {
            // Not even tried: the broker (PAM) is not asked while the block lasts.
            tracing::warn!(?ip, user = ?user, retry_in_secs = left.as_secs(), "login refused: blocked after too many failures");
            return Ok(CredentialDecision::Reject);
        }
        let broker = self.broker.clone();
        let outcome = tokio::task::spawn_blocking(move || broker.login(&user, &pass))
            .await
            .map_err(ironrdp_server::CredentialValidationError::new)?;
        match outcome {
            Ok(ports) => {
                self.guard
                    .throttle
                    .lock()
                    .expect("throttle mutex")
                    .record_success(ip, &credentials.username);
                tracing::info!(?ip, user = ?credentials.username, "login accepted");
                self.slot.install(ports);
                Ok(CredentialDecision::Accept)
            }
            Err(LoginError::Denied) => {
                let blocked = self
                    .guard
                    .throttle
                    .lock()
                    .expect("throttle mutex")
                    .record_failure(ip, &credentials.username, std::time::Instant::now());
                tracing::warn!(?ip, user = ?credentials.username, blocked, "login failed");
                Ok(CredentialDecision::Reject)
            }
            Err(LoginError::Backend(e)) => {
                tracing::error!("login: {e}");
                Err(ironrdp_server::CredentialValidationError::new(
                    std::io::Error::other(e),
                ))
            }
        }
    }
}

/// EGFX negotiation. The library intersects the client's flags with the server's: `AVC_DISABLED` is lost
/// when the server does not list it, and the client would be sent H.264 it declared it cannot decode. The
/// server's preferences therefore repeat the client's `AVC_DISABLED` of the same version.
#[derive(Default)]
struct GfxHandler {
    client: Vec<CapabilitySet>,
}

impl GfxHandler {
    fn avc_disabled(&self, version: impl Fn(&CapabilitySet) -> Option<bool>) -> bool {
        self.client.iter().filter_map(version).any(|off| off)
    }
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        tracing::debug!("egfx caps client: {pdu:?}");
        self.client = pdu
            .0
            .iter()
            .filter_map(|raw| raw.parsed().ok().flatten())
            .collect();
    }
    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        let off_107 = self.avc_disabled(|c| match c {
            CapabilitySet::V10_7 { flags } => {
                Some(flags.contains(CapabilitiesV107Flags::AVC_DISABLED))
            }
            _ => None,
        });
        let off_10 = self.avc_disabled(|c| match c {
            CapabilitySet::V10 { flags } => {
                Some(flags.contains(CapabilitiesV10Flags::AVC_DISABLED))
            }
            _ => None,
        });
        let mut v107 = CapabilitiesV107Flags::SMALL_CACHE;
        v107.set(CapabilitiesV107Flags::AVC_DISABLED, off_107);
        let mut v10 = CapabilitiesV10Flags::SMALL_CACHE;
        v10.set(CapabilitiesV10Flags::AVC_DISABLED, off_10);
        vec![
            CapabilitySet::V10_7 { flags: v107 },
            CapabilitySet::V10 { flags: v10 },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
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
        Box::new(GfxHandler::default())
    }
    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(
            GfxHandler::default(),
        ))));
        *self.0.gfx.lock().expect("gfx mutex") = Some(server.clone());
        Some((GfxDvcBridge::new(server.clone()), server))
    }
}

struct ClipFactory {
    shared: Arc<Shared>,
    local: LocalContent,
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
        let on_content = Arc::new(move |content: Content| {
            if let Some(backend) = clipboard.lock().expect("clipboard mutex").as_mut() {
                let result = match content {
                    Content::Text(t) => backend.set_text(&t),
                    Content::Image(png) => backend.set_image(&png),
                    Content::Files(list) => backend.set_files(list),
                };
                if let Err(e) = result {
                    tracing::warn!("clipboard: {e}");
                }
            }
        });
        Box::new(RdpClipboard::new(
            self.send_fn(),
            self.local.clone(),
            on_content,
        ))
    }
}

impl CliprdrServerFactory for ClipFactory {}

struct Input(Slot<Box<dyn InputSink>>);

impl Input {
    /// Applies `f` to the input port of the current session (ignored while no session is open).
    fn with<R>(&self, f: impl FnOnce(&mut dyn InputSink) -> R) -> Option<R> {
        self.0
            .lock()
            .expect("input mutex")
            .as_mut()
            .map(|i| f(i.as_mut()))
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
    fn keyboard_layout(&mut self, klid: u32) {
        let Some(layout) = rdsktp_core::klid_to_xkb(klid) else {
            tracing::debug!("keyboard layout {klid:#010x}: unknown, the session keeps its own");
            return;
        };
        tracing::info!(
            "keyboard layout {klid:#010x} -> {}({})",
            layout.layout,
            layout.variant
        );
        if let Some(Err(e)) = self.with(|i| i.set_keyboard_layout(&layout)) {
            tracing::warn!("keyboard layout: {e}");
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

/// Desktop size announced to the client: the current one (it changes with Display Control), otherwise the configured one.
struct Display {
    initial: DesktopSize,
    shared: Arc<Shared>,
}
/// Everything is drawn through EGFX: the only display updates are the resizes requested by the pump.
struct Resizes(tokio::sync::mpsc::UnboundedReceiver<DisplayUpdate>);

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Resizes {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        match self.0.recv().await {
            Some(update) => Ok(Some(update)),
            None => std::future::pending().await,
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        match *self.shared.desktop.lock().expect("desktop mutex") {
            Some((width, height)) => DesktopSize {
                width: width as u16,
                height: height as u16,
            },
            None => self.initial,
        }
    }
    async fn monitors(&mut self) -> Option<Vec<ironrdp_pdu::gcc::Monitor>> {
        let layout = self.shared.layout.lock().expect("layout mutex").clone()?;
        Some(pump::monitors(&layout))
    }
    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.shared.display.lock().expect("display mutex") = Some(tx);
        self.shared.client_loops.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Resizes(rx)))
    }
    /// The client asks for a new size: handed to the pump, which resizes the capture and announces it through EGFX.
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let specs: Vec<MonitorSpec> = layout
            .monitors()
            .iter()
            .map(|m| {
                let (w, h) = m.dimensions();
                let (w, h) = MonitorLayoutEntry::adjust_display_size(w, h);
                let (x, y) = m.position().unwrap_or((0, 0));
                MonitorSpec {
                    x,
                    y,
                    width: w,
                    height: h,
                    primary: m.is_primary(),
                }
            })
            .collect();
        let Some(new) = Layout::from_monitors(&specs) else {
            return;
        };
        tracing::debug!(
            outputs = new.outputs.len(),
            "display control: layout requested"
        );
        *self.shared.resize.lock().expect("resize mutex") = Some(new);
        self.shared.wake.notify();
    }
}

/// Multi-monitor client (mstsc): the desktop follows the monitors it announces in its GCC data, negotiated from the
/// start (no deactivation-reactivation). Limited to `max_outputs` monitors; a client with a single monitor keeps the
/// server's layout.
fn monitor_policy(shared: Arc<Shared>, max_outputs: usize) -> ironrdp_server::MonitorPolicy {
    Arc::new(move |client| {
        let specs: Vec<MonitorSpec> = client
            .iter()
            .take(max_outputs)
            .map(|m| MonitorSpec {
                x: m.left,
                y: m.top,
                width: (m.right - m.left + 1).max(0) as u32,
                height: (m.bottom - m.top + 1).max(0) as u32,
                primary: m.flags.contains(ironrdp_pdu::gcc::MonitorFlags::PRIMARY),
            })
            .collect();
        let layout = (specs.len() >= 2)
            .then(|| Layout::from_monitors(&specs))
            .flatten();
        *shared.client_initial.lock().expect("client layout mutex") = layout.clone();
        let layout = layout?;
        tracing::debug!(
            outputs = layout.outputs.len(),
            "client monitors adopted at connection"
        );
        let (w, h) = layout.desktop_size();
        // Announced now: the pump applies the layout to the capture without a reactivation.
        *shared.desktop.lock().expect("desktop mutex") = Some((w, h));
        *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
        *shared.announced.lock().expect("announced mutex") = Some(layout.clone());
        let monitors = pump::monitors(&layout);
        Some((
            ironrdp_server::DesktopSize {
                width: w as u16,
                height: h as u16,
            },
            monitors,
        ))
    })
}

/// Runs the RDP server with a fixed desktop (test mode, autologin) until it stops.
pub async fn run(config: ServerConfig, ports: Ports) -> anyhow::Result<()> {
    serve(config, Some(ports), None).await
}

/// Runs the RDP server in login mode: each connection is authenticated (TLS mode, credentials from the
/// ClientInfoPdu) and opens its session through the `SessionBroker`. Incompatible with NLA (see ADR 008).
pub async fn run_with_broker(
    config: ServerConfig,
    broker: Arc<dyn SessionBroker>,
) -> anyhow::Result<()> {
    anyhow::ensure!(config.nla.is_none(), "login mode is incompatible with NLA");
    serve(config, None, Some(broker)).await
}

async fn serve(
    config: ServerConfig,
    initial: Option<Ports>,
    broker: Option<Arc<dyn SessionBroker>>,
) -> anyhow::Result<()> {
    // ring AND aws-lc-rs are in the dependency tree: choose explicitly (already installed = ok).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let guard = LoginGuard::new(config.throttle);
    let has_broker = broker.is_some();
    let shared = Arc::new(Shared::default());
    let local: LocalContent = LocalContent::default();
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
        .with_display_handler(Display {
            initial: DesktopSize {
                width: desktop.0 as u16,
                height: desktop.1 as u16,
            },
            shared: shared.clone(),
        })
        .with_gfx_factory(Some(Box::new(GfxFactory(shared.clone()))))
        .with_cliprdr_factory(Some(Box::new(clip_factory)))
        .with_sound_factory(audio_enabled.then(|| {
            Box::new(SoundFactory {
                shared: shared.clone(),
                provider: Arc::new(SlotAudio(slot.audio.clone())),
            }) as Box<dyn ironrdp_server::SoundServerFactory>
        }))
        .with_rdpdr_factory(Some(Box::new(DriveFactory {
            shared: shared.clone(),
            host: slot.drives.clone(),
            printers: slot.printers.clone(),
            modes: config.printer_modes.clone(),
        })))
        .with_credential_validator(broker.map(|broker| {
            Arc::new(BrokerValidator {
                broker,
                slot: slot.clone(),
                guard: guard.clone(),
            }) as Arc<dyn ironrdp_server::CredentialValidator>
        }))
        .with_connection_handler(has_broker.then(|| {
            Box::new(GuardHandler(guard.clone())) as Box<dyn ironrdp_server::ConnectionHandler>
        }))
        .build();
    server.set_monitor_policy(Some(monitor_policy(
        shared.clone(),
        config.layout.outputs.len(),
    )));
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
            event: std::env::var("RDSKTP_PUMP")
                .is_ok_and(|v| v == "tick")
                .not(),
        },
    );
    // Local -> remote text: poll the port, announce the delayed copy.
    let poller_shared = shared.clone();
    let poller = std::thread::Builder::new()
        .name("clipboard-poll".into())
        .spawn(move || {
            while !poller_shared.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let (text, image, files) = match clipboard.lock().expect("clipboard mutex").as_mut()
                {
                    Some(c) => (c.poll_text(), c.poll_image(), c.poll_files()),
                    None => continue,
                };
                match text {
                    Ok(Some(t)) => announce(&send, &local, Content::Text(t)),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
                match image {
                    Ok(Some(png)) => announce(&send, &local, Content::Image(png)),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("clipboard: {e}"),
                }
                match files {
                    Ok(Some(list)) => announce(&send, &local, Content::Files(list)),
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

#[cfg(test)]
mod gfx_tests {
    use super::*;
    use ironrdp_egfx::pdu::CapabilitiesAdvertisePdu;

    fn advertise(caps: &[CapabilitySet]) -> GfxHandler {
        let mut h = GfxHandler::default();
        h.capabilities_advertise(&CapabilitiesAdvertisePdu::from_typed(caps));
        h
    }

    #[test]
    fn a_client_without_h264_keeps_avc_disabled_in_the_preferences() {
        let h = advertise(&[
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE | CapabilitiesV107Flags::AVC_DISABLED,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE,
            },
        ]);
        let prefs = h.preferred_capabilities();
        assert!(matches!(&prefs[0], CapabilitySet::V10_7 { flags }
            if flags.contains(CapabilitiesV107Flags::AVC_DISABLED)));
        assert!(matches!(&prefs[1], CapabilitySet::V10 { flags }
            if !flags.contains(CapabilitiesV10Flags::AVC_DISABLED)));
    }

    #[test]
    fn a_client_with_h264_is_left_alone() {
        let h = advertise(&[CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE,
        }]);
        assert!(
            matches!(&h.preferred_capabilities()[0], CapabilitySet::V10_7 { flags }
            if !flags.contains(CapabilitiesV107Flags::AVC_DISABLED))
        );
    }
}
