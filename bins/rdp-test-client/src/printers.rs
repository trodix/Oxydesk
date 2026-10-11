//! `printers` scenario: the client redirects a printer; a job written to the socket the desktop makes for
//! it (one connection = one job) must come out, byte for byte, on the client's printer. The recording of the
//! client's printer is a directory of files (`record`): the test client writes its jobs there, a real client
//! prints through a CUPS backend that records them there.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::report::Report;

#[derive(Debug, Clone)]
pub struct PrintersCheck {
    /// Socket of the printer on the desktop side.
    pub socket: PathBuf,
    /// Directory where the client's printer records the jobs it receives.
    pub record: PathBuf,
    /// Size in KiB of the big job.
    pub kib: u64,
}

fn payload(kib: u64) -> Vec<u8> {
    let mut out = b"%!PS-RDSKTP bonjour de la session \xc3\xa9\xc3\xa0\n".to_vec();
    out.extend((0..kib * 1024).map(|i| (i.wrapping_mul(31).wrapping_add(7) % 251) as u8));
    out
}

/// Sends `data` as one job; returns the answer of the spool.
fn submit(socket: &Path, data: &[u8]) -> Result<(), String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    s.write_all(data).map_err(|e| format!("send: {e}"))?;
    s.shutdown(std::net::Shutdown::Write)
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut answer = String::new();
    s.read_to_string(&mut answer)
        .map_err(|e| format!("answer: {e}"))?;
    if answer.trim() == "OK" {
        Ok(())
    } else {
        Err(format!("the spool answered {:?}", answer.trim()))
    }
}

/// Waits for a file of `record` with exactly `data`.
fn recorded(record: &Path, data: &[u8], timeout: Duration) -> Result<String, String> {
    let end = Instant::now() + timeout;
    loop {
        let mut seen = Vec::new();
        if let Ok(rd) = std::fs::read_dir(record) {
            for e in rd.flatten() {
                if let Ok(got) = std::fs::read(e.path()) {
                    if got == data {
                        return Ok(e.file_name().to_string_lossy().into_owned());
                    }
                    seen.push(format!(
                        "{} ({} bytes)",
                        e.file_name().to_string_lossy(),
                        got.len()
                    ));
                }
            }
        }
        if Instant::now() >= end {
            return Err(format!(
                "no recorded job with the {} bytes sent; found: {seen:?}",
                data.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn printers(c: &PrintersCheck, timeout: Duration, r: &mut Report) {
    let timeout = timeout.max(Duration::from_secs(20));
    // The printer is announced after the connection: the socket appears a moment later.
    let end = Instant::now() + timeout;
    while !c.socket.exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let run = || -> Result<String, String> {
        let small = b"Bonjour imprimante\n".to_vec();
        submit(&c.socket, &small)?;
        recorded(&c.record, &small, timeout)?;
        let big = payload(c.kib);
        let started = Instant::now();
        submit(&c.socket, &big)?;
        recorded(&c.record, &big, timeout)?;
        // Several jobs in a row on the same printer.
        for i in 0..3 {
            let job = format!("travail {i}\n").into_bytes();
            submit(&c.socket, &job)?;
            recorded(&c.record, &job, timeout)?;
        }
        Ok(format!(
            "5 jobs, {} KiB in {:.1} s",
            c.kib,
            started.elapsed().as_secs_f64()
        ))
    };
    match run() {
        Ok(detail) => r.check("printers", true, detail),
        Err(e) => r.check("printers", false, e),
    }
}
