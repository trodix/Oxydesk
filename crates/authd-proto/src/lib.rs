//! Protocole entre `rdsktp` (non privilégié) et `authd` (root : PAM + démarrage des sessions).
//!
//! Une requête = une connexion Unix. Trame : `[longueur: u32 big-endian][JSON]`. En cas de succès,
//! la réponse `Granted` transporte, par `SCM_RIGHTS`, un descripteur déjà connecté à l'agent de
//! session de l'utilisateur : aucun chemin de socket de l'utilisateur n'est exposé à `rdsktp`.

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
    #[error("message invalide : {0}")]
    Invalid(String),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Request {
    Login { username: String, password: String },
}

// Le mot de passe ne doit jamais apparaître dans un journal.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login { username, .. } => f.debug_struct("Login").field("username", username).field("password", &"***").finish(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Response {
    /// Accompagné d'un descripteur connecté à l'agent de session.
    Granted { uid: u32 },
    /// Refus d'authentification, sans détail (pas d'oracle sur l'existence des comptes).
    Denied,
    Error { message: String },
}

fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    let json = serde_json::to_vec(msg).map_err(|e| ProtoError::Invalid(e.to_string()))?;
    if json.len() > MAX_FRAME {
        return Err(ProtoError::Invalid("trame trop grande".into()));
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
        return Err(ProtoError::Invalid("trame trop grande".into()));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    let req: Request = decode(&body)?;
    let Request::Login { username, password } = &req;
    if username.len() > MAX_USERNAME || password.len() > MAX_PASSWORD {
        return Err(ProtoError::Invalid("identifiants trop longs".into()));
    }
    Ok(req)
}

/// Envoie la réponse ; `fd` (au plus un) est transmis avec le premier octet.
pub fn send_response(stream: &mut UnixStream, resp: &Response, fd: Option<RawFd>) -> Result<(), ProtoError> {
    let buf = encode(resp)?;
    match fd {
        None => stream.write_all(&buf)?,
        Some(fd) => {
            let fds = [fd];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            let sent = sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&buf)], &cmsg, MsgFlags::empty(), None)
                .map_err(|e| ProtoError::Io(e.into()))?;
            if sent < buf.len() {
                stream.write_all(&buf[sent..])?;
            }
        }
    }
    Ok(())
}

/// Reçoit la réponse et le descripteur éventuel (ouvert en `CLOEXEC`).
pub fn recv_response(stream: &mut UnixStream) -> Result<(Response, Option<OwnedFd>), ProtoError> {
    let mut buf = vec![0u8; 4 + MAX_FRAME];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
    let (got, fd) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut cmsg_space), MsgFlags::MSG_CMSG_CLOEXEC)
            .map_err(|e| ProtoError::Io(e.into()))?;
        let mut fd = None;
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(fds) = c {
                    for raw in fds {
                        // SAFETY: le noyau vient de créer ce descripteur pour ce processus ; on en prend la propriété.
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
        return Err(ProtoError::Invalid("connexion fermée sans réponse".into()));
    }
    // La longueur peut arriver en plusieurs morceaux : on complète par des lectures normales.
    let mut have = got;
    while have < 4 {
        let n = stream.read(&mut buf[have..4])?;
        if n == 0 {
            return Err(ProtoError::Invalid("réponse tronquée".into()));
        }
        have += n;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::Invalid("trame trop grande".into()));
    }
    if have < 4 + len {
        stream.read_exact(&mut buf[have..4 + len])?;
    }
    Ok((decode(&buf[4..4 + len])?, fd))
}

/// Nom d'utilisateur Unix raisonnable : refuse tout ce qui pourrait servir à injecter un nom d'unité
/// systemd ou un chemin (`/`, `@`, espaces, caractères de contrôle…).
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    ok_first && name.len() <= 32 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn request_roundtrip_and_password_not_in_debug() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Login { username: "alice".into(), password: "s3cret!".into() };
        send_request(&mut a, &req).unwrap();
        let got = read_request(&mut b).unwrap();
        assert_eq!(got, req);
        assert!(!format!("{got:?}").contains("s3cret"));
    }

    #[test]
    fn oversized_credentials_are_rejected() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::Login { username: "u".repeat(MAX_USERNAME + 1), password: "x".into() };
        send_request(&mut a, &req).unwrap();
        assert!(read_request(&mut b).is_err());
    }

    #[test]
    fn granted_response_carries_a_working_fd() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let (agent_side, mut agent_peer) = UnixStream::pair().unwrap();
        send_response(&mut server, &Response::Granted { uid: 1000 }, Some(agent_side.as_fd().as_raw_fd())).unwrap();
        let (resp, fd) = recv_response(&mut client).unwrap();
        assert_eq!(resp, Response::Granted { uid: 1000 });
        let mut passed = UnixStream::from(fd.expect("descripteur transmis"));
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
        for bad in ["", "Alice", "a b", "a/b", "a@b", "-x", "9x", "a\nb", "../x", &"a".repeat(33)] {
            assert!(!valid_username(bad), "{bad:?}");
        }
    }
}
