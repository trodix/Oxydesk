//! `rdsktp-authd` : seul composant privilégié. Authentifie via PAM, démarre l'agent de session de
//! l'utilisateur (`rdsktp-agent@<user>.service`) et transmet à `rdsktp` un descripteur connecté à cet agent.
//! Aucun secret dans les journaux ni dans argv.

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

/// uid et gid d'un compte local via `getpwnam_r`.
fn lookup_uid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: tampon et structure valides pour la durée de l'appel.
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut res) };
    (rc == 0 && !res.is_null()).then_some(pwd.pw_uid)
}

fn agent_socket(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/rdsktp-agent.sock"))
}

/// Démarre l'agent (idempotent) et attend que son socket accepte les connexions.
fn connect_agent(username: &str, uid: u32) -> anyhow::Result<UnixStream> {
    let unit = format!("rdsktp-agent@{username}.service");
    let status = Command::new("systemctl").args(["start", "--no-block", &unit]).status()?;
    anyhow::ensure!(status.success(), "systemctl start {unit} : {status}");
    let sock = agent_socket(uid);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if sock.exists() {
            if let Ok(s) = UnixStream::connect(&sock) {
                return Ok(s);
            }
        }
        // L'unité a pu échouer (PAM, KWin) : inutile d'attendre jusqu'au bout.
        let failed = Command::new("systemctl").args(["is-failed", "--quiet", &unit]).status().is_ok_and(|s| s.success());
        anyhow::ensure!(!failed, "l'unité {unit} a échoué");
        anyhow::ensure!(Instant::now() < deadline, "délai dépassé en attendant l'agent");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn handle(mut stream: UnixStream, cfg: &Config, pam: &Pam) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let Request::Login { username, password } = read_request(&mut stream)?;
    stream.set_read_timeout(None)?;
    tracing::info!(user = %username, "demande de connexion");
    let deny = |stream: &mut UnixStream| -> anyhow::Result<()> {
        // Ralentit le bruteforce ; même réponse pour compte inconnu, mot de passe faux ou compte interdit.
        std::thread::sleep(Duration::from_secs(1));
        send_response(stream, &Response::Denied, None)?;
        Ok(())
    };
    if !valid_username(&username) {
        tracing::warn!("nom d'utilisateur refusé (format)");
        return deny(&mut stream);
    }
    let uid = lookup_uid(&username);
    if pam.authenticate(&username, &password)? != PamOutcome::Granted || uid.is_none_or(|u| u < cfg.min_uid) {
        tracing::warn!(user = %username, "authentification refusée");
        return deny(&mut stream);
    }
    let uid = uid.expect("vérifié ci-dessus");
    match connect_agent(&username, uid) {
        Ok(agent) => {
            send_response(&mut stream, &Response::Granted { uid }, Some(agent.as_raw_fd()))?;
            tracing::info!(user = %username, uid, "session accordée");
        }
        Err(e) => {
            tracing::error!(user = %username, "agent indisponible : {e:#}");
            send_response(&mut stream, &Response::Error { message: "session indisponible".into() }, None)?;
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
    // Accès réservé à root et au groupe `rdsktp` (propriétaire du service `rdsktp`).
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
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "rdsktp-authd doit tourner en root");
    let pam = Arc::new(Pam::load(&cfg.pam_service)?);
    let listener = bind(&cfg.socket)?;
    tracing::info!(socket = %cfg.socket.display(), "en écoute");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let (cfg, pam) = (cfg.clone(), pam.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &cfg, &pam) {
                tracing::warn!("requête abandonnée : {e:#}");
            }
        });
    }
    Ok(())
}
