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

#[cfg(test)]
mod tests {
    use super::*;

    type Log = Arc<Mutex<Vec<(u32, DriveOp)>>>;

    /// A drive whose "server" answers each call at once, as the script says.
    fn proxy(
        answer: impl Fn(&DriveOp) -> DriveResult + Send + Sync + 'static,
    ) -> (ProxyDrive, Log) {
        let pending = Arc::new(Pending::default());
        let log: Log = Arc::default();
        let (p, l) = (pending.clone(), log.clone());
        let send: LinkSend = Arc::new(move |msg| {
            let FromAgent::DriveCall {
                req_id,
                drive_id,
                op,
            } = msg
            else {
                return false;
            };
            let result = answer(op);
            l.lock().unwrap().push((*drive_id, op.clone()));
            p.complete(*req_id, result);
            true
        });
        (
            ProxyDrive {
                id: 7,
                send,
                pending,
            },
            log,
        )
    }

    fn info(size: u64) -> DriveStatInfo {
        DriveStatInfo {
            size,
            is_dir: false,
            read_only: true,
            modified: 1,
            accessed: 2,
            created: 3,
        }
    }

    #[test]
    fn every_failure_of_the_server_is_a_drive_error() {
        let pairs = [
            (DriveFailure::NotFound, DriveError::NotFound),
            (DriveFailure::Exists, DriveError::Exists),
            (DriveFailure::Denied, DriveError::Denied),
            (DriveFailure::NotDirectory, DriveError::NotDirectory),
            (DriveFailure::IsDirectory, DriveError::IsDirectory),
            (DriveFailure::NotEmpty, DriveError::NotEmpty),
            (DriveFailure::NoSpace, DriveError::NoSpace),
            (DriveFailure::Invalid, DriveError::Invalid),
            (DriveFailure::Busy, DriveError::Busy),
            (DriveFailure::Io("x".into()), DriveError::Io("x".into())),
        ];
        for (wire, expected) in pairs {
            let (d, _) = proxy({
                let wire = wire.clone();
                move |_| DriveResult::Err(wire.clone())
            });
            assert_eq!(d.stat("f").unwrap_err(), expected);
        }
    }

