//! Printers redirected by the client, seen from the agent: each one is a `RemotePrinter` whose jobs are sent
//! to the server through the link, and which the spool exposes in the user's session (socket and CUPS queue).

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdsktp_agent_proto::{FromAgent, PrintOp, PrintResult};
use rdsktp_core::error::PrintError;
use rdsktp_core::{PrintJob, PrintLanguage, PrinterHost, PrinterInfo, RemotePrinter};

use crate::drives::LinkSend;

/// Longest wait for an answer: the server itself gives up on the client after 30 s.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// The calls waiting for their answer.
#[derive(Default)]
pub struct Pending {
    next: Mutex<(u32, HashMap<u32, Sender<PrintResult>>)>,
}

impl Pending {
    pub fn complete(&self, req_id: u32, result: PrintResult) {
        let waiter = self.next.lock().expect("pending mutex").1.remove(&req_id);
        if let Some(w) = waiter {
            let _ = w.send(result);
        }
    }

    /// The link ended: the waiters are woken with an error.
    pub fn fail_all(&self) {
        self.next.lock().expect("pending mutex").1.clear();
    }

    fn register(&self) -> (u32, Receiver<PrintResult>) {
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

fn language(code: u8) -> PrintLanguage {
    match code {
        1 => PrintLanguage::Pcl,
        2 => PrintLanguage::Text,
        _ => PrintLanguage::PostScript,
    }
}

/// A printer of the client reached through the server.
#[derive(Clone)]
struct ProxyPrinter {
    id: u32,
    send: LinkSend,
    pending: Arc<Pending>,
}

impl ProxyPrinter {
    fn call(&self, op: PrintOp) -> Result<PrintResult, PrintError> {
        let (req_id, rx) = self.pending.register();
        if !(self.send)(&FromAgent::PrintCall {
            req_id,
            printer_id: self.id,
            op,
        }) {
            self.pending.forget(req_id);
            return Err(PrintError::Io("link to the server closed".into()));
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(PrintResult::Refused(m)) => Err(PrintError::Refused(m)),
            Ok(PrintResult::Io(m)) => Err(PrintError::Io(m)),
            Ok(r) => Ok(r),
            Err(_) => {
                self.pending.forget(req_id);
                Err(PrintError::Io("no answer from the server".into()))
            }
        }
    }
}

struct ProxyJob {
    printer: ProxyPrinter,
    job: u64,
}

impl RemotePrinter for ProxyPrinter {
    fn begin(&self, title: &str) -> Result<Box<dyn PrintJob>, PrintError> {
        match self.call(PrintOp::Begin {
            title: title.into(),
        })? {
            PrintResult::Job(job) => Ok(Box::new(ProxyJob {
                printer: self.clone(),
                job,
            })),
            _ => Err(PrintError::Io("unexpected answer from the server".into())),
        }
    }
}

impl PrintJob for ProxyJob {
    fn write(&mut self, data: &[u8]) -> Result<(), PrintError> {
        // One block at a time: the order of the bytes is the order of the answers.
        for chunk in data.chunks(rdsktp_core::printer::MAX_PRINT_BLOCK as usize) {
            self.printer.call(PrintOp::Write {
                job: self.job,
                data: chunk.to_vec(),
            })?;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), PrintError> {
        self.printer.call(PrintOp::Finish(self.job)).map(|_| ())
    }

    fn abort(self: Box<Self>) {
        let _ = self.printer.call(PrintOp::Abort(self.job));
    }
}

/// The printers of one connection to the server, exposed through `host`.
pub struct Printers {
    host: Arc<dyn PrinterHost>,
    send: LinkSend,
    pub pending: Arc<Pending>,
    names: Mutex<HashMap<u32, String>>,
}

impl Printers {
    pub fn new(host: Arc<dyn PrinterHost>, send: LinkSend) -> Self {
        Self {
            host,
            send,
            pending: Arc::new(Pending::default()),
            names: Mutex::new(HashMap::new()),
        }
    }

    pub fn added(&self, id: u32, name: String, driver: String, is_default: bool, lang: u8) {
        let printer = ProxyPrinter {
            id,
            send: self.send.clone(),
            pending: self.pending.clone(),
        };
        let info = PrinterInfo {
            name: name.clone(),
            driver,
            is_default,
            language: language(lang),
        };
        self.names
            .lock()
            .expect("names mutex")
            .insert(id, name.clone());
        // The CUPS queue can take a moment (lpadmin): not in the reader thread.
        let host = self.host.clone();
        std::thread::spawn(move || host.added(&name, &info, Arc::new(printer)));
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
    //! The whole relay in one process: a job written to the socket of the agent's spool goes through the
    //! agent protocol and the server's link to the (fake) printer of the client, and back.
    use super::*;
    use rdsktp_agent_proto::{read_msg, write_msg, ToAgent};
    use rdsktp_remote::AgentLink;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[derive(Default)]
    struct Client(Mutex<Vec<Vec<u8>>>);
    struct Job(Arc<Client>, Vec<u8>);
    impl PrintJob for Job {
        fn write(&mut self, d: &[u8]) -> Result<(), PrintError> {
            self.1.extend_from_slice(d);
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<(), PrintError> {
            self.0 .0.lock().unwrap().push(self.1);
            Ok(())
        }
        fn abort(self: Box<Self>) {}
    }
    struct Printer(Arc<Client>);
    impl RemotePrinter for Printer {
        fn begin(&self, _: &str) -> Result<Box<dyn PrintJob>, PrintError> {
            Ok(Box::new(Job(self.0.clone(), Vec::new())))
        }
    }

    #[test]
    fn a_job_of_the_session_reaches_the_printer_of_the_client() {
        let (server_end, mut agent_end) = UnixStream::pair().unwrap();
        write_msg(
            &mut agent_end,
            &FromAgent::Hello {
                outputs: 1,
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        let link = AgentLink::connect(server_end, 1).unwrap();
        let dir = std::env::temp_dir()
            .join(format!("rdsktp-relay-{}", std::process::id()))
            .join(rdsktp_spool::SOCKET_DIR);
        let spool = rdsktp_spool::PrinterSpool::new(dir.clone(), None).unwrap();
        let writer = Arc::new(Mutex::new(agent_end.try_clone().unwrap()));
        let printers = Arc::new(Printers::new(
            Arc::new(spool),
            Arc::new(move |m: &FromAgent| write_msg(&mut *writer.lock().unwrap(), m).is_ok()),
        ));
        // The agent's reader loop.
        let reader = printers.clone();
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = read_msg::<_, ToAgent>(&mut agent_end) {
                match msg {
                    ToAgent::PrinterAdded {
                        printer_id,
                        name,
                        driver,
                        is_default,
                        language,
                    } => reader.added(printer_id, name, driver, is_default, language),
                    ToAgent::PrinterRemoved { printer_id } => reader.removed(printer_id),
                    ToAgent::PrintReply { req_id, result } => {
                        reader.pending.complete(req_id, result)
                    }
                    _ => {}
                }
            }
        });

        let client = Arc::new(Client::default());
        let host = AgentLink::printer_host(&link);
        host.added(
            "Bureau",
            &PrinterInfo {
                name: "Bureau".into(),
                driver: "MS Publisher Imagesetter".into(),
                is_default: false,
                language: PrintLanguage::PostScript,
            },
            Arc::new(Printer(client.clone())),
        );
        let socket = dir.join("Bureau.sock");
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let data: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
        let mut s = UnixStream::connect(&socket).expect("the printer has a socket");
        s.write_all(&data).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        assert_eq!(answer.trim(), "OK");
        assert_eq!(client.0.lock().unwrap().as_slice(), [data]);

        // The printer goes away with its socket.
        host.removed("Bureau");
        for _ in 0..100 {
            if !socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
