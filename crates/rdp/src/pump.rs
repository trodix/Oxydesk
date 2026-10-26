//! Frame pump: capture -> encode -> EGFX, at a fixed rate. One frame at a time,
//! dropped (no queue) if the client does not acknowledge fast enough (EGFX backpressure).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use oxydesk_core::layout::{Layout, OutputId};
use oxydesk_core::ports::{CaptureSource, EncoderFactory, FrameEncoder};
use oxydesk_core::{FrameData, NetSample, RateConfig, RateController, DEFAULT_BITRATE_BPS};
use tokio::sync::mpsc::UnboundedSender;

/// State shared between the IronRDP factories (created per connection) and the worker threads.
#[derive(Default)]
pub struct Shared {
    pub sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    pub gfx: Mutex<Option<GfxServerHandle>>,
    pub stop: AtomicBool,
    /// Notified by the capture on every new frame (`event` mode).
    pub wake: Arc<oxydesk_core::wake::Wake>,
    /// Capture and encoders of a new session, stored by the composition root (fixed mode) or by
    /// the credential validator (login mode); the pump adopts them on the next tick.
    pub ports: Mutex<Option<PumpPorts>>,
    /// Layout requested by the client (Display Control), applied by the pump on its next tick.
    pub resize: Mutex<Option<Layout>>,
    /// Current layout, announced to the client in the Monitor Layout PDU; kept up to date by the pump.
    pub layout: Mutex<Option<Layout>>,
    /// Layout deduced from the monitors the client announced at connection, applied once a session is adopted.
    pub client_initial: Mutex<Option<Layout>>,
    /// Same layout, already negotiated with the client at connection: applying it needs no reactivation.
    pub announced: Mutex<Option<Layout>>,
    /// Current desktop size, announced to the clients at connection; kept up to date by the pump.
    pub desktop: Mutex<Option<(u32, u32)>>,
    /// Display updates of the current connection (deactivation-reactivation on resize).
    pub display: Mutex<Option<UnboundedSender<ironrdp_server::DisplayUpdate>>>,
    /// Number of client loops started (`RdpServerDisplay::updates` calls): a resize is complete,
    /// client side, when this counter moves past the value observed when it was requested.
    pub client_loops: std::sync::atomic::AtomicU64,
    /// Colour depth chosen by the client, in bits per pixel (`0`: none announced yet, true colour).
    pub color_depth: std::sync::atomic::AtomicU8,
    /// Bitmap updates handed to IronRDP and not taken yet: while it encodes one (RemoteFX is slow), the pump keeps
    /// the newest picture instead of piling up pictures that would be shown late.
    pub bitmaps_queued: std::sync::atomic::AtomicUsize,
}

pub struct PumpPorts {
    pub capture: Box<dyn CaptureSource>,
    /// One encoder per output, in layout order.
    pub encoders: Vec<Box<dyn FrameEncoder>>,
    /// Encoders for outputs added by a layout change.
    pub make_encoder: Option<EncoderFactory>,
}

pub struct PumpParams {
    pub layout: Layout,
    pub fps: f32,
    /// `true`: the pump is woken by the capture instead of a fixed rate (`fps` becomes a maximum wait delay).
    pub event: bool,
    /// Regulate the bitrate and the window of frames in flight from the client's acknowledgements.
    pub adaptive: bool,
    /// `true`: no EGFX, the desktop goes out as bitmap updates (RemoteFX) from the start of the connection.
    pub rfx: bool,
}

/// Bitmap updates that may wait for IronRDP while it encodes another one.
const MAX_QUEUED_BITMAPS: usize = 1;

/// How often the regulation looks at what the client acknowledged.
const RATE_EVERY: Duration = Duration::from_millis(500);

/// One output = one encoder + its last frame (re-encoded for a new client if the desktop is static).
struct OutputState {
    encoder: Box<dyn FrameEncoder>,
    last: Option<oxydesk_core::Frame>,
    /// The pixels the client was last given as bitmap updates: only what differs is sent next.
    shown: Vec<u8>,
    /// A picture was read but not sent (the client is behind): it goes out as soon as the client is ready.
    pending: bool,
}

/// How long a connection may go without a ready EGFX before falling back to bitmap updates.
const LEGACY_AFTER: Duration = Duration::from_secs(3);

/// The pixels of a pointer PDU: RDP wants BGRA rows from the bottom of the image to the top (the hot spot stays
/// counted from the top-left corner), the desktop provides RGBA rows from the top.
fn rdp_pointer_pixels(shape: &oxydesk_core::CursorShape) -> Vec<u8> {
    let stride = usize::from(shape.width) * 4;
    if stride == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(shape.rgba.len());
    for row in shape.rgba.chunks_exact(stride).rev() {
        for px in row.as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    out
}

/// The RDP pointer PDU for a cursor change (a single cache slot: the client keeps the latest shape).
fn pointer_update(update: &oxydesk_core::CursorUpdate) -> ironrdp_server::DisplayUpdate {
    match update {
        oxydesk_core::CursorUpdate::Hidden => ironrdp_server::DisplayUpdate::HidePointer,
        oxydesk_core::CursorUpdate::Shape(s) => {
            ironrdp_server::DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
                cache_index: 0,
                width: s.width,
                height: s.height,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                data: rdp_pointer_pixels(s),
            })
        }
    }
}