    #[test]
    fn operations_travel_to_the_server_with_the_drive_id() {
        let (d, log) = proxy(|op| match op {
            DriveOp::Stat(_) => DriveResult::Stat(info(9)),
            DriveOp::List(_) => DriveResult::List(vec![("a".into(), info(1))]),
            DriveOp::Open { .. } => DriveResult::Handle(5),
            DriveOp::Read { .. } => DriveResult::Data(vec![1, 2]),
            DriveOp::Write { data, .. } => DriveResult::Written(data.len() as u32),
            DriveOp::Space => DriveResult::Space {
                total: 10,
                available: 4,
            },
            _ => DriveResult::Ok,
        });
        let st = d.stat("f").unwrap();
        assert_eq!(
            (st.size, st.read_only, st.modified, st.accessed, st.created),
            (9, true, 1, 2, 3)
        );
        let list = d.list("").unwrap();
        assert_eq!((list[0].name.as_str(), list[0].stat.size), ("a", 1));
        for (mode, wire) in [
            (OpenMode::Read, 0),
            (OpenMode::ReadWrite, 1),
            (OpenMode::Truncate, 2),
            (OpenMode::Create, 3),
        ] {
            assert_eq!(d.open("f", mode).unwrap(), DriveHandle(5));
            let last = log.lock().unwrap().last().unwrap().1.clone();
            assert_eq!(
                last,
                DriveOp::Open {
                    path: "f".into(),
                    mode: wire
                }
            );
        }
        assert_eq!(d.read(DriveHandle(5), 3, 2).unwrap(), [1, 2]);
        assert_eq!(d.write(DriveHandle(5), 0, b"abc").unwrap(), 3);
        d.flush(DriveHandle(5)).unwrap();
        d.mkdir("d").unwrap();
        d.remove("d", true).unwrap();
        d.rename("a", "b").unwrap();
        d.truncate("a", 0).unwrap();
        d.set_modified("a", 5).unwrap();
        d.close(DriveHandle(5));
        let space = d.space().unwrap();
        assert_eq!((space.total, space.available), (10, 4));
        assert!(log.lock().unwrap().iter().all(|(id, _)| *id == 7));
        assert!(log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, op)| *op == DriveOp::Close(5)));
    }

    #[test]
    fn an_answer_of_the_wrong_kind_is_an_error_not_a_panic() {
        let (d, _) = proxy(|_| DriveResult::Ok);
        for r in [
            d.stat("f").map(drop),
            d.list("").map(drop),
            d.open("f", OpenMode::Read).map(drop),
            d.read(DriveHandle(1), 0, 1).map(drop),
            d.write(DriveHandle(1), 0, b"x").map(drop),
            d.space().map(drop),
        ] {
            assert_eq!(r.unwrap_err(), unexpected());
        }
        let (d, _) = proxy(|_| DriveResult::Data(vec![]));
        assert_eq!(d.mkdir("d").unwrap_err(), unexpected());
    }

    #[test]
    fn a_closed_link_fails_the_call_and_leaves_nothing_waiting() {
        let pending = Arc::new(Pending::default());
        let d = ProxyDrive {
            id: 1,
            send: Arc::new(|_| false),
            pending: pending.clone(),
        };
        assert!(matches!(d.stat("f"), Err(DriveError::Io(m)) if m.contains("closed")));
        assert!(pending.next.lock().unwrap().1.is_empty());
    }

    #[test]
    fn waiters_are_woken_with_an_error_when_the_link_ends() {
        let pending = Arc::new(Pending::default());
        let (id, rx) = pending.register();
        pending.fail_all();
        assert!(rx.recv().is_err(), "the sender was dropped");
        // A late answer for a forgotten call is ignored.
        pending.complete(id, DriveResult::Ok);
        let (id2, rx2) = pending.register();
        assert_ne!(id, id2, "ids are not reused");
        pending.complete(id2, DriveResult::Ok);
        assert_eq!(rx2.recv().unwrap(), DriveResult::Ok);
    }

    #[test]
    fn request_ids_wrap_around_without_collision() {
        let pending = Pending::default();
        pending.next.lock().unwrap().0 = u32::MAX;
        let (id, _rx) = pending.register();
        assert_eq!(id, 0);
        let (next, _rx2) = pending.register();
        assert_eq!(next, 1);
    }

    #[derive(Default)]
    struct Host(Mutex<Vec<String>>);

    impl DriveHost for Host {
        fn added(&self, name: &str, _: Arc<dyn RemoteDrive>) {
            self.0.lock().unwrap().push(format!("+{name}"));
        }
        fn removed(&self, name: &str) {
            self.0.lock().unwrap().push(format!("-{name}"));
        }
    }

    fn wait_for(host: &Host, n: usize) -> Vec<String> {
        for _ in 0..500 {
            let v = host.0.lock().unwrap().clone();
            if v.len() >= n {
                return v;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host was not called");
    }

    #[test]
    fn drives_are_mounted_then_unmounted_by_id() {
        let host = Arc::new(Host::default());
        let drives = Drives::new(host.clone(), Arc::new(|_| true));
        drives.added(1, "C".into());
        assert_eq!(wait_for(&host, 1), ["+C"]);
        drives.removed(1);
        assert_eq!(wait_for(&host, 2), ["+C", "-C"]);
        // An unknown or already removed drive is ignored.
        drives.removed(1);
        drives.removed(42);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(host.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn closing_unmounts_everything_and_wakes_the_waiters() {
        let host = Arc::new(Host::default());
        let drives = Drives::new(host.clone(), Arc::new(|_| true));
        drives.added(1, "C".into());
        drives.added(2, "D".into());
        wait_for(&host, 2);
        let (_, rx) = drives.pending.register();
        drives.close();
        assert!(rx.recv().is_err());
        let mut calls = host.0.lock().unwrap().clone();
        calls.sort();
        assert_eq!(calls, ["+C", "+D", "-C", "-D"]);
        drives.close();
        assert_eq!(host.0.lock().unwrap().len(), 4, "nothing left to unmount");
    }
}
