//! Printers of the client seen through RDPDR (MS-RDPEPC): an announcement per printer, then one job
//! as `Create` (start), `Write`s (the bytes) and `Close` (end).

use std::sync::Arc;

use ironrdp_rdpdr::pdu::efs::{
    CreateDisposition, CreateOptions, DesiredAccess, DeviceAnnounceHeader, NtStatus,
    RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII, RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER,
};
use ironrdp_server::RdpdrServerMessage;
use rdsktp_core::error::{DriveError, PrintError};
use rdsktp_core::printer::{PrintJob, PrintLanguage, PrinterInfo, RemotePrinter};

use crate::drive::{Body, Completed, Link};

/// Largest block of one write request of a job.
const PRINT_BLOCK: usize = 64 * 1024;

/// What the client announces for a printer (MS-RDPEPC 2.2.2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterAnnounce {
    pub driver: String,
    pub name: String,
    pub is_default: bool,
}

impl PrinterAnnounce {
    /// Decodes the device data of a printer announcement; `None` if it is cut short.
    pub fn decode(data: &[u8]) -> Option<Self> {
        let word = |at: usize| -> Option<usize> {
            let b = data.get(at..at + 4)?;
            Some(u32::from_le_bytes(b.try_into().ok()?) as usize)
        };
        let flags = word(0)? as u32;
        // Then the code page (ignored), the lengths of the three names and of the cached fields.
        let (pnp, driver, name) = (word(8)?, word(12)?, word(16)?);
        let ascii = flags & RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII != 0;
        let mut at = 24usize;
        let text = |len: usize, at: &mut usize| -> Option<String> {
            let bytes = data.get(*at..at.checked_add(len)?)?;
            *at += len;
            Some(decode_text(bytes, ascii))
        };
        let _ = text(pnp, &mut at)?;
        let driver = text(driver, &mut at)?;
        let name = text(name, &mut at)?;
        Some(Self {
            driver,
            name,
            is_default: flags & RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER != 0,
        })
    }

    pub fn info(&self) -> PrinterInfo {
        PrinterInfo {
            name: self.name.clone(),
            driver: self.driver.clone(),
            is_default: self.is_default,
            language: PrintLanguage::from_driver(&self.driver),
        }
    }
}

fn decode_text(bytes: &[u8], ascii: bool) -> String {
    if ascii {
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        return String::from_utf8_lossy(&bytes[..end]).into_owned();
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The announcement of a device of the `Print` type.
pub fn announced_printer(device: &DeviceAnnounceHeader) -> Option<PrinterAnnounce> {
    PrinterAnnounce::decode(device.device_data())
}

fn refused(done: &Completed, what: &str) -> PrintError {
    PrintError::Refused(format!("{what}: {:?}", done.status))
}

fn failed(e: DriveError) -> PrintError {
    PrintError::Io(e.to_string())
}

/// A printer of the client.
pub struct RdpPrinter {
    pub(crate) link: Arc<Link>,
    pub(crate) device_id: u32,
}

impl RemotePrinter for RdpPrinter {
    fn begin(&self, title: &str) -> Result<Box<dyn PrintJob>, PrintError> {
        tracing::debug!(device = self.device_id, title, "print job");
        let done = self
            .link
            .call(RdpdrServerMessage::Create {
                device_id: self.device_id,
                path: String::new(),
                desired_access: DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
                    | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
                    | DesiredAccess::SYNCHRONIZE,
                create_disposition: CreateDisposition::FILE_OPEN_IF,
                create_options: CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            })
            .map_err(failed)?;
        if done.status != NtStatus::SUCCESS {
            return Err(refused(&done, "start of the job"));
        }
        let Body::Create(file_id) = done.body else {
            return Err(PrintError::Io("unexpected answer to a job start".into()));
        };
        Ok(Box::new(RdpPrintJob {
            link: self.link.clone(),
            device_id: self.device_id,
            file_id,
            offset: 0,
        }))
    }
}

struct RdpPrintJob {
    link: Arc<Link>,
    device_id: u32,
    file_id: u32,
    offset: u64,
}

impl RdpPrintJob {
    fn close(&self) -> Result<(), PrintError> {
        let done = self
            .link
            .call(RdpdrServerMessage::Close {
                device_id: self.device_id,
                file_id: self.file_id,
            })
            .map_err(failed)?;
        if done.status == NtStatus::SUCCESS {
            Ok(())
        } else {
            Err(refused(&done, "end of the job"))
        }
    }
}

impl PrintJob for RdpPrintJob {
    fn write(&mut self, data: &[u8]) -> Result<(), PrintError> {
        for block in data.chunks(PRINT_BLOCK) {
            let done = self
                .link
                .call(RdpdrServerMessage::Write {
                    device_id: self.device_id,
                    file_id: self.file_id,
                    data: block.to_vec(),
                    offset: self.offset,
                })
                .map_err(failed)?;
            if done.status != NtStatus::SUCCESS {
                return Err(refused(&done, "data of the job"));
            }
            self.offset += block.len() as u64;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), PrintError> {
        self.close()
    }

    fn abort(self: Box<Self>) {
        // The protocol has no cancellation: closing the handle is the only way to free it.
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    fn announce(flags: u32, driver: &[u8], name: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in [flags, 0, 0, driver.len() as u32, name.len() as u32, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(driver);
        out.extend_from_slice(name);
        out
    }

    #[test]
    fn a_windows_announcement_is_read_as_utf16() {
        let data = announce(
            RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER,
            &utf16("Generic / Text Only"),
            &utf16("Imprimante é"),
        );
        let a = PrinterAnnounce::decode(&data).expect("announce");
        assert_eq!(a.driver, "Generic / Text Only");
        assert_eq!(a.name, "Imprimante é");
        assert!(a.is_default);
        assert_eq!(a.info().language, PrintLanguage::Text);
    }

    #[test]
    fn an_ascii_announcement_is_read_as_bytes() {
        let data = announce(
            RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII,
            b"MS Publisher Imagesetter\0",
            b"office\0",
        );
        let a = PrinterAnnounce::decode(&data).expect("announce");
        assert_eq!(a.driver, "MS Publisher Imagesetter");
        assert_eq!(a.name, "office");
        assert!(!a.is_default);
        assert_eq!(a.info().language, PrintLanguage::PostScript);
    }

    #[test]
    fn the_pnp_name_and_the_cached_fields_are_skipped() {
        let mut data = Vec::new();
        let pnp = utf16("USB\\VID_1234");
        let (driver, name) = (utf16("HP PCL 6"), utf16("hp"));
        for v in [
            0,
            0,
            pnp.len() as u32,
            driver.len() as u32,
            name.len() as u32,
            3,
        ] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.extend_from_slice(&pnp);
        data.extend_from_slice(&driver);
        data.extend_from_slice(&name);
        data.extend_from_slice(&[1, 2, 3]);
        let a = PrinterAnnounce::decode(&data).expect("announce");
        assert_eq!((a.driver.as_str(), a.name.as_str()), ("HP PCL 6", "hp"));
        assert_eq!(a.info().language, PrintLanguage::Pcl);
    }

    #[test]
    fn a_cut_announcement_is_refused() {
        let data = announce(0, &utf16("drv"), &utf16("name"));
        assert!(PrinterAnnounce::decode(&data[..data.len() - 3]).is_none());
        assert!(PrinterAnnounce::decode(&data[..10]).is_none());
        assert!(PrinterAnnounce::decode(&[]).is_none());
    }
}
