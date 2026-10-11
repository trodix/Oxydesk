//! Link with the agent: a reader thread dispatches messages to "latest wins" mailboxes;
//! writes are serialized by a lock.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::Duration;

use rdsktp_agent_proto::{
    read_msg, write_msg, FileInfo, FileRange, FromAgent, Rect, ToAgent, MAX_OUTPUTS,
};
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::{FileEntry, FileExchange, FileList, RangeRequest, RangeSender};

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
    /// One mailbox per output; the list follows the layout (`LayoutApplied`).
    video: RwLock<Vec<Mutex<Mailbox>>>,
    clipboard: Mutex<Option<String>>,
    clipboard_image: Mutex<Option<Vec<u8>>>,
    /// File selection of the desktop not yet taken, and the bookkeeping of the ranges exchanged with the agent.
    clipboard_files: Mutex<Option<FileList>>,
    files: Arc<FileExchange>,
    /// Latest pointer change not yet taken (only the last one matters).
    cursor: Mutex<Option<rdsktp_core::CursorUpdate>>,
    audio: Mutex<AudioQueue>,
    audio_cv: Condvar,
    /// Answer of the agent to the last `Resize` (None while pending).
    resize_reply: Mutex<Option<bool>>,
    resize_cv: Condvar,
    /// Answer of the agent to the last `SetLayout` (None while pending).
    layout_reply: Mutex<Option<bool>>,
    layout_cv: Condvar,
    dead: AtomicBool,
    waker: Mutex<Option<Arc<rdsktp_core::wake::Wake>>>,
}

fn to_info(e: &FileEntry) -> FileInfo {
    FileInfo {
        path: e.path.clone(),
        size: e.size,
        is_dir: e.is_dir,
        modified: e.modified,
        read_only: e.read_only,
    }
}

fn from_info(i: FileInfo) -> FileEntry {
    FileEntry {
        path: i.path,
        size: i.size,
        is_dir: i.is_dir,
        modified: i.modified,
        read_only: i.read_only,
    }
}

