//! Printers redirected by the client (RDPDR, MS-RDPEPC): a printer of the client's machine that the
//! desktop can print to.
//!
//! `RemotePrinter` is the port the desktop side uses (a socket, a CUPS queue, ...): blocking and not
//! tied to any protocol. `PrinterHost` is what the RDP side calls when the client announces or
//! removes a printer. A job is never held in memory: its bytes go to the client as they arrive.

use std::sync::Arc;

use crate::error::PrintError;

/// Longest block moved in one write (what one RDPDR request carries).
pub const MAX_PRINT_BLOCK: u32 = 1024 * 1024;

/// What the printer of the client understands. The client passes the bytes of a job to its printer
/// as they are (a raw job), so the desktop must produce this language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PrintLanguage {
    #[default]
    PostScript,
    Pcl,
    /// Plain text.
    Text,
}

impl PrintLanguage {
    /// Deduced from the name of the Windows driver the client announces (FreeRDP announces
    /// `MS Publisher Imagesetter`, a PostScript driver). Unknown drivers are taken as PostScript.
    pub fn from_driver(driver: &str) -> Self {
        let d = driver.to_ascii_lowercase();
        if d.contains("text only") || d.contains("generic / text") {
            Self::Text
        } else if d.contains("postscript") || d.contains(" ps") || d.contains("imagesetter") {
            Self::PostScript
        } else if d.contains("pcl") {
            Self::Pcl
        } else {
            Self::PostScript
        }
    }
}

/// A printer as the client announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterInfo {
    /// Name on the client's machine.
    pub name: String,
    /// Name of the Windows driver on the client.
    pub driver: String,
    pub is_default: bool,
    pub language: PrintLanguage,
}

/// A print job being sent to the client.
pub trait PrintJob: Send {
    /// Sends the next bytes of the job; returns once the client has taken them.
    fn write(&mut self, data: &[u8]) -> Result<(), PrintError>;
    /// The job is complete: the client prints it.
    fn finish(self: Box<Self>) -> Result<(), PrintError>;
    /// The job is cancelled (the source went away): the client drops it.
    fn abort(self: Box<Self>);
}

/// A printer of the client.
pub trait RemotePrinter: Send + Sync {
    /// Starts a job. `title` is informative only.
    fn begin(&self, title: &str) -> Result<Box<dyn PrintJob>, PrintError>;
}

/// Where the printers announced by the client are made available to the desktop.
pub trait PrinterHost: Send + Sync {
    /// A printer appeared; `name` is unique among the printers of the connection and made of
    /// characters safe for a file name and a CUPS queue (see [`printer_name`]). Called outside the
    /// connection thread.
    fn added(&self, name: &str, info: &PrinterInfo, printer: Arc<dyn RemotePrinter>);
    /// The printer is gone (client removed it or disconnected); its jobs must stop.
    fn removed(&self, name: &str);
}

/// Makes `name` usable as a socket name and a CUPS queue name: ASCII letters, digits, `-` and `_`.
pub fn printer_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "printer".into()
    } else {
        out.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_follow_the_driver() {
        assert_eq!(
            PrintLanguage::from_driver("MS Publisher Imagesetter"),
            PrintLanguage::PostScript
        );
        assert_eq!(
            PrintLanguage::from_driver("Generic / Text Only"),
            PrintLanguage::Text
        );
        assert_eq!(
            PrintLanguage::from_driver("HP Universal Printing PCL 6"),
            PrintLanguage::Pcl
        );
        assert_eq!(
            PrintLanguage::from_driver("Xerox Phaser PostScript"),
            PrintLanguage::PostScript
        );
        assert_eq!(
            PrintLanguage::from_driver("Brother HL-L2350DW"),
            PrintLanguage::PostScript
        );
        assert_eq!(PrintLanguage::from_driver(""), PrintLanguage::PostScript);
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(printer_name("HP LaserJet 400"), "HP_LaserJet_400");
        assert_eq!(
            printer_name("\\\\srv\\Imprimante (copie 2)"),
            "srv_Imprimante_copie_2"
        );
        assert_eq!(printer_name("../etc"), "etc");
        assert_eq!(printer_name("é"), "printer");
        assert_eq!(printer_name(""), "printer");
        assert!(printer_name(&"a".repeat(100)).len() <= 48);
    }
}
