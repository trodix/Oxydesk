//! `oxydesk-authd`: the only privileged component. Authenticates through PAM, starts the user's session agent
//! (`oxydesk-agent@<user>.service`) and hands `oxydesk` a descriptor connected to that agent.
//! No secret in logs or in argv.

mod pam;

use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxydesk_authd_proto::{
    read_request, send_response, valid_username, Request, Response, SessionPolicy,
};
use pam::{Pam, PamOutcome};

const DEFAULT_SOCKET: &str = "/run/oxydesk/authd.sock";
const RUNTIME_DIR: &str = "/run/oxydesk";

struct Config {
    socket: PathBuf,
    pam_service: String,
    min_uid: u32,
    /// Users PAM has accepted since this process started: only they can be rejoined without a password.
    authenticated: Mutex<HashSet<String>>,
}

/// Whether `username` may rejoin its running session without a password (automatic reconnection): an account
/// this `authd` authenticated itself, above the minimum uid, whose agent is running.
fn may_attach(
    known: &HashSet<String>,
    username: &str,
    uid: Option<u32>,
    min_uid: u32,
    agent_running: bool,
) -> bool {
    known.contains(username) && uid.is_some_and(|u| u >= min_uid) && agent_running
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
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut res,
        )
    };
    (rc == 0 && !res.is_null()).then_some(pwd.pw_uid)
}

fn agent_socket(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/oxydesk-agent.sock"))
}

/// Where the number of outputs wanted for the next start of a user's agent is left; the unit reads it
/// (`EnvironmentFile`) and it comes before `agent.conf` on the agent's command line.
fn outputs_file(runtime_dir: &Path, username: &str) -> PathBuf {
    runtime_dir.join(format!("agent-{username}.env"))
}

/// Content of the file the unit reads: the outputs wanted, or nothing if the number is not usable.
fn outputs_env(outputs: u32) -> Option<String> {
    (1..=16)
        .contains(&outputs)
        .then(|| format!("OXYDESK_AGENT_OUTPUTS=--outputs {outputs}\n"))
}

/// What a login does about the desktop session of the user.
#[derive(Debug, PartialEq, Eq)]
enum SessionPlan {
    /// A session of our own (private virtual KWin).
    Start,
    /// Join the Plasma session whose Wayland socket is this (relative to the user's runtime directory).
    Attach(String),
    /// The policy wants an existing session and there is none.
    Refuse,
}

/// The policy applied to what the user already has (`existing`: the socket of their Plasma session).
fn plan(policy: SessionPolicy, existing: Option<String>) -> SessionPlan {
    match (policy, existing) {
        (SessionPolicy::New, _) | (SessionPolicy::Auto, None) => SessionPlan::Start,
        (_, Some(socket)) => SessionPlan::Attach(socket),
        (SessionPolicy::Attach, None) => SessionPlan::Refuse,
    }
}

/// A Wayland socket name that is safe on the agent's command line.
fn valid_socket_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Socket of the Plasma session `uid` already has: the compositor `proc_root` shows for them that is a real
/// session (a `kwin_wayland` that names its `--socket` and is not `--virtual`, like the ones we start) and
/// whose socket exists in `run_dir`.
fn existing_session(proc_root: &Path, run_dir: &Path, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(proc_root).ok()?.flatten() {
        let dir = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        if std::fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(raw) = std::fs::read(dir.join("cmdline")) else {
            continue;
        };
        let argv: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        let is_kwin = argv
            .first()
            .is_some_and(|a| a.rsplit(|b| *b == b'/').next() == Some(b"kwin_wayland".as_slice()));
        if !is_kwin || argv.contains(&b"--virtual".as_slice()) {
            continue;
        }
        let socket = argv
            .windows(2)
            .find(|w| w[0] == b"--socket")
            .and_then(|w| std::str::from_utf8(w[1]).ok())
            .filter(|s| valid_socket_name(s));
        if let Some(socket) = socket.filter(|s| run_dir.join(s).exists()) {
            return Some(socket.to_owned());
        }
    }
    None
}

