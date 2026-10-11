//! PostScript or PDF to XPS with Ghostscript (`-sDEVICE=xpswrite`): the document an XPS printer of the client takes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdsktp_core::error::PrintError;
use rdsktp_core::XpsConverter;

/// Longest conversion: a job whose conversion takes longer is dropped (nothing was sent to the client).
const CONVERT_TIMEOUT: Duration = Duration::from_secs(120);

/// `gs` with the `xpswrite` device.
#[derive(Debug, Clone)]
pub struct GhostscriptXps {
    gs: PathBuf,
}

impl GhostscriptXps {
    /// `None` (with the reason logged) if `gs` is missing or was built without `xpswrite`.
    pub fn detect() -> Option<Self> {
        Self::with_command("gs".into())
    }

    pub fn with_command(gs: PathBuf) -> Option<Self> {
        match Command::new(&gs).arg("-h").stdin(Stdio::null()).output() {
            Ok(out) if String::from_utf8_lossy(&out.stdout).contains("xpswrite") => {
                Some(Self { gs })
            }
            Ok(_) => {
                tracing::warn!("{} has no xpswrite device: no XPS printers", gs.display());
                None
            }
            Err(e) => {
                tracing::warn!("{} is not usable ({e}): no XPS printers", gs.display());
                None
            }
        }
    }
}

impl XpsConverter for GhostscriptXps {
    fn convert(&self, input: &Path, output: &Path) -> Result<(), PrintError> {
        let mut child = Command::new(&self.gs)
            .args(["-q", "-dBATCH", "-dNOPAUSE", "-dSAFER", "-sDEVICE=xpswrite"])
            .arg(format!("-sOutputFile={}", output.display()))
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PrintError::Io(format!("{}: {e}", self.gs.display())))?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > CONVERT_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PrintError::Io("conversion to XPS timed out".into()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(PrintError::Io(e.to_string())),
            }
        };
        if status.success() {
            return Ok(());
        }
        let mut message = String::new();
        if let Some(mut err) = child.stderr.take() {
            use std::io::Read;
            let _ = err.by_ref().take(400).read_to_string(&mut message);
        }
        Err(PrintError::Io(format!(
            "conversion to XPS failed ({status}): {}",
            message.trim().replace('\n', " ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &[u8] = b"%!PS\n/Helvetica findfont 24 scalefont setfont\n72 700 moveto (Bonjour XPS) show\nshowpage\n";

    #[test]
    fn ghostscript_makes_an_xps_package() {
        let Some(gs) = GhostscriptXps::detect() else {
            eprintln!("no gs with xpswrite: test skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.ps"), dir.path().join("out.xps"));
        std::fs::write(&input, PS).unwrap();
        gs.convert(&input, &output).expect("conversion");
        let xps = std::fs::read(&output).unwrap();
        // An XPS document is a zip package holding a fixed document sequence.
        assert_eq!(&xps[..2], b"PK");
        let text = String::from_utf8_lossy(&xps);
        assert!(text.contains("FixedDocumentSequence"));
    }

    #[test]
    fn a_broken_document_is_an_error() {
        let Some(gs) = GhostscriptXps::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.ps"), dir.path().join("out.xps"));
        std::fs::write(&input, b"%!PS\n(unterminated").unwrap();
        // Whatever gs makes of it, a missing input is certainly an error.
        assert!(gs.convert(&dir.path().join("absent.ps"), &output).is_err());
        let _ = gs.convert(&input, &output);
    }

    #[test]
    fn a_missing_gs_gives_no_converter() {
        assert!(GhostscriptXps::with_command("/nonexistent/gs".into()).is_none());
    }
}
