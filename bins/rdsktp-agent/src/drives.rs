//! Drives redirected by the client, seen from the agent: each one is a `RemoteDrive` whose operations are
//! asked to the server through the link, and which is mounted in the user's session.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdsktp_agent_proto::{DriveFailure, DriveOp, DriveResult, DriveStatInfo, FromAgent};
use rdsktp_core::error::DriveError;
use rdsktp_core::{
    DriveEntry, DriveHandle, DriveHost, DriveSpace, DriveStat, OpenMode, RemoteDrive,
};

/// Longest wait for an answer: the server itself gives up on the client after 30 s.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Sends a message to the server; false if the link is down.
pub type LinkSend = Arc<dyn Fn(&FromAgent) -> bool + Send + Sync>;

/// The calls waiting for their answer.
#[derive(Default)]
pub struct Pending {
    next: Mutex<(u32, HashMap<u32, Sender<DriveResult>>)>,
}

impl Pending {
    pub fn complete(&self, req_id: u32, result: DriveResult) {
        let waiter = self.next.lock().expect("pending mutex").1.remove(&req_id);
        if let Some(w) = waiter {
            let _ = w.send(result);
        }
    }

    /// The link ended: the waiters are woken with an error.
    pub fn fail_all(&self) {
        self.next.lock().expect("pending mutex").1.clear();
    }

    fn register(&self) -> (u32, std::sync::mpsc::Receiver<DriveResult>) {
        let (tx, rx) = channel();
        let mut g = self.next.lock().expect("pending mutex");
        g.0 = g.0.wrapping_add(1);
        let id = g.0;
        g.1.insert(id, tx);
        (id, rx)
    }

    fn forget(&self, req_id: u32) {
        self.next.lock().expect("pending mutex").1.remove(&req_id);
    }
}

fn error(f: DriveFailure) -> DriveError {
    match f {
        DriveFailure::NotFound => DriveError::NotFound,
        DriveFailure::Exists => DriveError::Exists,
        DriveFailure::Denied => DriveError::Denied,
        DriveFailure::NotDirectory => DriveError::NotDirectory,
        DriveFailure::IsDirectory => DriveError::IsDirectory,
        DriveFailure::NotEmpty => DriveError::NotEmpty,
        DriveFailure::NoSpace => DriveError::NoSpace,
        DriveFailure::Invalid => DriveError::Invalid,
        DriveFailure::Busy => DriveError::Busy,
        DriveFailure::Io(m) => DriveError::Io(m),
    }
}

fn stat(s: DriveStatInfo) -> DriveStat {
    DriveStat {
        size: s.size,
        is_dir: s.is_dir,
        read_only: s.read_only,
        modified: s.modified,
        accessed: s.accessed,
        created: s.created,
    }
}

fn unexpected() -> DriveError {
    DriveError::Io("unexpected answer from the server".into())
}

/// A drive of the client reached through the server.
pub struct ProxyDrive {
    id: u32,
    send: LinkSend,
    pending: Arc<Pending>,
}

impl ProxyDrive {
    fn call(&self, op: DriveOp) -> Result<DriveResult, DriveError> {
        let (req_id, rx) = self.pending.register();
        if !(self.send)(&FromAgent::DriveCall {
            req_id,
            drive_id: self.id,
            op,
        }) {
            self.pending.forget(req_id);
            return Err(DriveError::Io("link to the server closed".into()));
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(DriveResult::Err(f)) => Err(error(f)),
            Ok(r) => Ok(r),
            Err(_) => {
                self.pending.forget(req_id);
                Err(DriveError::Io("no answer from the server".into()))
            }
        }
    }

    fn unit(&self, op: DriveOp) -> Result<(), DriveError> {
        match self.call(op)? {
            DriveResult::Ok => Ok(()),
            _ => Err(unexpected()),
        }
    }
}

