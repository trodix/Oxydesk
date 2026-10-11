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

/// The language of a raw job: the client passes the bytes of a job to its printer as they are, so the desktop
/// must produce a language that the printer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PrintLanguage {
    #[default]
    PostScript,
    Pcl,
    /// Plain text.
    Text,
}

impl PrintLanguage {
    /// The language that the name of a Windows driver states explicitly (FreeRDP announces
    /// `MS Publisher Imagesetter`, a PostScript driver). `None` when the name says nothing: a language is never
    /// assumed.
    pub fn from_driver(driver: &str) -> Option<Self> {
        let d = driver.to_ascii_lowercase();
        let words: Vec<&str> = d
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        if d.contains("text only") || d.contains("generic / text") {
            Some(Self::Text)
        } else if d.contains("postscript")
            || d.contains("imagesetter")
            || d.contains("laserwriter")
            || words.contains(&"ps")
        {
            Some(Self::PostScript)
        } else if d.contains("pcl") {
            Some(Self::Pcl)
        } else {
            None
        }
    }

    /// `ps`, `pcl` or `text` (the spelling of `--printer-mode`).
    fn from_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "ps" | "postscript" => Some(Self::PostScript),
            "pcl" => Some(Self::Pcl),
            "text" | "txt" => Some(Self::Text),
            _ => None,
        }
    }
}

/// Drivers that make a document rather than drive a device (PDF, XPS, fax, notes): Windows refuses a raw job.
pub fn document_only_driver(driver: &str) -> bool {
    let d = driver.to_ascii_lowercase();
    ["pdf", "xps", "onenote", "fax"]
        .iter()
        .any(|w| d.contains(w))
}

/// What is sent for a job of one printer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintFormat {
    /// The bytes in this language, as they are.
    Raw(PrintLanguage),
    /// An XPS document, rendered by the driver of the client (`DR_PRN_USING_XPS`).
    Xps,
}

/// How the format of a printer is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrintMode {
    /// From what the client announces and the configuration; an undecidable printer is refused.
    #[default]
    Auto,
    /// Raw jobs; the language is that of the driver name, or given here.
    Raw(Option<PrintLanguage>),
    /// XPS jobs; the client must have announced that it takes XPS.
    Xps,
}

/// Why no format could be chosen for a printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeError(pub String);

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModeError {}

/// The format of the jobs of a printer. Decided once, when the printer is announced: there is no fallback to
/// another format once a job is sent (the printer could print twice).
pub fn resolve_format(
    driver: &str,
    xps_offered: bool,
    mode: PrintMode,
) -> Result<PrintFormat, ModeError> {
    let known = PrintLanguage::from_driver(driver);
    match mode {
        PrintMode::Raw(language) => {
            if document_only_driver(driver) {
                return Err(ModeError(format!(
                    "driver \"{driver}\" makes documents and refuses raw jobs: use mode xps"
                )));
            }
            language.or(known).map(PrintFormat::Raw).ok_or_else(|| {
                ModeError(format!(
                    "raw mode needs a language and driver \"{driver}\" does not state one: use raw:ps, raw:pcl or raw:text"
                ))
            })
        }
        PrintMode::Xps => {
            if xps_offered {
                Ok(PrintFormat::Xps)
            } else {
                Err(ModeError(
                    "the client did not announce that it takes XPS: use mode raw".into(),
                ))
            }
        }
        PrintMode::Auto => match known {
            Some(l) if !document_only_driver(driver) => Ok(PrintFormat::Raw(l)),
            _ if xps_offered => Ok(PrintFormat::Xps),
            _ => Err(ModeError(format!(
                "driver \"{driver}\" states no print language and the client takes no XPS: set a mode, for example raw:ps"
            ))),
        },
    }
}

/// The mode of each printer: a default and rules `pattern=mode` (`*` matches anything), the first match wins.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrinterModes {
    default: PrintMode,
    rules: Vec<(String, PrintMode)>,
}

impl PrinterModes {
    /// `auto`, `raw`, `raw:pcl`, `xps`, or a comma-separated list of those, each optionally preceded by `pattern=`
    /// (`"xps,HP*=raw:pcl"`: the entry without a pattern is the default).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut modes = Self::default();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((pattern, mode)) if !pattern.trim().is_empty() => {
                    modes
                        .rules
                        .push((pattern.trim().to_owned(), parse_mode(mode)?));
                }
                Some(_) => return Err(format!("empty pattern in \"{entry}\"")),
                None => modes.default = parse_mode(entry)?,
            }
        }
        Ok(modes)
    }

    pub fn mode_for(&self, printer: &str) -> PrintMode {
        self.rules
            .iter()
            .find(|(pattern, _)| glob(pattern, printer))
            .map_or(self.default, |(_, mode)| *mode)
    }
}

fn parse_mode(text: &str) -> Result<PrintMode, String> {
    let text = text.trim();
    match text.split_once(':') {
        None => match text.to_ascii_lowercase().as_str() {
            "auto" => Ok(PrintMode::Auto),
            "raw" => Ok(PrintMode::Raw(None)),
            "xps" => Ok(PrintMode::Xps),
            _ => Err(format!(
                "unknown print mode \"{text}\" (auto, raw, raw:ps|pcl|text, xps)"
            )),
        },
        Some((raw, language)) if raw.eq_ignore_ascii_case("raw") => {
            PrintLanguage::from_word(language)
                .map(|l| PrintMode::Raw(Some(l)))
                .ok_or_else(|| format!("unknown print language \"{language}\" (ps, pcl, text)"))
        }
        Some(_) => Err(format!("unknown print mode \"{text}\"")),
    }
}

