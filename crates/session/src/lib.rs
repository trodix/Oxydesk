//! Adaptateur `SessionProvider` : session KWin virtuelle (headless) pour l'utilisateur courant.
//!
//! M3 = autologin : le processus tourne déjà sous l'utilisateur cible (service systemd, linger). Le login
//! distant avec PAM arrive en M7.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rdsktp_core::ports::SessionProvider;

#[derive(Debug, Clone)]
pub struct KwinSessionConfig {
    /// Nom du socket Wayland créé par KWin (relatif à `XDG_RUNTIME_DIR`).
    pub socket: String,
    pub width: u32,
    pub height: u32,
    /// Nombre d'écrans virtuels (de même taille, côte à côte).
    pub outputs: u32,
    /// Programmes lancés dans la session une fois KWin prêt (argv complet).
    pub programs: Vec<Vec<String>>,
}

pub struct KwinSession {
    cfg: KwinSessionConfig,
    kwin: Option<Child>,
    children: Vec<Child>,
}

/// Version de KWin lue depuis `kwin_wayland --version` (« kwin 6.3.6 »).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KwinVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

pub fn parse_kwin_version(output: &str) -> Option<KwinVersion> {
    let line = output.lines().find(|l| l.trim_start().starts_with("kwin "))?;
    let mut it = line.trim().strip_prefix("kwin ")?.split('.');
    let mut next = || it.next()?.trim().parse::<u32>().ok();
    Some(KwinVersion { major: next()?, minor: next()?, patch: next().unwrap_or(0) })
}

/// Le KWin supporté est Plasma 6.3 ou plus récent ; les interfaces utilisées sont des détails d'implémentation KDE.
fn check_version(v: KwinVersion) -> Result<(), String> {
    if v.major != 6 || v.minor < 3 {
        return Err(format!("KWin {}.{}.{} non supporté (Plasma 6.3+ requis)", v.major, v.minor, v.patch));
    }
    Ok(())
}

impl KwinSession {
    pub fn new(cfg: KwinSessionConfig) -> Self {
        Self { cfg, kwin: None, children: Vec::new() }
    }

    fn runtime_dir() -> Result<PathBuf, String> {
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).ok_or_else(|| "XDG_RUNTIME_DIR absent (session utilisateur requise)".to_string())
    }

    /// Environnement commun à KWin et aux programmes de la session.
    fn session_env(cmd: &mut Command, runtime: &std::path::Path, socket: &str) {
        cmd.env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "KDE")
            // Indispensable pour que KWin retrouve le .desktop qui autorise la capture et les entrées (ADR 004).
            .env("XDG_MENU_PREFIX", "plasma-")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("WAYLAND_DISPLAY", socket);
        // Thème de curseur transparent : le backend virtuel de KWin dessine toujours son curseur
        // logiciel dans l'image capturée (même en mode `hidden`/`metadata`), ce qui le doublerait
        // avec celui du client RDP. Variables d'environnement seules : la config de l'utilisateur
        // (kcminputrc) n'est pas modifiée.
        let icons = runtime.join(BLANK_ICONS_DIR);
        if icons.join(BLANK_THEME).is_dir() {
            cmd.env("XCURSOR_PATH", &icons).env("XCURSOR_THEME", BLANK_THEME).env("XCURSOR_SIZE", "24");
        }
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}/bus", runtime.display()));
        }
    }
}

const BLANK_ICONS_DIR: &str = "rdsktp-icons";
const BLANK_THEME: &str = "rdsktp-blank";
/// Noms de curseurs demandés par KWin/Qt ; tous pointent vers le même fichier transparent.
const CURSOR_NAMES: &[&str] = &[
    "default", "arrow", "top_left_arrow", "pointer", "text", "xterm", "ibeam", "hand1", "hand2", "pointing_hand",
    "watch", "wait", "progress", "crosshair", "cross", "fleur", "move", "size_all", "size_hor", "size_ver",
    "sb_h_double_arrow", "sb_v_double_arrow", "size_fdiag", "size_bdiag", "not-allowed", "forbidden",
    "question_arrow", "help", "grab", "grabbing", "openhand", "closedhand", "copy", "alias", "context-menu",
    "cell", "vertical-text", "col-resize", "row-resize", "e-resize", "w-resize", "n-resize", "s-resize",
    "ne-resize", "nw-resize", "se-resize", "sw-resize", "ew-resize", "ns-resize", "nesw-resize", "nwse-resize",
];

/// Fichier Xcursor d'une image 1x1 entièrement transparente (taille nominale 24).
fn blank_xcursor() -> Vec<u8> {
    let le = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    let mut out = b"Xcur".to_vec();
    out.extend(le(&[16, 0x1_0000, 1])); // en-tête, version, 1 entrée
    out.extend(le(&[0xfffd_0002, 24, 28])); // type image, taille nominale, offset du chunk
    out.extend(le(&[36, 0xfffd_0002, 24, 1, 1, 1, 0, 0, 0])); // chunk : w=h=1, hotspot 0, délai 0
    out.extend(le(&[0])); // pixel ARGB transparent
    out
}

