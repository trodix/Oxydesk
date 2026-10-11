//! Printers of the client made available to the desktop (adapter of `PrinterHost`).
//!
//! Each printer is a Unix socket `<base>/<name>.sock`: one connection is one job, whose bytes go to the
//! client as they arrive. A CUPS queue per printer (optional, see [`Cups`]) lets applications print to it:
//! CUPS converts the document to the language of the printer, then the `rdsktp` backend
//! (`rdsktp-cups-backend`) copies it to the socket.

mod cups;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rdsktp_core::printer::{PrintJob, PrinterHost, PrinterInfo, RemotePrinter};

pub use cups::{queue_name, Cups, SCHEME};

/// Bytes read from a job at once.
const READ_BLOCK: usize = 64 * 1024;
/// A job whose source is silent for this long is cancelled.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Time between two looks at the listening socket (also the delay to stop serving a printer).
const ACCEPT_POLL: Duration = Duration::from_millis(40);
/// Name of the directory holding the sockets; the CUPS backend only accepts sockets in such a directory.
pub const SOCKET_DIR: &str = "rdsktp-printers";

struct Served {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket: PathBuf,
    queue: Option<String>,
}

/// Serves each printer announced by the client under `base/<name>.sock`.
pub struct PrinterSpool {
    base: PathBuf,
    cups: Option<Cups>,
    served: Mutex<HashMap<String, Served>>,
}

impl PrinterSpool {
    /// `base` is created if needed (private to the user); sockets and queues left by a previous
    /// process are removed.
    pub fn new(base: PathBuf, cups: Option<Cups>) -> io::Result<Self> {
        std::fs::create_dir_all(&base)?;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
        if let Some(cups) = &cups {
            cups.remove_stale(&base);
        }
        if let Ok(stale) = std::fs::read_dir(&base) {
            for entry in stale.flatten() {
                if entry.path().extension().is_some_and(|e| e == "sock") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(Self {
            base,
            cups,
            served: Mutex::default(),
        })
    }

    /// Where the socket of the printer `name` is.
    pub fn socket_path(&self, name: &str) -> PathBuf {
        self.base.join(format!("{name}.sock"))
    }

    fn serve(
        &self,
        name: &str,
        printer: Arc<dyn RemotePrinter>,
    ) -> io::Result<(Arc<AtomicBool>, JoinHandle<()>, PathBuf)> {
        let socket = self.socket_path(name);
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            let name = name.to_owned();
            std::thread::Builder::new()
                .name(format!("print-{name}"))
                .spawn(move || accept_jobs(&listener, &printer, &stop, &name))?
        };
        Ok((stop, thread, socket))
    }
}

fn accept_jobs(
    listener: &UnixListener,
    printer: &Arc<dyn RemotePrinter>,
    stop: &AtomicBool,
    name: &str,
) {
    // One job at a time per printer: the next one waits (its source is held back by the socket).
    let turn = Arc::new(Mutex::new(()));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (printer, turn, name) = (printer.clone(), turn.clone(), name.to_owned());
                let _ = std::thread::Builder::new()
                    .name(format!("job-{name}"))
                    .spawn(move || {
                        let _turn = turn.lock().unwrap_or_else(|e| e.into_inner());
                        run_job(stream, &*printer, &name);
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(e) => {
                tracing::warn!(printer = name, "print socket failed: {e}");
                return;
            }
        }
    }
}

/// Sends the bytes of one connection to the client as one job. The source is told the outcome with
/// one last line: `OK` or `ERROR: <reason>`.
fn run_job(mut stream: UnixStream, printer: &dyn RemotePrinter, name: &str) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let outcome = match printer.begin("rdsktp") {
        Ok(job) => copy_job(&mut stream, job, name),
        Err(e) => Err(e.to_string()),
    };
    match &outcome {
        Ok(bytes) => tracing::info!(printer = name, bytes, "print job sent"),
        Err(e) => tracing::warn!(printer = name, "print job failed: {e}"),
    }
    let _ = match outcome {
        Ok(_) => stream.write_all(b"OK\n"),
        Err(e) => stream.write_all(format!("ERROR: {}\n", e.replace('\n', " ")).as_bytes()),
    };
}

