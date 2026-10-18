//! Protocol between `oxydesk` (unprivileged) and `authd` (root: PAM + session startup).
//!
//! One request = one Unix connection. Frame: `[length: u32 big-endian][JSON]`. On success,
//! the `Granted` response carries, through `SCM_RIGHTS`, a descriptor already connected to the user's
//! session agent: no user socket path is exposed to `oxydesk`.

use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_USERNAME: usize = 256;
pub const MAX_PASSWORD: usize = 1024;
const MAX_FRAME: usize = 8 * 1024;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("E/S : {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid message: {0}")]
    Invalid(String),
}

fn default_keep() -> bool {
    true
}

/// What a successful login does about the desktop session of the user.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionPolicy {
    /// Always a session of our own (a private virtual KWin).
    #[default]
    New,
    /// Always the Plasma session the user already has; refused when there is none.
    Attach,
    /// The user's existing Plasma session if there is one, else a session of our own.
    Auto,
}

impl SessionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Attach => "attach",
            Self::Auto => "auto",
        }
    }
}

impl std::str::FromStr for SessionPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "new" => Ok(Self::New),
            "attach" => Ok(Self::Attach),
            "auto" => Ok(Self::Auto),
            other => Err(format!(
                "unknown session-policy {other:?} (new|attach|auto)"
            )),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Request {
    /// `outputs`: number of outputs wanted for a session started by this login (the client's monitors);
    /// 0 = not specified (the agent's configuration decides).
    Login {
        username: String,
        password: String,
        #[serde(default)]
        outputs: u32,
        /// Old clients do not send it: a session of our own, as before.
        #[serde(default)]
        policy: SessionPolicy,
        /// Joining a session: keep the client's extra monitors when it leaves (old clients: kept too).
        #[serde(default = "default_keep")]
        keep_virtual_outputs: bool,
    },
    /// Rejoins the running session of a user authenticated earlier by this `authd` (automatic reconnection):
    /// no password, no session started.
    Attach {
        username: String,
        #[serde(default)]
        outputs: u32,
    },
}

// The password must never appear in a log.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login {
                username,
                outputs,
                policy,
                keep_virtual_outputs,
                ..
            } => f
                .debug_struct("Login")
                .field("username", username)
                .field("password", &"***")
                .field("outputs", outputs)
                .field("policy", policy)
                .field("keep_virtual_outputs", keep_virtual_outputs)
                .finish(),
            Self::Attach { username, outputs } => f
                .debug_struct("Attach")
                .field("username", username)
                .field("outputs", outputs)
                .finish(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Response {
    /// Accompanied by a descriptor connected to the session agent.
    Granted {
        uid: u32,
    },
    /// Authentication refused, without detail (no oracle on account existence).
    Denied,
    Error {
        message: String,
    },
}

fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    let json = serde_json::to_vec(msg).map_err(|e| ProtoError::Invalid(e.to_string()))?;
    if json.len() > MAX_FRAME {
        return Err(ProtoError::Invalid("frame too large".into()));
    }
    let mut out = (json.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&json);
    Ok(out)
}

fn decode<T: for<'a> Deserialize<'a>>(body: &[u8]) -> Result<T, ProtoError> {
    serde_json::from_slice(body).map_err(|e| ProtoError::Invalid(e.to_string()))
}

pub fn send_request(stream: &mut UnixStream, req: &Request) -> Result<(), ProtoError> {
    stream.write_all(&encode(req)?)?;
    Ok(())
}

pub fn read_request(stream: &mut UnixStream) -> Result<Request, ProtoError> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::Invalid("frame too large".into()));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    let req: Request = decode(&body)?;
    let (username, password) = match &req {
        Request::Login {
            username, password, ..
        } => (username, password.len()),
        Request::Attach { username, .. } => (username, 0),
    };
    if username.len() > MAX_USERNAME || password > MAX_PASSWORD {
        return Err(ProtoError::Invalid("credentials too long".into()));
    }
    Ok(req)
}

/// Sends the response; `fd` (at most one) is passed with the first byte.
pub fn send_response(
    stream: &mut UnixStream,
    resp: &Response,
    fd: Option<RawFd>,
) -> Result<(), ProtoError> {
    let buf = encode(resp)?;
    match fd {
        None => stream.write_all(&buf)?,
        Some(fd) => {
            let fds = [fd];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            let sent = sendmsg::<()>(
                stream.as_raw_fd(),
                &[IoSlice::new(&buf)],
                &cmsg,
                MsgFlags::empty(),
                None,
            )
            .map_err(|e| ProtoError::Io(e.into()))?;
            if sent < buf.len() {
                stream.write_all(&buf[sent..])?;
            }
        }
    }
    Ok(())
}

