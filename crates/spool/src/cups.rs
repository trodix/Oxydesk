//! CUPS queues for the printers of the client, made with `lpadmin`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxydesk_core::printer::{printer_name, PrintFormat, PrintLanguage, PrinterInfo};

/// URI scheme of the backend (`oxydesk:/path/to/socket`).
pub const SCHEME: &str = "oxydesk";

/// Name of the queue of a printer: the user's name keeps the queues of several sessions apart.
pub fn queue_name(user: &str, printer: &str) -> String {
    format!("{}-{}", printer_name(user), printer_name(printer))
        .chars()
        .take(100)
        .collect()
}

/// The PPD that makes CUPS produce what is sent to the client: the language of a raw printer, or PostScript for an
/// XPS printer (the socket converts it to XPS).
fn model(format: PrintFormat) -> &'static str {
    match format {
        PrintFormat::Raw(PrintLanguage::Pcl) => "drv:///sample.drv/generpcl.ppd",
        PrintFormat::Raw(PrintLanguage::Text) => "drv:///cupsfilters.drv/textonly.ppd",
        PrintFormat::Raw(PrintLanguage::PostScript) | PrintFormat::Xps => {
            "drv:///sample.drv/generic.ppd"
        }
    }
}

/// A path that can go in a device URI as it is.
fn uri_safe(path: &Path) -> bool {
    path.to_str().is_some_and(|p| {
        p.starts_with('/')
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    })
}

/// The CUPS commands to use and for whom the queues are made.
#[derive(Debug, Clone)]
pub struct Cups {
    lpadmin: PathBuf,
    lpstat: PathBuf,
    user: String,
}

impl Cups {
    /// `None` (with the reason logged) if the commands are missing or the scheduler does not answer.
    pub fn detect(user: &str) -> Option<Self> {
        let cups = Self::with_commands("lpadmin".into(), "lpstat".into(), user);
        match cups.run(&cups.lpstat, &["-r"]) {
            Ok(out) if out.contains("is running") => Some(cups),
            Ok(out) => {
                tracing::warn!("CUPS is not running ({}): no print queue", out.trim());
                None
            }
            Err(e) => {
                tracing::warn!("CUPS is not usable ({e}): no print queue");
                None
            }
        }
    }

    pub fn with_commands(lpadmin: PathBuf, lpstat: PathBuf, user: &str) -> Self {
        Self {
            lpadmin,
            lpstat,
            user: user.to_owned(),
        }
    }

