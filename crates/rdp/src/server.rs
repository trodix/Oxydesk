use std::collections::BTreeSet;
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
use oxydesk_core::error::{AudioError, LoginError};
use oxydesk_core::layout::{Layout, MonitorSpec};
use oxydesk_core::ports::{
    AudioParams, AudioProvider, AudioSource, ClipboardBackend, DesktopPorts, InputSink,
    SessionBroker,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::clipboard::{announce, Content, LocalContent, RdpClipboard};
use crate::drive::DriveFactory;
use crate::pump::{self, PumpParams, PumpPorts, Shared};
use crate::reconnect::{Reconnect, Resume, COOKIE_LEN};
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
    pub throttle: oxydesk_core::ThrottleConfig,
    /// Format of the jobs of each redirected printer (ADR 015).
    pub printer_modes: oxydesk_core::PrinterModes,
    /// How long after a network loss a client resumes its session by itself (Auto-Reconnect cookie); zero: never.
    pub reconnect_timeout: Duration,
    /// A client that stops answering is dropped after this long, which frees the server for its reconnection.
    pub dead_peer_timeout: Duration,
    /// Bitrate and frames in flight follow what the client's link carries (`--adaptive-bitrate`).
    pub adaptive_bitrate: bool,
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
    drives: Slot<Arc<dyn oxydesk_core::drive::DriveHost>>,
    printers: Slot<Arc<dyn oxydesk_core::printer::PrinterHost>>,
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
    throttle: Arc<Mutex<oxydesk_core::LoginThrottle>>,
    peer: Arc<Mutex<Option<std::net::IpAddr>>>,
}

impl LoginGuard {
    fn new(cfg: oxydesk_core::ThrottleConfig) -> Self {
        Self {
            throttle: Arc::new(Mutex::new(oxydesk_core::LoginThrottle::new(cfg))),
            peer: Arc::default(),
        }
    }

    fn peer(&self) -> Option<std::net::IpAddr> {
        *self.peer.lock().expect("peer mutex")
    }
}

/// Drops the connections of blocked addresses before any protocol exchange, and tells the reconnection
/// cookies when a connection ends.
struct GuardHandler(LoginGuard, Arc<Reconnect>);

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

    fn on_disconnected(
        &mut self,
        _peer: std::net::SocketAddr,
        _duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> ironrdp_server::PostConnectionAction {
        // An error is a loss (network, crash): the client may come back. Otherwise it left on purpose.
        self.1
            .connection_ended(error.is_none(), std::time::Instant::now());
        ironrdp_server::PostConnectionAction::Continue
    }
}

/// Validates the received credentials (TLS mode) by opening the user's session through the broker.
struct BrokerValidator {
    broker: Arc<dyn SessionBroker>,
    slot: SessionSlot,
    guard: LoginGuard,
    shared: Arc<Shared>,
    reconnect: Arc<Reconnect>,
    /// User whose connection was just accepted: the cookie to hand over is theirs.
    accepted: Mutex<Option<String>>,
}

impl BrokerValidator {
    /// The monitors announced at connection (before the login): the session starts with as many outputs.
    fn client_outputs(&self) -> Option<u32> {
        self.shared
            .client_initial
            .lock()
            .expect("client layout mutex")
            .as_ref()
            .map(|l| l.outputs.len() as u32)
    }

    fn accept(&self, user: &str, ports: DesktopPorts) {
        self.slot.install(ports);
        *self.accepted.lock().expect("accepted mutex") = Some(user.to_owned());
    }
}

