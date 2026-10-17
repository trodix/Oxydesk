//! Adapter of the `SessionBroker` port: authentication and user session delegated to
//! `rdsktp-authd` (PAM, root) and `rdsktp-agent` (as the user), this process
//! having no privilege. The desktop ports (`CaptureSource`, `InputSink`, …) are proxies
//! to the agent; video arrives already encoded.

mod drives;
mod link;
mod ports;
mod printers;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use rdsktp_authd_proto::{recv_response, send_request, Request, Response};
use rdsktp_core::error::LoginError;
use rdsktp_core::ports::{DesktopPorts, SessionBroker};

pub use drives::AgentDrives;
pub use link::AgentLink;
pub use printers::AgentPrinters;

pub struct RemoteBroker {
    authd_socket: PathBuf,
    /// Number of outputs when the client does not announce its monitors (server configuration).
    outputs: u32,
}

impl RemoteBroker {
    pub fn new(authd_socket: impl Into<PathBuf>, outputs: u32) -> Self {
        Self {
            authd_socket: authd_socket.into(),
            outputs,
        }
    }
}

impl SessionBroker for RemoteBroker {
    fn login(
        &self,
        username: &str,
        password: &str,
        outputs: Option<u32>,
    ) -> Result<DesktopPorts, LoginError> {
        let outputs = outputs.map_or(self.outputs, |n| {
            n.clamp(1, rdsktp_core::layout::MAX_OUTPUTS as u32)
        });
        let backend = |e: &dyn std::fmt::Display| LoginError::Backend(e.to_string());
        let mut authd = UnixStream::connect(&self.authd_socket)
            .map_err(|e| backend(&format!("authd unreachable: {e}")))?;
        // KWin startup may take time; authd bounds the wait itself.
        authd
            .set_read_timeout(Some(Duration::from_secs(90)))
            .map_err(|e| backend(&e))?;
        send_request(
            &mut authd,
            &Request::Login {
                username: username.into(),
                password: password.into(),
                outputs,
            },
        )
        .map_err(|e| backend(&e))?;
        let (resp, fd) = recv_response(&mut authd).map_err(|e| backend(&e))?;
        match resp {
            Response::Denied => Err(LoginError::Denied),
            Response::Error { message } => Err(LoginError::Backend(message)),
            Response::Granted { uid } => {
                let fd = fd
                    .ok_or_else(|| LoginError::Backend("authd did not pass a descriptor".into()))?;
                tracing::info!(user = username, uid, "session granted by authd");
                let link = AgentLink::connect(UnixStream::from(fd), outputs)
                    .map_err(LoginError::Backend)?;
                Ok(ports::desktop_ports(link, outputs))
            }
        }
    }
}
