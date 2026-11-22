//! A PC/SC service for the smart card readers redirected by the RDP client (adapter of `SmartcardHost`).
//!
//! [`PcscServer`] listens on a Unix socket and speaks the client/server protocol of pcsc-lite (what
//! `libpcsclite` speaks to `/run/pcscd/pcscd.comm`): the session sets `PCSCLITE_CSOCK_NAME` to the socket and
//! OpenSC, p11-kit, Firefox, Java... use the readers of the client as if they were local. Each request is
//! translated into calls on [`RemoteSmartcard`], the winscard-like port that the RDP side implements
//! (MS-RDPESC). The same design as the smart card support of xrdp.
//!
//! The reader table, the wire format and the state translation are pure ([`wire`], [`table`]); [`fake`] is a
//! scripted smart card subsystem for the tests and the `fake_reader` example.

mod conn;
pub mod fake;
mod ids;
mod monitor;
mod service;
mod socket;
pub mod table;
pub mod wire;

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use oxydesk_core::smartcard::{RemoteSmartcard, SmartcardHost};

pub use service::Options;
use service::{lock, Service, WRITE_TIMEOUT};

/// How long stopping the server waits for the client threads to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

/// Directory (under the runtime directory of the user) and name of the socket the session's applications find
/// through `PCSCLITE_CSOCK_NAME`. A fixed place, so that the environment of the session can name it before the
/// service exists.
pub const SOCKET_DIR: &str = "oxydesk-pcsc";
pub const SOCKET_NAME: &str = "pcsc.sock";

/// The socket of the session whose runtime directory is `runtime_dir` (`$XDG_RUNTIME_DIR`).
pub fn default_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(SOCKET_DIR).join(SOCKET_NAME)
}

/// The PC/SC socket of the session.
pub struct PcscServer {
    service: Arc<Service>,
    socket: PathBuf,
    /// A handle on the listening socket, to wake the thread that accepts when stopping.
    listener: UnixListener,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl PcscServer {
    /// Starts serving at `socket_path` (the parent directory is created with mode 0700 if missing, the socket is mode 0600, a stale
    /// socket is replaced, a symlink or another kind of file is refused). The service is up at once but has no reader until a device is
    /// attached (see the implementation of [`SmartcardHost`]).
    pub fn start(socket_path: &Path) -> io::Result<Arc<Self>> {
        Self::start_with(socket_path, Options::default())
    }

    /// [`start`](Self::start) with explicit [`Options`].
    pub fn start_with(socket_path: &Path, options: Options) -> io::Result<Arc<Self>> {
        let listener = socket::bind_private(socket_path)?;
        let service = Service::new(options);
        let for_thread = listener.try_clone()?;
        let accept = std::thread::Builder::new()
            .name("pcsc-accept".into())
            .spawn({
                let service = service.clone();
                move || accept_loop(&service, &for_thread)
            })?;
        tracing::info!(socket = %socket_path.display(), "PC/SC service listening");
        Ok(Arc::new(Self {
            service,
            socket: socket_path.to_path_buf(),
            listener,
            accept: Mutex::new(Some(accept)),
        }))
    }

    /// The socket to give to the applications (`PCSCLITE_CSOCK_NAME`).
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Stops the service: the device is detached, the clients are disconnected and the socket is removed.
    /// Idempotent; also done when the server is dropped.
    pub fn shutdown(&self) {
        let Some(thread) = lock(&self.accept).take() else {
            return;
        };
        self.service.shutdown(SHUTDOWN_WAIT);
        // Wakes the thread blocked in `accept`.
        // SAFETY: the descriptor is that of `self.listener`, which is open.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = thread.join();
        socket::remove_socket(&self.socket);
        tracing::info!(socket = %self.socket.display(), "PC/SC service stopped");
    }
}

impl Drop for PcscServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl SmartcardHost for PcscServer {
    fn attached(&self, smartcard: Arc<dyn RemoteSmartcard>) {
        self.service.attach(smartcard);
    }

    fn detached(&self) {
        self.service.detach();
    }
}

fn accept_loop(service: &Arc<Service>, listener: &UnixListener) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if service.is_stopping() {
                    break;
                }
                spawn_connection(service, stream);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                if service.is_stopping() {
                    break;
                }
                tracing::warn!("PC/SC accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn spawn_connection(service: &Arc<Service>, stream: UnixStream) {
    if !socket::peer_is_trusted(&stream) {
        tracing::warn!("PC/SC client of another user refused");
        return;
    }
    let (Ok(reader), Ok(()), Ok(writer)) = (
        stream.try_clone(),
        stream.set_write_timeout(Some(WRITE_TIMEOUT)),
        stream.try_clone(),
    ) else {
        return;
    };
    drop(stream);
    let Some(shared) = service.add_conn(writer) else {
        tracing::warn!("PC/SC: too many clients, one refused");
        return;
    };
    let id = shared.id;
    let service_for_thread = service.clone();
    let spawned = std::thread::Builder::new()
        .name("pcsc-client".into())
        .spawn(move || conn::serve(service_for_thread, shared, reader));
    if let Err(e) = spawned {
        tracing::warn!("PC/SC client thread not started: {e}");
        service.remove_conn(id);
    }
}