fn copy_job(
    stream: &mut UnixStream,
    mut job: Box<dyn PrintJob>,
    _name: &str,
) -> Result<u64, String> {
    let mut buf = vec![0u8; READ_BLOCK];
    let mut total = 0u64;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return job.finish().map(|()| total).map_err(|e| e.to_string()),
            Ok(n) => {
                if let Err(e) = job.write(&buf[..n]) {
                    job.abort();
                    return Err(e.to_string());
                }
                total += n as u64;
            }
            Err(e) => {
                job.abort();
                return Err(e.to_string());
            }
        }
    }
}

impl PrinterHost for PrinterSpool {
    fn added(&self, name: &str, info: &PrinterInfo, printer: Arc<dyn RemotePrinter>) {
        match self.serve(name, printer) {
            Ok((stop, thread, socket)) => {
                tracing::info!(printer = name, socket = %socket.display(), "printer available");
                let queue = self
                    .cups
                    .as_ref()
                    .and_then(|cups| cups.add_queue(name, info, &socket));
                let old = self.served.lock().expect("served").insert(
                    name.to_owned(),
                    Served {
                        stop,
                        thread: Some(thread),
                        socket,
                        queue,
                    },
                );
                if let Some(old) = old {
                    self.stop(old);
                }
            }
            Err(e) => tracing::warn!(printer = name, "cannot serve the redirected printer: {e}"),
        }
    }

    fn removed(&self, name: &str) {
        let served = self.served.lock().expect("served").remove(name);
        if let Some(served) = served {
            self.stop(served);
            tracing::info!(printer = name, "printer removed");
        }
    }
}

impl PrinterSpool {
    fn stop(&self, mut served: Served) {
        if let (Some(cups), Some(queue)) = (&self.cups, &served.queue) {
            cups.remove_queue(queue);
        }
        served.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = served.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&served.socket);
    }
}

impl Drop for PrinterSpool {
    fn drop(&mut self) {
        let all: Vec<Served> = self
            .served
            .lock()
            .map(|mut m| m.drain().map(|(_, s)| s).collect())
            .unwrap_or_default();
        for served in all {
            self.stop(served);
        }
    }
}

/// Name of the user running this process.
pub fn current_user() -> String {
    // SAFETY: `getpwuid` returns null or a valid entry, whose name is copied before any other call.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return std::env::var("USER").unwrap_or_else(|_| "user".into());
        }
        std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