#[async_trait::async_trait]
impl ironrdp_server::CredentialValidator for BrokerValidator {
    async fn validate_reconnect(
        &self,
        cookie: &[u8; COOKIE_LEN],
    ) -> Result<ironrdp_server::CredentialDecision, ironrdp_server::CredentialValidationError> {
        use ironrdp_server::CredentialDecision;
        if !self.reconnect.enabled() {
            return Ok(CredentialDecision::Reject);
        }
        let ip = self.guard.peer();
        let user = match self.reconnect.resume(cookie, ip, std::time::Instant::now()) {
            Resume::Valid(user) => user,
            Resume::Refused(why) => {
                tracing::info!(?ip, "automatic reconnection refused: {why}");
                return Ok(CredentialDecision::Reject);
            }
        };
        let broker = self.broker.clone();
        let outputs = self.client_outputs();
        let name = user.clone();
        let outcome = tokio::task::spawn_blocking(move || broker.attach(&name, outputs))
            .await
            .map_err(ironrdp_server::CredentialValidationError::new)?;
        match outcome {
            Ok(ports) => {
                tracing::info!(?ip, user = ?user, "automatic reconnection accepted");
                self.accept(&user, ports);
                Ok(CredentialDecision::Accept)
            }
            Err(LoginError::Denied) => {
                tracing::info!(?ip, user = ?user, "automatic reconnection: no session to rejoin");
                Ok(CredentialDecision::Reject)
            }
            Err(LoginError::Backend(e)) => {
                tracing::error!("reconnection: {e}");
                Err(ironrdp_server::CredentialValidationError::new(
                    std::io::Error::other(e),
                ))
            }
        }
    }