impl RemoteDrive for ProxyDrive {
    fn stat(&self, path: &str) -> Result<DriveStat, DriveError> {
        match self.call(DriveOp::Stat(path.into()))? {
            DriveResult::Stat(s) => Ok(stat(s)),
            _ => Err(unexpected()),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<DriveEntry>, DriveError> {
        match self.call(DriveOp::List(path.into()))? {
            DriveResult::List(entries) => Ok(entries
                .into_iter()
                .map(|(name, s)| DriveEntry {
                    name,
                    stat: stat(s),
                })
                .collect()),
            _ => Err(unexpected()),
        }
    }

    fn open(&self, path: &str, mode: OpenMode) -> Result<DriveHandle, DriveError> {
        let mode = match mode {
            OpenMode::Read => 0,
            OpenMode::ReadWrite => 1,
            OpenMode::Truncate => 2,
            OpenMode::Create => 3,
        };
        match self.call(DriveOp::Open {
            path: path.into(),
            mode,
        })? {
            DriveResult::Handle(h) => Ok(DriveHandle(h)),
            _ => Err(unexpected()),
        }
    }

    fn read(&self, file: DriveHandle, offset: u64, len: u32) -> Result<Vec<u8>, DriveError> {
        match self.call(DriveOp::Read {
            handle: file.0,
            offset,
            len,
        })? {
            DriveResult::Data(d) => Ok(d),
            _ => Err(unexpected()),
        }
    }

    fn write(&self, file: DriveHandle, offset: u64, data: &[u8]) -> Result<u32, DriveError> {
        match self.call(DriveOp::Write {
            handle: file.0,
            offset,
            data: data.to_vec(),
        })? {
            DriveResult::Written(n) => Ok(n),
            _ => Err(unexpected()),
        }
    }

    fn flush(&self, file: DriveHandle) -> Result<(), DriveError> {
        self.unit(DriveOp::Flush(file.0))
    }

    fn close(&self, file: DriveHandle) {
        let _ = self.unit(DriveOp::Close(file.0));
    }

    fn mkdir(&self, path: &str) -> Result<(), DriveError> {
        self.unit(DriveOp::Mkdir(path.into()))
    }

    fn remove(&self, path: &str, is_dir: bool) -> Result<(), DriveError> {
        self.unit(DriveOp::Remove {
            path: path.into(),
            is_dir,
        })
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), DriveError> {
        self.unit(DriveOp::Rename {
            from: from.into(),
            to: to.into(),
        })
    }

    fn truncate(&self, path: &str, size: u64) -> Result<(), DriveError> {
        self.unit(DriveOp::Truncate {
            path: path.into(),
            size,
        })
    }

    fn set_modified(&self, path: &str, seconds: i64) -> Result<(), DriveError> {
        self.unit(DriveOp::SetModified {
            path: path.into(),
            seconds,
        })
    }

    fn space(&self) -> Result<DriveSpace, DriveError> {
        match self.call(DriveOp::Space)? {
            DriveResult::Space { total, available } => Ok(DriveSpace { total, available }),
            _ => Err(unexpected()),
        }
    }
}

/// The drives of one connection to the server, mounted through `host`.
pub struct Drives {
    host: Arc<dyn DriveHost>,
    send: LinkSend,
    pub pending: Arc<Pending>,
    names: Mutex<HashMap<u32, String>>,
}

impl Drives {
    pub fn new(host: Arc<dyn DriveHost>, send: LinkSend) -> Self {
        Self {
            host,
            send,
            pending: Arc::new(Pending::default()),
            names: Mutex::new(HashMap::new()),
        }
    }

    pub fn added(&self, id: u32, name: String) {
        let drive = Arc::new(ProxyDrive {
            id,
            send: self.send.clone(),
            pending: self.pending.clone(),
        });
        self.names
            .lock()
            .expect("names mutex")
            .insert(id, name.clone());
        // The mount can take a moment (fusermount): not in the reader thread.
        let host = self.host.clone();
        std::thread::spawn(move || host.added(&name, drive));
    }

    pub fn removed(&self, id: u32) {
        if let Some(name) = self.names.lock().expect("names mutex").remove(&id) {
            let host = self.host.clone();
            std::thread::spawn(move || host.removed(&name));
        }
    }

    /// The connection ended: nothing of the client remains in the session.
    pub fn close(&self) {
        self.pending.fail_all();
        let names: Vec<String> = self
            .names
            .lock()
            .expect("names mutex")
            .drain()
            .map(|(_, n)| n)
            .collect();
        for name in names {
            self.host.removed(&name);
        }
    }
}
