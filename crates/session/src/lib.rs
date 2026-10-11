//! `SessionProvider` adapter: virtual (headless) KWin session for the current user.
//!
//! M3 = autologin: the process already runs as the target user (systemd service, linger). Remote
//! login with PAM arrives in M7.

mod cursor_theme;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rdsktp_core::ports::SessionProvider;

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
            .args(["--no-global-shortcuts"])
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
}
