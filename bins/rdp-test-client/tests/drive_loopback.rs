//! The drive-redirection server (`DriveBackend`, `RdpDrive`) against the client side over a
//! directory (`DirectoryDrive`), without network: what the desktop does with a `RemoteDrive` ends
//! up as real operations on the directory.
#![cfg(feature = "ironrdp")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ironrdp_rdpdr::server::RdpdrServer;
use ironrdp_server::ServerEvent;
use ironrdp_svc::{SvcMessage, SvcProcessor};
use rdp_test_client::drive_client::DirectoryDrive;
use rdsktp_core::error::DriveError;
use rdsktp_core::{DriveHost, OpenMode, RemoteDrive};
use rdsktp_rdp::drive::{DriveBackend, EventSender};

#[derive(Default)]
struct Host {
    drives: Mutex<Vec<(String, Arc<dyn RemoteDrive>)>>,
    changed: Condvar,
    removed: Mutex<Vec<String>>,
}

impl DriveHost for Host {
    fn added(&self, name: &str, drive: Arc<dyn RemoteDrive>) {
        self.drives.lock().unwrap().push((name.to_owned(), drive));
        self.changed.notify_all();
    }

    fn removed(&self, name: &str) {
        self.removed.lock().unwrap().push(name.to_owned());
    }
}

struct Wire {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn encode(messages: Vec<SvcMessage>, out: &mut VecDeque<Vec<u8>>) {
    for m in messages {
        out.push_back(m.encode_unframed_pdu().expect("encode"));
    }
}

/// Connects a server backend and a client over `root`; returns the drive the host received.
fn connect(root: &std::path::Path, host: Arc<Host>) -> (Arc<dyn RemoteDrive>, Wire) {
    let (tx, rx): (_, Receiver<ServerEvent>) = mpsc::channel();
    let tx = Mutex::new(tx);
    let send: EventSender = Arc::new(move |event| tx.lock().unwrap().send(event).is_ok());
    let mut server = RdpdrServer::new(Box::new(DriveBackend::new(send, Some(host.clone()), None)));
    let mut client = DirectoryDrive::channel(root.to_path_buf(), 7, "Share", "TESTPC");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let (mut to_client, mut to_server) = (VecDeque::new(), VecDeque::new());
        encode(server.start().expect("server start"), &mut to_client);
        encode(client.start().expect("client start"), &mut to_server);
        while !flag.load(Ordering::Relaxed) {
            if let Ok(ServerEvent::Rdpdr(tag, message)) = rx.recv_timeout(Duration::from_millis(2))
            {
                server.set_next_tag(tag);
                if let Ok(m) = server.send_message(message) {
                    encode(m, &mut to_client);
                }
            }
            while let Some(p) = to_client.pop_front() {
                encode(client.process(&p).expect("client process"), &mut to_server);
            }
            while let Some(p) = to_server.pop_front() {
                encode(server.process(&p).expect("server process"), &mut to_client);
            }
        }
    });
    let mut drives = host.drives.lock().unwrap();
    while drives.is_empty() {
        let (guard, timeout) = host
            .changed
            .wait_timeout(drives, Duration::from_secs(10))
            .unwrap();
        drives = guard;
        assert!(!timeout.timed_out(), "the drive was never announced");
    }
    let drive = drives[0].1.clone();
    assert_eq!(drives[0].0, "Share");
    (
        drive,
        Wire {
            stop,
            thread: Some(thread),
        },
    )
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rdsktp-drive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn lists_stats_and_reads_files_in_blocks() {
    let root = tmp("read");
    let big = pattern(700_000); // several requests
    std::fs::write(root.join("big.bin"), &big).unwrap();
    std::fs::write(root.join("small.txt"), b"hello").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("inner.txt"), b"in").unwrap();
    let (drive, _wire) = connect(&root, Arc::new(Host::default()));

    let mut names: Vec<_> = drive
        .list("")
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.stat.is_dir, e.stat.size))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("big.bin".to_owned(), false, 700_000),
            ("small.txt".to_owned(), false, 5),
            ("sub".to_owned(), true, 0),
        ]
    );
    assert_eq!(drive.list("sub").unwrap()[0].name, "inner.txt");
    assert!(drive.stat("sub").unwrap().is_dir);
    assert_eq!(drive.stat("big.bin").unwrap().size, 700_000);
    assert!(drive.stat("big.bin").unwrap().modified > 1_600_000_000);
    assert_eq!(drive.stat("nope"), Err(DriveError::NotFound));
    assert_eq!(drive.stat("nope/deeper"), Err(DriveError::NotFound));

    let f = drive.open("big.bin", OpenMode::Read).unwrap();
    assert_eq!(drive.read(f, 0, 700_000).unwrap(), big);
    assert_eq!(drive.read(f, 600_000, 1_000_000).unwrap(), &big[600_000..]);
    assert_eq!(drive.read(f, 700_000, 10).unwrap(), Vec::<u8>::new());
    drive.close(f);
    let space = drive.space().unwrap();
    assert_eq!(space.total, 1_000_000 * 8 * 512);
    assert!(space.available < space.total);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writes_creates_renames_and_deletes() {
    let root = tmp("write");
    let (drive, _wire) = connect(&root, Arc::new(Host::default()));
    let data = pattern(600_000);

    let f = drive.open("new.bin", OpenMode::Create).unwrap();
    assert_eq!(drive.write(f, 0, &data).unwrap(), 600_000);
    assert_eq!(drive.write(f, 600_000, b"tail").unwrap(), 4);
    drive.flush(f).unwrap();
    drive.close(f);
    let mut expected = data.clone();
    expected.extend_from_slice(b"tail");
    assert_eq!(std::fs::read(root.join("new.bin")).unwrap(), expected);
    assert_eq!(
        drive.open("new.bin", OpenMode::Create),
        Err(DriveError::Exists)
    );

    drive.mkdir("dir").unwrap();
    assert_eq!(drive.mkdir("dir"), Err(DriveError::Exists));
    drive.rename("new.bin", "dir/moved.bin").unwrap();
    assert!(!root.join("new.bin").exists());
    assert!(root.join("dir/moved.bin").exists());

    drive.truncate("dir/moved.bin", 10).unwrap();
    assert_eq!(
        std::fs::read(root.join("dir/moved.bin")).unwrap(),
        &expected[..10]
    );
    let f = drive.open("dir/moved.bin", OpenMode::Truncate).unwrap();
    drive.close(f);
    assert_eq!(
        std::fs::metadata(root.join("dir/moved.bin")).unwrap().len(),
        0
    );

    drive.set_modified("dir/moved.bin", 1_700_000_000).unwrap();
    assert_eq!(drive.stat("dir/moved.bin").unwrap().modified, 1_700_000_000);

    assert_eq!(drive.remove("dir", true), Err(DriveError::NotEmpty));
    drive.remove("dir/moved.bin", false).unwrap();
    drive.remove("dir", true).unwrap();
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    assert_eq!(drive.remove("dir", true), Err(DriveError::NotFound));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dropped_connection_fails_the_calls_and_removes_the_drive() {
    let root = tmp("drop");
    let host = Arc::new(Host::default());
    let (drive, wire) = connect(&root, host.clone());
    assert!(drive.list("").is_ok());
    drop(wire); // the wire (and the server with its backend) goes away
    assert!(matches!(drive.list(""), Err(DriveError::Io(_))));
    assert_eq!(*host.removed.lock().unwrap(), ["Share"]);
    let _ = std::fs::remove_dir_all(&root);
}