/// Sends the outputs as uncompressed bitmap updates (clients without EGFX; IronRDP encodes them).
/// `false`: the capture is lost.
#[allow(clippy::too_many_arguments)]
fn send_bitmaps(
    tx: &UnboundedSender<ironrdp_server::DisplayUpdate>,
    queued: &std::sync::atomic::AtomicUsize,
    capture: &mut dyn CaptureSource,
    outputs: &mut [OutputState],
    layout: &Layout,
    repeat: &mut bool,
    warned_encoded: &mut bool,
    depth: oxydesk_core::ColorDepth,
) -> bool {
    use std::num::{NonZeroU16, NonZeroUsize};
    let origins = bitmap_origins(layout);
    let mut deferred = false;
    // IronRDP still has a picture to encode: only the newest one is kept, and it goes out when the client is ready.
    // Looked at once per round: the outputs of one round go out together.
    let behind = queued.load(Ordering::SeqCst) >= MAX_QUEUED_BITMAPS;
    for (i, out) in outputs.iter_mut().enumerate() {
        let info = &layout.outputs[i];
        match capture.recv_latest(OutputId(i as u32)) {
            Ok(Some(f)) if (f.width, f.height) == (info.width, info.height) => {
                out.last = Some(f);
                if behind {
                    out.pending = true;
                    deferred = true;
                    continue;
                }
            }
            Ok(Some(f)) => {
                tracing::debug!(
                    output = i,
                    frame = ?(f.width, f.height),
                    expected = ?(info.width, info.height),
                    "frame of another size than the layout: dropped"
                );
                continue;
            }
            Ok(None) if behind => {
                deferred |= *repeat || out.pending;
                continue;
            }
            Ok(None) if (*repeat || out.pending) && out.last.is_some() => {}
            Ok(None) => continue,
            Err(e) => {
                tracing::error!("capture: {e}");
                return false;
            }
        }
        let Some(frame) = out.last.as_ref() else {
            continue;
        };
        let pixels = match crate::raw::to_bgra(frame) {
            Ok(Some(mut p)) => {
                depth.reduce_bgra(&mut p);
                p
            }
            Ok(None) => {
                if !std::mem::replace(warned_encoded, true) {
                    tracing::warn!(
                        "client without EGFX: frames already encoded by the agent cannot be sent"
                    );
                }
                continue;
            }
            Err(e) => {
                tracing::error!("raw frame: {e}");
                continue;
            }
        };
        // A client that asks again (new connection, resize) is given the whole picture.
        if *repeat {
            out.shown.clear();
        }
        let (full_w, full_h) = (info.width as usize, info.height as usize);
        let rect = if out.shown.len() == pixels.len() {
            crate::changed_area::changed_rect(&out.shown, &pixels, full_w, full_h)
        } else {
            Some((0, 0, full_w, full_h))
        };
        let Some((rx, ry, rw, rh)) = rect else {
            continue; // nothing moved
        };
        let data = if (rw, rh) == (full_w, full_h) {
            pixels.clone()
        } else {
            crate::changed_area::crop(&pixels, full_w, (rx, ry, rw, rh))
        };
        out.shown = pixels;
        let (Some(width), Some(height), Some(stride)) = (
            NonZeroU16::new(rw as u16),
            NonZeroU16::new(rh as u16),
            NonZeroUsize::new(rw * 4),
        ) else {
            continue;
        };
        out.pending = false;
        queued.fetch_add(1, Ordering::SeqCst);
        let _ = tx.send(ironrdp_server::DisplayUpdate::Bitmap(
            ironrdp_server::BitmapUpdate {
                x: (origins[i].0 + rx) as u16,
                y: (origins[i].1 + ry) as u16,
                width,
                height,
                format: ironrdp_server::PixelFormat::BgrA32,
                data: data.into(),
                stride,
            },
        ));
    }
    if !deferred {
        *repeat = false;
    }
    true
}

/// Where each output is drawn in the bitmap desktop: the client's monitors arranged as it announced them, the
/// top-left of the whole at (0, 0). A client puts the bitmaps on its monitors by these coordinates, so an output
/// that is on its right monitor must be drawn on the right of the desktop, whatever its rank.
fn bitmap_origins(layout: &Layout) -> Vec<(usize, usize)> {
    let monitors = monitors(layout);
    let left = monitors.iter().map(|m| m.left).min().unwrap_or(0);
    let top = monitors.iter().map(|m| m.top).min().unwrap_or(0);
    monitors
        .iter()
        .map(|m| ((m.left - left) as usize, (m.top - top) as usize))
        .collect()
}

/// Monitors as the client expects them (MS-RDPBCGR 2.2.1.3.6.1): its own arrangement, the primary one at (0, 0) and the
/// others around it (negative coordinates for those at its left or above). Without the client's frame (layout of the
/// server's own), the outputs' arrangement.
pub(crate) fn monitors(layout: &Layout) -> Vec<Monitor> {
    let rects: Vec<(i32, i32, u32, u32)> = if layout.frame.len() == layout.outputs.len() {
        layout
            .frame
            .iter()
            .map(|f| (f.x, f.y, f.width, f.height))
            .collect()
    } else {
        layout
            .outputs
            .iter()
            .map(|o| (o.x, o.y, o.width, o.height))
            .collect()
    };
    let (px, py) = layout
        .outputs
        .iter()
        .zip(&rects)
        .find(|(o, _)| o.primary)
        .map_or((0, 0), |(_, r)| (r.0, r.1));
    layout
        .outputs
        .iter()
        .zip(&rects)
        .map(|(o, &(x, y, w, h))| Monitor {
            left: x - px,
            top: y - py,
            right: x - px + w as i32 - 1,
            bottom: y - py + h as i32 - 1,
            flags: if o.primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        })
        .collect()
}

/// Layout of a new session that has `outputs` outputs: the configured one when the count is the same, otherwise
/// (login mode: the session was made for the client's monitors) outputs of the configured size side by side.
/// `None` when the count is not usable.
fn session_layout(configured: &Layout, outputs: usize) -> Option<Layout> {
    if !(1..=oxydesk_core::layout::MAX_OUTPUTS).contains(&outputs) {
        return None;
    }
    if outputs == configured.outputs.len() {
        return Some(configured.clone());
    }
    let first = configured.outputs.first()?;
    Some(Layout::side_by_side(
        outputs as u32,
        first.width,
        first.height,
    ))
}