/// Receives the response and the optional descriptor (opened with `CLOEXEC`).
pub fn recv_response(stream: &mut UnixStream) -> Result<(Response, Option<OwnedFd>), ProtoError> {
    let mut buf = vec![0u8; 4 + MAX_FRAME];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
    let (got, fd) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_space),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )
        .map_err(|e| ProtoError::Io(e.into()))?;
        let mut fd = None;
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(fds) = c {
                    for raw in fds {
                        // SAFETY: the kernel just created this descriptor for this process; we take ownership of it.
                        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                        if fd.is_none() {
                            fd = Some(owned);
                        }
                    }
                }
            }
        }
        (msg.bytes, fd)
    };
    if got == 0 {
        return Err(ProtoError::Invalid(
            "connection closed without a response".into(),
        ));
    }
    // The length may arrive in several chunks: complete it with normal reads.
    let mut have = got;
    while have < 4 {
        let n = stream.read(&mut buf[have..4])?;
        if n == 0 {
            return Err(ProtoError::Invalid("truncated response".into()));
        }
        have += n;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::Invalid("frame too large".into()));
    }
    if have < 4 + len {
        stream.read_exact(&mut buf[have..4 + len])?;
    }
    Ok((decode(&buf[4..4 + len])?, fd))
}

/// Reasonable Unix user name: rejects anything that could be used to inject a systemd unit name
/// or a path (`/`, `@`, spaces, control characters…).
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    let ok_first = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    ok_first
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn request_roundtrip_and_password_not_in_debug() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Login {
            username: "alice".into(),
            password: "s3cret!".into(),
            outputs: 2,
            policy: SessionPolicy::Auto,
            keep_virtual_outputs: true,
        };
        send_request(&mut a, &req).unwrap();
        let got = read_request(&mut b).unwrap();
        assert_eq!(got, req);
        assert!(!format!("{got:?}").contains("s3cret"));
    }

    #[test]
    fn a_request_without_outputs_still_decodes() {
        let json = br#"{"type":"Login","username":"alice","password":"x"}"#;
        let mut frame = (json.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(json);
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&frame).unwrap();
        assert!(matches!(
            read_request(&mut b).unwrap(),
            Request::Login {
                outputs: 0,
                policy: SessionPolicy::New,
                keep_virtual_outputs: true,
                ..
            }
        ));
    }

    #[test]
    fn the_policy_is_read_from_its_name_and_unknown_names_list_the_valid_ones() {
        for p in [
            SessionPolicy::New,
            SessionPolicy::Attach,
            SessionPolicy::Auto,
        ] {
            assert_eq!(p.as_str().parse(), Ok(p));
        }
        assert_eq!(SessionPolicy::default(), SessionPolicy::New);
        let e = "both".parse::<SessionPolicy>().unwrap_err();
        assert!(e.contains("both") && e.contains("new|attach|auto"));
    }

    #[test]
    fn oversized_credentials_are_rejected() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Login {
            username: "u".repeat(MAX_USERNAME + 1),
            password: "x".into(),
            outputs: 0,
            policy: SessionPolicy::New,
            keep_virtual_outputs: false,
        };
        send_request(&mut a, &req).unwrap();
        assert!(read_request(&mut b).is_err());
    }

    #[test]
    fn attach_request_roundtrips_and_carries_no_password() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Attach {
            username: "alice".into(),
            outputs: 2,
        };
        send_request(&mut a, &req).unwrap();
        assert_eq!(read_request(&mut b).unwrap(), req);
        assert!(!format!("{req:?}").contains("password"));
    }

    #[test]
    fn oversized_attach_username_is_rejected() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Attach {
            username: "u".repeat(MAX_USERNAME + 1),
            outputs: 1,
        };
        send_request(&mut a, &req).unwrap();
        assert!(read_request(&mut b).is_err());
    }

    #[test]
    fn granted_response_carries_a_working_fd() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let (agent_side, mut agent_peer) = UnixStream::pair().unwrap();
        send_response(
            &mut server,
            &Response::Granted { uid: 1000 },
            Some(agent_side.as_fd().as_raw_fd()),
        )
        .unwrap();
        let (resp, fd) = recv_response(&mut client).unwrap();
        assert_eq!(resp, Response::Granted { uid: 1000 });
        let mut passed = UnixStream::from(fd.expect("descriptor passed"));
        passed.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        agent_peer.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn denied_has_no_fd() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        send_response(&mut server, &Response::Denied, None).unwrap();
        let (resp, fd) = recv_response(&mut client).unwrap();
        assert_eq!(resp, Response::Denied);
        assert!(fd.is_none());
    }

    #[test]
    fn username_validation() {
        for ok in ["alice", "dev", "_apt", "a-b_c9"] {
            assert!(valid_username(ok), "{ok}");
        }
        for bad in [
            "",
            "Alice",
            "a b",
            "a/b",
            "a@b",
            "-x",
            "9x",
            "a\nb",
            "../x",
            &"a".repeat(33),
        ] {
            assert!(!valid_username(bad), "{bad:?}");
        }
    }
}
