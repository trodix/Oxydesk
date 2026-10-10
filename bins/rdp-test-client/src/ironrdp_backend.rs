//! IronRDP backend of the test client: TCP → TLS → connection → EGFX (H.264) + text CLIPRDR.
//!
//! A worker thread owns the blocking connection; the `Session` API talks to it through a command
//! channel and a shared state (framebuffer, received clipboard text).
//! TLS: the server certificate is NOT verified (local test server only).

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_connector::connection_activation::ConnectionActivationState;
use ironrdp_connector::sspi::generator::NetworkRequest;
use ironrdp_connector::sspi::network_client::NetworkClient;
use ironrdp_connector::{ClientConnector, Config, Credentials, DesktopSize, Sequence as _};
use ironrdp_core::{decode_cursor, impl_as_any, ReadCursor};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::{
    DisplayControlMonitorLayout, DisplayControlPdu, MonitorLayoutEntry,
};
use ironrdp_dvc::{encode_dvc_messages, DrdynvcClient, DvcMessage, DvcProcessor};
use ironrdp_egfx::client::{
    BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface,
};
use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
use ironrdp_egfx::pdu::GfxPdu;
use ironrdp_graphics::zgfx;
use ironrdp_input::{Database, Operation, Scancode};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_pdu::PduResult;
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_svc::ChannelFlags;
use rdsktp_rdp::clipboard::{announce, LocalText, SendFn};
use rdsktp_rdp::TextClipboard;
use rustls::pki_types::ServerName;
use tracing::debug;

use crate::session::{Frame, MonitorRequest, Session};

/// Connection parameters.
#[derive(Debug, Clone)]
pub struct Options {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Provided only by `RDP_TEST_PASSWORD`; if present, CredSSP/NLA is enabled.
    pub password: Option<String>,
    /// TLS only: credentials go in the ClientInfoPdu (`rdsktp --login` mode, no CredSSP).
    pub no_nla: bool,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
struct State {
    frame: Option<Frame>,
    seq: u64,
    clip_inbox: VecDeque<String>,
    audio_inbox: VecDeque<crate::audio::AudioChunk>,
    error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("client state")
    }
    fn fail(&self, msg: String) {
        let mut s = self.lock();
        s.error.get_or_insert(msg);
        self.cond.notify_all();
    }
}

enum Command {
    Key(u16, bool),
    Clipboard(String),
    Monitors(Vec<MonitorRequest>),
    Close,
    /// Drops the socket without the RDP shutdown sequence.
    Abort,
}

pub struct IronRdpSession {
    opts: Options,
    shared: Arc<Shared>,
    tx: Option<Sender<Command>>,
    worker: Option<JoinHandle<()>>,
    last_seq: u64,
}

impl IronRdpSession {
    pub fn new(opts: Options) -> Self {
        Self {
            opts,
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                cond: Condvar::new(),
            }),
            tx: None,
            worker: None,
            last_seq: 0,
        }
    }

    fn send(&self, cmd: Command) -> Result<(), String> {
        let tx = self.tx.as_ref().ok_or("not connected")?;
        tx.send(cmd).map_err(|_| {
            self.shared
                .lock()
                .error
                .clone()
                .unwrap_or_else(|| "connection closed".into())
        })
    }
}