/// Records how the session that is about to be created is made (an agent that already runs keeps its own: its
/// compositor cannot always change the number of outputs). A stale wish is removed when there is none.
fn want_session(unit: &str, username: &str, outputs: u32, plan: &SessionPlan, keep_virtual: bool) {
    let running = Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success());
    if running {
        return;
    }
    let file = outputs_file(Path::new(RUNTIME_DIR), username);
    let result = match agent_env(outputs, plan, keep_virtual) {
        Some(content) => std::fs::write(&file, content),
        None => std::fs::remove_file(&file).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        }),
    };
    if let Err(e) = result {
        tracing::warn!(file = %file.display(), "outputs of the session not recorded: {e}");
    }
}

/// Content of the file the unit reads for this plan: how the session is made (`Attach`) or how many outputs it
/// starts with (`Start`).
fn agent_env(outputs: u32, plan: &SessionPlan, keep_virtual: bool) -> Option<String> {
    match plan {
        SessionPlan::Start => outputs_env(outputs),
        SessionPlan::Attach(socket) => Some(format!(
            "OXYDESK_AGENT_SESSION=--session attach --kwin-socket {socket} --virtual-outputs {}\n",
            if keep_virtual { "keep" } else { "remove" }
        )),
        SessionPlan::Refuse => None,
    }
}

/// Whether the agent of `unit` runs: a running one is joined, whatever the policy.
fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts the agent (idempotent) and waits for its socket to accept connections.
fn connect_agent(
    username: &str,
    uid: u32,
    outputs: u32,
    plan: &SessionPlan,
    keep_virtual: bool,
) -> anyhow::Result<UnixStream> {
    let unit = format!("oxydesk-agent@{username}.service");
    want_session(&unit, username, outputs, plan, keep_virtual);
    let status = Command::new("systemctl")
        .args(["start", "--no-block", &unit])
        .status()?;
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
        let failed = Command::new("systemctl")
            .args(["is-failed", "--quiet", &unit])
            .status()
            .is_ok_and(|s| s.success());
        anyhow::ensure!(!failed, "unit {unit} failed");
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for the agent");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn agent_running(username: &str) -> bool {
    Command::new("systemctl")
        .args([
            "is-active",
            "--quiet",
            &format!("oxydesk-agent@{username}.service"),
        ])
        .status()
        .is_ok_and(|s| s.success())
}

/// Connects to the agent of a running session, without starting anything.
fn join_agent(uid: u32) -> anyhow::Result<UnixStream> {
    Ok(UnixStream::connect(agent_socket(uid))?)
}

fn attach(mut stream: UnixStream, cfg: &Config, username: &str) -> anyhow::Result<()> {
    let uid = valid_username(username)
        .then(|| lookup_uid(username))
        .flatten();
    let known = cfg.authenticated.lock().expect("authenticated mutex");
    let allowed = may_attach(&known, username, uid, cfg.min_uid, agent_running(username));
    drop(known);
    let Some(uid) = uid.filter(|_| allowed) else {
        tracing::warn!(user = %username, "reconnection refused");
        return send_response(&mut stream, &Response::Denied, None).map_err(Into::into);
    };
    match join_agent(uid) {
        Ok(agent) => {
            send_response(
                &mut stream,
                &Response::Granted { uid },
                Some(agent.as_raw_fd()),
            )?;
            tracing::info!(user = %username, uid, "session rejoined");
        }
        Err(e) => {
            tracing::warn!(user = %username, "agent unreachable: {e}");
            send_response(&mut stream, &Response::Denied, None)?;
        }
    }
    Ok(())
}

