//! Backend CLIPRDR « texte Unicode seulement », utilisable côté serveur et côté client de test.

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};

pub type SendFn = Arc<dyn Fn(ClipboardMessage) + Send + Sync>;
pub type OnText = Arc<dyn Fn(String) + Send + Sync>;

/// Texte local partagé : ce que l'on propose à l'autre extrémité.
pub type LocalText = Arc<Mutex<Option<String>>>;

pub struct TextClipboard {
    send: SendFn,
    local: LocalText,
    on_text: OnText,
}

impl std::fmt::Debug for TextClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextClipboard")
    }
}

ironrdp_core::impl_as_any!(TextClipboard);

impl TextClipboard {
    /// `send` : émet un message CLIPRDR vers la connexion ; `on_text` : texte reçu de l'autre extrémité.
    pub fn new(send: SendFn, local: LocalText, on_text: OnText) -> Self {
        Self {
            send,
            local,
            on_text,
        }
    }

    fn unicode_format() -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
    }
}

/// Annonce un nouveau texte local à l'autre extrémité (copie différée).
pub fn announce(send: &SendFn, local: &LocalText, text: String) {
    *local.lock().expect("clipboard mutex") = Some(text);
    send(ClipboardMessage::SendInitiateCopy(vec![
        TextClipboard::unicode_format(),
    ]));
}

/// Décode un CF_UNICODETEXT (UTF-16LE terminé par NUL).
pub fn decode_unicode_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

impl CliprdrBackend for TextClipboard {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let formats = if self.local.lock().expect("clipboard mutex").is_some() {
            vec![Self::unicode_format()]
        } else {
            vec![]
        };
        (self.send)(ClipboardMessage::SendInitiateCopy(formats));
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT)
        {
            (self.send)(ClipboardMessage::SendInitiatePaste(
                ClipboardFormatId::CF_UNICODETEXT,
            ));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let text = self.local.lock().expect("clipboard mutex").clone();
        let response = match text {
            Some(t) if request.format == ClipboardFormatId::CF_UNICODETEXT => {
                FormatDataResponse::new_unicode_string(&t)
            }
            _ => FormatDataResponse::new_error(),
        };
        use ironrdp_cliprdr::pdu::OwnedFormatDataResponse;
        let owned: OwnedFormatDataResponse = ironrdp_core::IntoOwned::into_owned(response);
        (self.send)(ClipboardMessage::SendFormatData(owned));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if !response.is_error() {
            (self.on_text)(decode_unicode_text(response.data()));
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
    fn decodes_utf16_with_emoji_and_nul() {
        let mut bytes: Vec<u8> = "é€😀"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        bytes.extend([0, 0, 0x41, 0]);
        assert_eq!(decode_unicode_text(&bytes), "é€😀");
    }
}