fn write_blank_cursor_theme(runtime: &std::path::Path) -> std::io::Result<()> {
    let dir = runtime.join(BLANK_ICONS_DIR).join(BLANK_THEME);
    let cursors = dir.join("cursors");
    std::fs::create_dir_all(&cursors)?;
    std::fs::write(dir.join("index.theme"), format!("[Icon Theme]\nName={BLANK_THEME}\n"))?;
    std::fs::write(cursors.join("left_ptr"), blank_xcursor())?;
    for name in CURSOR_NAMES {
        let link = cursors.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("left_ptr", link)?;
    }
    Ok(())
}

impl SessionProvider for KwinSession {
    fn start(&mut self) -> Result<(), String> {
        let runtime = Self::runtime_dir()?;
        let out = Command::new("kwin_wayland").arg("--version").output().map_err(|e| format!("kwin_wayland introuvable : {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let version = parse_kwin_version(&text).ok_or_else(|| format!("version de KWin illisible : {text:?}"))?;
        check_version(version)?;

        // Un échec ici est sans gravité : le curseur du serveur sera simplement visible dans l'image.
        let _ = write_blank_cursor_theme(&runtime);

        // Le cache des services doit connaître notre .desktop avant le démarrage de KWin.
        let mut sycoca = Command::new("kbuildsycoca6");
        Self::session_env(&mut sycoca, &runtime, &self.cfg.socket);
        let _ = sycoca.stdout(Stdio::null()).stderr(Stdio::null()).status();

        let socket_path = runtime.join(&self.cfg.socket);
        let _ = std::fs::remove_file(&socket_path);
        let mut kwin = Command::new("kwin_wayland");
        Self::session_env(&mut kwin, &runtime, &self.cfg.socket);
        kwin.args(["--virtual", "--socket", &self.cfg.socket])
            .args(["--width", &self.cfg.width.to_string(), "--height", &self.cfg.height.to_string()])
            .args(["--output-count", &self.cfg.outputs.max(1).to_string()])
            .args(["--no-lockscreen", "--no-global-shortcuts"])
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null());
        let mut child = kwin.spawn().map_err(|e| format!("démarrage de kwin_wayland : {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(20);
        while !socket_path.exists() {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("kwin_wayland s'est arrêté ({status}) avant d'être prêt"));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err("kwin_wayland n'a pas créé son socket dans le délai".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kwin = Some(child);

        // Les services activés par D-Bus (kactivitymanagerd, kded…) héritent de l'environnement
        // du bus, pas du nôtre : sans WAYLAND_DISPLAY ils démarrent en xcb et plasmashell abandonne.
        for extra in [&["--systemd"][..], &[][..]] {
            let mut cmd = Command::new("dbus-update-activation-environment");
            Self::session_env(&mut cmd, &runtime, &self.cfg.socket);
            cmd.args(extra).args([
                "WAYLAND_DISPLAY", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "XDG_MENU_PREFIX", "QT_QPA_PLATFORM",
            ]);
            if cmd.status().map(|s| s.success()).unwrap_or(false) {
                break;
            }
        }

        for argv in &self.cfg.programs {
            let Some((prog, args)) = argv.split_first() else { continue };
            let mut cmd = Command::new(prog);
            Self::session_env(&mut cmd, &runtime, &self.cfg.socket);
            let child = cmd.args(args).stdin(Stdio::null()).spawn().map_err(|e| format!("démarrage de {prog} : {e}"))?;
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
    #[test]
    fn blank_xcursor_is_well_formed() {
        let b = super::blank_xcursor();
        assert_eq!(&b[..4], b"Xcur");
        assert_eq!(b.len(), 16 + 12 + 36 + 4);
        assert_eq!(&b[b.len() - 4..], &[0, 0, 0, 0]);
        // l'offset annoncé (28) pointe bien sur le chunk (16 octets d'en-tête + 12 de table)
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 36);
    }

    use super::*;

    #[test]
    fn parses_version_ignoring_qt_noise() {
        let out = "QThreadStorage: Thread 0x1 exited\nkwin 6.3.6\n";
        assert_eq!(parse_kwin_version(out), Some(KwinVersion { major: 6, minor: 3, patch: 6 }));
    }

    #[test]
    fn rejects_old_or_garbage() {
        assert_eq!(parse_kwin_version("rien"), None);
        assert!(check_version(KwinVersion { major: 6, minor: 2, patch: 0 }).is_err());
        assert!(check_version(KwinVersion { major: 5, minor: 27, patch: 0 }).is_err());
        assert!(check_version(KwinVersion { major: 6, minor: 4, patch: 1 }).is_ok());
    }
}
