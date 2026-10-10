//! Pompe de frames : capture -> encodage -> EGFX, à cadence fixe. Une frame à la fois,
//! abandon (pas de file) si le client n'acquitte pas assez vite (backpressure EGFX).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use rdsktp_core::layout::{Layout, OutputId};
use rdsktp_core::ports::{CaptureSource, FrameEncoder};
use rdsktp_core::FrameData;
use tokio::sync::mpsc::UnboundedSender;

/// État partagé entre les fabriques IronRDP (créées par connexion) et les threads de travail.
#[derive(Default)]
pub struct Shared {
    pub sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    pub gfx: Mutex<Option<GfxServerHandle>>,
    pub stop: AtomicBool,
    /// Notifié par la capture à chaque nouvelle frame (mode `event`).
    pub wake: Arc<rdsktp_core::wake::Wake>,
    /// Capture et encodeurs d'une nouvelle session, déposés par le composition root (mode fixe) ou par
    /// le validateur d'identifiants (mode login) ; la pompe les adopte au tick suivant.
    pub ports: Mutex<Option<PumpPorts>>,
}

pub struct PumpPorts {
    pub capture: Box<dyn CaptureSource>,
    /// Un encodeur par sortie, dans l'ordre de la disposition.
    pub encoders: Vec<Box<dyn FrameEncoder>>,
}

pub struct PumpParams {
    pub layout: Layout,
    pub fps: f32,
    /// `true` : la pompe est réveillée par la capture au lieu d'une cadence fixe (`fps` devient un délai maximal d'attente).
    pub event: bool,
}

/// Une sortie = un encodeur + sa dernière frame (ré-encodée pour un nouveau client si le bureau est statique).
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
            flags: if o.primary { MonitorFlags::PRIMARY } else { MonitorFlags::empty() },
        })
        .collect()
}

pub fn spawn(
    shared: Arc<Shared>,
    p: PumpParams,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("frame-pump".into())
        .spawn(move || {
            let period = Duration::from_secs_f32(1.0 / p.fps);
            let started = Instant::now();
            let mut next = Instant::now();
            let (desktop_w, desktop_h) = p.layout.desktop_size();
            let mut capture: Option<Box<dyn CaptureSource>> = None;
            let mut outputs: Vec<OutputState> = Vec::new();
            // Connexion courante et une surface EGFX par sortie (vide tant que non créées).
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
                        next = Instant::now(); // en retard : on ne rattrape pas.
                    }
                }
                let sent_before = sent;
                if let Some(new) = shared.ports.lock().expect("ports mutex").take() {
                    if new.encoders.len() != p.layout.outputs.len() {
                        tracing::error!("session ignorée : {} encodeur(s) pour {} sortie(s)", new.encoders.len(), p.layout.outputs.len());
                    } else {
                        let mut c = new.capture;
                        c.set_waker(shared.wake.clone());
                        capture = Some(c);
                        outputs = new.encoders.into_iter().map(|encoder| OutputState { encoder, last: None }).collect();
                        current = None;
                    }
                }
                let Some(capture_src) = capture.as_mut() else { continue };
                let Some(handle) = shared.gfx.lock().expect("gfx mutex").clone() else {
                    continue;
                };
                if !current.as_ref().is_some_and(|(h, _)| Arc::ptr_eq(h, &handle)) {
                    current = Some((handle.clone(), Vec::new())); // nouvelle connexion
                }
                let (handle, surfaces) = current.as_mut().expect("set above");
                {
                    let mut g = handle.lock().expect("gfx handle mutex");
                    if !g.is_ready() || !g.supports_avc420() {
                        continue;
                    }
                    if surfaces.is_empty() {
                        // Plusieurs écrans : ResetGraphics annonce la liste des moniteurs avant les surfaces.
                        if p.layout.outputs.len() > 1 {
                            g.resize_with_monitors(desktop_w as u16, desktop_h as u16, monitors(&p.layout));
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
                    let Some(frame) = out.last.as_ref() else { continue };
                    let pre_encoded = matches!(frame.data, FrameData::Encoded { .. });
                    let encoded = match out.encoder.encode(frame) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::error!("encodage: {e}");
                            lost = true;
                            break;
                        }
                    };
                    if pre_encoded {
                        // Une image inter déjà encodée ne se renvoie pas : le « repeat » repart d'une clé de l'agent.
                        out.last = None;
                    }
                    let info = &p.layout.outputs[i];
                    let (messages, channel) = {
                        let mut g = handle.lock().expect("gfx handle mutex");
                        let region = Avc420Region::full_frame(info.width as u16, info.height as u16, 22);
                        let ts = started.elapsed().as_millis() as u32;
                        if g.send_avc420_frame(surfaces[i], &encoded.data, &[region], ts).is_none() {
                            dropped += 1;
                            continue;
                        }
                        (g.drain_output(), g.channel_id())
                    };
                    sent += 1;
                    if let Some(channel) = channel {
                        match encode_dvc_messages(channel, messages, ironrdp_svc::ChannelFlags::empty()) {
                            Ok(svc) => {
                                if let Some(tx) = shared.sender.lock().expect("sender mutex").as_ref() {
                                    let _ = tx.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages: svc }));
                                }
                            }
                            Err(e) => tracing::error!("encodage DVC: {e}"),
                        }
                    }
                }
                if lost {
                    // Session perdue (agent mort, flux fermé) : on attend la suivante au lieu d'arrêter la pompe.
                    capture = None;
                    outputs.clear();
                    current = None;
                    continue;
                }
                drain = sent > sent_before; // des images peuvent encore attendre : pas de sommeil
                repeat = false;
                if report.elapsed() >= Duration::from_secs(10) {
                    tracing::info!(sent, dropped, "frame-pump");
                    report = Instant::now();
                }
            }
        })
        .expect("spawn frame-pump")
}
