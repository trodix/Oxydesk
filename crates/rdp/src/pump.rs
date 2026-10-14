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
use rdsktp_core::layout::{Layout, OutputId};
use rdsktp_core::ports::{CaptureSource, EncoderFactory, FrameEncoder};
use rdsktp_core::{FrameData, NetSample, RateConfig, RateController, DEFAULT_BITRATE_BPS};
use tokio::sync::mpsc::UnboundedSender;

/// State shared between the IronRDP factories (created per connection) and the worker threads.
#[derive(Default)]
pub struct Shared {
    pub sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    pub gfx: Mutex<Option<GfxServerHandle>>,
    pub stop: AtomicBool,
    /// Notified by the capture on every new frame (`event` mode).
    pub wake: Arc<rdsktp_core::wake::Wake>,
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
}

/// How often the regulation looks at what the client acknowledged.
const RATE_EVERY: Duration = Duration::from_millis(500);

/// One output = one encoder + its last frame (re-encoded for a new client if the desktop is static).
struct OutputState {
    encoder: Box<dyn FrameEncoder>,
    last: Option<rdsktp_core::Frame>,
}

/// How long a connection may go without a ready EGFX before falling back to bitmap updates.
const LEGACY_AFTER: Duration = Duration::from_secs(3);

/// The RDP pointer PDU for a cursor change (a single cache slot: the client keeps the latest shape).
fn pointer_update(update: &rdsktp_core::CursorUpdate) -> ironrdp_server::DisplayUpdate {
    match update {
        rdsktp_core::CursorUpdate::Hidden => ironrdp_server::DisplayUpdate::HidePointer,
        rdsktp_core::CursorUpdate::Shape(s) => {
            ironrdp_server::DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
                cache_index: 0,
                width: s.width,
                height: s.height,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                data: s.rgba.clone(),
            })
        }
    }
}