    fn reconnect_cookie(&self) -> Option<ironrdp_pdu::rdp::session_info::ServerAutoReconnect> {
        if !self.reconnect.enabled() {
            return None;
        }
        let user = self.accepted.lock().expect("accepted mutex").take()?;
        Some(
            self.reconnect
                .issue(&user, self.guard.peer(), std::time::Instant::now()),
        )
    }

    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<ironrdp_server::CredentialDecision, ironrdp_server::CredentialValidationError> {
        use ironrdp_server::CredentialDecision;
        let (user, pass) = (credentials.username.clone(), credentials.password.clone());
        let ip = self.guard.peer();
        if pass.is_empty() {
            // A client resuming by cookie that the server no longer honours sends no password, and tries again
            // and again: that is not a guess to count against its address.
            tracing::info!(?ip, user = ?user, "login refused: no password");
            return Ok(CredentialDecision::Reject);
        }
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
        let outputs = self.client_outputs();
        let outcome = tokio::task::spawn_blocking(move || broker.login(&user, &pass, outputs))
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
                self.accept(&credentials.username, ports);
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

struct Input {
    slot: Slot<Box<dyn InputSink>>,
    /// Current layout: the pointer coordinates of the client follow its own arrangement of monitors.
    shared: Arc<Shared>,
    keys: HeldKeys,
    /// Last pointer position and number of moves (diagnostic trace of the coordinates the client sends).
    pointer: (u16, u16, u32),
}

/// The keys the client holds down. A lost key release (focus change on the client, a dropped fast-path PDU) would
/// leave a key stuck in the session: the client's synchronize event releases whatever is still believed held.
#[derive(Default)]
struct HeldKeys(BTreeSet<u16>);

impl HeldKeys {
    fn track(&mut self, scancode: u16, pressed: bool) {
        if pressed {
            self.0.insert(scancode);
        } else {
            self.0.remove(&scancode);
        }
    }

    /// The keys to release, in a stable order; none is held afterwards.
    fn take(&mut self) -> Vec<u16> {
        std::mem::take(&mut self.0).into_iter().collect()
    }
}

impl Input {
    /// Applies `f` to the input port of the current session (ignored while no session is open).
    fn with<R>(&self, f: impl FnOnce(&mut dyn InputSink) -> R) -> Option<R> {
        self.slot
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
            KeyboardEvent::Synchronize(_) => {
                let stuck = self.keys.take();
                if !stuck.is_empty() {
                    tracing::debug!(
                        "keyboard synchronize: releasing {} held key(s)",
                        stuck.len()
                    );
                }
                for scancode in stuck {
                    if let Some(Err(e)) = self.with(|i| i.key(scancode, false)) {
                        tracing::warn!("input: {e}");
                    }
                }
                return;
            }
            _ => return,
        };
        let scancode = u16::from(code) | if extended { 0xE000 } else { 0 };
        self.keys.track(scancode, pressed);
        if let Some(Err(e)) = self.with(|i| i.key(scancode, pressed)) {
            tracing::warn!("input: {e}");
        }
    }
    fn keyboard_layout(&mut self, klid: u32) {
        let Some(layout) = oxydesk_core::klid_to_xkb(klid) else {
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
        use oxydesk_core::ports::PointerButton as B;
        match event {
            MouseEvent::Move { x, y } => {
                self.pointer.2 += 1;
                self.pointer = (x, y, self.pointer.2);
                if self.pointer.2 % 100 == 1 {
                    tracing::debug!(x, y, "pointer moves (1 in 100)");
                }
            }
            MouseEvent::LeftPressed | MouseEvent::RightPressed => tracing::debug!(
                x = self.pointer.0,
                y = self.pointer.1,
                ?event,
                "pointer button"
            ),
            _ => {}
        }
        // The client's point in the layout of the outputs.
        let point = match event {
            MouseEvent::Move { x, y } => self
                .shared
                .layout
                .lock()
                .expect("layout mutex")
                .as_ref()
                .map(|l| l.from_client_point(i32::from(x), i32::from(y))),
            _ => None,
        };
        let r = self.with(|i| match event {
            MouseEvent::Move { x, y } => {
                let (x, y) = point.unwrap_or((i32::from(x), i32::from(y)));
                i.pointer_move(x, y)
            }
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
        tracing::debug!(
            monitors = ?specs.iter().map(|m| (m.x, m.y, m.width, m.height, m.primary)).collect::<Vec<_>>(),
            "display control: monitors received"
        );
        let Some(new) = Layout::from_monitors(&specs) else {
            tracing::warn!("display control: layout refused");
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
        tracing::debug!(
            monitors = ?specs.iter().map(|m| (m.x, m.y, m.width, m.height, m.primary)).collect::<Vec<_>>(),
            "monitors announced by the client"
        );
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
    let reconnect = Arc::new(Reconnect::new(config.reconnect_timeout));
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
        .with_input_handler(Input {
            slot: slot.input.clone(),
            shared: shared.clone(),
            keys: HeldKeys::default(),
            pointer: (0, 0, 0),
        })
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
                shared: shared.clone(),
                reconnect: reconnect.clone(),
                accepted: Mutex::default(),
            }) as Arc<dyn ironrdp_server::CredentialValidator>
        }))
        .with_connection_handler(has_broker.then(|| {
            Box::new(GuardHandler(guard.clone(), reconnect.clone()))
                as Box<dyn ironrdp_server::ConnectionHandler>
        }))
        .build();
    // Login mode: the session is made for the client's monitors, up to the maximum; otherwise the capture was
    // created with `outputs` outputs and that is the limit.
    let max_monitors = if has_broker {
        oxydesk_core::layout::MAX_OUTPUTS
    } else {
        config.layout.outputs.len()
    };
    server.set_dead_peer_timeout(config.dead_peer_timeout);
    server.set_monitor_policy(Some(monitor_policy(shared.clone(), max_monitors)));
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
            event: std::env::var("OXYDESK_PUMP")
                .is_ok_and(|v| v == "tick")
                .not(),
            adaptive: config.adaptive_bitrate,
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

#[cfg(test)]
mod input_tests {
    use super::*;
    use ironrdp_pdu::input::fast_path::SynchronizeFlags;
    use oxydesk_core::error::InputError;

    type Log = Arc<Mutex<Vec<(u16, bool)>>>;

    struct Recorder(Log);

    impl InputSink for Recorder {
        fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
            self.0.lock().unwrap().push((scancode, pressed));
            Ok(())
        }
        fn pointer_move(&mut self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
    }

    fn input() -> (Input, Log) {
        let log = Log::default();
        let slot: Slot<Box<dyn InputSink>> =
            Arc::new(Mutex::new(Some(Box::new(Recorder(log.clone())))));
        (
            Input {
                slot,
                shared: Arc::new(Shared::default()),
                keys: HeldKeys::default(),
                pointer: (0, 0, 0),
            },
            log,
        )
    }

    fn press(i: &mut Input, code: u8, extended: bool) {
        i.keyboard(KeyboardEvent::Pressed { code, extended });
    }

    fn release(i: &mut Input, code: u8, extended: bool) {
        i.keyboard(KeyboardEvent::Released { code, extended });
    }

    fn sync(i: &mut Input) {
        i.keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::empty()));
    }