    fn run(&self, program: &Path, args: &[&str]) -> Result<String, String> {
        let spawn = || {
            Command::new(program)
                .args(args)
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .output()
        };
        // ETXTBSY: a program written an instant ago is still held open by a concurrent fork (tests).
        let out = match spawn() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                spawn()
            }
            other => other,
        }
        .map_err(|e| format!("{}: {e}", program.display()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "{} {}: {}",
                program.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Creates the queue of a printer served on `socket`; returns its name.
    pub fn add_queue(
        &self,
        name: &str,
        info: &PrinterInfo,
        socket: &Path,
        session: &str,
    ) -> Option<String> {
        if !uri_safe(socket) {
            tracing::warn!(socket = %socket.display(), "unusual socket path: no print queue");
            return None;
        }
        let queue = queue_name(&self.user, name);
        let uri = format!("{SCHEME}:{}", socket.display());
        let description = format!("{} (RDP client)", info.name);
        let allow = format!("allow:{}", self.user);
        // The location tells which RDP session the queue belongs to.
        let location = format!("RDP session {session}");
        let result = self.run(
            &self.lpadmin,
            &[
                "-p",
                &queue,
                "-E",
                "-v",
                &uri,
                "-m",
                model(info.format),
                "-D",
                &description,
                "-L",
                &location,
                "-o",
                "printer-is-shared=false",
                "-u",
                &allow,
            ],
        );
        match result {
            Ok(_) => {
                tracing::info!(queue, "print queue created");
                Some(queue)
            }
            Err(e) => {
                tracing::warn!("cannot create the print queue ({e}); the socket stays available");
                None
            }
        }
    }

    pub fn remove_queue(&self, queue: &str) {
        match self.run(&self.lpadmin, &["-x", queue]) {
            Ok(_) => tracing::info!(queue, "print queue removed"),
            Err(e) => tracing::warn!("cannot remove the print queue ({e})"),
        }
    }

    /// Removes the queues of a previous process: those whose device is a socket under `base`.
    pub fn remove_stale(&self, base: &Path) {
        let Ok(listing) = self.run(&self.lpstat, &["-v"]) else {
            return;
        };
        let prefix = format!("{SCHEME}:{}/", base.display());
        for line in listing.lines() {
            // "device for NAME: oxydesk:/run/user/1000/oxydesk-printers/NAME.sock"
            let Some(rest) = line.strip_prefix("device for ") else {
                continue;
            };
            if let Some((queue, uri)) = rest.split_once(": ") {
                if uri.trim().starts_with(&prefix) {
                    self.remove_queue(queue);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn queue_names_are_safe() {
        assert_eq!(
            queue_name("sébastien", "HP Laser (1)"),
            "s_bastien-HP_Laser_1"
        );
        assert_eq!(queue_name("a", "b"), "a-b");
        assert!(queue_name(&"u".repeat(80), &"p".repeat(80)).len() <= 100);
    }

    #[test]
    fn languages_choose_the_model() {
        assert!(model(PrintFormat::Raw(PrintLanguage::PostScript)).ends_with("generic.ppd"));
        assert!(model(PrintFormat::Raw(PrintLanguage::Pcl)).ends_with("generpcl.ppd"));
        assert!(model(PrintFormat::Raw(PrintLanguage::Text)).ends_with("textonly.ppd"));
        // An XPS printer is fed PostScript, which the socket converts.
        assert!(model(PrintFormat::Xps).ends_with("generic.ppd"));
    }

    #[test]
    fn only_plain_paths_go_in_a_uri() {
        assert!(uri_safe(Path::new(
            "/run/user/1000/oxydesk-printers/a.sock"
        )));
        assert!(!uri_safe(Path::new("/run/user/1000/a b.sock")));
        assert!(!uri_safe(Path::new("relative.sock")));
        assert!(!uri_safe(Path::new("/tmp/é.sock")));
    }

    /// A shell script standing for a CUPS command: it records its arguments and prints `stdout`.
    fn fake(dir: &Path, name: &str, stdout: &str, exit: i32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}/{name}.log'\ncat <<'EOF'\n{stdout}\nEOF\nexit {exit}\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn info() -> PrinterInfo {
        PrinterInfo {
            name: "Office".into(),
            driver: "HP PCL 6".into(),
            is_default: false,
            format: PrintFormat::Raw(PrintLanguage::Pcl),
        }
    }

    #[test]
    fn a_queue_is_created_for_the_user_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cups = Cups::with_commands(
            fake(dir.path(), "lpadmin", "", 0),
            fake(dir.path(), "lpstat", "", 0),
            "alice",
        );
        let socket = Path::new("/run/user/1000/oxydesk-printers/Office.sock");
        let queue = cups
            .add_queue("Office", &info(), socket, "42-1")
            .expect("queue");
        assert_eq!(queue, "alice-Office");
        cups.remove_queue(&queue);
        let log = std::fs::read_to_string(dir.path().join("lpadmin.log")).unwrap();
        let mut lines = log.lines();
        let add = lines.next().unwrap();
        assert!(add
            .contains("-p alice-Office -E -v oxydesk:/run/user/1000/oxydesk-printers/Office.sock"));
        assert!(add.contains("-m drv:///sample.drv/generpcl.ppd"));
        assert!(add.contains("-u allow:alice"));
        assert!(add.contains("-L RDP session 42-1"));
        assert_eq!(lines.next().unwrap(), "-x alice-Office");
    }

    #[test]
    fn a_failing_lpadmin_gives_no_queue() {
        let dir = tempfile::tempdir().unwrap();
        let cups = Cups::with_commands(
            fake(dir.path(), "lpadmin", "", 1),
            fake(dir.path(), "lpstat", "", 0),
            "alice",
        );
        let socket = Path::new("/run/user/1000/oxydesk-printers/Office.sock");
        assert!(cups.add_queue("Office", &info(), socket, "42-1").is_none());
    }

    #[test]
    fn stale_queues_under_the_base_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let listing = "device for alice-Old: oxydesk:/run/user/1000/oxydesk-printers/Old.sock\n\
                       device for other-Q: oxydesk:/run/user/1001/oxydesk-printers/Q.sock\n\
                       device for lj: socket://10.0.0.5:9100";
        let cups = Cups::with_commands(
            fake(dir.path(), "lpadmin", "", 0),
            fake(dir.path(), "lpstat", listing, 0),
            "alice",
        );
        cups.remove_stale(Path::new("/run/user/1000/oxydesk-printers"));
        let log = std::fs::read_to_string(dir.path().join("lpadmin.log")).unwrap();
        assert_eq!(log.trim(), "-x alice-Old");
    }
}