impl Session for IronRdpSession {
    fn connect(&mut self) -> Result<(u32, u32), String> {
        // Fresh state: a previous connection (closed or lost) must not leak into this one.
        *self.shared.lock() = State::default();
        self.last_seq = 0;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u32), String>>();
        let shared = self.shared.clone();
        let opts = self.opts.clone();
        self.worker = Some(std::thread::spawn(move || {
            if let Err(e) = run_worker(opts, shared.clone(), cmd_rx, &ready_tx) {
                let msg = format!("{e:#}");
                let _ = ready_tx.send(Err(msg.clone()));
                shared.fail(msg);
            }
        }));
        self.tx = Some(cmd_tx);
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "connection timed out".to_string())?
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Frame, String> {
        let deadline = Instant::now() + timeout;
        let mut s = self.shared.lock();
        loop {
            if s.seq > self.last_seq {
                if let Some(f) = &s.frame {
                    self.last_seq = s.seq;
                    return Ok(f.clone());
                }
            }
            if let Some(e) = &s.error {
                return Err(e.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("no image received in time".into());
            }
            s = self
                .shared
                .cond
                .wait_timeout(s, left)
                .expect("client state")
                .0;
        }
    }

    fn send_key(&mut self, scancode: u16, pressed: bool) -> Result<(), String> {
        self.send(Command::Key(scancode, pressed))
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.send(Command::Clipboard(text.to_owned()))
    }

    fn next_audio(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<crate::audio::AudioChunk>, String> {
        let deadline = Instant::now() + timeout;
        let mut s = self.shared.lock();
        loop {
            if let Some(c) = s.audio_inbox.pop_front() {
                return Ok(Some(c));
            }
            if let Some(e) = &s.error {
                return Err(e.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            s = self
                .shared
                .cond
                .wait_timeout(s, left)
                .expect("client state")
                .0;
        }
    }

    fn next_clipboard_text(&mut self, timeout: Duration) -> Result<Option<String>, String> {
        let deadline = Instant::now() + timeout;
        let mut s = self.shared.lock();
        loop {
            if let Some(t) = s.clip_inbox.pop_front() {
                return Ok(Some(t));
            }
            if let Some(e) = &s.error {
                return Err(e.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            s = self
                .shared
                .cond
                .wait_timeout(s, left)
                .expect("client state")
                .0;
        }
    }

    fn set_monitors(&mut self, monitors: &[MonitorRequest]) -> Result<(), String> {
        self.send(Command::Monitors(monitors.to_vec()))
    }

    fn close(&mut self) {
        self.stop(Command::Close);
    }

    fn abort(&mut self) {
        self.stop(Command::Abort);
    }
}

impl IronRdpSession {
    fn stop(&mut self, cmd: Command) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(cmd);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// EGFX

struct GfxHandler {
    shared: Arc<Shared>,
    origins: HashMap<u16, (u32, u32)>,
}

impl GraphicsPipelineHandler for GfxHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        let mut s = self.shared.lock();
        s.frame = Some(Frame {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        });
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        self.origins.entry(surface.id).or_insert((0, 0));
    }

    fn on_surface_mapped(&mut self, surface_id: u16, x: u32, y: u32) {
        self.origins.insert(surface_id, (x, y));
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.origins.remove(&surface_id);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        let (ox, oy) = self
            .origins
            .get(&update.surface_id)
            .copied()
            .unwrap_or((0, 0));
        let mut s = self.shared.lock();
        let Some(frame) = s.frame.as_mut() else {
            return;
        };
        let (w, h) = (u32::from(update.width), u32::from(update.height));
        let x0 = ox + u32::from(update.destination_rectangle.left);
        let y0 = oy + u32::from(update.destination_rectangle.top);
        for row in 0..h {
            let y = y0 + row;
            if y >= frame.height || x0 >= frame.width {
                break;
            }
            let n = w.min(frame.width - x0) as usize * 4;
            let src = (row * w) as usize * 4;
            let dst = ((y * frame.width + x0) * 4) as usize;
            if src + n <= update.data.len() {
                frame.rgba[dst..dst + n].copy_from_slice(&update.data[src..src + n]);
            }
        }
        s.seq += 1;
        self.shared.cond.notify_all();
    }
}

/// H.264 decoder through libopenh264 loaded at runtime. Accepts Annex-B or AVC (length prefix).
struct OpenH264Decoder {
    inner: openh264::decoder::Decoder,
    annex_b: Vec<u8>,
}

/// `H264Decoder` of ironrdp-egfx does not receive the surface identifier whereas each surface has its
/// own stream (own P references): we keep one decoder per surface, chosen thanks to the queue
/// of identifiers that `SurfaceTap` feeds in the exact order of the `WireToSurface1`.
struct PerSurfaceDecoder {
    pending: Arc<Mutex<VecDeque<u16>>>,
    decoders: HashMap<u16, OpenH264Decoder>,
}

impl H264Decoder for PerSurfaceDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        let id = self
            .pending
            .lock()
            .expect("pending")
            .pop_front()
            .unwrap_or(0);
        if let std::collections::hash_map::Entry::Vacant(v) = self.decoders.entry(id) {
            v.insert(OpenH264Decoder::new().map_err(|e| DecoderError::msg(e.to_string()))?);
        }
        self.decoders.get_mut(&id).expect("inserted").decode(data)
    }

    fn reset(&mut self) {
        self.decoders.clear();
        self.pending.lock().expect("pending").clear();
    }
}

/// EGFX channel wrapper: re-reads (with its own ZGFX window, fed with the same bytes) the PDUs
/// before handing them to the client, only to note the surface of each `WireToSurface1`.
struct SurfaceTap {
    inner: GraphicsPipelineClient,
    pending: Arc<Mutex<VecDeque<u16>>>,
    zgfx: zgfx::Decompressor,
    buf: Vec<u8>,
}

impl_as_any!(SurfaceTap);

impl DvcProcessor for SurfaceTap {
    fn channel_name(&self) -> &str {
        self.inner.channel_name()
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.inner.start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.buf.clear();
        if self.zgfx.decompress(payload, &mut self.buf).is_ok() {
            let mut cursor = ReadCursor::new(&self.buf);
            while !cursor.is_empty() {
                match decode_cursor::<GfxPdu>(&mut cursor) {
                    Ok(GfxPdu::WireToSurface1(w)) => {
                        self.pending
                            .lock()
                            .expect("pending")
                            .push_back(w.surface_id);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        self.inner.process(channel_id, payload)
    }

    fn close(&mut self, channel_id: u32) {
        self.inner.close(channel_id);
    }
}

fn locate_openh264() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("RDSKTP_OPENH264_LIB") {
        return Ok(p.into());
    }
    [
        "/usr/lib64/libopenh264.so.8",
        "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
        "/usr/lib64/libopenh264.so",
        "/usr/lib/x86_64-linux-gnu/libopenh264.so",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .ok_or_else(|| anyhow!("libopenh264 not found (set RDSKTP_OPENH264_LIB)"))
}

impl OpenH264Decoder {
    fn new() -> anyhow::Result<Self> {
        let path = locate_openh264()?;
        // SAFETY: system library chosen by the operator; test client only.
        let api = unsafe { openh264::OpenH264API::from_blob_path_unchecked(&path) }
            .map_err(|e| anyhow!("loading {}: {e}", path.display()))?;
        let inner = openh264::decoder::Decoder::with_api_config(
            api,
            openh264::decoder::DecoderConfig::new(),
        )
        .map_err(|e| anyhow!("decoder creation: {e}"))?;
        Ok(Self {
            inner,
            annex_b: Vec::new(),
        })
    }

    /// Normalizes to Annex-B (the server stream already is; otherwise AVC conversion).
    fn normalize(&mut self, data: &[u8]) {
        self.annex_b.clear();
        if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) {
            self.annex_b.extend_from_slice(data);
            return;
        }
        let mut off = 0;
        while off + 4 <= data.len() {
            let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                as usize;
            off += 4;
            let Some(end) = off.checked_add(len).filter(|&e| e <= data.len()) else {
                break;
            };
            self.annex_b.extend_from_slice(&[0, 0, 0, 1]);
            self.annex_b.extend_from_slice(&data[off..end]);
            off = end;
        }
    }
}

impl H264Decoder for OpenH264Decoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.normalize(data);
        let yuv = self
            .inner
            .decode(&self.annex_b)
            .map_err(|e| DecoderError::new("OpenH264 decoding", e))?
            .ok_or_else(|| DecoderError::msg("OpenH264 produced no image"))?;
        let (w, h) = openh264::formats::YUVSource::dimensions(&yuv);
        let mut rgba = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(DecodedFrame::new(rgba, w as u32, h as u32))
    }
}

// ---------------------------------------------------------------------------------------------
// Audio (RDPSND)

/// Receives the server's PCM packets into `State::audio_inbox`.
struct AudioSink {
    shared: Arc<Shared>,
    formats: Vec<AudioFormat>,
}

impl std::fmt::Debug for AudioSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioSink")
    }
}

impl AudioSink {
    fn new(shared: Arc<Shared>) -> Self {
        let pcm = |rate: u32| AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: rate,
            n_avg_bytes_per_sec: rate * 4,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        };
        // A single format: ironrdp-rdpsnd (client) sends the intersection in HashSet order,
        // so the packets' `format_no` cannot be read back with several formats.
        Self {
            shared,
            formats: vec![pcm(48_000)],
        }
    }
}

