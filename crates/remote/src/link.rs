//! Liaison avec l'agent : un thread lecteur dispatche les messages vers des boîtes aux lettres
//! « dernier gagne » ; les écritures sont sérialisées par un verrou.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rdsktp_agent_proto::{read_msg, write_msg, FromAgent, ToAgent};
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;

/// Images en attente par sortie. Les images H.264 sont inter-dépendantes : on ne peut pas n'en garder
/// que la dernière, on les livre toutes dans l'ordre tant que la file reste courte.
#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Frame>,
    /// Trou dans le flux inter-images : tout est ignoré jusqu'à la prochaine image clé.
    wait_key: bool,
}

/// Au-delà, le consommateur est trop lent (contre-pression EGFX) : on abandonne la file et on repart d'une clé
/// plutôt que d'accumuler de la latence.
const MAX_QUEUE: usize = 4;

#[derive(Default)]
struct AudioQueue {
    bytes: VecDeque<u8>,
    open: bool,
}

pub struct AgentLink {
    writer: Mutex<UnixStream>,
    video: Vec<Mutex<Mailbox>>,
    clipboard: Mutex<Option<String>>,
    audio: Mutex<AudioQueue>,
    audio_cv: Condvar,
    dead: AtomicBool,
    waker: Mutex<Option<Arc<rdsktp_core::wake::Wake>>>,
}