    #[test]
    fn synchronize_releases_the_keys_still_held() {
        let (mut i, log) = input();
        press(&mut i, 0x38, false); // Alt
        press(&mut i, 0x1D, true); // right Ctrl (extended)
        press(&mut i, 0x1E, false); // A
        release(&mut i, 0x1E, false);
        log.lock().unwrap().clear();
        sync(&mut i);
        assert_eq!(
            *log.lock().unwrap(),
            vec![(0x38, false), (0xE01D, false)],
            "only Alt and right Ctrl, in a stable order, as releases"
        );
    }

    #[test]
    fn synchronize_releases_a_key_only_once() {
        let (mut i, log) = input();
        press(&mut i, 0x2A, false);
        sync(&mut i);
        log.lock().unwrap().clear();
        sync(&mut i);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn synchronize_without_held_keys_sends_nothing() {
        let (mut i, log) = input();
        press(&mut i, 0x1E, false);
        release(&mut i, 0x1E, false);
        log.lock().unwrap().clear();
        sync(&mut i);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_released_key_is_no_longer_tracked() {
        let (mut i, log) = input();
        press(&mut i, 0x38, false);
        release(&mut i, 0x38, false);
        press(&mut i, 0x3A, false); // Caps Lock, still held
        log.lock().unwrap().clear();
        sync(&mut i);
        assert_eq!(*log.lock().unwrap(), vec![(0x3A, false)]);
    }

    #[test]
    fn extended_and_plain_keys_with_the_same_code_are_distinct() {
        let (mut i, log) = input();
        press(&mut i, 0x1D, false); // left Ctrl
        press(&mut i, 0x1D, true); // right Ctrl
        release(&mut i, 0x1D, true);
        log.lock().unwrap().clear();
        sync(&mut i);
        assert_eq!(*log.lock().unwrap(), vec![(0x1D, false)]);
    }

    #[test]
    fn without_a_session_synchronize_is_harmless() {
        let (mut i, _) = input();
        press(&mut i, 0x38, false);
        *i.slot.lock().unwrap() = None;
        sync(&mut i);
        // The held keys were dropped with the release that had nowhere to go.
        let log = Log::default();
        *i.slot.lock().unwrap() = Some(Box::new(Recorder(log.clone())));
        sync(&mut i);
        assert!(log.lock().unwrap().is_empty());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod dead_peer_tests {
    use std::time::Duration;

    use ironrdp_server::tokio::net::{TcpListener, TcpStream};

    /// Connections are served one at a time: a link cut without a FIN must not hold the listener for minutes.
    #[tokio::test]
    async fn an_accepted_connection_gives_up_on_a_silent_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        ironrdp_server::detect_dead_peers(&stream, Duration::from_secs(8)).unwrap();
        let sock = socket2::SockRef::from(&stream);
        assert!(sock.keepalive().unwrap());
        assert!(sock.tcp_keepalive_time().unwrap() <= Duration::from_secs(15));
        let limit = sock.tcp_user_timeout().unwrap().unwrap();
        assert!(limit <= Duration::from_secs(60), "{limit:?}");
    }
}

#[cfg(test)]
mod reconnect_tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use ironrdp_server::{CredentialDecision, CredentialValidator as _, Credentials};
    use oxydesk_core::error::{CaptureError, ClipboardError, InputError};
    use oxydesk_core::frame::Frame;
    use oxydesk_core::layout::OutputId;
    use oxydesk_core::ports::CaptureSource;

    use super::*;

    struct NoCapture;
    impl CaptureSource for NoCapture {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
    }
    struct NoInput;
    impl InputSink for NoInput {
        fn key(&mut self, _: u16, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn pointer_move(&mut self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
    }
    struct NoClipboard;
    impl ClipboardBackend for NoClipboard {
        fn set_text(&mut self, _: &str) -> Result<(), ClipboardError> {
            Ok(())
        }
        fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
            Ok(None)
        }
    }

    fn ports() -> DesktopPorts {
        DesktopPorts {
            capture: Box::new(NoCapture),
            encoders: Vec::new(),
            make_encoder: None,
            input: Box::new(NoInput),
            clipboard: Box::new(NoClipboard),
            audio: None,
            drives: None,
            printers: None,
        }
    }

    /// Accepts the password "secret"; a session to rejoin exists while `session` is set.
    struct Broker {
        session: std::sync::atomic::AtomicBool,
        attached: Mutex<Vec<String>>,
    }
    impl SessionBroker for Broker {
        fn login(
            &self,
            user: &str,
            password: &str,
            _: Option<u32>,
        ) -> Result<DesktopPorts, LoginError> {
            let _ = user;
            if password == "secret" {
                Ok(ports())
            } else {
                Err(LoginError::Denied)
            }
        }
        fn attach(&self, user: &str, _: Option<u32>) -> Result<DesktopPorts, LoginError> {
            self.attached.lock().unwrap().push(user.to_owned());
            if self.session.load(Ordering::Relaxed) {
                Ok(ports())
            } else {
                Err(LoginError::Denied)
            }
        }
    }

    struct Rig {
        validator: BrokerValidator,
        broker: Arc<Broker>,
        handler: GuardHandler,
    }

    fn rig() -> Rig {
        let broker = Arc::new(Broker {
            session: true.into(),
            attached: Mutex::default(),
        });
        let shared = Arc::new(Shared::default());
        let guard = LoginGuard::new(oxydesk_core::ThrottleConfig {
            max_ip_failures: 2,
            ..Default::default()
        });
        *guard.peer.lock().unwrap() = Some(IpAddr::from([10, 0, 0, 1]));
        let reconnect = Arc::new(Reconnect::new(Duration::from_secs(10)));
        Rig {
            validator: BrokerValidator {
                broker: broker.clone(),
                slot: SessionSlot::new(shared.clone()),
                guard: guard.clone(),
                shared,
                reconnect: reconnect.clone(),
                accepted: Mutex::default(),
            },
            broker,
            handler: GuardHandler(guard, reconnect),
        }
    }

    fn creds(password: &str) -> Credentials {
        Credentials {
            username: "alice".into(),
            password: password.into(),
            domain: None,
        }
    }

    /// The 28 bytes mstsc sends back for the cookie it was given.
    fn client_cookie(c: &ironrdp_pdu::rdp::session_info::ServerAutoReconnect) -> [u8; COOKIE_LEN] {
        let mut out = [0u8; COOKIE_LEN];
        out[0..4].copy_from_slice(&28u32.to_le_bytes());
        out[4..8].copy_from_slice(&1u32.to_le_bytes());
        out[8..12].copy_from_slice(&c.logon_id.to_le_bytes());
        out[12..].copy_from_slice(&crate::reconnect::verifier(&c.random_bits));
        out
    }

    fn lost(r: &mut Rig) {
        use ironrdp_server::ConnectionHandler as _;
        let peer = "10.0.0.1:50000".parse().unwrap();
        let error = anyhow::anyhow!("connection reset");
        r.handler
            .on_disconnected(peer, Duration::from_secs(60), Some(&error));
    }

    #[tokio::test]
    async fn a_login_is_given_a_cookie_that_resumes_the_session_without_a_password() {
        let mut r = rig();
        assert_eq!(
            r.validator.validate(&creds("secret")).await.unwrap(),
            CredentialDecision::Accept
        );
        let cookie = r
            .validator
            .reconnect_cookie()
            .expect("cookie after a login");
        assert!(r.validator.reconnect_cookie().is_none(), "handed over once");

        lost(&mut r);
        let decision = r
            .validator
            .validate_reconnect(&client_cookie(&cookie))
            .await;
        assert_eq!(decision.unwrap(), CredentialDecision::Accept);
        assert_eq!(*r.broker.attached.lock().unwrap(), ["alice"]);
        // The resumed connection gets a fresh cookie, and the old one is spent.
        let next = r.validator.reconnect_cookie().expect("new cookie");
        assert_ne!(next.logon_id, cookie.logon_id);
        lost(&mut r);
        assert_eq!(
            r.validator
                .validate_reconnect(&client_cookie(&cookie))
                .await
                .unwrap(),
            CredentialDecision::Reject
        );
    }

    #[tokio::test]
    async fn leaving_on_purpose_forgets_the_cookie() {
        use ironrdp_server::ConnectionHandler as _;
        let mut r = rig();
        r.validator.validate(&creds("secret")).await.unwrap();
        let cookie = r.validator.reconnect_cookie().unwrap();
        r.handler.on_disconnected(
            "10.0.0.1:50000".parse().unwrap(),
            Duration::from_secs(5),
            None,
        );
        assert_eq!(
            r.validator
                .validate_reconnect(&client_cookie(&cookie))
                .await
                .unwrap(),
            CredentialDecision::Reject
        );
        assert!(r.broker.attached.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_session_to_rejoin_the_cookie_is_refused() {
        let mut r = rig();
        r.validator.validate(&creds("secret")).await.unwrap();
        let cookie = r.validator.reconnect_cookie().unwrap();
        lost(&mut r);
        r.broker.session.store(false, Ordering::Relaxed);
        assert_eq!(
            r.validator
                .validate_reconnect(&client_cookie(&cookie))
                .await
                .unwrap(),
            CredentialDecision::Reject
        );
        assert!(r.validator.reconnect_cookie().is_none());
    }

    #[tokio::test]
    async fn the_attempts_of_a_client_that_lost_its_cookie_do_not_block_its_address() {
        // Twenty automatic retries without a password: the limit is two failures per address.
        let r = rig();
        for _ in 0..20 {
            assert_eq!(
                r.validator.validate(&creds("")).await.unwrap(),
                CredentialDecision::Reject
            );
        }
        assert_eq!(
            r.validator.validate(&creds("secret")).await.unwrap(),
            CredentialDecision::Accept
        );
    }

    #[tokio::test]
    async fn a_zero_window_turns_the_feature_off() {
        let mut r = rig();
        r.validator.reconnect = Arc::new(Reconnect::new(Duration::ZERO));
        r.validator.validate(&creds("secret")).await.unwrap();
        assert!(r.validator.reconnect_cookie().is_none());
    }
}

#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use oxydesk_core::error::InputError;
    use oxydesk_core::ports::PointerButton;

    /// An input port that records what the session would receive.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl InputSink for Recorder {
        fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("key {scancode:#x} {pressed}"));
            Ok(())
        }
        fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
            self.0.lock().unwrap().push(format!("move {x},{y}"));
            Ok(())
        }
        fn pointer_button(
            &mut self,
            button: PointerButton,
            pressed: bool,
        ) -> Result<(), InputError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("button {button:?} {pressed}"));
            Ok(())
        }
        fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
            self.0.lock().unwrap().push(format!("wheel {dx},{dy}"));
            Ok(())
        }
    }

    fn input(shared: Arc<Shared>) -> (Input, Recorder) {
        let recorder = Recorder::default();
        let slot: Slot<Box<dyn InputSink>> = Arc::new(Mutex::new(Some(Box::new(recorder.clone()))));
        (
            Input {
                slot,
                shared,
                keys: HeldKeys::default(),
                pointer: (0, 0, 0),
            },
            recorder,
        )
    }

    fn log(r: &Recorder) -> Vec<String> {
        r.0.lock().unwrap().clone()
    }

    #[test]
    fn held_keys_are_listed_once_in_order_and_forgotten_when_taken() {
        let mut keys = HeldKeys::default();
        keys.track(0x1e, true);
        keys.track(0x1e, true);
        keys.track(0x10, true);
        keys.track(0x2a, true);
        keys.track(0x2a, false);
        assert_eq!(keys.take(), vec![0x10, 0x1e]);
        assert!(keys.take().is_empty());
    }

    #[test]
    fn keys_are_forwarded_with_the_extended_prefix() {
        let (mut input, rec) = input(Arc::default());
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x1d,
            extended: true,
        });
        input.keyboard(KeyboardEvent::Released {
            code: 0x1d,
            extended: true,
        });
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x1e,
            extended: false,
        });
        assert_eq!(
            log(&rec),
            ["key 0xe01d true", "key 0xe01d false", "key 0x1e true"]
        );
    }

    #[test]
    fn a_synchronize_releases_the_keys_whose_release_was_lost() {
        let (mut input, rec) = input(Arc::default());
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x2a,
            extended: false,
        });
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x38,
            extended: true,
        });
        input.keyboard(KeyboardEvent::Released {
            code: 0x2a,
            extended: false,
        });
        input.keyboard(KeyboardEvent::Synchronize(
            ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
        ));
        let events = log(&rec);
        assert_eq!(events.last().unwrap(), "key 0xe038 false");
        // Only the key still held is released again.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.as_str() == "key 0x2a false")
                .count(),
            1
        );
        input.keyboard(KeyboardEvent::Synchronize(
            ironrdp_pdu::input::fast_path::SynchronizeFlags::empty(),
        ));
        assert_eq!(
            log(&rec).len(),
            events.len(),
            "nothing held, nothing released"
        );
    }

    #[test]
    fn events_are_dropped_while_no_session_is_open() {
        let (mut input, rec) = input(Arc::default());
        input.slot.lock().unwrap().take();
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x1e,
            extended: false,
        });
        input.mouse(MouseEvent::Move { x: 5, y: 5 });
        assert!(log(&rec).is_empty());
    }

    #[test]
    fn buttons_and_wheels_are_translated() {
        let (mut input, rec) = input(Arc::default());
        for event in [
            MouseEvent::LeftPressed,
            MouseEvent::LeftReleased,
            MouseEvent::RightPressed,
            MouseEvent::RightReleased,
            MouseEvent::MiddlePressed,
            MouseEvent::MiddleReleased,
            MouseEvent::Button4Pressed,
            MouseEvent::Button4Released,
            MouseEvent::Button5Pressed,
            MouseEvent::Button5Released,
            MouseEvent::VerticalScroll { value: 120 },
            MouseEvent::Scroll { x: -120, y: 240 },
            MouseEvent::RelMove { x: 1, y: 1 },
        ] {
            input.mouse(event);
        }
        assert_eq!(
            log(&rec),
            [
                "button Left true",
                "button Left false",
                "button Right true",
                "button Right false",
                "button Middle true",
                "button Middle false",
                "button Back true",
                "button Back false",
                "button Forward true",
                "button Forward false",
                "wheel 0,120",
                "wheel -120,240",
            ]
        );
    }

    #[test]
    fn the_pointer_follows_the_layout_of_the_clients_monitors() {
        let shared = Arc::new(Shared::default());
        let (mut input, rec) = input(shared.clone());
        // Without a layout the coordinates are passed as they are.
        input.mouse(MouseEvent::Move { x: 10, y: 20 });
        // The client has a 1000x500 monitor at its left and the primary 1920x1080 one at the right: its space starts
        // at the first one, the server's outputs side by side from the primary.
        let layout = Layout::from_monitors(&[
            MonitorSpec {
                x: -1000,
                y: 0,
                width: 1000,
                height: 500,
                primary: false,
            },
            MonitorSpec {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                primary: true,
            },
        ])
        .expect("layout");
        *shared.layout.lock().unwrap() = Some(layout.clone());
        input.mouse(MouseEvent::Move { x: 10, y: 20 });
        input.mouse(MouseEvent::Move { x: 1010, y: 20 });
        let expected = [
            layout.from_client_point(10, 20),
            layout.from_client_point(1010, 20),
        ];
        let events = log(&rec);
        assert_eq!(events[0], "move 10,20");
        assert_eq!(
            events[1],
            format!("move {},{}", expected[0].0, expected[0].1)
        );
        assert_eq!(
            events[2],
            format!("move {},{}", expected[1].0, expected[1].1)
        );
        assert_ne!(events[1], events[2]);
    }

    #[test]
    fn a_known_keyboard_layout_is_forwarded_and_an_unknown_one_is_ignored() {
        let (mut input, _rec) = input(Arc::default());
        input.keyboard_layout(0x0000_040c); // French
        input.keyboard_layout(0xdead_beef);
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    fn display() -> (Display, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        (
            Display {
                initial: DesktopSize {
                    width: 1920,
                    height: 1080,
                },
                shared: shared.clone(),
            },
            shared,
        )
    }

    #[test]
    fn the_desktop_size_is_the_current_one_else_the_configured_one() {
        let (mut d, shared) = display();
        let size = tokio_test_block(d.size());
        assert_eq!((size.width, size.height), (1920, 1080));
        *shared.desktop.lock().unwrap() = Some((3840, 1080));
        let size = tokio_test_block(d.size());
        assert_eq!((size.width, size.height), (3840, 1080));
    }

    fn tokio_test_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn monitors_are_announced_only_once_a_layout_exists() {
        let (mut d, shared) = display();
        assert!(tokio_test_block(d.monitors()).is_none());
        *shared.layout.lock().unwrap() = Some(Layout::side_by_side(2, 1280, 720));
        assert_eq!(tokio_test_block(d.monitors()).unwrap().len(), 2);
    }

    #[test]
    fn each_client_loop_gets_its_own_channel_and_is_counted() {
        let (mut d, shared) = display();
        let _first = tokio_test_block(d.updates()).unwrap();
        let _second = tokio_test_block(d.updates()).unwrap();
        assert_eq!(shared.client_loops.load(Ordering::SeqCst), 2);
        assert!(shared.display.lock().unwrap().is_some());
    }

    fn client_monitor(
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        primary: bool,
    ) -> ironrdp_pdu::gcc::Monitor {
        ironrdp_pdu::gcc::Monitor {
            left,
            top,
            right,
            bottom,
            flags: if primary {
                ironrdp_pdu::gcc::MonitorFlags::PRIMARY
            } else {
                ironrdp_pdu::gcc::MonitorFlags::empty()
            },
        }
    }

    #[test]
    fn a_single_monitor_client_keeps_the_layout_of_the_server() {
        let shared = Arc::new(Shared::default());
        let policy = monitor_policy(shared.clone(), 4);
        assert!(policy(&[client_monitor(0, 0, 1919, 1079, true)]).is_none());
        assert!(shared.layout.lock().unwrap().is_none());
        assert!(shared.client_initial.lock().unwrap().is_none());
    }

    #[test]
    fn a_multi_monitor_client_gets_a_desktop_made_of_its_monitors() {
        let shared = Arc::new(Shared::default());
        let policy = monitor_policy(shared.clone(), 4);
        let (size, monitors) = policy(&[
            client_monitor(0, 0, 1919, 1079, true),
            client_monitor(1920, 0, 3839, 1079, false),
        ])
        .expect("layout adopted");
        assert_eq!((size.width, size.height), (3840, 1080));
        assert_eq!(monitors.len(), 2);
        assert_eq!(*shared.desktop.lock().unwrap(), Some((3840, 1080)));
        assert!(shared.layout.lock().unwrap().is_some());
        assert!(shared.announced.lock().unwrap().is_some());
        assert!(shared.client_initial.lock().unwrap().is_some());
    }

    #[test]
    fn monitors_beyond_the_limit_are_ignored() {
        let shared = Arc::new(Shared::default());
        // The server can only have one output: two announced monitors leave one, which is not a multi-monitor layout.
        let policy = monitor_policy(shared.clone(), 1);
        let answer = policy(&[
            client_monitor(0, 0, 1919, 1079, true),
            client_monitor(1920, 0, 3839, 1079, false),
        ]);
        assert!(answer.is_none());
    }

    #[test]
    fn the_audio_of_a_session_that_does_not_exist_yet_is_refused() {
        let audio = SlotAudio(Arc::default());
        let result = audio.open(AudioParams {
            sample_rate: 44100,
            channels: 2,
        });
        assert!(matches!(result, Err(AudioError::Backend(m)) if m.contains("no audio session")));
    }
}
