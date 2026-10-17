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
use oxydesk_core::{
    PrintFormat, PrintLanguage, PrinterHost, PrinterInfo, PrinterModes, RemotePrinter,
};
use oxydesk_rdp::drive::{DriveBackend, EventSender};
use rdp_test_client::drive_client::DirectoryDrive;

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

struct Setup {
    driver: &'static str,
    /// The client announces that it takes XPS.
    xps_client: bool,
    modes: PrinterModes,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            driver: "Generic / Text Only",
            xps_client: false,
            modes: PrinterModes::default(),
        }
    }
}

/// What the client received, in order: `x` for `DR_PRN_USING_XPS`, `i` for a device I/O request.
#[derive(Default)]
struct Seen {
    order: Mutex<String>,
}

impl Seen {
    fn using_xps(&self) -> usize {
        self.order.lock().unwrap().matches('x').count()
    }
}

fn connect(
    record: &std::path::Path,
    host: Arc<Host>,
) -> (Arc<dyn RemotePrinter>, PrinterInfo, Wire) {
    let (printer, info, wire, _) = connect_with(record, host, Setup::default());
    (printer.expect("printer"), info.expect("info"), wire)
}

/// `None` for the printer when the server did not redirect it (no announcement within 2 s).
fn connect_with(
    record: &std::path::Path,
    host: Arc<Host>,
    setup: Setup,
) -> (
    Option<Arc<dyn RemotePrinter>>,
    Option<PrinterInfo>,
    Wire,
    Arc<Seen>,
) {
    let (tx, rx): (_, Receiver<ServerEvent>) = mpsc::channel();
    let tx = Mutex::new(tx);
    let send: EventSender = Arc::new(move |event| tx.lock().unwrap().send(event).is_ok());
    let mut server = RdpdrServer::new(Box::new(
        DriveBackend::new(send, None, Some(host.clone())).with_modes(setup.modes),
    ));
    let mut client = if setup.xps_client {
        DirectoryDrive::xps_printer_channel(
            record.to_path_buf(),
            3,
            "Bureau",
            setup.driver,
            "TESTPC",
        )
    } else {
        DirectoryDrive::printer_channel(
            record.to_path_buf(),
            3,
            "Bureau",
            setup.driver,
            None,
            "TESTPC",
        )
    };
    let seen = Arc::new(Seen::default());
    let sniff = seen.clone();
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
                // Unframed: RDPDR_CTYP_PRN (0x5052) + PAKID_PRN_USING_XPS (0x5543), or
                // RDPDR_CTYP_CORE (0x4472) + PAKID_CORE_DEVICE_IOREQUEST (0x4952).
                if p.starts_with(&[0x52, 0x50, 0x43, 0x55]) {
                    sniff.order.lock().unwrap().push('x');
                } else if p.starts_with(&[0x72, 0x44, 0x52, 0x49]) {
                    sniff.order.lock().unwrap().push('i');
                }
                encode(client.process(&p).expect("client process"), &mut to_server);
            }
            while let Some(p) = to_server.pop_front() {
                encode(server.process(&p).expect("server process"), &mut to_client);
            }
        }
    });
    let wire = Wire {
        stop,
        thread: Some(thread),
    };
    let mut printers = host.printers.lock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while printers.is_empty() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return (None, None, wire, seen);
        }
        let (guard, _) = host
            .changed
            .wait_timeout(printers, left.min(Duration::from_secs(10)))
            .unwrap();
        printers = guard;
    }
    let (name, info, printer) = printers[0].clone();
    assert_eq!(name, "Bureau");
    (Some(printer), Some(info), wire, seen)
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oxydesk-print-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn announces_the_printer_and_records_jobs_byte_for_byte() {
    let record = tmp("jobs");
    let (printer, info, _wire) = connect(&record, Arc::new(Host::default()));
    assert_eq!(info.driver, "Generic / Text Only");
    assert_eq!(info.format, PrintFormat::Raw(PrintLanguage::Text));

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

#[test]
fn xps_mode_tells_the_client_before_the_first_job() {
    let record = tmp("xps");
    let setup = Setup {
        driver: "Brother HL-L2350DW",
        xps_client: true,
        modes: PrinterModes::parse("xps").unwrap(),
    };
    let (printer, info, _wire, seen) = connect_with(&record, Arc::new(Host::default()), setup);
    assert_eq!(info.unwrap().format, PrintFormat::Xps);
    let printer = printer.unwrap();
    let mut job = printer.begin("xps").expect("begin");
    job.write(b"PK-fake-xps").expect("write");
    job.finish().expect("finish");
    assert_eq!(
        std::fs::read(record.join("job-1.bin")).unwrap(),
        b"PK-fake-xps"
    );
    // Told once, and before the first request of the job.
    let order = seen.order.lock().unwrap().clone();
    assert!(order.starts_with('x'), "{order}");
    assert_eq!(seen.using_xps(), 1);
    let _ = std::fs::remove_dir_all(&record);
}

#[test]
fn auto_mode_keeps_a_known_language_raw_even_when_the_client_takes_xps() {
    let record = tmp("auto-raw");
    let setup = Setup {
        driver: "HP Universal Printing PCL 6",
        xps_client: true,
        modes: PrinterModes::default(),
    };
    let (printer, info, _wire, seen) = connect_with(&record, Arc::new(Host::default()), setup);
    assert_eq!(info.unwrap().format, PrintFormat::Raw(PrintLanguage::Pcl));
    let mut job = printer.unwrap().begin("raw").expect("begin");
    job.write(b"\x1bE").expect("write");
    job.finish().expect("finish");
    assert_eq!(seen.using_xps(), 0);
    let _ = std::fs::remove_dir_all(&record);
}

#[test]
fn an_undecidable_printer_is_not_redirected() {
    let record = tmp("undecided");
    // No language in the driver name, and a client that does not take XPS: nothing is guessed.
    let setup = Setup {
        driver: "Brother HL-L2350DW",
        xps_client: false,
        modes: PrinterModes::default(),
    };
    let (printer, _, _wire, seen) = connect_with(&record, Arc::new(Host::default()), setup);
    assert!(printer.is_none());
    assert_eq!(seen.using_xps(), 0);
    let _ = std::fs::remove_dir_all(&record);
}