fn handle(mut stream: UnixStream, cfg: &Config, pam: &Pam) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (username, password, outputs, policy, keep_virtual) = match read_request(&mut stream)? {
        Request::Login {
            username,
            password,
            outputs,
            policy,
            keep_virtual_outputs,
        } => (username, password, outputs, policy, keep_virtual_outputs),
        Request::Attach { username, .. } => {
            stream.set_read_timeout(None)?;
            tracing::info!(user = %username, "reconnection request");
            return attach(stream, cfg, &username);
        }
    };
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
    if pam.authenticate(&username, &password)? != PamOutcome::Granted
        || uid.is_none_or(|u| u < cfg.min_uid)
    {
        tracing::warn!(user = %username, "authentication refused");
        return deny(&mut stream);
    }
    let uid = uid.expect("checked above");
    cfg.authenticated
        .lock()
        .expect("authenticated mutex")
        .insert(username.clone());
    let unit = format!("oxydesk-agent@{username}.service");
    let plan = if unit_active(&unit) {
        // Already running (a session of ours, or one joined earlier): it is joined as it is.
        SessionPlan::Start
    } else {
        let run_dir = PathBuf::from(format!("/run/user/{uid}"));
        plan(policy, existing_session(Path::new("/proc"), &run_dir, uid))
    };
    if plan == SessionPlan::Refuse {
        tracing::warn!(user = %username, "no Plasma session to join (session-policy attach)");
        send_response(
            &mut stream,
            &Response::Error {
                message: "no Plasma session to join".into(),
            },
            None,
        )?;
        return Ok(());
    }
    if let SessionPlan::Attach(socket) = &plan {
        tracing::info!(user = %username, socket, "joining the Plasma session of the user");
    }
    match connect_agent(&username, uid, outputs, &plan, keep_virtual) {
        Ok(agent) => {
            send_response(
                &mut stream,
                &Response::Granted { uid },
                Some(agent.as_raw_fd()),
            )?;
            tracing::info!(user = %username, uid, "session granted");
        }
        Err(e) => {
            tracing::error!(user = %username, "agent unavailable: {e:#}");
            send_response(
                &mut stream,
                &Response::Error {
                    message: "session unavailable".into(),
                },
                None,
            )?;
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
    // Access restricted to root and the `oxydesk` group (owner of the `oxydesk` service).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = lookup_gid("oxydesk") {
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
    let rc = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &mut grp,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut res,
        )
    };
    (rc == 0 && !res.is_null()).then_some(grp.gr_gid)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Arc::new(Config {
        socket: arg(&args, "--socket")
            .unwrap_or_else(|| DEFAULT_SOCKET.into())
            .into(),
        pam_service: arg(&args, "--pam-service").unwrap_or_else(|| "oxydesk".into()),
        min_uid: arg(&args, "--min-uid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000),
        authenticated: Mutex::default(),
    });
    anyhow::ensure!(
        unsafe { libc::geteuid() } == 0,
        "oxydesk-authd must run as root"
    );
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

#[cfg(test)]
mod outputs_tests {
    use super::*;

    #[test]
    fn only_a_user_authenticated_here_with_a_running_agent_can_be_rejoined() {
        let known: HashSet<String> = ["alice".to_owned()].into();
        assert!(may_attach(&known, "alice", Some(1000), 1000, true));
        // Never logged in through PAM since the start (or someone else's session).
        assert!(!may_attach(&known, "bob", Some(1001), 1000, true));
        // No session to rejoin: nothing is started without a password.
        assert!(!may_attach(&known, "alice", Some(1000), 1000, false));
        // Unknown account, or a system account.
        assert!(!may_attach(&known, "alice", None, 1000, true));
        assert!(!may_attach(&known, "alice", Some(999), 1000, true));
    }

    #[test]
    fn the_wish_is_an_option_of_the_agent_or_nothing() {
        assert_eq!(
            outputs_env(2).as_deref(),
            Some("OXYDESK_AGENT_OUTPUTS=--outputs 2\n")
        );
        assert_eq!(outputs_env(0), None);
        assert_eq!(outputs_env(17), None);
    }

    #[test]
    fn the_policy_decides_between_our_own_session_and_the_existing_one() {
        let some = || Some("wayland-0".to_owned());
        assert_eq!(plan(SessionPolicy::New, some()), SessionPlan::Start);
        assert_eq!(plan(SessionPolicy::New, None), SessionPlan::Start);
        let attach = SessionPlan::Attach("wayland-0".into());
        assert_eq!(plan(SessionPolicy::Attach, some()), attach);
        assert_eq!(plan(SessionPolicy::Attach, None), SessionPlan::Refuse);
        assert_eq!(plan(SessionPolicy::Auto, some()), attach);
        assert_eq!(plan(SessionPolicy::Auto, None), SessionPlan::Start);
    }

    #[test]
    fn the_unit_reads_how_the_session_is_made() {
        assert_eq!(
            agent_env(2, &SessionPlan::Start, false).as_deref(),
            Some("OXYDESK_AGENT_OUTPUTS=--outputs 2\n")
        );
        assert_eq!(
            agent_env(2, &SessionPlan::Attach("wayland-0".into()), false).as_deref(),
            Some("OXYDESK_AGENT_SESSION=--session attach --kwin-socket wayland-0 --virtual-outputs remove\n")
        );
        // The client's extra monitors stay after it leaves, when the server is configured so.
        assert_eq!(
            agent_env(2, &SessionPlan::Attach("wayland-0".into()), true).as_deref(),
            Some("OXYDESK_AGENT_SESSION=--session attach --kwin-socket wayland-0 --virtual-outputs keep\n")
        );
        assert_eq!(agent_env(2, &SessionPlan::Refuse, false), None);
    }

    #[test]
    fn only_plain_socket_names_reach_the_command_line() {
        assert!(valid_socket_name("wayland-0"));
        assert!(!valid_socket_name(""));
        assert!(!valid_socket_name("a b"));
        assert!(!valid_socket_name("../x"));
        assert!(!valid_socket_name("x;rm"));
        assert!(!valid_socket_name(&"a".repeat(65)));
    }

    /// A fake `/proc` owned by the current user, and a runtime directory holding `sockets`.
    fn fake_session(name: &str, procs: &[(u32, &[&str])], sockets: &[&str]) -> (PathBuf, PathBuf) {
        let root = temp(name);
        let (proc_root, run) = (root.join("proc"), root.join("run"));
        std::fs::create_dir_all(&run).unwrap();
        for (pid, argv) in procs {
            let dir = proc_root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let mut raw = argv.join("\0").into_bytes();
            raw.push(0);
            std::fs::write(dir.join("cmdline"), raw).unwrap();
        }
        for s in sockets {
            std::fs::write(run.join(s), b"").unwrap();
        }
        std::fs::create_dir_all(&proc_root).unwrap();
        (proc_root, run)
    }

    fn me() -> u32 {
        // SAFETY: no argument, cannot fail.
        unsafe { libc::geteuid() }
    }

    #[test]
    fn the_real_session_of_the_user_is_found_and_our_virtual_ones_are_not() {
        let real = [
            "/usr/bin/kwin_wayland",
            "--wayland-fd",
            "7",
            "--socket",
            "wayland-0",
            "--xwayland",
        ];
        let virt = [
            "/usr/bin/kwin_wayland",
            "--virtual",
            "--socket",
            "oxydesk-0",
            "--width",
            "1920",
        ];
        let (p, r) = fake_session(
            "real",
            &[(10, &virt), (11, &real)],
            &["wayland-0", "oxydesk-0"],
        );
        assert_eq!(existing_session(&p, &r, me()).as_deref(), Some("wayland-0"));
        // Only a virtual one: nothing to join.
        let (p, r) = fake_session("virtual", &[(10, &virt)], &["oxydesk-0"]);
        assert_eq!(existing_session(&p, &r, me()), None);
        // The process names a socket that does not exist (not started yet, or gone).
        let (p, r) = fake_session("nosocket", &[(11, &real)], &[]);
        assert_eq!(existing_session(&p, &r, me()), None);
        // Another program, an unusable socket name, or another user.
        let other = ["/usr/bin/plasmashell", "--socket", "wayland-0"];
        let (p, r) = fake_session("other", &[(12, &other)], &["wayland-0"]);
        assert_eq!(existing_session(&p, &r, me()), None);
        let odd = ["/usr/bin/kwin_wayland", "--socket", "../evil"];
        let (p, r) = fake_session("odd", &[(13, &odd)], &["../evil"]);
        assert_eq!(existing_session(&p, &r, me()), None);
        let (p, r) = fake_session("uid", &[(11, &real)], &["wayland-0"]);
        assert_eq!(existing_session(&p, &r, me().wrapping_add(1)), None);
        assert_eq!(existing_session(Path::new("/nonexistent"), &r, me()), None);
    }

    #[test]
    fn the_file_is_per_user_in_the_runtime_directory() {
        assert_eq!(
            outputs_file(Path::new("/run/oxydesk"), "alice"),
            PathBuf::from("/run/oxydesk/agent-alice.env")
        );
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options_are_read_by_name() {
        let a = args(&["--socket", "/tmp/s", "--min-uid", "500"]);
        assert_eq!(arg(&a, "--socket").as_deref(), Some("/tmp/s"));
        assert_eq!(arg(&a, "--min-uid").as_deref(), Some("500"));
        assert_eq!(arg(&a, "--pam-service"), None);
        // An option without value at the end is absent, not an out-of-bounds read.
        assert_eq!(arg(&args(&["--socket"]), "--socket"), None);
    }

    #[test]
    fn the_agent_socket_is_in_the_runtime_directory_of_the_user() {
        assert_eq!(
            agent_socket(1000),
            PathBuf::from("/run/user/1000/oxydesk-agent.sock")
        );
    }

    #[test]
    fn accounts_are_looked_up_in_the_system_database() {
        assert_eq!(lookup_uid("root"), Some(0));
        assert_eq!(lookup_uid("no-such-account-oxydesk"), None);
        assert_eq!(lookup_uid("bad\0name"), None);
        assert_eq!(lookup_gid("root"), Some(0));
        assert_eq!(lookup_gid("no-such-group-oxydesk"), None);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxydesk-authd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_socket_is_restricted_and_replaces_a_stale_one() {
        let dir = temp("bind");
        let path = dir.join("sub/authd.sock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind(&path).expect("bind over a stale file");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
        // It really listens.
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(path).is_ok()
        });
        listener.accept().unwrap();
        assert!(client.join().unwrap());
        // The directory is created when missing.
        let nested = dir.join("a/b/c.sock");
        bind(&nested).expect("creates the directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn config(min_uid: u32, known: &[&str]) -> Config {
        Config {
            socket: PathBuf::new(),
            pam_service: "oxydesk".into(),
            min_uid,
            authenticated: Mutex::new(known.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    fn denied(cfg: &Config, username: &str) -> bool {
        let (server, mut client) = UnixStream::pair().unwrap();
        attach(server, cfg, username).unwrap();
        let (resp, fd) = oxydesk_authd_proto::recv_response(&mut client).unwrap();
        assert!(fd.is_none(), "no descriptor with a refusal");
        resp == Response::Denied
    }

    #[test]
    fn a_reconnection_is_denied_without_a_prior_login_or_for_a_bad_name() {
        // root exists but nobody authenticated it here (and its uid is below the minimum).
        assert!(denied(&config(1000, &[]), "root"));
        assert!(denied(&config(0, &[]), "root"));
        // Authenticated, but a system account.
        assert!(denied(&config(1000, &["root"]), "root"));
        // Syntactically invalid or unknown names never reach the lookup.
        assert!(denied(&config(0, &["../etc"]), "../etc"));
        assert!(denied(
            &config(0, &["no-such-account-oxydesk"]),
            "no-such-account-oxydesk"
        ));
    }
}