impl AgentLink {
    /// Waits for the agent's `Hello` and starts the reader thread.
    pub fn connect(stream: UnixStream, expected_outputs: u32) -> Result<Arc<Self>, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(|e| e.to_string())?;
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        // The desktop session outlives connections: the agent may still have another number of outputs than
        // configured (a previous client changed it); it is then brought back to the configured layout below.
        let (hello_outputs, width, height) =
            match read_msg::<_, FromAgent>(&mut reader).map_err(|e| e.to_string())? {
                Some(FromAgent::Hello {
                    outputs,
                    width,
                    height,
                }) if (1..=MAX_OUTPUTS as u32).contains(&outputs) => (outputs, width, height),
                Some(FromAgent::Hello { outputs, .. }) => {
                    return Err(format!("the agent exposes {outputs} output(s)"));
                }
                Some(FromAgent::Fatal(m)) => return Err(format!("agent: {m}")),
                _ => return Err("the agent did not send Hello".into()),
            };
        reader.set_read_timeout(None).map_err(|e| e.to_string())?;
        let link = Arc::new(Self {
            writer: Mutex::new(stream),
            video: RwLock::new(
                (0..hello_outputs)
                    .map(|_| Mutex::new(Mailbox::default()))
                    .collect(),
            ),
            clipboard: Mutex::new(None),
            clipboard_image: Mutex::new(None),
            clipboard_files: Mutex::new(None),
            files: Arc::new(FileExchange::default()),
            cursor: Mutex::new(None),
            audio: Mutex::new(AudioQueue::default()),
            audio_cv: Condvar::new(),
            resize_reply: Mutex::new(None),
            resize_cv: Condvar::new(),
            layout_reply: Mutex::new(None),
            layout_cv: Condvar::new(),
            dead: AtomicBool::new(false),
            waker: Mutex::new(None),
        });
        let l = link.clone();
        std::thread::Builder::new()
            .name("agent-reader".into())
            .spawn(move || {
                loop {
                    match read_msg::<_, FromAgent>(&mut reader) {
                        Ok(Some(m)) => l.dispatch(&l, m),
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("agent: {e}");
                            break;
                        }
                    }
                }
                l.dead.store(true, Ordering::Relaxed);
                l.files.fail_all();
                l.audio_cv.notify_all();
                l.resize_cv.notify_all();
                l.layout_cv.notify_all();
                l.wake();
            })
            .map_err(|e| e.to_string())?;
        if hello_outputs != expected_outputs {
            let outputs = (0..expected_outputs)
                .map(|i| Rect {
                    x: (i * width) as i32,
                    y: 0,
                    width,
                    height,
                })
                .collect();
            if !link.set_layout(outputs, Duration::from_secs(20)) {
                return Err(format!(
                    "the agent exposes {hello_outputs} output(s), {expected_outputs} expected, and could not change"
                ));
            }
        }
        Ok(link)
    }

    fn dispatch(&self, this: &Arc<Self>, msg: FromAgent) {
        match msg {
            FromAgent::Hello { .. } => {}
            FromAgent::Video {
                output,
                keyframe,
                width,
                height,
                data,
            } => {
                let video = self.video.read().expect("video lock");
                let Some(slot) = video.get(output as usize) else {
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
                    drop(video);
                    self.send(&ToAgent::RequestKeyframe { output });
                    return;
                } else {
                    mb.queue.push_back(frame);
                }
                drop(mb);
                drop(video);
                self.wake();
            }
            FromAgent::VideoRaw {
                output,
                width,
                height,
                data,
            } => {
                let video = self.video.read().expect("video lock");
                let Some(slot) = video.get(output as usize) else {
                    return;
                };
                let mut mb = slot.lock().expect("mailbox mutex");
                // Every raw frame is self-contained: only the latest one is worth keeping.
                if !mb.wait_key {
                    mb.queue.clear();
                    mb.queue.push_back(Frame {
                        output: OutputId(output),
                        width,
                        height,
                        stride: width * 4,
                        format: PixelFormat::Bgra8,
                        data: FrameData::Shm(data),
                    });
                }
                drop(mb);
                drop(video);
                self.wake();
            }
            FromAgent::Cursor(c) => {
                let update = match c {
                    None => Some(rdsktp_core::CursorUpdate::Hidden),
                    Some(c) => {
                        rdsktp_core::CursorShape::new(c.width, c.height, c.hot_x, c.hot_y, c.rgba)
                            .map(rdsktp_core::CursorUpdate::Shape)
                    }
                };
                if let Some(update) = update {
                    *self.cursor.lock().expect("cursor mutex") = Some(update);
                    self.wake();
                }
            }
            FromAgent::ClipboardText(t) => {
                *self.clipboard.lock().expect("clipboard mutex") = Some(t)
            }
            FromAgent::ClipboardImage(d) => {
                *self.clipboard_image.lock().expect("clipboard mutex") = Some(d)
            }
            FromAgent::Files { list_id, files } => {
                let weak: Weak<AgentLink> = Arc::downgrade(this);
                let send: RangeSender = Arc::new(move |r: RangeRequest| {
                    weak.upgrade().is_some_and(|l| {
                        l.send(&ToAgent::FileRead(FileRange {
                            req_id: r.req_id,
                            list_id: r.list_id,
                            index: r.index,
                            offset: r.offset,
                            len: r.len,
                        }))
                    })
                });
                let entries = files.into_iter().map(from_info).collect();
                let list = self.files.remote_list(list_id, entries, send);
                *self.clipboard_files.lock().expect("clipboard mutex") = Some(list);
            }
            FromAgent::FileRead(r) => {
                // Reading a disk must not stall the reader thread.
                let this = this.clone();
                std::thread::spawn(move || {
                    let data = this.files.read_served(r.list_id, r.index, r.offset, r.len);
                    this.send(&ToAgent::FileData {
                        req_id: r.req_id,
                        ok: data.is_some(),
                        data: data.unwrap_or_default(),
                    });
                });
            }
            FromAgent::FileData { req_id, ok, data } => {
                self.files.complete(req_id, ok.then_some(data));
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
            FromAgent::LayoutApplied { ok, outputs } => {
                if ok {
                    // Here, in the reader thread: the frames that follow belong to the new layout, so the
                    // mailboxes must be ready before the next message is dispatched.
                    let mut video = self.video.write().expect("video lock");
                    video.resize_with(outputs as usize, Default::default);
                    for slot in video.iter() {
                        let mut mb = slot.lock().expect("mailbox mutex");
                        mb.queue.clear();
                        mb.wait_key = false; // the agent starts every output with a keyframe
                    }
                }
                *self.layout_reply.lock().expect("layout mutex") = Some(ok);
                self.layout_cv.notify_all();
            }
            FromAgent::Resized { ok } => {
                *self.resize_reply.lock().expect("resize mutex") = Some(ok);
                self.resize_cv.notify_all();
            }
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
            .read()
            .expect("video lock")
            .get(output.0 as usize)?
            .lock()
            .expect("mailbox mutex")
            .queue
            .pop_front()
    }

    /// Asks the agent for uncompressed pixels (clients without H.264) or, with `false`, for its H.264 stream again.
    pub fn set_raw_pixels(&self, raw: bool) {
        self.send(&ToAgent::SetPixels { raw });
        // Frames of the other kind still in the mailboxes are useless: wait for the agent's next ones.
        for slot in self.video.read().expect("video lock").iter() {
            let mut mb = slot.lock().expect("mailbox mutex");
            mb.queue.clear();
            mb.wait_key = !raw;
        }
    }

    /// The next `take_frame` will only return a keyframe, requested from the agent.
    pub fn request_keyframe(&self, output: OutputId) {
        if let Some(slot) = self
            .video
            .read()
            .expect("video lock")
            .get(output.0 as usize)
        {
            let mut mb = slot.lock().expect("mailbox mutex");
            mb.queue.clear();
            mb.wait_key = true;
        }
        self.send(&ToAgent::RequestKeyframe { output: output.0 });
    }

    /// Asks the agent to resize its desktop and waits for its answer. The pending frames (old size) are dropped.
    pub fn resize(&self, width: u32, height: u32, timeout: Duration) -> bool {
        *self.resize_reply.lock().expect("resize mutex") = None;
        if !self.send(&ToAgent::Resize { width, height }) {
            return false;
        }
        let guard = self.resize_reply.lock().expect("resize mutex");
        let (guard, _) = self
            .resize_cv
            .wait_timeout_while(guard, timeout, |r| r.is_none() && !self.is_dead())
            .expect("resize mutex");
        let ok = guard.unwrap_or(false);
        drop(guard);
        if ok {
            for slot in self.video.read().expect("video lock").iter() {
                let mut mb = slot.lock().expect("mailbox mutex");
                mb.queue.clear();
                mb.wait_key = true;
            }
        }
        ok
    }

    /// Asks the agent to apply a whole layout (number, sizes and positions of the outputs) and waits for its
    /// answer. The mailboxes follow the new number of outputs.
    pub fn set_layout(&self, outputs: Vec<Rect>, timeout: Duration) -> bool {
        *self.layout_reply.lock().expect("layout mutex") = None;
        if !self.send(&ToAgent::SetLayout { outputs }) {
            return false;
        }
        let guard = self.layout_reply.lock().expect("layout mutex");
        let (guard, _) = self
            .layout_cv
            .wait_timeout_while(guard, timeout, |r| r.is_none() && !self.is_dead())
            .expect("layout mutex");
        guard.unwrap_or(false)
    }

    pub fn take_cursor(&self) -> Option<rdsktp_core::CursorUpdate> {
        self.cursor.lock().expect("cursor mutex").take()
    }

    pub fn take_clipboard(&self) -> Option<String> {
        self.clipboard.lock().expect("clipboard mutex").take()
    }

    pub fn take_clipboard_image(&self) -> Option<Vec<u8>> {
        self.clipboard_image.lock().expect("clipboard mutex").take()
    }

    pub fn take_clipboard_files(&self) -> Option<FileList> {
        self.clipboard_files.lock().expect("clipboard mutex").take()
    }

    /// Offers files copied on the remote side to the agent (read back by range as they are pasted).
    pub fn set_clipboard_files(&self, list: FileList) -> bool {
        let files = list.entries.iter().map(to_info).collect();
        let list_id = self.files.serve(list);
        self.send(&ToAgent::FilesSet { list_id, files })
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

    /// Agent answering `Resize` requests with `ok`.
    fn answering(mut agent: UnixStream, ok: bool) {
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = read_msg::<_, ToAgent>(&mut agent) {
                if matches!(msg, ToAgent::Resize { .. }) {
                    write_msg(&mut agent, &FromAgent::Resized { ok }).unwrap();
                }
            }
        });
    }

    #[test]
    fn resize_waits_for_the_agent_and_drops_old_frames() {
        let (link, mut agent) = pair();
        write_msg(&mut agent, &video(true, 1)).unwrap();
        wait_for(|| {
            link.video.read().unwrap()[0]
                .lock()
                .unwrap()
                .queue
                .front()
                .map(|_| ())
        });
        answering(agent, true);
        assert!(link.resize(1024, 576, Duration::from_secs(5)));
        assert!(
            link.take_frame(OutputId(0)).is_none(),
            "old size frames are dropped"
        );
    }

    #[test]
    fn resize_refused_by_the_agent_or_silent_agent_fails() {
        let (link, agent) = pair();
        answering(agent, false);
        assert!(!link.resize(1024, 576, Duration::from_secs(5)));
        let (link, _silent) = pair();
        let t = Instant::now();
        assert!(!link.resize(1024, 576, Duration::from_millis(200)));
        assert!(t.elapsed() < Duration::from_secs(2));
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

    fn hello(outputs: u32) -> FromAgent {
        FromAgent::Hello {
            outputs,
            width: 8,
            height: 8,
        }
    }

    #[test]
    fn hello_with_another_output_count_is_brought_back_to_the_configured_one() {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(&mut agent, &hello(3)).unwrap();
        layout_agent(agent, true);
        let link = AgentLink::connect(server, 2).unwrap();
        wait_for(|| link.take_frame(OutputId(1)).map(tag));
        assert!(link.take_frame(OutputId(2)).is_none(), "third output gone");
    }

    #[test]
    fn hello_with_another_output_count_refused_by_the_agent_is_an_error() {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(&mut agent, &hello(2)).unwrap();
        layout_agent(agent, false);
        assert!(AgentLink::connect(server, 1).is_err());
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(&mut agent, &hello(0)).unwrap();
        assert!(AgentLink::connect(server, 1).is_err());
    }

    #[test]
    fn raw_pixels_are_requested_and_only_the_latest_frame_is_kept() {
        let (link, mut agent) = pair();
        link.set_raw_pixels(true);
        let req: ToAgent = read_msg(&mut agent).unwrap().unwrap();
        assert_eq!(req, ToAgent::SetPixels { raw: true });
        for t in [1u8, 2] {
            let raw = FromAgent::VideoRaw {
                output: 0,
                width: 1,
                height: 1,
                data: vec![t, 0, 0, 255],
            };
            write_msg(&mut agent, &raw).unwrap();
        }
        let frame = wait_for(|| {
            std::thread::sleep(Duration::from_millis(50));
            link.take_frame(OutputId(0))
        });
        let FrameData::Shm(px) = &frame.data else {
            panic!("raw frame expected")
        };
        assert_eq!((frame.stride, px[0]), (4, 2));
        assert!(link.take_frame(OutputId(0)).is_none());
        // Back to H.264: raw frames still in flight are dropped until the keyframe.
        link.set_raw_pixels(false);
        write_msg(
            &mut agent,
            &FromAgent::VideoRaw {
                output: 0,
                width: 1,
                height: 1,
                data: vec![9, 0, 0, 255],
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(link.take_frame(OutputId(0)).is_none());
    }

    #[test]
    fn pointer_changes_keep_only_the_latest() {
        use rdsktp_agent_proto::CursorImage;
        use rdsktp_core::CursorUpdate;
        let (link, mut agent) = pair();
        let image = |w: u16, len: usize| {
            FromAgent::Cursor(Some(CursorImage {
                width: w,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![7; len],
            }))
        };
        write_msg(&mut agent, &image(1, 4)).unwrap();
        write_msg(&mut agent, &FromAgent::Cursor(None)).unwrap();
        write_msg(&mut agent, &image(2, 8)).unwrap();
        let update = wait_for(|| {
            std::thread::sleep(Duration::from_millis(50));
            match link.take_cursor() {
                Some(CursorUpdate::Shape(s)) if s.width == 2 => Some(s),
                _ => None,
            }
        });
        assert_eq!(update.rgba.len(), 8);
        assert!(link.take_cursor().is_none());
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
        write_msg(&mut agent, &FromAgent::ClipboardImage(vec![9, 8, 7])).unwrap();
        assert_eq!(wait_for(|| link.take_clipboard_image()), vec![9, 8, 7]);
        link.open_audio(48_000, 2);
        write_msg(&mut agent, &FromAgent::Audio(vec![7; 8])).unwrap();
        let mut buf = [0u8; 8];
        assert!(link.read_audio(&mut buf));
        assert_eq!(buf, [7; 8]);
        drop(agent);
        wait_for(|| link.is_dead().then_some(()));
        assert!(!link.read_audio(&mut buf));
    }

    #[test]
    fn files_flow_both_ways_by_range() {
        use rdsktp_agent_proto::read_msg;
        let (link, mut agent) = pair();
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let info = |path: &str, size| FileInfo {
            path: path.into(),
            size,
            is_dir: false,
            modified: 0,
            read_only: false,
        };

        // Desktop -> server: the agent announces a list; the server reads a range through the agent.
        write_msg(
            &mut agent,
            &FromAgent::Files {
                list_id: 5,
                files: vec![info("a.bin", 3000)],
            },
        )
        .unwrap();
        let list = wait_for(|| link.take_clipboard_files());
        assert_eq!(list.entries[0].path, "a.bin");
        let source = list.source.clone();
        let reader = std::thread::spawn(move || source.read(0, 1000, 500).unwrap());
        let Some(ToAgent::FileRead(r)) = read_msg::<_, ToAgent>(&mut agent).unwrap() else {
            panic!("FileRead expected")
        };
        assert_eq!((r.list_id, r.index, r.offset, r.len), (5, 0, 1000, 500));
        write_msg(
            &mut agent,
            &FromAgent::FileData {
                req_id: r.req_id,
                ok: true,
                data: data[1000..1500].to_vec(),
            },
        )
        .unwrap();
        assert_eq!(reader.join().unwrap(), data[1000..1500]);

        // Server -> desktop: the server offers a list; the agent reads a range, served by the server.
        struct Mem(Vec<u8>);
        impl rdsktp_core::FileSource for Mem {
            fn read(
                &self,
                _: u32,
                offset: u64,
                len: u32,
            ) -> Result<Vec<u8>, rdsktp_core::error::ClipboardError> {
                let s = (offset as usize).min(self.0.len());
                Ok(self.0[s..(s + len as usize).min(self.0.len())].to_vec())
            }
        }
        let entry = from_info(info("b.bin", 3000));
        assert!(link.set_clipboard_files(FileList {
            entries: vec![entry],
            source: Arc::new(Mem(data.clone())),
        }));
        let Some(ToAgent::FilesSet { list_id, files }) =
            read_msg::<_, ToAgent>(&mut agent).unwrap()
        else {
            panic!("FilesSet expected")
        };
        assert_eq!(files[0].path, "b.bin");
        write_msg(
            &mut agent,
            &FromAgent::FileRead(FileRange {
                req_id: 11,
                list_id,
                index: 0,
                offset: 2900,
                len: 500,
            }),
        )
        .unwrap();
        let Some(ToAgent::FileData {
            req_id,
            ok,
            data: got,
        }) = read_msg::<_, ToAgent>(&mut agent).unwrap()
        else {
            panic!("FileData expected")
        };
        assert_eq!((req_id, ok), (11, true));
        assert_eq!(got, data[2900..]);
        // Unknown list: refused.
        write_msg(
            &mut agent,
            &FromAgent::FileRead(FileRange {
                req_id: 12,
                list_id: list_id + 9,
                index: 0,
                offset: 0,
                len: 1,
            }),
        )
        .unwrap();
        let Some(ToAgent::FileData { ok, .. }) = read_msg::<_, ToAgent>(&mut agent).unwrap() else {
            panic!("FileData expected")
        };
        assert!(!ok);
    }

    /// Agent answering `SetLayout` with `LayoutApplied`, then one keyframe per output.
    fn layout_agent(mut agent: UnixStream, ok: bool) {
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = read_msg::<_, ToAgent>(&mut agent) {
                if let ToAgent::SetLayout { outputs } = msg {
                    let n = outputs.len() as u32;
                    write_msg(&mut agent, &FromAgent::LayoutApplied { ok, outputs: n }).unwrap();
                    for output in 0..n {
                        let f = FromAgent::Video {
                            output,
                            keyframe: true,
                            width: 8,
                            height: 8,
                            data: vec![40 + output as u8],
                        };
                        write_msg(&mut agent, &f).unwrap();
                    }
                }
            }
        });
    }

    fn rect(x: i32) -> Rect {
        Rect {
            x,
            y: 0,
            width: 8,
            height: 8,
        }
    }

    #[test]
    fn set_layout_adds_and_removes_mailboxes() {
        let (link, agent) = pair();
        layout_agent(agent, true);
        assert!(link.set_layout(vec![rect(0), rect(8), rect(16)], Duration::from_secs(5)));
        // The keyframes sent right after the answer land in the new mailboxes.
        for output in 0..3u32 {
            let got = wait_for(|| link.take_frame(OutputId(output)).map(tag));
            assert_eq!(got, (true, 40 + output as u8));
        }
        assert!(link.set_layout(vec![rect(0)], Duration::from_secs(5)));
        assert_eq!(
            wait_for(|| link.take_frame(OutputId(0)).map(tag)),
            (true, 40)
        );
        assert!(link.take_frame(OutputId(1)).is_none(), "output removed");
    }

    #[test]
    fn set_layout_refused_or_silent_agent_fails() {
        let (link, agent) = pair();
        layout_agent(agent, false);
        assert!(!link.set_layout(vec![rect(0), rect(8)], Duration::from_secs(5)));
        assert!(
            link.take_frame(OutputId(1)).is_none(),
            "mailboxes unchanged"
        );
        let (link, _agent) = pair();
        assert!(!link.set_layout(vec![rect(0)], Duration::from_millis(200)));
    }
}
