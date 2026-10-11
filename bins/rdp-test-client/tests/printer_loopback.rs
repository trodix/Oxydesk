//! The printer-redirection server (`DriveBackend`, `RdpPrinter`) against the client side recording jobs in a
//! directory, without network: what the desktop does with a `RemotePrinter` ends up as files.
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
use rdsktp_core::{PrintLanguage, PrinterHost, PrinterInfo, RemotePrinter};
use rdsktp_rdp::drive::{DriveBackend, EventSender};

type Announced = Vec<(String, PrinterInfo, Arc<dyn RemotePrinter>)>;

#[derive(Default)]
struct Host {
    printers: Mutex<Announced>,
    changed: Condvar,
}

impl PrinterHost for Host {
    fn added(&self, name: &str, info: &PrinterInfo, printer: Arc<dyn RemotePrinter>) {
        self.printers
            .lock()
            .unwrap()
            .push((name.to_owned(), info.clone(), printer));
        self.changed.notify_all();
    }

    fn removed(&self, _: &str) {}
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

fn connect(
    record: &std::path::Path,
    host: Arc<Host>,
) -> (Arc<dyn RemotePrinter>, PrinterInfo, Wire) {
    let (tx, rx): (_, Receiver<ServerEvent>) = mpsc::channel();
    let tx = Mutex::new(tx);
    let send: EventSender = Arc::new(move |event| tx.lock().unwrap().send(event).is_ok());
    let mut server = RdpdrServer::new(Box::new(DriveBackend::new(send, None, Some(host.clone()))));
    let mut client = DirectoryDrive::printer_channel(
        record.to_path_buf(),
        3,
        "Bureau",
        "Generic / Text Only",
        None,
        "TESTPC",
    );
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
    let mut printers = host.printers.lock().unwrap();
    while printers.is_empty() {
        let (guard, timeout) = host
            .changed
            .wait_timeout(printers, Duration::from_secs(10))
            .unwrap();
        printers = guard;
        assert!(!timeout.timed_out(), "the printer was never announced");
    }
    let (name, info, printer) = printers[0].clone();
    assert_eq!(name, "Bureau");
    (
        printer,
        info,
        Wire {
            stop,
            thread: Some(thread),
        },
    )
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rdsktp-print-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn announces_the_printer_and_records_jobs_byte_for_byte() {
    let record = tmp("jobs");
    let (printer, info, _wire) = connect(&record, Arc::new(Host::default()));
    assert_eq!(info.driver, "Generic / Text Only");
    assert_eq!(info.language, PrintLanguage::Text);

    // A job in several blocks, bigger than the 64 KiB blocks of the adapter.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let mut job = printer.begin("test").expect("begin");
    for chunk in data.chunks(100_000) {
        job.write(chunk).expect("write");
    }
    job.finish().expect("finish");
    assert_eq!(std::fs::read(record.join("job-1.bin")).unwrap(), data);

    // A second job after the first, and an aborted one: the printer stays usable.
    let mut job = printer.begin("deux").expect("begin");
    job.write(b"deux\n").expect("write");
    job.finish().expect("finish");
    assert_eq!(std::fs::read(record.join("job-2.bin")).unwrap(), b"deux\n");
    let mut job = printer.begin("abandonne").expect("begin");
    job.write(b"abc").expect("write");
    job.abort();
    let mut job = printer.begin("quatre").expect("begin");
    job.write(b"quatre").expect("write");
    job.finish().expect("finish");
    assert_eq!(std::fs::read(record.join("job-4.bin")).unwrap(), b"quatre");
    let _ = std::fs::remove_dir_all(&record);
}
