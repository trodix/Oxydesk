//! Link with the agent: a reader thread dispatches messages to "latest wins" mailboxes;
//! writes are serialized by a lock.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rdsktp_agent_proto::{read_msg, write_msg, FromAgent, ToAgent};
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;

/// Pending frames per output. H.264 frames are inter-dependent: one cannot keep only
/// the last one, they are all delivered in order as long as the queue stays short.
#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Frame>,
    /// Gap in the inter-frame stream: everything is ignored until the next keyframe.
    wait_key: bool,
}

/// Beyond this, the consumer is too slow (EGFX back-pressure): drop the queue and restart from a keyframe
/// rather than accumulating latency.
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
    /// Waits for the agent's `Hello` and starts the reader thread.
    pub fn connect(stream: UnixStream, expected_outputs: u32) -> Result<Arc<Self>, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(|e| e.to_string())?;
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        match read_msg::<_, FromAgent>(&mut reader).map_err(|e| e.to_string())? {
            Some(FromAgent::Hello { outputs, .. }) if outputs == expected_outputs => {}
            Some(FromAgent::Hello { outputs, .. }) => {
                return Err(format!(
                    "the agent exposes {outputs} output(s), {expected_outputs} expected"
                ));
            }
            Some(FromAgent::Fatal(m)) => return Err(format!("agent: {m}")),
            _ => return Err("the agent did not send Hello".into()),
        }
        reader.set_read_timeout(None).map_err(|e| e.to_string())?;
        let link = Arc::new(Self {
            writer: Mutex::new(stream),
            video: (0..expected_outputs)
                .map(|_| Mutex::new(Mailbox::default()))
                .collect(),
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
                            tracing::warn!("agent: {e}");
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
            FromAgent::Video {
                output,
                keyframe,
                width,
                height,
                data,
            } => {
                let Some(slot) = self.video.get(output as usize) else {
                    return;
                };
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
                    // A keyframe makes the pending frames useless.
                    mb.queue.clear();
                    mb.queue.push_back(frame);
                    mb.wait_key = false;
                } else if mb.wait_key {
                    // Unusable inter frame: the reference is missing.
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
            FromAgent::ClipboardText(t) => {
                *self.clipboard.lock().expect("clipboard mutex") = Some(t)
            }
            FromAgent::Audio(bytes) => {
                let mut q = self.audio.lock().expect("audio mutex");
                if q.open {
                    q.bytes.extend(bytes);
                    // Bound (~1 s at 48 kHz 16-bit stereo): drop the oldest rather than accumulate latency.
                    while q.bytes.len() > 192_000 {
                        q.bytes.drain(..19_200);
                    }
                    self.audio_cv.notify_all();
                }
            }
            FromAgent::Fatal(m) => tracing::error!("agent: {m}"),
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
        self.video
            .get(output.0 as usize)?
            .lock()
            .expect("mailbox mutex")
            .queue
            .pop_front()
    }

    /// The next `take_frame` will only return a keyframe, requested from the agent.
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
        self.send(&ToAgent::AudioOpen {
            sample_rate,
            channels,
        });
    }

    pub fn close_audio(&self) {
        self.audio.lock().expect("audio mutex").open = false;
        self.audio_cv.notify_all();
        self.send(&ToAgent::AudioClose);
    }

    /// Blocks until `buf.len()` bytes; `false` if the stream is closed or the link is cut.
    pub fn read_audio(&self, buf: &mut [u8]) -> bool {
        let mut q = self.audio.lock().expect("audio mutex");
        while q.bytes.len() < buf.len() {
            if !q.open || self.is_dead() {
                return false;
            }
            q = self
                .audio_cv
                .wait_timeout(q, Duration::from_millis(200))
                .expect("audio mutex")
                .0;
        }
        for b in buf.iter_mut() {
            *b = q.bytes.pop_front().expect("length checked");
        }
        true
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        // Closes the connection: the agent sees it as a disconnection and stops its threads.
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
        write_msg(
            &mut agent,
            &FromAgent::Hello {
                outputs: 1,
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        (AgentLink::connect(server, 1).unwrap(), agent)
    }

    fn video(keyframe: bool, tag: u8) -> FromAgent {
        FromAgent::Video {
            output: 0,
            keyframe,
            width: 64,
            height: 64,
            data: vec![tag],
        }
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let t = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn tag(f: Frame) -> (bool, u8) {
        match f.data {
            FrameData::Encoded { keyframe, data } => (keyframe, data[0]),
            _ => panic!("frame not encoded"),
        }
    }

    #[test]
    fn hello_with_wrong_output_count_is_rejected() {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(
            &mut agent,
            &FromAgent::Hello {
                outputs: 2,
                width: 1,
                height: 1,
            },
        )
        .unwrap();
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
            write_msg(&mut agent, &video(false, t)).unwrap(); // never consumed
        }
        agent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let req: ToAgent = read_msg(&mut agent).unwrap().unwrap();
        assert_eq!(req, ToAgent::RequestKeyframe { output: 0 });
        write_msg(&mut agent, &video(false, 90)).unwrap(); // still waiting for a keyframe: ignored
        write_msg(&mut agent, &video(true, 91)).unwrap();
        let got = wait_for(|| link.take_frame(OutputId(0)).map(tag));
        assert_eq!(got, (true, 91), "only the resync keyframe must come out");
        assert!(link.take_frame(OutputId(0)).is_none());
    }

    #[test]
    fn consumed_frames_flow_without_resync() {
        let (link, mut agent) = pair();
        write_msg(&mut agent, &video(true, 1)).unwrap();
        assert_eq!(
            wait_for(|| link.take_frame(OutputId(0)).map(tag)),
            (true, 1)
        );
        write_msg(&mut agent, &video(false, 2)).unwrap();
        assert_eq!(
            wait_for(|| link.take_frame(OutputId(0)).map(tag)),
            (false, 2)
        );
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
