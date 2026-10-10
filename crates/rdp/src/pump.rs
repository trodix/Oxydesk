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
use rdsktp_core::ports::{CaptureSource, FrameEncoder};
use rdsktp_core::FrameData;
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
}

pub struct PumpPorts {
    pub capture: Box<dyn CaptureSource>,
    /// One encoder per output, in layout order.
    pub encoders: Vec<Box<dyn FrameEncoder>>,
}

pub struct PumpParams {
    pub layout: Layout,
    pub fps: f32,
    /// `true`: the pump is woken by the capture instead of a fixed rate (`fps` becomes a maximum wait delay).
    pub event: bool,
}

/// One output = one encoder + its last frame (re-encoded for a new client if the desktop is static).
struct OutputState {
    encoder: Box<dyn FrameEncoder>,
    last: Option<rdsktp_core::Frame>,
}

fn monitors(layout: &Layout) -> Vec<Monitor> {
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
            let (desktop_w, desktop_h) = p.layout.desktop_size();
            let mut capture: Option<Box<dyn CaptureSource>> = None;
            let mut outputs: Vec<OutputState> = Vec::new();
            // Current connection and one EGFX surface per output (empty until created).
            let mut current: Option<(GfxServerHandle, Vec<u16>)> = None;
            let (mut sent, mut dropped) = (0u64, 0u64);
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
                        outputs = new
                            .encoders
                            .into_iter()
                            .map(|encoder| OutputState {
                                encoder,
                                last: None,
                            })
                            .collect();
                        current = None;
                    }
                }
                let Some(capture_src) = capture.as_mut() else {
                    continue;
                };
                let Some(handle) = shared.gfx.lock().expect("gfx mutex").clone() else {
                    continue;
                };
                if !current
                    .as_ref()
                    .is_some_and(|(h, _)| Arc::ptr_eq(h, &handle))
                {
                    current = Some((handle.clone(), Vec::new())); // new connection
                }
                let (handle, surfaces) = current.as_mut().expect("set above");
                {
                    let mut g = handle.lock().expect("gfx handle mutex");
                    if !g.is_ready() || !g.supports_avc420() {
                        continue;
                    }
                    if surfaces.is_empty() {
                        // Multiple screens: ResetGraphics announces the monitor list before the surfaces.
                        if p.layout.outputs.len() > 1 {
                            g.resize_with_monitors(
                                desktop_w as u16,
                                desktop_h as u16,
                                monitors(&p.layout),
                            );
                        }
                        let mut created = Vec::new();
                        for o in &p.layout.outputs {
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
                    if g.should_backpressure() {
                        dropped += 1;
                        continue;
                    }
                }
                let mut lost = false;
                for (i, out) in outputs.iter_mut().enumerate() {
                    match capture_src.recv_latest(OutputId(i as u32)) {
                        Ok(Some(f)) => out.last = Some(f),
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
                    let encoded = match out.encoder.encode(frame) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::error!("encoding: {e}");
                            lost = true;
                            break;
                        }
                    };
                    if pre_encoded {
                        // An already encoded inter frame is not re-sent: the "repeat" restarts from a keyframe from the agent.
                        out.last = None;
                    }
                    let info = &p.layout.outputs[i];
                    let (messages, channel) = {
                        let mut g = handle.lock().expect("gfx handle mutex");
                        let region =
                            Avc420Region::full_frame(info.width as u16, info.height as u16, 22);
                        let ts = started.elapsed().as_millis() as u32;
                        if g.send_avc420_frame(surfaces[i], &encoded.data, &[region], ts)
                            .is_none()
                        {
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
                    tracing::info!(sent, dropped, "frame-pump");
                    report = Instant::now();
                }
            }
        })
        .expect("spawn frame-pump")
}
