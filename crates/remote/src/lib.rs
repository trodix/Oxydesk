//! Adapter of the `SessionBroker` port: authentication and user session delegated to
//! `oxydesk-authd` (PAM, root) and `oxydesk-agent` (as the user), this process
//! having no privilege. The desktop ports (`CaptureSource`, `InputSink`, …) are proxies
//! to the agent; video arrives already encoded.

mod drives;
mod link;
mod ports;
mod printers;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use oxydesk_authd_proto::{recv_response, send_request, Request, Response, SessionPolicy};
use oxydesk_core::error::LoginError;
use oxydesk_core::ports::{DesktopPorts, SessionBroker};

pub use drives::AgentDrives;
pub use link::AgentLink;
pub use oxydesk_authd_proto::SessionPolicy as Policy;
pub use printers::AgentPrinters;

pub struct RemoteBroker {
    authd_socket: PathBuf,
    /// Number of outputs when the client does not announce its monitors (server configuration).
    outputs: u32,
    /// Whether a login joins the user's existing Plasma session or starts one (server configuration).
    policy: SessionPolicy,
    /// Whether the extra monitors of a client that joined a session stay when it leaves (server configuration).
    keep_virtual_outputs: bool,
}

impl RemoteBroker {
    pub fn new(authd_socket: impl Into<PathBuf>, outputs: u32) -> Self {
        Self {
            authd_socket: authd_socket.into(),
            outputs,
            policy: SessionPolicy::default(),
            keep_virtual_outputs: true,
        }
    }

    pub fn with_virtual_outputs_kept(mut self, keep: bool) -> Self {
        self.keep_virtual_outputs = keep;
        self
    }

    pub fn with_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = policy;
        self
    }
}

impl RemoteBroker {
    /// One request to `authd`, then the agent link it hands over.
    fn session(&self, request: &Request, outputs: u32) -> Result<DesktopPorts, LoginError> {
        let backend = |e: &dyn std::fmt::Display| LoginError::Backend(e.to_string());
        let mut authd = UnixStream::connect(&self.authd_socket)
            .map_err(|e| backend(&format!("authd unreachable: {e}")))?;
        // KWin startup may take time; authd bounds the wait itself.
        authd
            .set_read_timeout(Some(Duration::from_secs(90)))
            .map_err(|e| backend(&e))?;
        send_request(&mut authd, request).map_err(|e| backend(&e))?;
        let (resp, fd) = recv_response(&mut authd).map_err(|e| backend(&e))?;
        match resp {
            Response::Denied => Err(LoginError::Denied),
            Response::Error { message } => Err(LoginError::Backend(message)),
            Response::Granted { uid } => {
                let fd = fd
                    .ok_or_else(|| LoginError::Backend("authd did not pass a descriptor".into()))?;
                tracing::info!(uid, "session granted by authd");
                let link = AgentLink::connect(UnixStream::from(fd), outputs)
                    .map_err(LoginError::Backend)?;
                Ok(ports::desktop_ports(link, outputs))
            }
        }
    }

    fn outputs_for(&self, outputs: Option<u32>) -> u32 {
        outputs.map_or(self.outputs, |n| {
            n.clamp(1, oxydesk_core::layout::MAX_OUTPUTS as u32)
        })
    }
}

impl SessionBroker for RemoteBroker {
    fn login(
        &self,
        username: &str,
        password: &str,
        outputs: Option<u32>,
    ) -> Result<DesktopPorts, LoginError> {
        let outputs = self.outputs_for(outputs);
        self.session(
            &Request::Login {
                username: username.into(),
                password: password.into(),
                outputs,
                policy: self.policy,
                keep_virtual_outputs: self.keep_virtual_outputs,
            },
            outputs,
        )
    }

    fn attach(&self, username: &str, outputs: Option<u32>) -> Result<DesktopPorts, LoginError> {
        let outputs = self.outputs_for(outputs);
        self.session(
            &Request::Attach {
                username: username.into(),
                outputs,
            },
            outputs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn broker_with_authd(answer: Response) -> (RemoteBroker, std::thread::JoinHandle<Request>) {
        let dir = std::env::temp_dir().join(format!(
            "oxydesk-remote-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("authd.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let request = oxydesk_authd_proto::read_request(&mut conn).unwrap();
            oxydesk_authd_proto::send_response(&mut conn, &answer, None).unwrap();
            request
        });
        (RemoteBroker::new(path, 1), server)
    }

    #[test]
    fn attach_sends_an_attach_request_without_password_and_maps_a_refusal() {
        let (broker, authd) = broker_with_authd(Response::Denied);
        let result = broker.attach("alice", Some(3));
        assert!(matches!(result, Err(LoginError::Denied)));
        assert_eq!(
            authd.join().unwrap(),
            Request::Attach {
                username: "alice".into(),
                outputs: 3
            }
        );
    }

    #[test]
    fn login_carries_the_session_policy_of_the_server() {
        let (broker, authd) = broker_with_authd(Response::Denied);
        let broker = broker
            .with_policy(SessionPolicy::Auto)
            .with_virtual_outputs_kept(true);
        assert!(matches!(
            broker.login("alice", "pw", Some(2)),
            Err(LoginError::Denied)
        ));
        assert_eq!(
            authd.join().unwrap(),
            Request::Login {
                username: "alice".into(),
                password: "pw".into(),
                outputs: 2,
                policy: SessionPolicy::Auto,
                keep_virtual_outputs: true,
            }
        );
        // Without a setting, a login joins the user's session if there is one, else starts one.
        let (broker, authd) = broker_with_authd(Response::Denied);
        let _ = broker.login("bob", "pw", None);
        assert!(matches!(
            authd.join().unwrap(),
            Request::Login {
                policy: SessionPolicy::Auto,
                keep_virtual_outputs: true,
                ..
            }
        ));
    }

    #[test]
    fn attach_reports_an_authd_error_and_an_unreachable_authd() {
        let (broker, authd) = broker_with_authd(Response::Error {
            message: "no agent".into(),
        });
        assert!(matches!(
            broker.attach("alice", None),
            Err(LoginError::Backend(m)) if m == "no agent"
        ));
        authd.join().unwrap();
        let nowhere = RemoteBroker::new("/nonexistent/authd.sock", 1);
        assert!(matches!(
            nowhere.attach("alice", None),
            Err(LoginError::Backend(m)) if m.contains("authd unreachable")
        ));
    }
}