impl AgentLink {
    /// Attend le `Hello` de l'agent et démarre le thread lecteur.
    pub fn connect(stream: UnixStream, expected_outputs: u32) -> Result<Arc<Self>, String> {
        stream.set_read_timeout(Some(Duration::from_secs(60))).map_err(|e| e.to_string())?;
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        match read_msg::<_, FromAgent>(&mut reader).map_err(|e| e.to_string())? {
            Some(FromAgent::Hello { outputs, .. }) if outputs == expected_outputs => {}
            Some(FromAgent::Hello { outputs, .. }) => {
                return Err(format!("l'agent expose {outputs} sortie(s), {expected_outputs} attendue(s)"));
            }
            Some(FromAgent::Fatal(m)) => return Err(format!("agent : {m}")),
            _ => return Err("l'agent n'a pas envoyé Hello".into()),
        }
        reader.set_read_timeout(None).map_err(|e| e.to_string())?;
        let link = Arc::new(Self {
            writer: Mutex::new(stream),
            video: (0..expected_outputs).map(|_| Mutex::new(Mailbox::default())).collect(),
            clipboard: Mutex::new(None),
            audio: Mutex::new(AudioQueue::default()),
            audio_cv: Condvar::new(),
            dead: AtomicBool::new(false),
            waker: Mutex::new(None),
        });
        let l = link.clone();
        std::thread::Builder::new()
            .name("agent-reader".into())
            .spawn(move || {
                loop {
                    match read_msg::<_, FromAgent>(&mut reader) {
                        Ok(Some(m)) => l.dispatch(m),
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("agent : {e}");
                            break;
                        }
                    }
                }
                l.dead.store(true, Ordering::Relaxed);
                l.audio_cv.notify_all();
                l.wake();
            })
            .map_err(|e| e.to_string())?;
        Ok(link)
    }

    fn dispatch(&self, msg: FromAgent) {
        match msg {
            FromAgent::Hello { .. } => {}
            FromAgent::Video { output, keyframe, width, height, data } => {
                let Some(slot) = self.video.get(output as usize) else { return };
                let mut mb = slot.lock().expect("mailbox mutex");
                let frame = Frame {
                    output: OutputId(output),
                    width,
                    height,
                    stride: 0,
                    format: PixelFormat::Bgra8,
                    data: FrameData::Encoded { keyframe, data },
                };
                if keyframe {
                    // Une clé rend les images en attente inutiles.
                    mb.queue.clear();
                    mb.queue.push_back(frame);
                    mb.wait_key = false;
                } else if mb.wait_key {
                    // Image inter inutilisable : la référence manque.
                } else if mb.queue.len() >= MAX_QUEUE {
                    mb.queue.clear();
                    mb.wait_key = true;
                    drop(mb);
                    self.send(&ToAgent::RequestKeyframe { output });
                    return;
                } else {
                    mb.queue.push_back(frame);
                }
                drop(mb);
                self.wake();
            }
            FromAgent::ClipboardText(t) => *self.clipboard.lock().expect("clipboard mutex") = Some(t),
            FromAgent::Audio(bytes) => {
                let mut q = self.audio.lock().expect("audio mutex");
                if q.open {
                    q.bytes.extend(bytes);
                    // Borne (~1 s à 48 kHz stéréo 16 bits) : on jette le plus ancien plutôt que d'accumuler de la latence.
                    while q.bytes.len() > 192_000 {
                        q.bytes.drain(..19_200);
                    }
                    self.audio_cv.notify_all();
                }
            }
            FromAgent::Fatal(m) => tracing::error!("agent : {m}"),
        }
    }

    pub fn set_waker(&self, wake: Arc<rdsktp_core::wake::Wake>) {
        *self.waker.lock().expect("waker mutex") = Some(wake);
    }

    fn wake(&self) {
        if let Some(w) = self.waker.lock().expect("waker mutex").as_ref() {
            w.notify();
        }
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn send(&self, msg: &ToAgent) -> bool {
        let mut w = self.writer.lock().expect("writer mutex");
        let ok = write_msg(&mut *w, msg).is_ok();
        if !ok {
            self.dead.store(true, Ordering::Relaxed);
        }
        ok
    }

    pub fn take_frame(&self, output: OutputId) -> Option<Frame> {
        self.video.get(output.0 as usize)?.lock().expect("mailbox mutex").queue.pop_front()
    }

    /// Le prochain `take_frame` ne renverra qu'une image clé, demandée à l'agent.
    pub fn request_keyframe(&self, output: OutputId) {
        if let Some(slot) = self.video.get(output.0 as usize) {
            let mut mb = slot.lock().expect("mailbox mutex");
            mb.queue.clear();
            mb.wait_key = true;
        }
        self.send(&ToAgent::RequestKeyframe { output: output.0 });
    }

    pub fn take_clipboard(&self) -> Option<String> {
        self.clipboard.lock().expect("clipboard mutex").take()
    }

    pub fn open_audio(&self, sample_rate: u32, channels: u16) {
        let mut q = self.audio.lock().expect("audio mutex");
        q.bytes.clear();
        q.open = true;
        drop(q);
        self.send(&ToAgent::AudioOpen { sample_rate, channels });
    }

    pub fn close_audio(&self) {
        self.audio.lock().expect("audio mutex").open = false;
        self.audio_cv.notify_all();
        self.send(&ToAgent::AudioClose);
    }

    /// Bloque jusqu'à `buf.len()` octets ; `false` si le flux est fermé ou la liaison coupée.
    pub fn read_audio(&self, buf: &mut [u8]) -> bool {
        let mut q = self.audio.lock().expect("audio mutex");
        while q.bytes.len() < buf.len() {
            if !q.open || self.is_dead() {
                return false;
            }
            q = self.audio_cv.wait_timeout(q, Duration::from_millis(200)).expect("audio mutex").0;
        }
        for b in buf.iter_mut() {
            *b = q.bytes.pop_front().expect("longueur vérifiée");
        }
        true
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        // Ferme la connexion : l'agent le voit comme une déconnexion et arrête ses threads.
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn pair() -> (Arc<AgentLink>, UnixStream) {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(&mut agent, &FromAgent::Hello { outputs: 1, width: 64, height: 64 }).unwrap();
        (AgentLink::connect(server, 1).unwrap(), agent)
    }

    fn video(keyframe: bool, tag: u8) -> FromAgent {
        FromAgent::Video { output: 0, keyframe, width: 64, height: 64, data: vec![tag] }
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let t = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "délai dépassé");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn tag(f: Frame) -> (bool, u8) {
        match f.data {
            FrameData::Encoded { keyframe, data } => (keyframe, data[0]),
            _ => panic!("image non encodée"),
        }
    }

    #[test]
    fn hello_with_wrong_output_count_is_rejected() {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(&mut agent, &FromAgent::Hello { outputs: 2, width: 1, height: 1 }).unwrap();
        assert!(AgentLink::connect(server, 1).is_err());
    }

    #[test]
    fn short_backlog_is_delivered_in_order() {
        let (link, mut agent) = pair();
        for (key, t) in [(true, 1), (false, 2), (false, 3)] {
            write_msg(&mut agent, &video(key, t)).unwrap();
        }
        let mut got = Vec::new();
        wait_for(|| {
            got.extend(std::iter::from_fn(|| link.take_frame(OutputId(0)).map(tag)));
            (got.len() == 3).then_some(())
        });
        assert_eq!(got, vec![(true, 1), (false, 2), (false, 3)]);
    }

    #[test]
    fn overflowing_backlog_forces_a_keyframe_resync() {
        let (link, mut agent) = pair();
        write_msg(&mut agent, &video(true, 1)).unwrap();
        for t in 2..=(MAX_QUEUE as u8 + 2) {
            write_msg(&mut agent, &video(false, t)).unwrap(); // jamais consommées
        }
        agent.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req: ToAgent = read_msg(&mut agent).unwrap().unwrap();
        assert_eq!(req, ToAgent::RequestKeyframe { output: 0 });
        write_msg(&mut agent, &video(false, 90)).unwrap(); // toujours en attente de clé : ignorée
        write_msg(&mut agent, &video(true, 91)).unwrap();
        let got = wait_for(|| link.take_frame(OutputId(0)).map(tag));
        assert_eq!(got, (true, 91), "seule la clé de resynchronisation doit sortir");
        assert!(link.take_frame(OutputId(0)).is_none());
    }

    #[test]
    fn consumed_frames_flow_without_resync() {
        let (link, mut agent) = pair();
        write_msg(&mut agent, &video(true, 1)).unwrap();
        assert_eq!(wait_for(|| link.take_frame(OutputId(0)).map(tag)), (true, 1));
        write_msg(&mut agent, &video(false, 2)).unwrap();
        assert_eq!(wait_for(|| link.take_frame(OutputId(0)).map(tag)), (false, 2));
    }

    #[test]
    fn clipboard_audio_and_death() {
        let (link, mut agent) = pair();
        write_msg(&mut agent, &FromAgent::ClipboardText("héllo".into())).unwrap();
        assert_eq!(wait_for(|| link.take_clipboard()), "héllo");
        link.open_audio(48_000, 2);
        write_msg(&mut agent, &FromAgent::Audio(vec![7; 8])).unwrap();
        let mut buf = [0u8; 8];
        assert!(link.read_audio(&mut buf));
        assert_eq!(buf, [7; 8]);
        drop(agent);
        wait_for(|| link.is_dead().then_some(()));
        assert!(!link.read_audio(&mut buf));
    }
}
