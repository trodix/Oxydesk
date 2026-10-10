//! Pompe de frames : capture -> encodage -> EGFX, à cadence fixe. Une frame à la fois,
//! abandon (pas de file) si le client n'acquitte pas assez vite (backpressure EGFX).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::{CaptureSource, FrameEncoder};
use tokio::sync::mpsc::UnboundedSender;

/// État partagé entre les fabriques IronRDP (créées par connexion) et les threads de travail.
#[derive(Default)]
pub struct Shared {
    pub sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    pub gfx: Mutex<Option<GfxServerHandle>>,
    pub stop: AtomicBool,
}

pub struct PumpParams {
    pub width: u16,
    pub height: u16,
    pub fps: f32,
}

pub fn spawn(
    shared: Arc<Shared>,
    mut capture: Box<dyn CaptureSource>,
    mut encoder: Box<dyn FrameEncoder>,
    p: PumpParams,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("frame-pump".into())
        .spawn(move || {
            let period = Duration::from_secs_f32(1.0 / p.fps);
            let started = Instant::now();
            let mut next = Instant::now();
            let mut current: Option<(GfxServerHandle, Option<u16>)> = None;
            let (mut sent, mut dropped) = (0u64, 0u64);
            // Dernière frame capturée, ré-encodée pour un nouveau client si le bureau est statique.
            let mut last: Option<rdsktp_core::Frame> = None;
            let mut repeat = false;
            let mut report = Instant::now();
            while !shared.stop.load(Ordering::Relaxed) {
                next += period;
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                } else {
                    next = Instant::now(); // en retard : on ne rattrape pas.
                }
                let Some(handle) = shared.gfx.lock().expect("gfx mutex").clone() else {
                    continue;
                };
                if !current
                    .as_ref()
                    .is_some_and(|(h, _)| Arc::ptr_eq(h, &handle))
                {
                    current = Some((handle.clone(), None)); // nouvelle connexion
                }
                let (handle, surface) = current.as_mut().expect("set above");
                {
                    let mut g = handle.lock().expect("gfx handle mutex");
                    if !g.is_ready() || !g.supports_avc420() {
                        continue;
                    }
                    if surface.is_none() {
                        let Some(id) = g.create_surface(p.width, p.height) else {
                            continue;
                        };
                        g.map_surface_to_output(id, 0, 0);
                        *surface = Some(id);
                        encoder.request_keyframe();
                        repeat = true;
                    }
                    if g.should_backpressure() {
                        dropped += 1;
                        continue;
                    }
                }
                match capture.recv_latest(OutputId(0)) {
                    Ok(Some(f)) => {
                        last = Some(f);
                        repeat = false;
                    }
                    Ok(None) if repeat && last.is_some() => repeat = false,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("capture: {e}");
                        break;
                    }
                }
                let Some(frame) = last.as_ref() else { continue };
                let encoded = match encoder.encode(frame) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!("encodage: {e}");
                        break;
                    }
                };
                let (messages, channel) = {
                    let mut g = handle.lock().expect("gfx handle mutex");
                    let region = Avc420Region::full_frame(p.width, p.height, 22);
                    let ts = started.elapsed().as_millis() as u32;
                    if g.send_avc420_frame(surface.expect("surface"), &encoded.data, &[region], ts)
                        .is_none()
                    {
                        dropped += 1;
                        continue;
                    }
                    (g.drain_output(), g.channel_id())
                };
                sent += 1;
                if let Some(channel) = channel {
                    match encode_dvc_messages(channel, messages, ironrdp_svc::ChannelFlags::empty())
                    {
                        Ok(svc) => {
                            if let Some(tx) = shared.sender.lock().expect("sender mutex").as_ref() {
                                let _ =
                                    tx.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                                        messages: svc,
                                    }));
                            }
                        }
                        Err(e) => tracing::error!("encodage DVC: {e}"),
                    }
                }
                if report.elapsed() >= Duration::from_secs(10) {
                    tracing::info!(sent, dropped, "frame-pump");
                    report = Instant::now();
                }
            }
        })
        .expect("spawn frame-pump")
}