/// Sends the outputs as uncompressed bitmap updates (clients without EGFX; IronRDP encodes them).
/// `false`: the capture is lost.
fn send_bitmaps(
    tx: &UnboundedSender<ironrdp_server::DisplayUpdate>,
    capture: &mut dyn CaptureSource,
    outputs: &mut [OutputState],
    layout: &Layout,
    repeat: &mut bool,
    warned_encoded: &mut bool,
) -> bool {
    use std::num::{NonZeroU16, NonZeroUsize};
    for (i, out) in outputs.iter_mut().enumerate() {
        let info = &layout.outputs[i];
        match capture.recv_latest(OutputId(i as u32)) {
            Ok(Some(f)) if (f.width, f.height) == (info.width, info.height) => out.last = Some(f),
            Ok(Some(_)) => continue,
            Ok(None) if *repeat && out.last.is_some() => {}
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
            Ok(Some(p)) => p,
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
        let (Some(width), Some(height), Some(stride)) = (
            NonZeroU16::new(info.width as u16),
            NonZeroU16::new(info.height as u16),
            NonZeroUsize::new(info.width as usize * 4),
        ) else {
            continue;
        };
        let _ = tx.send(ironrdp_server::DisplayUpdate::Bitmap(
            ironrdp_server::BitmapUpdate {
                x: info.x.max(0) as u16,
                y: info.y.max(0) as u16,
                width,
                height,
                format: ironrdp_server::PixelFormat::BgrA32,
                data: pixels.into(),
                stride,
            },
        ));
    }
    *repeat = false;
    true
}

pub(crate) fn monitors(layout: &Layout) -> Vec<Monitor> {
    layout
        .outputs
        .iter()
        .map(|o| Monitor {
            left: o.x,
            top: o.y,
            right: o.x + o.width as i32 - 1,
            bottom: o.y + o.height as i32 - 1,
            flags: if o.primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        })
        .collect()
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
            // Client without EGFX (or never ready): bitmap updates after `LEGACY_AFTER`, from the start of the connection.
            let (mut loops_seen, mut connected_at) = (0u64, Instant::now());
            let mut legacy_repeat = true;
            let mut warned_encoded = false;
            // What the source was last asked for: uncompressed pixels (client without H.264) or its own stream.
            let mut raw_requested = false;
            // Rate regulation of the current connection (restarted with it).
            let mut rate: Option<RateController> = None;
            let mut rate_at = Instant::now();
            let mut cursor: Option<rdsktp_core::CursorUpdate> = None;
            let mut cursor_pending = false;
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
                            rdsktp_core::CursorUpdate::Shape(s) => tracing::debug!(
                                w = s.width,
                                h = s.height,
                                hot_x = s.hot_x,
                                hot_y = s.hot_y,
                                "pointer shape"
                            ),
                            rdsktp_core::CursorUpdate::Hidden => tracing::debug!("pointer hidden"),
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
                    if new.encoders.len() != p.layout.outputs.len() {
                        tracing::error!(
                            "session ignored: {} encoder(s) for {} output(s)",
                            new.encoders.len(),
                            p.layout.outputs.len()
                        );
                    } else {
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
                            })
                            .collect();
                        current = None;
                        // A new session starts at the configured size.
                        layout = p.layout.clone();
                        reset_graphics = false;
                        *shared.resize.lock().expect("resize mutex") = None;
                        *shared.desktop.lock().expect("desktop mutex") =
                            Some(layout.desktop_size());
                        *shared.layout.lock().expect("layout mutex") = Some(layout.clone());
                    }
                }
                let requested = shared
                    .resize
                    .lock()
                    .expect("resize mutex")
                    .take()
                    .or_else(|| {
                        // Monitors announced by the client at connection: followed once the session exists.
                        if capture.is_some() {
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
                    if new == layout {
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
                let Some(handle) = shared.gfx.lock().expect("gfx mutex").clone() else {
                    continue;
                };
                if !handle.lock().expect("gfx handle mutex").is_ready() {
                    if connected_at.elapsed() >= LEGACY_AFTER && !raw_requested {
                        tracing::info!("no EGFX after {LEGACY_AFTER:?}: uncompressed pixels requested");
                        capture_src.set_raw_pixels(true);
                        raw_requested = true;
                    }
                    if connected_at.elapsed() >= LEGACY_AFTER && loops > 0 {
                        let tx = shared.display.lock().expect("display mutex").clone();
                        if let Some(tx) = tx {
                            if !send_bitmaps(
                                &tx,
                                capture_src.as_mut(),
                                &mut outputs,
                                &layout,
                                &mut legacy_repeat,
                                &mut warned_encoded,
                            ) {
                                capture = None;
                                outputs.clear();
                                current = None;
                            }
                        }
                    }
                    continue;
                }
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
                        let mut created = Vec::new();
                        for o in &layout.outputs {
                            let Some(id) = g.create_surface(o.width as u16, o.height as u16) else {
                                continue 'pump;
                            };
                            g.map_surface_to_output(id, o.x.max(0) as u32, o.y.max(0) as u32);
                            created.push(id);
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
                                g.set_max_frames_in_flight(t.max_in_flight);
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
                            continue;
                        }
                        (g.drain_output(), g.channel_id())
                    };
                    sent += 1;
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
                repeat = false;
                if report.elapsed() >= Duration::from_secs(10) {
                    let q = handle.lock().expect("gfx handle mutex").qoe_snapshot();
                    tracing::info!(
                        sent,
                        dropped,
                        rtt_ms = q.as_ref().map_or(0.0, |q| q.avg_rtt_ms),
                        best_rtt_ms = q.as_ref().map_or(0.0, |q| q.min_rtt_ms),
                        bitrate_bps = rate.as_ref().map_or(0, |r| r.target().bitrate_bps),
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
    use super::*;
    use rdsktp_core::error::{CaptureError, EncodeError};
    use rdsktp_core::ports::EncodedFrame;

    #[test]
    fn pointer_pdu_carries_the_shape_and_hidden_hides() {
        let shape =
            rdsktp_core::CursorShape::new(2, 1, 1, 0, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        match pointer_update(&rdsktp_core::CursorUpdate::Shape(shape)) {
            ironrdp_server::DisplayUpdate::RGBAPointer(p) => {
                assert_eq!((p.width, p.height, p.hot_x, p.hot_y), (2, 1, 1, 0));
                assert_eq!(p.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("pointer shape expected, got {other:?}"),
        }
        assert!(matches!(
            pointer_update(&rdsktp_core::CursorUpdate::Hidden),
            ironrdp_server::DisplayUpdate::HidePointer
        ));
    }

    struct Capture {
        refuse_above: usize,
    }

    impl CaptureSource for Capture {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<rdsktp_core::Frame>, CaptureError> {
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
        fn encode(&mut self, _: &rdsktp_core::Frame) -> Result<EncodedFrame, EncodeError> {
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
    fn more_outputs_without_an_encoder_factory_are_ignored() {
        let (shared, handle, _) = start(4, false);
        *shared.resize.lock().unwrap() = Some(Layout::side_by_side(2, 640, 480));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*shared.desktop.lock().unwrap(), Some((800, 600)));
        stop(&shared, handle);
    }
}
