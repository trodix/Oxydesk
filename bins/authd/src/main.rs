//! `rdsktp-authd`: the only privileged component. Authenticates through PAM, starts the user's session agent
//! (`rdsktp-agent@<user>.service`) and hands `rdsktp` a descriptor connected to that agent.
//! No secret in logs or in argv.

mod pam;

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pam::{Pam, PamOutcome};
use rdsktp_authd_proto::{read_request, send_response, valid_username, Request, Response};

const DEFAULT_SOCKET: &str = "/run/rdsktp/authd.sock";

struct Config {
    socket: PathBuf,
    pam_service: String,
    min_uid: u32,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

/// uid and gid of a local account via `getpwnam_r`.
fn lookup_uid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid buffer and structure for the duration of the call.
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut res) };
    (rc == 0 && !res.is_null()).then_some(pwd.pw_uid)
}

fn agent_socket(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/rdsktp-agent.sock"))
}

/// Starts the agent (idempotent) and waits for its socket to accept connections.
fn connect_agent(username: &str, uid: u32) -> anyhow::Result<UnixStream> {
    let unit = format!("rdsktp-agent@{username}.service");
    let status = Command::new("systemctl").args(["start", "--no-block", &unit]).status()?;
    anyhow::ensure!(status.success(), "systemctl start {unit}: {status}");
    let sock = agent_socket(uid);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if sock.exists() {
            if let Ok(s) = UnixStream::connect(&sock) {
                return Ok(s);
            }
        }
        // The unit may have failed (PAM, KWin): no point waiting until the end.
        let failed = Command::new("systemctl").args(["is-failed", "--quiet", &unit]).status().is_ok_and(|s| s.success());
        anyhow::ensure!(!failed, "unit {unit} failed");
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for the agent");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn handle(mut stream: UnixStream, cfg: &Config, pam: &Pam) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let Request::Login { username, password } = read_request(&mut stream)?;
    stream.set_read_timeout(None)?;
    tracing::info!(user = %username, "login request");
    let deny = |stream: &mut UnixStream| -> anyhow::Result<()> {
        // Slows down brute force; same answer for unknown account, wrong password or forbidden account.
        std::thread::sleep(Duration::from_secs(1));
        send_response(stream, &Response::Denied, None)?;
        Ok(())
    };
    if !valid_username(&username) {
        tracing::warn!("user name rejected (format)");
        return deny(&mut stream);
    }
    let uid = lookup_uid(&username);
    if pam.authenticate(&username, &password)? != PamOutcome::Granted || uid.is_none_or(|u| u < cfg.min_uid) {
        tracing::warn!(user = %username, "authentication refused");
        return deny(&mut stream);
    }
    let uid = uid.expect("checked above");
    match connect_agent(&username, uid) {
        Ok(agent) => {
            send_response(&mut stream, &Response::Granted { uid }, Some(agent.as_raw_fd()))?;
            tracing::info!(user = %username, uid, "session granted");
        }
        Err(e) => {
            tracing::error!(user = %username, "agent unavailable: {e:#}");
            send_response(&mut stream, &Response::Error { message: "session unavailable".into() }, None)?;
        }
    }
    Ok(())
}

fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Access restricted to root and the `rdsktp` group (owner of the `rdsktp` service).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = lookup_gid("rdsktp") {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: chemin valide.
        unsafe { libc::chown(c.as_ptr(), 0, gid) };
    }
    Ok(listener)
}

fn lookup_gid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::group = std::ptr::null_mut();
    // SAFETY: idem `lookup_uid`.
    let rc = unsafe { libc::getgrnam_r(cname.as_ptr(), &mut grp, buf.as_mut_ptr().cast(), buf.len(), &mut res) };
    (rc == 0 && !res.is_null()).then_some(grp.gr_gid)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Arc::new(Config {
        socket: arg(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCKET.into()).into(),
        pam_service: arg(&args, "--pam-service").unwrap_or_else(|| "rdsktp".into()),
        min_uid: arg(&args, "--min-uid").and_then(|v| v.parse().ok()).unwrap_or(1000),
    });
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "rdsktp-authd must run as root");
    let pam = Arc::new(Pam::load(&cfg.pam_service)?);
    let listener = bind(&cfg.socket)?;
    tracing::info!(socket = %cfg.socket.display(), "listening");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let (cfg, pam) = (cfg.clone(), pam.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &cfg, &pam) {
                tracing::warn!("request dropped: {e:#}");
            }
        });
    }
    Ok(())
}
