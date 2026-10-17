//! CUPS backend `oxydesk`: copies a print job to the socket of a printer of an RDP client.
//!
//! Install as `/usr/lib/cups/backend/oxydesk`, owner root, mode 0700 (CUPS then runs it as root: the socket
//! belongs to the user of the session, in a directory only that user can enter). Device URI:
//! `oxydesk:/run/user/<uid>/oxydesk-printers/<printer>.sock`. The socket must be one the spool made for the user
//! who submitted the job, so a queue cannot be used to reach any other socket with the rights of the backend.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

/// `CUPS_BACKEND_FAILED`: the job is retried or the queue stopped, per the error policy of the queue.
const FAILED: u8 = 1;
/// `CUPS_BACKEND_CANCEL`: the job is dropped (the printer is gone with the client).
const CANCEL: u8 = 5;

fn uid_of(user: &str) -> Option<u32> {
    let name = CString::new(user).ok()?;
    // SAFETY: `getpwnam` reads a NUL-terminated string; the result is read at once, before any other call.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    // SAFETY: a non-null pointer from `getpwnam` points to a valid `passwd`.
    (!entry.is_null()).then(|| unsafe { (*entry).pw_uid })
}

fn socket_of(uri: &str) -> Option<&Path> {
    uri.strip_prefix("oxydesk:").map(Path::new)
}

/// Copies the job to the socket and waits for the answer of the spool: `OK` or `ERROR: reason`.
fn send(mut source: impl Read, socket: &Path) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    let copied = io::copy(&mut source, &mut stream).and_then(|_| stream.flush());
    // If the spool gave up midway it has already said why: its answer is read in both cases.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    match answer.trim() {
        "OK" => Ok(()),
        "" => Err(copied.err().map_or_else(
            || "the spool closed the connection without an answer".into(),
            |e| e.to_string(),
        )),
        error => Err(error.trim_start_matches("ERROR: ").to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 {
        println!("direct oxydesk \"Unknown\" \"Printer of an RDP client\"");
        return ExitCode::SUCCESS;
    }
    if args.len() != 6 && args.len() != 7 {
        eprintln!("Usage: oxydesk job-id user title copies options [file]");
        return ExitCode::from(FAILED);
    }
    let Ok(uri) = std::env::var("DEVICE_URI") else {
        eprintln!("ERROR: no DEVICE_URI");
        return ExitCode::from(FAILED);
    };
    let Some(socket) = socket_of(&uri) else {
        eprintln!("ERROR: not a oxydesk device: {uri}");
        return ExitCode::from(FAILED);
    };
    let Some(uid) = uid_of(&args[2]) else {
        eprintln!("ERROR: unknown user {}", args[2]);
        return ExitCode::from(FAILED);
    };
    if !socket.exists() {
        eprintln!("ERROR: the printer is no longer available (the RDP client is gone)");
        return ExitCode::from(CANCEL);
    }
    if !oxydesk_spool::is_spool_socket(socket, uid) {
        eprintln!(
            "ERROR: {} is not a printer socket of {}",
            socket.display(),
            args[2]
        );
        return ExitCode::from(FAILED);
    }
    eprintln!("INFO: sending the job to the RDP client");
    // A file argument is a raw job: the copies are made here, as the standard backends do.
    let result = match args.get(6) {
        Some(file) => {
            let copies = args[4].parse::<u32>().unwrap_or(1).max(1);
            (0..copies).try_for_each(|_| {
                File::open(file)
                    .map_err(|e| e.to_string())
                    .and_then(|f| send(f, socket))
            })
        }
        None => send(io::stdin().lock(), socket),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e}");
            ExitCode::from(CANCEL)
        }
    }
}