pub fn spawn(shared: Arc<Shared>, p: PumpParams) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("frame-pump".into())
        .spawn(move || {
            let period = Duration::from_secs_f32(1.0 / p.fps);
            let started = Instant::now();
            let mut next = Instant::now();
            let mut layout = p.layout.clone();
            *shared.desktop.lock().expect("desktop mutex") = Some(layout.desktop_size());
            *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
            // The next surfaces must be announced with ResetGraphics even for a single output.
            let mut reset_graphics = false;
            // Client loop to wait for before announcing the new size through EGFX (reactivation in progress).
            let mut reactivation: Option<u64> = None;
            let mut stale_logged: Option<(u32, u32)> = None;
            // Since when frames of another size than the layout keep arriving (and which size).
            let mut mismatch: Option<(Instant, (u32, u32))> = None;
            let mut capture: Option<Box<dyn CaptureSource>> = None;
            let mut outputs: Vec<OutputState> = Vec::new();
            let mut make_encoder: Option<EncoderFactory> = None;
            // Current connection and one EGFX surface per output (empty until created).
            let mut current: Option<(GfxServerHandle, Vec<u16>)> = None;
            let (mut sent, mut dropped) = (0u64, 0u64);
            // Frames sent and held back by backpressure, for each output (reported with the totals).
            let mut per_output: Vec<(u64, u64)> = Vec::new();
            // Client without EGFX (or never ready): bitmap updates after `LEGACY_AFTER`, from the start of the connection.
            let (mut loops_seen, mut connected_at) = (0u64, Instant::now());
            let mut legacy_repeat = true;
            let mut warned_encoded = false;
            // What the source was last asked for: uncompressed pixels (client without H.264) or its own stream.
            let mut raw_requested = false;
            // Rate regulation of the current connection (restarted with it).
            let mut rate: Option<RateController> = None;
            let mut rate_at = Instant::now();
            let mut cursor: Option<oxydesk_core::CursorUpdate> = None;
            let mut cursor_pending = false;
            // A session of the current connection has just arrived: only then is the layout announced by the client
            // at connection followed. Earlier, the previous session still exists and would take it before the login
            // has read it (the session would then start with the configured number of outputs).
            let mut fresh_session = false;
            let mut repeat = false;
            let mut report = Instant::now();
            let (mut seen, mut drain) = (0u64, false);
            'pump: while !shared.stop.load(Ordering::Relaxed) {
                if p.event {
                    if !drain {
                        shared.wake.wait(seen, period);
                    }
                    seen = shared.wake.generation();
                    drain = false;
                } else {
                    next += period;
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    } else {
                        next = Instant::now(); // late: do not catch up.
                    }
                }
                let sent_before = sent;
                if let Some(src) = capture.as_mut() {
                    if let Some(update) = src.take_cursor() {
                        match &update {
                            oxydesk_core::CursorUpdate::Shape(s) => tracing::debug!(
                                w = s.width,
                                h = s.height,
                                hot_x = s.hot_x,
                                hot_y = s.hot_y,
                                "pointer shape"
                            ),
                            oxydesk_core::CursorUpdate::Hidden => tracing::debug!("pointer hidden"),
                        }
                        cursor = Some(update);
                        cursor_pending = true;
                    }
                }
                if cursor_pending {
                    if let (Some(update), Some(tx)) = (
                        cursor.as_ref(),
                        shared.display.lock().expect("display mutex").as_ref(),
                    ) {
                        let _ = tx.send(pointer_update(update));
                        cursor_pending = false;
                    }
                }
                if let Some(new) = shared.ports.lock().expect("ports mutex").take() {
                    let Some(session_layout) = session_layout(&p.layout, new.encoders.len()) else {
                        tracing::error!(
                            "session ignored: {} encoder(s) (1 to {} expected)",
                            new.encoders.len(),
                            oxydesk_core::layout::MAX_OUTPUTS
                        );
                        continue;
                    };
                    {
                        let mut c = new.capture;
                        c.set_waker(shared.wake.clone());
                        capture = Some(c);
                        raw_requested = false; // a new source starts in its default mode
                        make_encoder = new.make_encoder;
                        outputs = new
                            .encoders
                            .into_iter()
                            .map(|encoder| OutputState {
                                encoder,
                                last: None,
                                shown: Vec::new(),
                                pending: false,
                            })
                            .collect();
                        current = None;
                        fresh_session = true;
                        // A new session starts at the configured size (of as many outputs as it has).
                        layout = session_layout;
                        reset_graphics = false;
                        *shared.resize.lock().expect("resize mutex") = None;
                        *shared.desktop.lock().expect("desktop mutex") =
                            Some(layout.desktop_size());
                        *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
                    }
                }
                let follow_client = std::mem::take(&mut fresh_session);
                let requested = shared
                    .resize
                    .lock()
                    .expect("resize mutex")
                    .take()
                    .or_else(|| {
                        // Monitors announced by the client at connection: followed once the session exists.
                        if capture.is_some() && follow_client {
                            shared
                                .client_initial
                                .lock()
                                .expect("client layout mutex")
                                .take()
                        } else {
                            None
                        }
                    });
                if let (Some(new), Some(src)) = (requested, capture.as_mut()) {
                    let added = new.outputs.len() > outputs.len() && make_encoder.is_none();
                    // A session that has just started is always given the layout: KWin restores the positions of the
                    // outputs of the user's previous sessions, which may not be the ones wanted.
                    if new.outputs == layout.outputs && !follow_client {
                        // Same outputs: only the client's frame (pointer coordinates) may differ.
                        if new.frame != layout.frame {
                            layout.frame = new.frame;
                            *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
                        }
                    } else if added {
                        tracing::warn!(
                            "layout with {} outputs ignored: no encoder factory",
                            new.outputs.len()
                        );
                    } else {
                        match src.apply_layout(&new) {
                            Ok(()) => {
                                let (w, h) = new.desktop_size();
                                tracing::info!(w, h, outputs = new.outputs.len(), "layout applied");
                                outputs.truncate(new.outputs.len());
                                while outputs.len() < new.outputs.len() {
                                    let make = make_encoder.as_ref().expect("checked above");
                                    let mut encoder = make(OutputId(outputs.len() as u32));
                                    if let Some(rc) = &rate {
                                        encoder.set_bitrate(rc.target().bitrate_bps);
                                    }
                                    outputs.push(OutputState {
                                        encoder,
                                        last: None,
                                        shown: Vec::new(),
                                pending: false,
                                    });
                                }
                                layout = new;
                                *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
                                *shared.desktop.lock().expect("desktop mutex") = Some((w, h));
                                reset_graphics = true;
                                // The client must renegotiate the desktop size (deactivation-reactivation)
                                // before the new EGFX surfaces, unless it was negotiated at connection.
                                let announced =
                                    shared.announced.lock().expect("announced mutex").take()
                                        == Some(layout.clone());
                                if let (false, Some(tx)) = (
                                    announced,
                                    shared.display.lock().expect("display mutex").as_ref(),
                                ) {
                                    reactivation = Some(shared.client_loops.load(Ordering::SeqCst));
                                    let _ = tx.send(ironrdp_server::DisplayUpdate::Resize(
                                        ironrdp_server::DesktopSize {
                                            width: w as u16,
                                            height: h as u16,
                                        },
                                    ));
                                }
                                if let Some((_, surfaces)) = current.as_mut() {
                                    surfaces.clear();
                                }
                                for (o, info) in outputs.iter_mut().zip(&layout.outputs) {
                                    // Old size: never re-encoded. A frame of the right size is kept, since a
                                    // static desktop sends no other.
                                    if o.last.as_ref().is_some_and(|f| {
                                        (f.width, f.height) != (info.width, info.height)
                                    }) {
                                        o.last = None;
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(
                                "layout ({} outputs) ignored: {e}",
                                new.outputs.len()
                            ),
                        }
                    }
                }
                if reactivation.is_some_and(|n| shared.client_loops.load(Ordering::SeqCst) > n) {
                    reactivation = None;
                }
                let Some(capture_src) = capture.as_mut() else {
                    continue;
                };
                if reactivation.is_some() {
                    continue; // the client is renegotiating the desktop size
                }
                let loops = shared.client_loops.load(Ordering::SeqCst);
                if loops != loops_seen {
                    (loops_seen, connected_at) = (loops, Instant::now());
                    legacy_repeat = true;
                }
                // The depth the user chose in the client: the encoders (the agent's too) and the bitmap path reduce to it.
                let depth = oxydesk_core::ColorDepth::from_bits(shared.color_depth.load(Ordering::SeqCst));
                for o in &mut outputs {
                    o.encoder.set_color_depth(depth);
                }
                let handle = shared.gfx.lock().expect("gfx mutex").clone();
                if handle.is_none() && !p.rfx {
                    continue;
                }
                let egfx = handle
                    .as_ref()
                    .filter(|h| !p.rfx && h.lock().expect("gfx handle mutex").is_ready());
                let Some(handle) = egfx.cloned() else {
                    // RemoteFX mode: bitmap updates from the start; otherwise once the EGFX has been missing for a while.
                    let legacy_after = if p.rfx { Duration::ZERO } else { LEGACY_AFTER };
                    if connected_at.elapsed() >= legacy_after && !raw_requested {
                        tracing::info!("bitmap updates ({}): uncompressed pixels requested", if p.rfx { "RemoteFX mode" } else { "no EGFX" });
                        capture_src.set_raw_pixels(true);
                        raw_requested = true;
                    }
                    if connected_at.elapsed() >= legacy_after && loops > 0 {
                        let tx = shared.display.lock().expect("display mutex").clone();
                        if let Some(tx) = tx {
                            if !send_bitmaps(
                                &tx,
                                &shared.bitmaps_queued,
                                capture_src.as_mut(),
                                &mut outputs,
                                &layout,
                                &mut legacy_repeat,
                                &mut warned_encoded,
                                depth,
                            ) {
                                capture = None;
                                outputs.clear();
                                current = None;
                            }
                        }
                    }
                    continue;
                };
                if !current
                    .as_ref()
                    .is_some_and(|(h, _)| Arc::ptr_eq(h, &handle))
                {
                    current = Some((handle.clone(), Vec::new())); // new connection
                    rate = p.adaptive.then(|| {
                        RateController::new(RateConfig::new(p.fps, DEFAULT_BITRATE_BPS))
                    });
                    rate_at = Instant::now();
                    // Whatever the previous connection left: back to the ceiling.
                    for o in &mut outputs {
                        o.encoder.set_bitrate(DEFAULT_BITRATE_BPS);
                    }
                }
                let (handle, surfaces) = current.as_mut().expect("set above");
                // A client that declared it cannot decode H.264 gets uncompressed pixels.
                let h264;
                {
                    let mut g = handle.lock().expect("gfx handle mutex");
                    h264 = g.supports_avc420();
                    if raw_requested == h264 {
                        tracing::info!(h264, "capture mode: {}", if h264 { "H.264 stream" } else { "uncompressed pixels" });
                        capture_src.set_raw_pixels(!h264);
                        raw_requested = !h264;
                    }
                    if surfaces.is_empty() {
                        tracing::debug!(
                            outputs = layout.outputs.len(),
                            reset_graphics,
                            desktop = ?layout.desktop_size(),
                            "creating the EGFX surfaces"
                        );
                        // Multiple screens: ResetGraphics announces the monitor list before the surfaces.
                        if layout.outputs.len() > 1 || reset_graphics {
                            let (desktop_w, desktop_h) = layout.desktop_size();
                            g.resize_with_monitors(
                                desktop_w as u16,
                                desktop_h as u16,
                                monitors(&layout),
                            );
                            reset_graphics = false;
                        }
                        // Surfaces are created in the order of the monitor list (primary first): that is the order in which
                        // the client puts them on its monitors, whatever the positions announced. `created` is indexed by
                        // output.
                        let order: Vec<usize> = (0..layout.outputs.len()).collect();
                        let mut created = vec![0u16; layout.outputs.len()];
                        for i in order {
                            let o = &layout.outputs[i];
                            let Some(id) = g.create_surface(o.width as u16, o.height as u16) else {
                                continue 'pump;
                            };
                            g.map_surface_to_output(id, o.x.max(0) as u32, o.y.max(0) as u32);
                            tracing::debug!(output = i, surface = id, x = o.x, y = o.y, "EGFX surface mapped");
                            created[i] = id;
                        }
                        *surfaces = created;
                        for o in &mut outputs {
                            o.encoder.request_keyframe();
                        }
                        repeat = true;
                    }
                    if let (Some(rc), true) = (rate.as_mut(), rate_at.elapsed() >= RATE_EVERY) {
                        rate_at = Instant::now();
                        if let Some(q) = g.qoe_snapshot() {
                            let sample = NetSample {
                                avg_rtt_ms: q.avg_rtt_ms,
                                min_rtt_ms: q.min_rtt_ms,
                                rtt_samples: q.total_rtt_samples,
                                sent,
                                dropped,
                            };
                            if let Some(t) = rc.update(&sample) {
                                tracing::info!(
                                    bitrate_bps = t.bitrate_bps,
                                    max_in_flight = t.max_in_flight,
                                    rtt_ms = q.avg_rtt_ms,
                                    best_rtt_ms = q.min_rtt_ms,
                                    sent,
                                    dropped,
                                    "rate regulation"
                                );
                                // One frame per output goes in flight at each tick: the window is for the whole connection.
                                g.set_max_frames_in_flight(t.max_in_flight * outputs.len().max(1) as u32);
                                for o in &mut outputs {
                                    o.encoder.set_bitrate(t.bitrate_bps);
                                }
                            }
                        }
                    }
                    if g.should_backpressure() {
                        dropped += 1;
                        if dropped % 200 == 1 {
                            tracing::debug!(
                                sent,
                                dropped,
                                "egfx backpressure: waiting for frame acknowledgements"
                            );
                        }
                        continue;
                    }
                }
                let mut lost = false;
                // An output whose frame waited for a free slot of the window is sent again at the next tick.
                let mut deferred = false;
                let outputs_len = outputs.len();
                for (i, out) in outputs.iter_mut().enumerate() {
                    match capture_src.recv_latest(OutputId(i as u32)) {
                        // Frame of another size: ignored (resize in progress, or the compositor did not
                        // start at the configured size, e.g. a size remembered from a previous session).
                        Ok(Some(f))
                            if (f.width, f.height)
                                != (layout.outputs[i].width, layout.outputs[i].height) =>
                        {
                            let size = (f.width, f.height);
                            if stale_logged != Some(size) {
                                tracing::debug!(
                                    "frame {}x{} ignored (expected {}x{})",
                                    f.width,
                                    f.height,
                                    layout.outputs[i].width,
                                    layout.outputs[i].height
                                );
                                stale_logged = Some(size);
                            }
                            let since = match mismatch {
                                Some((t, s)) if s == size => t,
                                _ => Instant::now(),
                            };
                            mismatch = Some((since, size));
                            // A resize in flight is over within a second: if the mismatch lasts,
                            // ask for the layout size, otherwise follow the compositor's size.
                            if since.elapsed() > Duration::from_secs(1)
                                && layout.outputs.len() == 1
                                && shared.resize.lock().expect("resize mutex").is_none()
                            {
                                mismatch = None;
                                let (lw, lh) = layout.desktop_size();
                                match capture_src.resize(lw, lh) {
                                    Ok(()) => tracing::info!(lw, lh, "desktop size restored"),
                                    Err(e) => {
                                        tracing::info!(
                                            "desktop stays {}x{} ({e}): announced to the client",
                                            f.width,
                                            f.height
                                        );
                                        *shared.resize.lock().expect("resize mutex") =
                                            Some(Layout::side_by_side(1, f.width, f.height & !1));
                                    }
                                }
                            }
                            continue;
                        }
                        Ok(Some(f)) => {
                            mismatch = None;
                            out.last = Some(f);
                        }
                        Ok(None) if repeat && out.last.is_some() => {}
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::error!("capture: {e}");
                            lost = true;
                            break;
                        }
                    }
                    let Some(frame) = out.last.as_ref() else {
                        continue;
                    };
                    let pre_encoded = matches!(frame.data, FrameData::Encoded { .. });
                    // A frame encoded but not sent leaves the encoder ahead of the client's decoder: the next inter
                    // frames are drawn over a picture the client does not have (smeared blocks, until a keyframe).
                    // Once an output has sent its frame the window can be full for the next one: check before encoding.
                    if h264 && handle.lock().expect("gfx handle mutex").should_backpressure() {
                        dropped += 1;
                        per_output.resize(outputs_len.max(per_output.len()), (0, 0));
                        per_output[i].1 += 1;
                        if pre_encoded {
                            out.last = None;
                            out.encoder.request_keyframe();
                        } else {
                            deferred = true;
                        }
                        continue;
                    }
                    let payload = if h264 {
                        match out.encoder.encode(frame) {
                            // An empty payload is a frame the rate control skipped.
                            Ok(e) if e.data.is_empty() => continue,
                            Ok(e) => e.data,
                            Err(e) => {
                                tracing::error!("encoding: {e}");
                                lost = true;
                                break;
                            }
                        }
                    } else {
                        match crate::raw::to_bgra(frame) {
                            Ok(Some(pixels)) => pixels,
                            Ok(None) => {
                                if !std::mem::replace(&mut warned_encoded, true) {
                                    tracing::warn!("client without H.264: frames already encoded by the agent cannot be sent");
                                }
                                continue;
                            }
                            Err(e) => {
                                tracing::error!("raw frame: {e}");
                                continue;
                            }
                        }
                    };
                    if pre_encoded {
                        // An already encoded inter frame is not re-sent: the "repeat" restarts from a keyframe from the agent.
                        out.last = None;
                    }
                    let info = &layout.outputs[i];
                    let (messages, channel) = {
                        let mut g = handle.lock().expect("gfx handle mutex");
                        let region =
                            Avc420Region::full_frame(info.width as u16, info.height as u16, 22);
                        let ts = started.elapsed().as_millis() as u32;
                        let queued = if h264 {
                            g.send_avc420_frame(surfaces[i], &payload, &[region], ts)
                        } else {
                            g.send_uncompressed_frame(
                                surfaces[i],
                                &payload,
                                info.width as u16,
                                info.height as u16,
                                ts,
                            )
                        };
                        if queued.is_none() {
                            dropped += 1;
                            out.encoder.request_keyframe();
                            continue;
                        }
                        (g.drain_output(), g.channel_id())
                    };
                    sent += 1;
                    per_output.resize(outputs_len.max(per_output.len()), (0, 0));
                    per_output[i].0 += 1;
                    if let Some(channel) = channel {
                        match encode_dvc_messages(
                            channel,
                            messages,
                            ironrdp_svc::ChannelFlags::empty(),
                        ) {
                            Ok(svc) => {
                                if let Some(tx) =
                                    shared.sender.lock().expect("sender mutex").as_ref()
                                {
                                    let _ = tx.send(ServerEvent::Egfx(
                                        EgfxServerMessage::SendMessages { messages: svc },
                                    ));
                                }
                            }
                            Err(e) => tracing::error!("DVC encoding: {e}"),
                        }
                    }
                }
                if lost {
                    // Session lost (agent dead, stream closed): wait for the next one instead of stopping the pump.
                    capture = None;
                    outputs.clear();
                    current = None;
                    continue;
                }
                drain = sent > sent_before; // frames may still be waiting: no sleep
                repeat = deferred;
                if report.elapsed() >= Duration::from_secs(10) {
                    let q = handle.lock().expect("gfx handle mutex").qoe_snapshot();
                    tracing::info!(
                        sent,
                        dropped,
                        rtt_ms = q.as_ref().map_or(0.0, |q| q.avg_rtt_ms),
                        best_rtt_ms = q.as_ref().map_or(0.0, |q| q.min_rtt_ms),
                        bitrate_bps = rate.as_ref().map_or(0, |r| r.target().bitrate_bps),
                        per_output = ?per_output,
                        "frame-pump"
                    );
                    report = Instant::now();
                }
            }
        })
        .expect("spawn frame-pump")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_with_another_number_of_outputs_gets_outputs_of_the_configured_size() {
        let configured = Layout::side_by_side(1, 1920, 1080);
        assert_eq!(session_layout(&configured, 1), Some(configured.clone()));
        let two = session_layout(&configured, 2).unwrap();
        assert_eq!(two.outputs.len(), 2);
        assert_eq!(two.desktop_size(), (3840, 1080));
        assert!(session_layout(&configured, 0).is_none());
        assert!(session_layout(&configured, oxydesk_core::layout::MAX_OUTPUTS + 1).is_none());
    }

    use super::*;
    use oxydesk_core::error::{CaptureError, EncodeError};
    use oxydesk_core::ports::EncodedFrame;

    #[test]
    fn pointer_pdu_carries_the_shape_and_hidden_hides() {
        let shape =
            oxydesk_core::CursorShape::new(2, 1, 1, 0, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        match pointer_update(&oxydesk_core::CursorUpdate::Shape(shape)) {
            ironrdp_server::DisplayUpdate::RGBAPointer(p) => {
                assert_eq!((p.width, p.height, p.hot_x, p.hot_y), (2, 1, 1, 0));
                assert_eq!(p.data, vec![3, 2, 1, 4, 7, 6, 5, 8]);
            }
            other => panic!("pointer shape expected, got {other:?}"),
        }
        assert!(matches!(
            pointer_update(&oxydesk_core::CursorUpdate::Hidden),
            ironrdp_server::DisplayUpdate::HidePointer
        ));
    }

    #[test]
    fn pointer_rows_go_bottom_up_in_bgra() {
        // 1 pixel wide, 2 rows: the top pixel is red, the bottom one is blue.
        let shape =
            oxydesk_core::CursorShape::new(1, 2, 0, 0, vec![255, 0, 0, 255, 0, 0, 255, 128])
                .unwrap();
        assert_eq!(
            rdp_pointer_pixels(&shape),
            vec![255, 0, 0, 128, 0, 0, 255, 255]
        );
    }

    struct Capture {
        refuse_above: usize,
    }

    impl CaptureSource for Capture {
        fn recv_latest(
            &mut self,
            _: OutputId,
        ) -> Result<Option<oxydesk_core::Frame>, CaptureError> {
            Ok(None)
        }
        fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
            if layout.outputs.len() > self.refuse_above {
                Err(CaptureError::ResizeUnsupported)
            } else {
                Ok(())
            }
        }
    }

    struct Encoder;

    impl FrameEncoder for Encoder {
        fn encode(&mut self, _: &oxydesk_core::Frame) -> Result<EncodedFrame, EncodeError> {
            Ok(EncodedFrame::default())
        }
    }

    fn start(
        refuse_above: usize,
        factory: bool,
    ) -> (
        Arc<Shared>,
        JoinHandle<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = made.clone();
        let shared = Arc::new(Shared::default());
        *shared.ports.lock().unwrap() = Some(PumpPorts {
            capture: Box::new(Capture { refuse_above }),
            encoders: vec![Box::new(Encoder) as Box<dyn FrameEncoder>],
            make_encoder: factory.then(|| {
                Box::new(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Box::new(Encoder) as Box<dyn FrameEncoder>
                }) as EncoderFactory
            }),
        });
        let handle = spawn(
            shared.clone(),
            PumpParams {
                layout: Layout::side_by_side(1, 800, 600),
                fps: 200.0,
                event: false,
                adaptive: false,
                rfx: false,
            },
        );
        // The session is adopted (and any pending request dropped) on the first tick.
        while shared.ports.lock().unwrap().is_some() {
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(20));
        (shared, handle, made)
    }

    fn wait_desktop(shared: &Shared, expected: (u32, u32)) -> bool {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if *shared.desktop.lock().unwrap() == Some(expected) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn stop(shared: &Shared, handle: JoinHandle<()>) {
        shared.stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn follows_a_growing_then_shrinking_number_of_outputs() {
        let (shared, handle, made) = start(4, true);
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(3, 640, 480));
        assert!(wait_desktop(&shared, (1920, 480)));
        assert_eq!(
            shared
                .layout
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .outputs
                .len(),
            3
        );
        assert_eq!(
            made.load(Ordering::SeqCst),
            2,
            "one encoder per added output"
        );
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(1, 800, 600));
        assert!(wait_desktop(&shared, (800, 600)));
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(2, 640, 480));
        assert!(wait_desktop(&shared, (1280, 480)));
        assert_eq!(
            made.load(Ordering::SeqCst),
            3,
            "the removed encoders are recreated on demand"
        );
        stop(&shared, handle);
    }

    #[test]
    fn refused_layout_keeps_the_current_one() {
        let (shared, handle, _) = start(1, true);
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(3, 640, 480));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*shared.desktop.lock().unwrap(), Some((800, 600)));
        assert_eq!(
            shared
                .layout
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .outputs
                .len(),
            1
        );
        stop(&shared, handle);
    }

    #[test]
    fn monitors_put_the_primary_at_the_origin() {
        let layout = Layout::from_monitors(&[
            oxydesk_core::layout::MonitorSpec {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                primary: true,
            },
            oxydesk_core::layout::MonitorSpec {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                primary: false,
            },
        ])
        .unwrap();
        assert_eq!(layout.desktop_size(), (3840, 1080));
        let m = monitors(&layout);
        assert_eq!((m[0].left, m[0].top, m[0].right), (0, 0, 1919));
        assert!(m[0].flags.contains(MonitorFlags::PRIMARY));
        assert_eq!((m[1].left, m[1].right), (-1920, -1));
    }

    #[test]
    fn bitmaps_follow_the_arrangement_of_the_client_monitors() {
        let spec = |x, primary| oxydesk_core::layout::MonitorSpec {
            x,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        };
        // The primary monitor is on the right of the other one: its output is drawn on the right of the desktop.
        let layout = Layout::from_monitors(&[spec(0, true), spec(-1920, false)]).unwrap();
        assert_eq!(bitmap_origins(&layout), vec![(1920, 0), (0, 0)]);
        let layout = Layout::from_monitors(&[spec(0, true), spec(1920, false)]).unwrap();
        assert_eq!(bitmap_origins(&layout), vec![(0, 0), (1920, 0)]);
        assert_eq!(
            bitmap_origins(&Layout::side_by_side(2, 640, 480)),
            vec![(0, 0), (640, 0)]
        );
    }

    #[test]
    fn more_outputs_without_an_encoder_factory_are_ignored() {
        let (shared, handle, _) = start(4, false);
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(2, 640, 480));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*shared.desktop.lock().unwrap(), Some((800, 600)));
        stop(&shared, handle);
    }

    // --- clients without EGFX: bitmap updates -------------------------------------------------------------------

    use oxydesk_core::{Frame, FrameData, PixelFormat};
    use std::collections::VecDeque;

    /// A capture that answers from a script, one answer per call.
    struct Scripted(VecDeque<Result<Option<Frame>, CaptureError>>);

    impl CaptureSource for Scripted {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
        fn apply_layout(&mut self, _: &Layout) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    fn frame(w: u32, h: u32, data: FrameData) -> Frame {
        Frame {
            output: OutputId(0),
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Rgba8,
            data,
        }
    }

    fn pixels(w: u32, h: u32) -> FrameData {
        FrameData::Shm(
            (0..w * h)
                .flat_map(|i| [1, 2, 3, (i % 256) as u8])
                .collect(),
        )
    }

    fn outputs(n: usize) -> Vec<OutputState> {
        (0..n)
            .map(|_| OutputState {
                encoder: Box::new(Encoder),
                last: None,
                shown: Vec::new(),
                pending: false,
            })
            .collect()
    }

    struct Run {
        sent: Vec<ironrdp_server::DisplayUpdate>,
        alive: bool,
        repeat: bool,
        warned: bool,
    }

    fn send(
        script: Vec<Result<Option<Frame>, CaptureError>>,
        layout: &Layout,
        state: &mut [OutputState],
        repeat: bool,
    ) -> Run {
        send_at(
            oxydesk_core::ColorDepth::TrueColor,
            script,
            layout,
            state,
            repeat,
        )
    }

    fn send_at(
        depth: oxydesk_core::ColorDepth,
        script: Vec<Result<Option<Frame>, CaptureError>>,
        layout: &Layout,
        state: &mut [OutputState],
        repeat: bool,
    ) -> Run {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut repeat, mut warned) = (repeat, false);
        let alive = send_bitmaps(
            &tx,
            &std::sync::atomic::AtomicUsize::new(0),
            &mut Scripted(script.into()),
            state,
            layout,
            &mut repeat,
            &mut warned,
            depth,
        );
        let mut sent = Vec::new();
        while let Ok(u) = rx.try_recv() {
            sent.push(u);
        }
        Run {
            sent,
            alive,
            repeat,
            warned,
        }
    }

    #[test]
    fn a_client_that_is_behind_gets_only_the_newest_picture_when_it_is_ready() {
        use std::sync::atomic::AtomicUsize;
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        let queued = AtomicUsize::new(MAX_QUEUED_BITMAPS);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut repeat, mut warned) = (false, false);
        let depth = oxydesk_core::ColorDepth::TrueColor;
        let mut round = |script: Vec<Result<Option<Frame>, CaptureError>>,
                         state: &mut [OutputState]| {
            send_bitmaps(
                &tx,
                &queued,
                &mut Scripted(script.into()),
                state,
                &layout,
                &mut repeat,
                &mut warned,
                depth,
            )
        };
        // Two pictures arrive while IronRDP is busy: nothing goes out, the newest is kept.
        assert!(round(vec![Ok(Some(frame(4, 2, pixels(4, 2))))], &mut state));
        assert!(round(
            vec![Ok(Some(frame(4, 2, FrameData::Shm(vec![9; 32]))))],
            &mut state
        ));
        assert!(rx.try_recv().is_err());
        assert!(state[0].pending);
        // IronRDP is free again and the desktop is static: the kept picture goes out, once.
        queued.store(0, Ordering::SeqCst);
        assert!(round(vec![Ok(None)], &mut state));
        match rx.try_recv() {
            Ok(ironrdp_server::DisplayUpdate::Bitmap(b)) => assert_eq!(&b.data[..4], &[9, 9, 9, 9]),
            other => panic!("the kept picture expected, got {other:?}"),
        }
        assert!(!state[0].pending);
        assert_eq!(queued.load(Ordering::SeqCst), 1);
        assert!(round(vec![Ok(None)], &mut state));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_frame_is_sent_as_a_bitmap_at_the_position_of_its_output() {
        let layout = Layout::side_by_side(2, 4, 2);
        let mut state = outputs(2);
        let run = send(
            vec![
                Ok(Some(frame(4, 2, pixels(4, 2)))),
                Ok(Some(frame(4, 2, pixels(4, 2)))),
            ],
            &layout,
            &mut state,
            false,
        );
        assert!(run.alive && !run.repeat);
        assert_eq!(run.sent.len(), 2);
        let positions: Vec<(u16, u16, u16, u16)> = run
            .sent
            .iter()
            .map(|u| match u {
                ironrdp_server::DisplayUpdate::Bitmap(b) => {
                    assert_eq!(b.data.len(), 4 * 2 * 4);
                    assert_eq!(b.stride.get(), 16);
                    (b.x, b.y, b.width.get(), b.height.get())
                }
                other => panic!("bitmap expected, got {other:?}"),
            })
            .collect();
        assert_eq!(positions, vec![(0, 0, 4, 2), (4, 0, 4, 2)]);
    }

    #[test]
    fn the_bitmaps_are_reduced_to_the_depth_chosen_by_the_client() {
        let layout = Layout::side_by_side(1, 1, 1);
        let mut state = outputs(1);
        let f = frame(1, 1, FrameData::Shm(vec![100, 100, 100, 40]));
        let run = send_at(
            oxydesk_core::ColorDepth::Rgb555,
            vec![Ok(Some(f))],
            &layout,
            &mut state,
            false,
        );
        match &run.sent[0] {
            ironrdp_server::DisplayUpdate::Bitmap(b) => assert_eq!(&b.data[..], &[99, 99, 99, 40]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_pixels_are_sent_as_bgra() {
        let layout = Layout::side_by_side(1, 1, 1);
        let mut state = outputs(1);
        let f = frame(1, 1, FrameData::Shm(vec![10, 20, 30, 40]));
        let run = send(vec![Ok(Some(f))], &layout, &mut state, false);
        match &run.sent[0] {
            ironrdp_server::DisplayUpdate::Bitmap(b) => assert_eq!(&b.data[..], &[30, 20, 10, 40]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_frame_of_another_size_is_ignored_and_a_missing_one_is_not_repeated() {
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        let run = send(
            vec![Ok(Some(frame(8, 8, pixels(8, 8))))],
            &layout,
            &mut state,
            false,
        );
        assert!(run.alive && run.sent.is_empty());
        assert!(state[0].last.is_none());
        let run = send(vec![Ok(None)], &layout, &mut state, false);
        assert!(run.alive && run.sent.is_empty());
    }

    #[test]
    fn a_new_client_gets_the_last_picture_again_even_if_the_desktop_is_static() {
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        send(
            vec![Ok(Some(frame(4, 2, pixels(4, 2))))],
            &layout,
            &mut state,
            false,
        );
        // No new frame, but the connection is new: `repeat` sends what was kept, once.
        let run = send(vec![Ok(None)], &layout, &mut state, true);
        assert_eq!(run.sent.len(), 1);
        assert!(!run.repeat);
        let run = send(vec![Ok(None)], &layout, &mut state, false);
        assert!(run.sent.is_empty());
    }

    #[test]
    fn a_lost_capture_is_reported_to_the_caller() {
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        let run = send(
            vec![Err(CaptureError::ResizeUnsupported)],
            &layout,
            &mut state,
            false,
        );
        assert!(!run.alive);
        assert!(run.sent.is_empty());
    }

    #[test]
    fn frames_already_encoded_by_the_agent_cannot_be_sent_as_bitmaps() {
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        let encoded = FrameData::Encoded {
            keyframe: true,
            data: vec![0, 0, 0, 1],
        };
        let run = send(
            vec![Ok(Some(frame(4, 2, encoded)))],
            &layout,
            &mut state,
            false,
        );
        assert!(run.alive && run.sent.is_empty());
        assert!(run.warned, "the limitation is reported");
    }

    #[test]
    fn a_truncated_buffer_is_skipped_without_stopping_the_stream() {
        let layout = Layout::side_by_side(1, 4, 2);
        let mut state = outputs(1);
        let short = FrameData::Shm(vec![0; 8]);
        let run = send(
            vec![Ok(Some(frame(4, 2, short)))],
            &layout,
            &mut state,
            false,
        );
        assert!(run.alive && run.sent.is_empty());
    }
}