/// Case-insensitive match of `text` against a pattern whose `*` stand for any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.to_lowercase(), text.to_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == t;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !t.starts_with(first) || t.len() < first.len() + last.len() || !t.ends_with(last) {
        return false;
    }
    let mut rest = &t[first.len()..t.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// A printer as the client announces it, with the format chosen for its jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterInfo {
    /// Name on the client's machine.
    pub name: String,
    /// Name of the Windows driver on the client.
    pub driver: String,
    pub is_default: bool,
    pub format: PrintFormat,
}

/// Turns a PostScript or PDF document into XPS (the format an XPS printer of the client takes).
pub trait XpsConverter: Send + Sync {
    fn convert(&self, input: &std::path::Path, output: &std::path::Path) -> Result<(), PrintError>;
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

    use PrintFormat::{Raw, Xps};
    use PrintLanguage::{Pcl, PostScript, Text};

    #[test]
    fn languages_follow_the_driver_only_when_it_says_so() {
        let l = PrintLanguage::from_driver;
        assert_eq!(l("MS Publisher Imagesetter"), Some(PostScript));
        assert_eq!(l("Microsoft PS Class Driver"), Some(PostScript));
        assert_eq!(l("Generic / Text Only"), Some(Text));
        assert_eq!(l("HP Universal Printing PCL 6"), Some(Pcl));
        assert_eq!(l("Xerox Phaser PostScript"), Some(PostScript));
        assert_eq!(l("Brother HL-L2350DW"), None);
        assert_eq!(l("Microsoft Print To PDF"), None);
        assert_eq!(l("Canon Pops"), None);
        assert_eq!(l(""), None);
    }

    #[test]
    fn document_drivers_are_recognised() {
        assert!(document_only_driver("Microsoft Print To PDF"));
        assert!(document_only_driver("Microsoft XPS Document Writer"));
        assert!(!document_only_driver("HP Universal Printing PCL 6"));
    }

    #[test]
    fn raw_needs_a_known_language() {
        let raw = |d, l| resolve_format(d, true, PrintMode::Raw(l));
        assert_eq!(raw("HP PCL 6", None), Ok(Raw(Pcl)));
        assert_eq!(raw("Brother HL", Some(Pcl)), Ok(Raw(Pcl)));
        assert_eq!(raw("HP PCL 6", Some(PostScript)), Ok(Raw(PostScript)));
        assert!(raw("Brother HL", None).is_err());
        assert!(raw("Microsoft Print To PDF", Some(PostScript)).is_err());
    }

    #[test]
    fn xps_needs_the_client_to_offer_it() {
        assert_eq!(resolve_format("Brother", true, PrintMode::Xps), Ok(Xps));
        assert!(resolve_format("Brother", false, PrintMode::Xps).is_err());
    }

    #[test]
    fn auto_never_guesses() {
        let auto = |d, xps| resolve_format(d, xps, PrintMode::Auto);
        // A language stated by the driver: raw, as before.
        assert_eq!(auto("HP PCL 6", true), Ok(Raw(Pcl)));
        assert_eq!(auto("HP PCL 6", false), Ok(Raw(Pcl)));
        // No language: XPS if the client takes it, otherwise the printer is refused with an explanation.
        assert_eq!(auto("Brother HL", true), Ok(Xps));
        assert!(auto("Brother HL", false).unwrap_err().0.contains("raw:ps"));
        // Drivers that make documents.
        assert_eq!(auto("Microsoft Print To PDF", true), Ok(Xps));
        assert!(auto("Microsoft Print To PDF", false).is_err());
    }

    #[test]
    fn modes_are_set_per_printer() {
        let m = PrinterModes::parse("xps, HP*=raw:pcl, Zebra=raw, *PDF=auto").unwrap();
        assert_eq!(m.mode_for("HP LaserJet"), PrintMode::Raw(Some(Pcl)));
        assert_eq!(m.mode_for("hp"), PrintMode::Raw(Some(Pcl)));
        assert_eq!(m.mode_for("Zebra"), PrintMode::Raw(None));
        assert_eq!(m.mode_for("Zebra 2"), PrintMode::Xps);
        assert_eq!(m.mode_for("Print to PDF"), PrintMode::Auto);
        assert_eq!(PrinterModes::default().mode_for("x"), PrintMode::Auto);
        assert_eq!(PrinterModes::parse("").unwrap(), PrinterModes::default());
        assert!(PrinterModes::parse("fast").is_err());
        assert!(PrinterModes::parse("raw:cobol").is_err());
        assert!(PrinterModes::parse("=xps").is_err());
    }

    #[test]
    fn patterns_match_inside() {
        assert!(glob("a*c*e", "abcde"));
        assert!(!glob("a*c*e", "abde"));
        assert!(glob("*", ""));
        assert!(!glob("ab*ba", "aba"));
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
