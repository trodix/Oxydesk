//! Serves a fake card reader over the PC/SC protocol, to try real `libpcsclite` clients against.
//!
//! ```text
//! cargo run -p oxydesk-pcsc --example fake_reader -- /tmp/pcsc-fake/pcscd.comm
//! PCSCLITE_CSOCK_NAME=/tmp/pcsc-fake/pcscd.comm opensc-tool -l
//! PCSCLITE_CSOCK_NAME=/tmp/pcsc-fake/pcscd.comm opensc-tool -a
//! ```
//!
//! Commands read on the standard input (when it is not a terminal that is closed): `remove` and `insert` the
//! card, `detach` and `attach` the device, `quit`. Stops on `quit` or when killed. With `--cycle-ms N` the card
//! is removed and inserted back every N milliseconds.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oxydesk_core::smartcard::{RemoteSmartcard, SmartcardHost};
use oxydesk_pcsc::fake::FakeSmartcard;
use oxydesk_pcsc::PcscServer;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut cycle: Option<u64> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cycle-ms" => cycle = args.next().and_then(|v| v.parse().ok()),
            _ => socket = Some(PathBuf::from(a)),
        }
    }
    let Some(socket) = socket else {
        eprintln!("usage: fake_reader [--cycle-ms N] <socket path>");
        std::process::exit(2);
    };
    let server = match PcscServer::start(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot serve {}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    let card = Arc::new(FakeSmartcard::new());
    let backend: Arc<dyn RemoteSmartcard> = card.clone();
    server.attached(backend.clone());
    println!("serving {}", socket.display());

    if let Some(ms) = cycle {
        let card = card.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(ms));
            card.remove_card();
            std::thread::sleep(Duration::from_millis(ms));
            card.insert_card();
        });
    }

    let stdin = std::io::stdin();
    let mut quit = false;
    for line in stdin.lock().lines().map_while(Result::ok) {
        match line.trim() {
            "remove" => card.remove_card(),
            "insert" => card.insert_card(),
            "detach" => server.detached(),
            "attach" => server.attached(backend.clone()),
            "quit" => {
                quit = true;
                break;
            }
            "" => {}
            other => eprintln!("unknown command {other:?}"),
        }
        println!("ok {}", line.trim());
    }
    if !quit {
        // No (more) input: keep serving until killed.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    server.shutdown();
}
