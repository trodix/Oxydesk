//! Adaptateur du port `SessionBroker` : authentification et session de l'utilisateur déléguées à
//! `rdsktp-authd` (PAM, root) et à `rdsktp-agent` (sous l'identité de l'utilisateur), ce processus
//! n'ayant aucun privilège. Les ports de bureau (`CaptureSource`, `InputSink`, …) sont des mandataires
//! vers l'agent ; la vidéo arrive déjà encodée.

mod link;
mod ports;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use rdsktp_authd_proto::{recv_response, send_request, Request, Response};
use rdsktp_core::error::LoginError;
use rdsktp_core::ports::{DesktopPorts, SessionBroker};

pub use link::AgentLink;

pub struct RemoteBroker {
    authd_socket: PathBuf,
    /// Nombre de sorties attendu (disposition fixée par la configuration du serveur).
    outputs: u32,
}

impl RemoteBroker {
    pub fn new(authd_socket: impl Into<PathBuf>, outputs: u32) -> Self {
        Self { authd_socket: authd_socket.into(), outputs }
    }
}

impl SessionBroker for RemoteBroker {
    fn login(&self, username: &str, password: &str) -> Result<DesktopPorts, LoginError> {
        let backend = |e: &dyn std::fmt::Display| LoginError::Backend(e.to_string());
        let mut authd = UnixStream::connect(&self.authd_socket).map_err(|e| backend(&format!("authd injoignable : {e}")))?;
        // Le démarrage de KWin peut prendre du temps ; authd borne lui-même l'attente.
        authd.set_read_timeout(Some(Duration::from_secs(90))).map_err(|e| backend(&e))?;
        send_request(&mut authd, &Request::Login { username: username.into(), password: password.into() }).map_err(|e| backend(&e))?;
        let (resp, fd) = recv_response(&mut authd).map_err(|e| backend(&e))?;
        match resp {
            Response::Denied => Err(LoginError::Denied),
            Response::Error { message } => Err(LoginError::Backend(message)),
            Response::Granted { uid } => {
                let fd = fd.ok_or_else(|| LoginError::Backend("authd n'a pas transmis de descripteur".into()))?;
                tracing::info!(user = username, uid, "session accordée par authd");
                let link = AgentLink::connect(UnixStream::from(fd), self.outputs).map_err(LoginError::Backend)?;
                Ok(ports::desktop_ports(link, self.outputs))
            }
        }
    }
}
