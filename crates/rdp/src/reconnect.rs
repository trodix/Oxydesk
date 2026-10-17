//! Auto-Reconnect cookies (MS-RDPBCGR 2.2.4.2 and 5.5): after a network loss the client presents the cookie the
//! server gave it and resumes its session without typing the password again.
//!
//! The server keeps one secret per login. The client proves it holds it with `HMAC-MD5(secret, 32 zero bytes)`
//! (with TLS there is no client random). A cookie is single use, belongs to one address, and is only valid for
//! `window` after the connection was lost; a connection the user closed on purpose leaves no cookie behind.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use ironrdp_pdu::rdp::session_info::ServerAutoReconnect;
use md5::Md5;

/// Size of the cookie in the Client Info: length, version, logon id, security verifier.
pub const COOKIE_LEN: usize = 28;
const VERSION: u32 = 1;
/// Cookies kept at once (one per logged-in user, in practice).
const MAX_ENTRIES: usize = 32;

fn hmac_md5(key: &[u8], data: &[u8]) -> Hmac<Md5> {
    let mut mac = <Hmac<Md5> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac
}

/// The security verifier the client sends for `secret` over TLS.
#[cfg(test)]
pub(crate) fn verifier(secret: &[u8; 16]) -> [u8; 16] {
    hmac_md5(secret, &[0; 32]).finalize().into_bytes().into()
}

fn random<const N: usize>() -> [u8; N] {
    use std::io::Read as _;
    let mut out = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut out))
        .expect("the kernel random source is readable");
    out
}

struct Entry {
    user: String,
    secret: [u8; 16],
    ip: Option<IpAddr>,
    /// When the connection holding the cookie was lost; `None` while it is up.
    lost_at: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    /// The client holds a live cookie: its user's session is resumed.
    Valid(String),
    /// A cookie was presented but cannot be honoured (unknown, expired, other address, wrong proof).
    Refused(&'static str),
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u32, Entry>,
    /// Cookie of the connection being served.
    current: Option<u32>,
}

pub struct Reconnect {
    window: Duration,
    inner: Mutex<Inner>,
}

impl Reconnect {
    /// `window`: how long after a loss the client may come back; zero turns auto-reconnect off.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            inner: Mutex::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.window.is_zero()
    }

    /// New cookie for a connection that just logged `user` in (from `ip`); it replaces the user's previous ones.
    pub fn issue(&self, user: &str, ip: Option<IpAddr>, now: Instant) -> ServerAutoReconnect {
        self.issue_with(user, ip, now, u32::from_le_bytes(random()), random())
    }

    fn issue_with(
        &self,
        user: &str,
        ip: Option<IpAddr>,
        now: Instant,
        logon_id: u32,
        secret: [u8; 16],
    ) -> ServerAutoReconnect {
        let mut inner = self.inner.lock().expect("reconnect mutex");
        inner.purge(self.window, now);
        inner.entries.retain(|_, e| e.user != user);
        if inner.entries.len() >= MAX_ENTRIES {
            let oldest = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.lost_at)
                .map(|(id, _)| *id);
            if let Some(id) = oldest {
                inner.entries.remove(&id);
            }
        }
        inner.entries.insert(
            logon_id,
            Entry {
                user: user.to_owned(),
                secret,
                ip,
                lost_at: None,
            },
        );
        inner.current = Some(logon_id);
        ServerAutoReconnect {
            logon_id,
            random_bits: secret,
        }
    }

    /// Checks the cookie of a connecting client; a valid one is consumed.
    pub fn resume(&self, cookie: &[u8; COOKIE_LEN], ip: Option<IpAddr>, now: Instant) -> Resume {
        let word = |at: usize| u32::from_le_bytes(cookie[at..at + 4].try_into().expect("4 bytes"));
        if word(0) != COOKIE_LEN as u32 || word(4) != VERSION {
            return Resume::Refused("malformed cookie");
        }
        let logon_id = word(8);
        let mut inner = self.inner.lock().expect("reconnect mutex");
        inner.purge(self.window, now);
        let Some(entry) = inner.entries.get(&logon_id) else {
            return Resume::Refused("unknown or expired cookie");
        };
        if entry.ip != ip {
            return Resume::Refused("cookie from another address");
        }
        if hmac_md5(&entry.secret, &[0; 32])
            .verify_slice(&cookie[12..])
            .is_err()
        {
            return Resume::Refused("wrong proof");
        }
        let entry = inner.entries.remove(&logon_id).expect("found above");
        Resume::Valid(entry.user)
    }

    /// The connection being served ended. A deliberate end (the user disconnected or logged off) forgets the
    /// cookie; a loss keeps it for `window`.
    pub fn connection_ended(&self, deliberate: bool, now: Instant) {
        let mut inner = self.inner.lock().expect("reconnect mutex");
        let Some(id) = inner.current.take() else {
            return;
        };
        if deliberate {
            inner.entries.remove(&id);
        } else if let Some(e) = inner.entries.get_mut(&id) {
            e.lost_at.get_or_insert(now);
        }
    }
}

