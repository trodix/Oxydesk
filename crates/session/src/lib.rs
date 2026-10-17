//! `SessionProvider` adapter: virtual (headless) KWin session for the current user.
//!
//! M3 = autologin: the process already runs as the target user (systemd service, linger). Remote
//! login with PAM arrives in M7.

mod cursor_theme;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use oxydesk_core::ports::SessionProvider;

#[derive(Debug, Clone)]
pub struct KwinSessionConfig {
    /// Name of the Wayland socket created by KWin (relative to `XDG_RUNTIME_DIR`).
    pub socket: String,
    pub width: u32,
    pub height: u32,
    /// Number of virtual screens (same size, side by side).
    pub outputs: u32,
    /// Programs launched in the session once KWin is ready (full argv).
    pub programs: Vec<Vec<String>>,
}

pub struct KwinSession {
    cfg: KwinSessionConfig,
    kwin: Option<Child>,
    children: Vec<Child>,
}

/// KWin version read from `kwin_wayland --version` ("kwin 6.3.6").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KwinVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

pub fn parse_kwin_version(output: &str) -> Option<KwinVersion> {
    let line = output
        .lines()
        .find(|l| l.trim_start().starts_with("kwin "))?;
    let mut it = line.trim().strip_prefix("kwin ")?.split('.');
    let mut next = || it.next()?.trim().parse::<u32>().ok();
    Some(KwinVersion {
        major: next()?,
        minor: next()?,
        patch: next().unwrap_or(0),
    })
}

/// The supported KWin is Plasma 6.3 or newer; the interfaces used are KDE implementation details.
fn check_version(v: KwinVersion) -> Result<(), String> {
    if v.major != 6 || v.minor < 3 {
        return Err(format!(
            "KWin {}.{}.{} not supported (Plasma 6.3+ required)",
            v.major, v.minor, v.patch
        ));
    }
    Ok(())
}

impl KwinSession {
    pub fn new(cfg: KwinSessionConfig) -> Self {
        Self {
            cfg,
            kwin: None,
            children: Vec::new(),
        }
    }

    fn runtime_dir() -> Result<PathBuf, String> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| "XDG_RUNTIME_DIR missing (user session required)".to_string())
    }

    /// Environment common to KWin and the session programs.
    fn session_env(cmd: &mut Command, runtime: &std::path::Path, socket: &str) {
        cmd.env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "KDE")
            // Required so that KWin finds the .desktop that authorizes capture and input (ADR 004).
            .env("XDG_MENU_PREFIX", "plasma-")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("WAYLAND_DISPLAY", socket);
        // Transparent cursor theme: KWin's virtual backend always draws its software cursor
        // into the captured image (even in `hidden`/`metadata` mode), which would double it
        // with the RDP client's. Environment variables only: the user's config
        // (kcminputrc) is not modified.
        let icons = runtime.join(BLANK_ICONS_DIR);
        if icons.join(BLANK_THEME).is_dir() {
            cmd.env("XCURSOR_PATH", &icons)
                .env("XCURSOR_THEME", BLANK_THEME)
                .env("XCURSOR_SIZE", "24");
        }
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            cmd.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", runtime.display()),
            );
        }
    }
}

/// Processes of `uid` that are a `kwin_wayland` serving the Wayland socket `socket` (read in `proc_root`, a `/proc`).
/// Left behind by an agent that was killed, they keep the socket and make every new session fail.
fn stale_compositors(proc_root: &std::path::Path, socket: &str, uid: u32) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if entry.metadata().map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        let is_kwin = argv.first().is_some_and(|a| a.ends_with(b"kwin_wayland"));
        let ours = argv
            .windows(2)
            .any(|w| w[0] == b"--socket" && w[1] == socket.as_bytes());
        if is_kwin && ours {
            found.push(pid);
        }
    }
    found
}

