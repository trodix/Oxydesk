//! Printers redirected by the client, for a session served by an agent: the agent makes their sockets and
//! CUPS queues in the user's session (it runs as that user, the server cannot) and sends the jobs through the
//! link, block after block (the agent waits for each answer before the next block of a job).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use oxydesk_agent_proto::{PrintOp, PrintResult, ToAgent};
use oxydesk_core::error::PrintError;
use oxydesk_core::{PrintFormat, PrintJob, PrintLanguage, PrinterHost, PrinterInfo, RemotePrinter};

use crate::link::AgentLink;

/// The `format` of `ToAgent::PrinterAdded`.
fn format_code(f: PrintFormat) -> u8 {
    match f {
        PrintFormat::Raw(PrintLanguage::PostScript) => 0,
        PrintFormat::Raw(PrintLanguage::Pcl) => 1,
        PrintFormat::Raw(PrintLanguage::Text) => 2,
        PrintFormat::Xps => 3,
    }
}

struct Registered {
    name: String,
    printer: Arc<dyn RemotePrinter>,
    jobs: Mutex<HashMap<u64, Box<dyn PrintJob>>>,
    next_job: std::sync::atomic::AtomicU64,
}

/// The printers of the connection, as the agent knows them.
#[derive(Default)]
pub(crate) struct Printers {
    inner: Mutex<(u32, HashMap<u32, Arc<Registered>>)>,
}

impl Printers {
    fn get(&self, id: u32) -> Option<Arc<Registered>> {
        self.inner
            .lock()
            .expect("printers mutex")
            .1
            .get(&id)
            .cloned()
    }

    fn add(&self, name: &str, printer: Arc<dyn RemotePrinter>) -> u32 {
        let mut g = self.inner.lock().expect("printers mutex");
        g.0 += 1;
        let id = g.0;
        g.1.insert(
            id,
            Arc::new(Registered {
                name: name.to_owned(),
                printer,
                jobs: Mutex::default(),
                next_job: 1.into(),
            }),
        );
        id
    }

    fn remove(&self, name: &str) -> Option<u32> {
        let mut g = self.inner.lock().expect("printers mutex");
        let id =
            g.1.iter()
                .find(|(_, r)| r.name == name)
                .map(|(id, _)| *id)?;
        let gone = g.1.remove(&id)?;
        drop(g);
        // The jobs in progress are dropped: the client drops them too.
        let jobs: Vec<_> = gone.jobs.lock().expect("jobs mutex").drain().collect();
        for (_, job) in jobs {
            job.abort();
        }
        Some(id)
    }
}

fn error(e: PrintError) -> PrintResult {
    match e {
        PrintError::Refused(m) => PrintResult::Refused(m),
        PrintError::Io(m) => PrintResult::Io(m),
    }
}

/// Runs the operation asked by the agent on the client's printer. Blocking: not for the reader thread.
pub(crate) fn execute(link: &AgentLink, printer_id: u32, op: PrintOp) -> PrintResult {
    let Some(printer) = link.printers.get(printer_id) else {
        return PrintResult::Refused("unknown printer".into());
    };
    // The agent cannot make this side relay more than one block at a time.
    if matches!(&op, PrintOp::Write { data, .. } if data.len() > oxydesk_core::printer::MAX_PRINT_BLOCK as usize)
    {
        return PrintResult::Refused("block too large".into());
    }
    let job = |id: u64| printer.jobs.lock().expect("jobs mutex").remove(&id);
    match op {
        PrintOp::Begin { title } => match printer.printer.begin(&title) {
            Ok(job) => {
                let id = printer
                    .next_job
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                printer.jobs.lock().expect("jobs mutex").insert(id, job);
                PrintResult::Job(id)
            }
            Err(e) => error(e),
        },
        PrintOp::Write { job: id, data } => {
            // Taken out of the table while writing: no other call can touch the job meanwhile.
            let Some(mut j) = job(id) else {
                return PrintResult::Refused("unknown job".into());
            };
            match j.write(&data) {
                Ok(()) => {
                    printer.jobs.lock().expect("jobs mutex").insert(id, j);
                    PrintResult::Ok
                }
                Err(e) => {
                    j.abort();
                    error(e)
                }
            }
        }
        PrintOp::Finish(id) => match job(id) {
            Some(j) => j.finish().map_or_else(error, |()| PrintResult::Ok),
            None => PrintResult::Refused("unknown job".into()),
        },
        PrintOp::Abort(id) => {
            if let Some(j) = job(id) {
                j.abort();
            }
            PrintResult::Ok
        }
    }
}

/// `PrinterHost` that hands the printers to the agent of the session.
pub struct AgentPrinters(pub(crate) Weak<AgentLink>);

impl PrinterHost for AgentPrinters {
    fn added(&self, name: &str, info: &PrinterInfo, printer: Arc<dyn RemotePrinter>) {
        let Some(link) = self.0.upgrade() else { return };
        let printer_id = link.printers.add(name, printer);
        link.send(&ToAgent::PrinterAdded {
            printer_id,
            name: name.to_owned(),
            driver: info.driver.clone(),
            is_default: info.is_default,
            format: format_code(info.format),
        });
    }

    fn removed(&self, name: &str) {
        let Some(link) = self.0.upgrade() else { return };
        if let Some(printer_id) = link.printers.remove(name) {
            link.send(&ToAgent::PrinterRemoved { printer_id });
        }
    }
}
