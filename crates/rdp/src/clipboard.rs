//! CLIPRDR backend for text (CF_UNICODETEXT) and images (CF_DIB, exchanged as PNG with the desktop),
//! usable on the server side and on the test client side.

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};

pub type SendFn = Arc<dyn Fn(ClipboardMessage) + Send + Sync>;
pub type OnContent = Arc<dyn Fn(Content) + Send + Sync>;

/// A clipboard content, in the local representation (LF text, PNG image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// A PNG file.
    Image(Vec<u8>),
}

impl Content {
    fn format(&self) -> ClipboardFormat {
        ClipboardFormat::new(match self {
            Self::Text(_) => ClipboardFormatId::CF_UNICODETEXT,
            Self::Image(_) => ClipboardFormatId::CF_DIB,
        })
    }
}

/// Shared local content: what we offer to the other end (the last copy wins).
pub type LocalContent = Arc<Mutex<Option<Content>>>;

pub struct RdpClipboard {
    send: SendFn,
    local: LocalContent,
    on_content: OnContent,
    /// Format of the paste request in flight (the response does not say which one it answers).
    requested: Option<ClipboardFormatId>,
}

impl std::fmt::Debug for RdpClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RdpClipboard")
    }
}

ironrdp_core::impl_as_any!(RdpClipboard);

impl RdpClipboard {
    /// `send`: emits a CLIPRDR message to the connection; `on_content`: content received from the other end.
    pub fn new(send: SendFn, local: LocalContent, on_content: OnContent) -> Self {
        Self {
            send,
            local,
            on_content,
            requested: None,
        }
    }
}

/// Announces a new local content to the other end (delayed copy).
pub fn announce(send: &SendFn, local: &LocalContent, content: Content) {
    let format = content.format();
    *local.lock().expect("clipboard mutex") = Some(content);
    send(ClipboardMessage::SendInitiateCopy(vec![format]));
}

/// Local line endings (LF) -> CF_UNICODETEXT (CRLF), without doubling the CRLFs already present.
pub fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// CF_UNICODETEXT (CRLF) -> local line endings (LF).
pub fn from_crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Decodes a CF_UNICODETEXT (NUL-terminated UTF-16LE), line endings as LF.
pub fn decode_unicode_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    from_crlf(&String::from_utf16_lossy(&units[..end]))
}

impl CliprdrBackend for RdpClipboard {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let formats = match &*self.local.lock().expect("clipboard mutex") {
            Some(c) => vec![c.format()],
            None => vec![],
        };
        (self.send)(ClipboardMessage::SendInitiateCopy(formats));
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Text wins over an image (an application often offers both).
        let has = |id| available_formats.iter().any(|f| f.id() == id);
        let wanted = if has(ClipboardFormatId::CF_UNICODETEXT) {
            ClipboardFormatId::CF_UNICODETEXT
        } else if has(ClipboardFormatId::CF_DIB) {
            ClipboardFormatId::CF_DIB
        } else {
            return;
        };
        self.requested = Some(wanted);
        (self.send)(ClipboardMessage::SendInitiatePaste(wanted));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let content = self.local.lock().expect("clipboard mutex").clone();
        let response = match content {
            Some(Content::Text(t)) if request.format == ClipboardFormatId::CF_UNICODETEXT => {
                FormatDataResponse::new_unicode_string(&to_crlf(&t))
            }
            Some(Content::Image(png)) if request.format == ClipboardFormatId::CF_DIB => {
                match crate::dib::png_to_dib(&png) {
                    Ok(dib) => FormatDataResponse::new_data(dib),
                    Err(e) => {
                        tracing::warn!("clipboard image not sent: {e}");
                        FormatDataResponse::new_error()
                    }
                }
            }
            _ => FormatDataResponse::new_error(),
        };
        use ironrdp_cliprdr::pdu::OwnedFormatDataResponse;
        let owned: OwnedFormatDataResponse = ironrdp_core::IntoOwned::into_owned(response);
        (self.send)(ClipboardMessage::SendFormatData(owned));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let requested = self.requested.take();
        if response.is_error() {
            return;
        }
        match requested {
            Some(ClipboardFormatId::CF_UNICODETEXT) => {
                (self.on_content)(Content::Text(decode_unicode_text(response.data())))
            }
            Some(ClipboardFormatId::CF_DIB) => match crate::dib::dib_to_png(response.data()) {
                Ok(png) => (self.on_content)(Content::Image(png)),
                Err(e) => tracing::warn!("clipboard image ignored: {e}"),
            },
            _ => {}
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_roundtrip_without_doubling() {
        assert_eq!(to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(from_crlf("a\r\nb\nc"), "a\nb\nc");
        assert_eq!(from_crlf(&to_crlf("l1\nl2\n")), "l1\nl2\n");
    }

    #[test]
    fn decodes_utf16_with_emoji_and_nul() {
        let mut bytes: Vec<u8> = "é€😀"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        bytes.extend([0, 0, 0x41, 0]);
        assert_eq!(decode_unicode_text(&bytes), "é€😀");
    }
}