/// Ends the compositors `stale_compositors` finds (SIGTERM, then SIGKILL after a few seconds) and clears their socket.
fn clear_stale_compositors(runtime: &std::path::Path, socket: &str) {
    use std::os::unix::fs::MetadataExt;
    let proc_root = std::path::Path::new("/proc");
    let Ok(uid) = std::fs::metadata("/proc/self").map(|m| m.uid()) else {
        return;
    };
    let pids = stale_compositors(proc_root, socket, uid);
    for signal in ["-TERM", "-KILL"] {
        for pid in &pids {
            if proc_root.join(pid.to_string()).exists() {
                let _ = Command::new("kill")
                    .arg(signal)
                    .arg(pid.to_string())
                    .status();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|p| proc_root.join(p.to_string()).exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if !pids.is_empty() {
        let _ = std::fs::remove_file(runtime.join(format!("{socket}.lock")));
    }
}

const BLANK_ICONS_DIR: &str = cursor_theme::ICONS_DIR;
const BLANK_THEME: &str = cursor_theme::THEME;

impl SessionProvider for KwinSession {
    fn start(&mut self) -> Result<(), String> {
        let runtime = Self::runtime_dir()?;
        let out = Command::new("kwin_wayland")
            .arg("--version")
            .output()
            .map_err(|e| format!("kwin_wayland not found: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let version = parse_kwin_version(&text)
            .ok_or_else(|| format!("unreadable KWin version: {text:?}"))?;
        check_version(version)?;

        // A failure here is harmless: the server cursor will simply be visible in the image.
        if cursor_theme::write_marker_theme(&runtime).is_ok() {
            cursor_theme::install_resolver();
        }

        // The service cache must know our .desktop before KWin starts.
        let mut sycoca = Command::new("kbuildsycoca6");
        Self::session_env(&mut sycoca, &runtime, &self.cfg.socket);
        let _ = sycoca.stdout(Stdio::null()).stderr(Stdio::null()).status();

        let socket_path = runtime.join(&self.cfg.socket);
        clear_stale_compositors(&runtime, &self.cfg.socket);
        let _ = std::fs::remove_file(&socket_path);
        let mut kwin = Command::new("kwin_wayland");
        Self::session_env(&mut kwin, &runtime, &self.cfg.socket);
        kwin.args(["--virtual", "--socket", &self.cfg.socket])
            .args([
                "--width",
                &self.cfg.width.to_string(),
                "--height",
                &self.cfg.height.to_string(),
            ])
            .args(["--output-count", &self.cfg.outputs.max(1).to_string()])
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null());
        let mut child = kwin
            .spawn()
            .map_err(|e| format!("starting kwin_wayland: {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(20);
        while !socket_path.exists() {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("kwin_wayland exited ({status}) before being ready"));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err("kwin_wayland did not create its socket in time".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kwin = Some(child);

        // Services activated through D-Bus (kactivitymanagerd, kded…) inherit the bus
        // environment, not ours: without WAYLAND_DISPLAY they start in xcb and plasmashell gives up.
        for extra in [&["--systemd"][..], &[][..]] {
            let mut cmd = Command::new("dbus-update-activation-environment");
            Self::session_env(&mut cmd, &runtime, &self.cfg.socket);
            cmd.args(extra).args([
                "WAYLAND_DISPLAY",
                "XDG_SESSION_TYPE",
                "XDG_CURRENT_DESKTOP",
                "XDG_MENU_PREFIX",
                "QT_QPA_PLATFORM",
            ]);
            if cmd.status().map(|s| s.success()).unwrap_or(false) {
                break;
            }
        }

        for argv in &self.cfg.programs {
            let Some((prog, args)) = argv.split_first() else {
                continue;
            };
            let mut cmd = Command::new(prog);
            Self::session_env(&mut cmd, &runtime, &self.cfg.socket);
            let child = cmd
                .args(args)
                .stdin(Stdio::null())
                .spawn()
                .map_err(|e| format!("starting {prog}: {e}"))?;
            self.children.push(child);
        }
        Ok(())
    }

    fn stop(&mut self) {
        for mut c in self.children.drain(..).chain(self.kwin.take()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for KwinSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /proc with one process per `(pid, argv)`.
    fn fake_proc(procs: &[(u32, &[&str])]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "oxydesk-proc-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (pid, argv) in procs {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let mut raw = argv.join("\0").into_bytes();
            raw.push(0);
            std::fs::write(dir.join("cmdline"), raw).unwrap();
        }
        std::fs::create_dir_all(root.join("self")).unwrap();
        root
    }

    fn me() -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").unwrap().uid()
    }

    #[test]
    fn a_compositor_left_on_our_socket_is_stale() {
        let root = fake_proc(&[
            (
                10,
                &[
                    "kwin_wayland",
                    "--virtual",
                    "--socket",
                    "oxydesk-0",
                    "--width",
                    "1920",
                ],
            ),
            (11, &["/usr/bin/kwin_wayland", "--socket", "oxydesk-0"]),
            // Another socket (the user's own desktop, a test session), and another program on ours.
            (12, &["kwin_wayland", "--socket", "wayland-0"]),
            (13, &["kwin_wayland_wrapper", "--socket", "oxydesk-0"]),
            (14, &["plasmashell"]),
        ]);
        let mut found = stale_compositors(&root, "oxydesk-0", me());
        found.sort_unstable();
        assert_eq!(found, [10, 11]);
        // Processes of another user are never touched.
        assert!(stale_compositors(&root, "oxydesk-0", me() + 1).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_version_ignoring_qt_noise() {
        let out = "QThreadStorage: Thread 0x1 exited\nkwin 6.3.6\n";
        assert_eq!(
            parse_kwin_version(out),
            Some(KwinVersion {
                major: 6,
                minor: 3,
                patch: 6
            })
        );
    }

    #[test]
    fn rejects_old_or_garbage() {
        assert_eq!(parse_kwin_version("rien"), None);
        assert!(check_version(KwinVersion {
            major: 6,
            minor: 2,
            patch: 0
        })
        .is_err());
        assert!(check_version(KwinVersion {
            major: 5,
            minor: 27,
            patch: 0
        })
        .is_err());
        assert!(check_version(KwinVersion {
            major: 6,
            minor: 4,
            patch: 1
        })
        .is_ok());
    }

    #[test]
    fn the_patch_number_is_optional_and_a_truncated_version_is_refused() {
        assert_eq!(
            parse_kwin_version("kwin 6.4"),
            Some(KwinVersion {
                major: 6,
                minor: 4,
                patch: 0
            })
        );
        assert_eq!(parse_kwin_version("kwin 6"), None);
        assert_eq!(parse_kwin_version("kwin six.three"), None);
    }

    #[test]
    fn the_lowest_supported_version_is_accepted_and_the_refusal_names_the_version() {
        assert!(check_version(KwinVersion {
            major: 6,
            minor: 3,
            patch: 0
        })
        .is_ok());
        let err = check_version(KwinVersion {
            major: 6,
            minor: 2,
            patch: 5,
        })
        .unwrap_err();
        assert!(err.contains("6.2.5") && err.contains("6.3"), "{err}");
        // A future major is not assumed compatible: the interfaces are KDE internals.
        assert!(check_version(KwinVersion {
            major: 7,
            minor: 0,
            patch: 0
        })
        .is_err());
    }
}