impl RdpsndClientHandler for AudioSink {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: std::borrow::Cow<'_, [u8]>) {
        let Some(f) = self.formats.get(format_no) else {
            return;
        };
        let mut s = self.shared.lock();
        if s.audio_inbox.len() < 1000 {
            s.audio_inbox.push_back(crate::audio::AudioChunk {
                arrived: Instant::now(),
                sample_rate: f.n_samples_per_sec,
                channels: f.n_channels,
                pcm: data.into_owned(),
            });
        }
        self.shared.cond.notify_all();
    }

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

// ---------------------------------------------------------------------------------------------
// Connection

struct NoNetwork;

impl NetworkClient for NoNetwork {
    fn send(&self, _: &NetworkRequest) -> ironrdp_connector::sspi::Result<Vec<u8>> {
        Err(ironrdp_connector::sspi::Error::new(
            ironrdp_connector::sspi::ErrorKind::NoAuthenticatingAuthority,
            "no Kerberos network request in the test client",
        ))
    }
}

#[derive(Debug)]
struct NoCertVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        s: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, s, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        s: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, s, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn tls_upgrade(stream: TcpStream, host: &str) -> anyhow::Result<(Tls, Vec<u8>)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerification(provider)))
        .with_no_client_auth();
    // No TLS session resumption (incompatible with CredSSP).
    config.resumption = rustls::client::Resumption::disabled();
    let name = ServerName::try_from(host.to_owned()).context("invalid server name")?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)?;
    let mut tls = rustls::StreamOwned::new(conn, stream);
    tls.flush()?; // advances the handshake
    let cert = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .context("server certificate missing")?;
    use x509_cert::der::Decode as _;
    let cert = x509_cert::Certificate::from_der(cert)?;
    let key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("public key not aligned")?
        .to_owned();
    Ok((tls, key))
}