impl Inner {
    fn purge(&mut self, window: Duration, now: Instant) {
        self.entries
            .retain(|_, e| e.lost_at.is_none_or(|t| now.duration_since(t) <= window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: u32 = 0x1234_5678;
    const SECRET: [u8; 16] = *b"0123456789abcdef";

    fn cookie(logon_id: u32, secret: &[u8; 16]) -> [u8; COOKIE_LEN] {
        let mut c = [0u8; COOKIE_LEN];
        c[0..4].copy_from_slice(&(COOKIE_LEN as u32).to_le_bytes());
        c[4..8].copy_from_slice(&VERSION.to_le_bytes());
        c[8..12].copy_from_slice(&logon_id.to_le_bytes());
        c[12..].copy_from_slice(&verifier(secret));
        c
    }

    fn ip(n: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, n]))
    }

    fn issued(window_secs: u64) -> (Reconnect, Instant) {
        let now = Instant::now();
        let r = Reconnect::new(Duration::from_secs(window_secs));
        let sent = r.issue_with("alice", ip(1), now, ID, SECRET);
        assert_eq!((sent.logon_id, sent.random_bits), (ID, SECRET));
        (r, now)
    }

    #[test]
    fn hmac_md5_matches_the_reference_vector() {
        // RFC 2202, test case 1.
        let tag = hmac_md5(&[0x0b; 16], b"Hi There").finalize().into_bytes();
        assert_eq!(
            tag.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "9294727a3638bb1c13f48ef8158bfc9d"
        );
    }

    #[test]
    fn a_lost_connection_resumes_within_the_window_and_only_once() {
        let (r, t0) = issued(10);
        r.connection_ended(false, t0);
        let c = cookie(ID, &SECRET);
        let t = t0 + Duration::from_secs(9);
        assert_eq!(r.resume(&c, ip(1), t), Resume::Valid("alice".into()));
        assert!(matches!(r.resume(&c, ip(1), t), Resume::Refused(_)));
    }

    #[test]
    fn the_window_counts_from_the_loss() {
        let (r, t0) = issued(10);
        // Up for an hour, then lost: the clock starts at the loss, not at the login.
        let lost = t0 + Duration::from_secs(3600);
        r.connection_ended(false, lost);
        let c = cookie(ID, &SECRET);
        assert!(matches!(
            r.resume(&c, ip(1), lost + Duration::from_secs(11)),
            Resume::Refused(_)
        ));
        let (r, t0) = issued(10);
        r.connection_ended(false, t0);
        assert!(matches!(
            r.resume(&c, ip(1), t0 + Duration::from_secs(10)),
            Resume::Valid(_)
        ));
    }

    #[test]
    fn a_deliberate_disconnection_leaves_no_cookie() {
        let (r, t0) = issued(10);
        r.connection_ended(true, t0);
        assert!(matches!(
            r.resume(&cookie(ID, &SECRET), ip(1), t0),
            Resume::Refused(_)
        ));
    }

    #[test]
    fn the_cookie_is_bound_to_its_address_and_its_secret() {
        let (r, t0) = issued(10);
        r.connection_ended(false, t0);
        assert_eq!(
            r.resume(&cookie(ID, &SECRET), ip(2), t0),
            Resume::Refused("cookie from another address")
        );
        assert_eq!(
            r.resume(&cookie(ID, b"fedcba9876543210"), ip(1), t0),
            Resume::Refused("wrong proof")
        );
        assert_eq!(
            r.resume(&cookie(ID + 1, &SECRET), ip(1), t0),
            Resume::Refused("unknown or expired cookie")
        );
        // The refusals did not burn the cookie.
        assert!(matches!(
            r.resume(&cookie(ID, &SECRET), ip(1), t0),
            Resume::Valid(_)
        ));
    }

    #[test]
    fn a_malformed_cookie_is_refused() {
        let (r, t0) = issued(10);
        let mut c = cookie(ID, &SECRET);
        c[4] = 2;
        assert_eq!(r.resume(&c, ip(1), t0), Resume::Refused("malformed cookie"));
    }

    #[test]
    fn a_new_login_replaces_the_cookie_of_the_same_user() {
        let (r, t0) = issued(10);
        r.issue_with("alice", ip(1), t0, ID + 1, *b"another-secret!!");
        r.connection_ended(false, t0);
        assert!(matches!(
            r.resume(&cookie(ID, &SECRET), ip(1), t0),
            Resume::Refused(_)
        ));
        assert!(matches!(
            r.resume(&cookie(ID + 1, b"another-secret!!"), ip(1), t0),
            Resume::Valid(_)
        ));
    }

    #[test]
    fn a_connection_that_never_logged_in_does_not_start_a_clock() {
        let (r, t0) = issued(10);
        r.connection_ended(false, t0);
        // A failed attempt in between ends without a cookie of its own: the first one keeps its original loss time.
        r.connection_ended(false, t0 + Duration::from_secs(8));
        assert!(matches!(
            r.resume(&cookie(ID, &SECRET), ip(1), t0 + Duration::from_secs(11)),
            Resume::Refused(_)
        ));
    }

    #[test]
    fn real_cookies_are_random() {
        let r = Reconnect::new(Duration::from_secs(10));
        let a = r.issue("alice", None, Instant::now());
        let b = r.issue("bob", None, Instant::now());
        assert_ne!(a.random_bits, b.random_bits);
        assert_ne!(a.random_bits, [0; 16]);
    }
}