/// Whether `path` is a socket made by this crate for the user `uid` (what the CUPS backend checks).
pub fn is_spool_socket(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return false;
    };
    if canonical != path {
        return false;
    }
    let (Ok(meta), Some(dir)) = (std::fs::symlink_metadata(path), path.parent()) else {
        return false;
    };
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    meta.file_type().is_socket()
        && meta.uid() == uid
        && dir_meta.is_dir()
        && dir_meta.uid() == uid
        && dir.file_name().is_some_and(|n| n == SOCKET_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdsktp_core::error::PrintError;
    use rdsktp_core::printer::PrintLanguage;

    #[derive(Default)]
    struct Recorder {
        jobs: Mutex<Vec<(Vec<u8>, &'static str)>>,
        fail_after: Option<usize>,
    }

    struct Job {
        at: Arc<Recorder>,
        data: Vec<u8>,
    }

    impl PrintJob for Job {
        fn write(&mut self, data: &[u8]) -> Result<(), PrintError> {
            if self.at.fail_after.is_some_and(|n| self.data.len() >= n) {
                return Err(PrintError::Refused("full".into()));
            }
            self.data.extend_from_slice(data);
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<(), PrintError> {
            self.at.jobs.lock().unwrap().push((self.data, "finished"));
            Ok(())
        }
        fn abort(self: Box<Self>) {
            self.at.jobs.lock().unwrap().push((self.data, "aborted"));
        }
    }

    struct Printer(Arc<Recorder>);

    impl RemotePrinter for Printer {
        fn begin(&self, _: &str) -> Result<Box<dyn PrintJob>, PrintError> {
            Ok(Box::new(Job {
                at: self.0.clone(),
                data: Vec::new(),
            }))
        }
    }

    fn info() -> PrinterInfo {
        PrinterInfo {
            name: "Office".into(),
            driver: "MS Publisher Imagesetter".into(),
            is_default: true,
            language: PrintLanguage::PostScript,
        }
    }

    fn wait_for(recorder: &Recorder, jobs: usize) -> Vec<(Vec<u8>, &'static str)> {
        for _ in 0..200 {
            let got = recorder.jobs.lock().unwrap().clone();
            if got.len() >= jobs {
                return got;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("job not received");
    }

    #[test]
    fn a_connection_is_a_job() {
        let dir = tempfile::tempdir().unwrap();
        let spool = PrinterSpool::new(dir.path().join(SOCKET_DIR), None).unwrap();
        let recorder = Arc::new(Recorder::default());
        spool.added("Office", &info(), Arc::new(Printer(recorder.clone())));

        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut c = UnixStream::connect(spool.socket_path("Office")).unwrap();
        c.write_all(&payload).unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        c.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "OK\n");
        let jobs = wait_for(&recorder, 1);
        assert_eq!(jobs[0].1, "finished");
        assert_eq!(jobs[0].0, payload);

        // Another job, sent while the first one is over.
        let mut c = UnixStream::connect(spool.socket_path("Office")).unwrap();
        c.write_all(b"%!PS second").unwrap();
        drop(c);
        let jobs = wait_for(&recorder, 2);
        assert_eq!(jobs[1].0, b"%!PS second");
    }

    #[test]
    fn a_refused_block_aborts_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let spool = PrinterSpool::new(dir.path().join(SOCKET_DIR), None).unwrap();
        let recorder = Arc::new(Recorder {
            fail_after: Some(100_000),
            ..Default::default()
        });
        spool.added("Office", &info(), Arc::new(Printer(recorder.clone())));
        let mut c = UnixStream::connect(spool.socket_path("Office")).unwrap();
        let _ = c.write_all(&vec![7u8; 400_000]);
        let _ = c.shutdown(std::net::Shutdown::Write);
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
        assert!(answer.starts_with("ERROR: "), "{answer}");
        assert_eq!(wait_for(&recorder, 1)[0].1, "aborted");
    }

    #[test]
    fn removing_a_printer_removes_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let spool = PrinterSpool::new(dir.path().join(SOCKET_DIR), None).unwrap();
        spool.added("Office", &info(), Arc::new(Printer(Arc::default())));
        let socket = spool.socket_path("Office");
        assert!(socket.exists());
        spool.removed("Office");
        assert!(!socket.exists());
        spool.added("Other", &info(), Arc::new(Printer(Arc::default())));
        let other = spool.socket_path("Other");
        drop(spool);
        assert!(!other.exists());
    }

    #[test]
    fn only_own_sockets_in_the_spool_directory_are_accepted() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let spool = PrinterSpool::new(dir.path().join(SOCKET_DIR), None).unwrap();
        spool.added("Office", &info(), Arc::new(Printer(Arc::default())));
        let socket = spool.socket_path("Office");
        let uid = std::fs::metadata(&socket).unwrap().uid();
        assert!(is_spool_socket(&socket, uid));
        assert!(!is_spool_socket(&socket, uid + 1));
        // A regular file, a socket elsewhere, a path through a link.
        std::fs::write(spool.base.join("file.sock"), b"x").unwrap();
        assert!(!is_spool_socket(&spool.base.join("file.sock"), uid));
        let elsewhere = dir.path().join("other.sock");
        let _l = UnixListener::bind(&elsewhere).unwrap();
        assert!(!is_spool_socket(&elsewhere, uid));
        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        assert!(!is_spool_socket(&link, uid));
    }
}