fn connector_config(opts: &Options) -> Config {
    Config {
        credentials: Credentials::UsernamePassword {
            username: opts.user.clone(),
            password: opts.password.clone().unwrap_or_default(),
        },
        domain: None,
        enable_tls: true,
        enable_credssp: opts.password.is_some() && !opts.no_nla,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: opts.width,
            height: opts.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "rdsktp-test-client".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: true,
        compression_type: None,
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

fn run_worker(
    opts: Options,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: &Sender<Result<(u32, u32), String>>,
) -> anyhow::Result<()> {
    let addr = (opts.host.as_str(), opts.port)
        .to_socket_addrs()?
        .next()
        .context("address not found")?;
    let tcp =
        TcpStream::connect_timeout(&addr, Duration::from_secs(10)).context("TCP connection")?;
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;

    // Clipboard: messages emitted by the backend are queued then sent by the worker.
    let (clip_tx, clip_rx) = mpsc::channel::<ClipboardMessage>();
    let clip_tx = Mutex::new(clip_tx);
    let send: SendFn = Arc::new(move |m| {
        let _ = clip_tx.lock().expect("clip_tx").send(m);
    });
    let local: LocalText = Arc::new(Mutex::new(None));
    let on_text = {
        let shared = shared.clone();
        Arc::new(move |t: String| {
            shared.lock().clip_inbox.push_back(t);
            shared.cond.notify_all();
        })
    };

    let mut connector = ClientConnector::new(connector_config(&opts), client_addr);
    let pending = Arc::new(Mutex::new(VecDeque::new()));
    let gfx = SurfaceTap {
        inner: GraphicsPipelineClient::new(
            Box::new(GfxHandler {
                shared: shared.clone(),
                origins: HashMap::new(),
            }),
            Some(Box::new(PerSurfaceDecoder {
                pending: pending.clone(),
                decoders: HashMap::new(),
            })),
        ),
        pending,
        zgfx: zgfx::Decompressor::new(),
        buf: Vec::new(),
    };
    connector.attach_static_channel(
        DrdynvcClient::new()
            .with_dynamic_channel(gfx)
            .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new()))),
    );
    connector.attach_static_channel(Rdpsnd::new(Box::new(AudioSink::new(shared.clone()))));
    connector.attach_static_channel(CliprdrClient::new(Box::new(TextClipboard::new(
        send.clone(),
        local.clone(),
        on_text,
    ))));

    let mut framed = ironrdp_blocking::Framed::new(tcp);
    let should_upgrade =
        ironrdp_blocking::connect_begin(&mut framed, &mut connector).context("connection start")?;
    let (tls, server_key) =
        tls_upgrade(framed.into_inner_no_leftover(), &opts.host).context("TLS")?;
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = ironrdp_blocking::Framed::new(tls);
    let result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        opts.host.clone().into(),
        server_key,
        None,
    )
    .context("connection finalization")?;

    let (w, h) = (
        u32::from(result.desktop_size.width),
        u32::from(result.desktop_size.height),
    );
    {
        let mut s = shared.lock();
        s.frame.get_or_insert_with(|| Frame {
            width: w,
            height: h,
            rgba: vec![0; (w * h * 4) as usize],
        });
    }
    let mut stage: ActiveStage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut image = DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    let mut input = Database::new();
    let _ = ready.send(Ok((w, h)));

    // Read with a short timeout to interleave commands and network.
    framed
        .get_inner()
        .0
        .sock
        .set_read_timeout(Some(Duration::from_millis(2)))?;

    loop {
        // Commands from the user.
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::Close => {
                    for out in stage.graceful_shutdown().map_err(|e| anyhow!("{e}"))? {
                        if let ActiveStageOutput::ResponseFrame(f) = out {
                            let _ = framed.write_all(&f);
                        }
                    }
                    return Ok(());
                }
                Command::Abort => return Ok(()),
                Command::Key(code, pressed) => {
                    let sc = Scancode::from_u16(code);
                    let op = if pressed {
                        Operation::KeyPressed(sc)
                    } else {
                        Operation::KeyReleased(sc)
                    };
                    let events = input.apply([op]);
                    let outs = stage
                        .process_fastpath_input(&mut image, &events)
                        .map_err(|e| anyhow!("{e}"))?;
                    write_outputs(&mut framed, outs)?;
                }
                Command::Clipboard(text) => announce(&send, &local, text),
                Command::Monitors(monitors) => {
                    let frame = encode_monitor_layout(&mut stage, &monitors)?;
                    framed.write_all(&frame)?;
                }
            }
        }
        // Outgoing CLIPRDR messages.
        while let Ok(msg) = clip_rx.try_recv() {
            if let Some(bytes) = encode_clipboard(&mut stage, msg)? {
                framed.write_all(&bytes)?;
            }
        }
        // Network.
        match framed.read_pdu() {
            Ok((action, payload)) => {
                let outs = stage
                    .process(&mut image, action, &payload)
                    .map_err(|e| anyhow!("{e}"))?;
                for out in outs {
                    match out {
                        ActiveStageOutput::ResponseFrame(f) => framed.write_all(&f)?,
                        ActiveStageOutput::DeactivateAll => reactivate(
                            &mut framed,
                            &result.activation_factory,
                            &mut stage,
                            &mut image,
                        )?,
                        ActiveStageOutput::Terminate(r) => {
                            return Err(anyhow!("session terminated by the server: {r:?}"))
                        }
                        other => debug!(?other, "output ignored"),
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(anyhow!(e).context("network read")),
        }
    }
}

/// Display Control layout of several monitors, ready to be written on the wire.
fn encode_monitor_layout(
    stage: &mut ActiveStage,
    monitors: &[MonitorRequest],
) -> anyhow::Result<Vec<u8>> {
    let entries = monitors
        .iter()
        .map(|m| {
            let (w, h) = MonitorLayoutEntry::adjust_display_size(m.width, m.height);
            let entry = if m.primary {
                MonitorLayoutEntry::new_primary(w, h)
            } else {
                MonitorLayoutEntry::new_secondary(w, h)
            };
            entry.and_then(|e| e.with_position(m.x, m.y))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("monitor layout: {e}"))?;
    let pdu: DisplayControlPdu = DisplayControlMonitorLayout::new(&entries)
        .map_err(|e| anyhow!("monitor layout: {e}"))?
        .into();
    let channel_id = stage
        .get_dvc::<DisplayControlClient>()
        .and_then(|dvc| dvc.channel_id())
        .context("Display Control channel not open")?;
    let messages = encode_dvc_messages(channel_id, vec![Box::new(pdu)], ChannelFlags::empty())
        .map_err(|e| anyhow!("{e}"))?;
    stage
        .encode_dvc_messages(messages)
        .map_err(|e| anyhow!("{e}"))
}

/// Deactivation-reactivation: replays capabilities and finalization, then resizes the decoded image.
fn reactivate(
    framed: &mut ironrdp_blocking::Framed<Tls>,
    factory: &ironrdp_connector::connection_activation::ConnectionActivationFactory,
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
) -> anyhow::Result<()> {
    let mut sequence = factory.create();
    let mut buf = ironrdp_core::WriteBuf::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        buf.clear();
        let written = match sequence.next_pdu_hint() {
            Some(hint) => {
                let pdu = loop {
                    match framed.read_by_hint(hint) {
                        Ok(pdu) => break pdu,
                        Err(e)
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                                && Instant::now() < deadline => {}
                        Err(e) => return Err(anyhow!(e).context("reactivation read")),
                    }
                };
                sequence.step(&pdu, &mut buf).map_err(|e| anyhow!("{e}"))?
            }
            None => sequence
                .step_no_input(&mut buf)
                .map_err(|e| anyhow!("{e}"))?,
        };
        if let Some(n) = written.size() {
            framed.write_all(&buf[..n])?;
        }
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
        } = sequence.connection_activation_state()
        {
            debug!(?desktop_size, "reactivated");
            *image = DecodedImage::new(
                ironrdp_graphics::image_processing::PixelFormat::RgbA32,
                desktop_size.width,
                desktop_size.height,
            );
            stage.set_fastpath_processor(
                ironrdp_session::fast_path::ProcessorBuilder {
                    io_channel_id: sequence.io_channel_id(),
                    user_channel_id: sequence.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    bulk_decompressor: None,
                }
                .build(),
            );
            stage.set_share_id(share_id);
            stage.set_enable_server_pointer(enable_server_pointer);
            return Ok(());
        }
    }
}

fn write_outputs(
    framed: &mut ironrdp_blocking::Framed<Tls>,
    outs: Vec<ActiveStageOutput>,
) -> anyhow::Result<()> {
    for out in outs {
        if let ActiveStageOutput::ResponseFrame(f) = out {
            framed.write_all(&f)?;
        }
    }
    Ok(())
}

fn encode_clipboard(
    stage: &mut ActiveStage,
    msg: ClipboardMessage,
) -> anyhow::Result<Option<Vec<u8>>> {
    let messages = {
        let Some(cliprdr) = stage.get_svc_processor::<CliprdrClient>() else {
            return Ok(None);
        };
        match msg {
            ClipboardMessage::SendFormatData(resp) => cliprdr.submit_format_data(resp),
            other => {
                let cliprdr = stage
                    .get_svc_processor_mut::<CliprdrClient>()
                    .context("cliprdr")?;
                match other {
                    ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
                    ClipboardMessage::SendInitiatePaste(id) => cliprdr.initiate_paste(id),
                    _ => return Ok(None),
                }
            }
        }
    }
    .map_err(|e| anyhow!("{e}"))?;
    Ok(Some(
        stage
            .process_svc_processor_messages(messages)
            .map_err(|e| anyhow!("{e}"))?,
    ))
}
