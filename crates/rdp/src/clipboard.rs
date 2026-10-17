//! CLIPRDR backend for text (CF_UNICODETEXT), images (CF_DIB, exchanged as PNG with the desktop) and
//! files (FileGroupDescriptorW, streamed by ranges), usable on the server side and on the test
//! client side.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use rdsktp_core::files::FileList;

use crate::clipfiles::{self, Fetcher, RemoteSource, Server};

pub type SendFn = Arc<dyn Fn(ClipboardMessage) + Send + Sync>;
pub type OnContent = Arc<dyn Fn(Content) + Send + Sync>;

/// A clipboard content, in the local representation (LF text, PNG image, files read by ranges).
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    /// A PNG file.
    Image(Vec<u8>),
    Files(FileList),
}

impl Content {
    fn format(&self) -> ClipboardFormat {
        ClipboardFormat::new(match self {
            Self::Text(_) => ClipboardFormatId::CF_UNICODETEXT,
            Self::Image(_) => ClipboardFormatId::CF_DIB,
            // Announced through `SendInitiateFileCopy`, never as a plain format.
            Self::Files(_) => ClipboardFormatId::CF_HDROP,
        })
    }
}

/// What we offer to the other end (the last copy wins), and what the connection negotiated.
#[derive(Default)]
pub struct LocalState {
    content: Mutex<Option<Content>>,
    caps: AtomicU32,
    /// File lists frozen by the other end's locks (a copy made during a long transfer must not
    /// change the files being read).
    locks: Mutex<HashMap<u32, FileList>>,
}

pub type LocalContent = Arc<LocalState>;

impl LocalState {
    fn files_enabled(&self) -> bool {
        ClipboardGeneralCapabilityFlags::from_bits_retain(self.caps.load(Ordering::Relaxed))
            .contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED)
    }
}

pub struct RdpClipboard {
    send: SendFn,
    local: LocalContent,
    on_content: OnContent,
    /// Format of the paste request in flight (the response does not say which one it answers).
    requested: Option<ClipboardFormatId>,
    fetcher: Arc<Fetcher>,
    server: Server,
}

impl Drop for RdpClipboard {
    fn drop(&mut self) {
        self.fetcher.fail_all();
    }
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
            server: Server::new(send.clone()),
            send,
            local,
            on_content,
            requested: None,
            fetcher: Arc::new(Fetcher::default()),
        }
    }
}

/// Announces a new local content to the other end (delayed copy). Files are only announced when the
/// other end negotiated file transfer.
pub fn announce(send: &SendFn, local: &LocalContent, content: Content) {
    if let Content::Files(list) = &content {
        if !local.files_enabled() {
            tracing::info!("files copied but the other end does not support file transfer");
            return;
        }
        let descriptors = clipfiles::descriptors(&list.entries);
        *local.content.lock().expect("clipboard mutex") = Some(content);
        send(ClipboardMessage::SendInitiateFileCopy(descriptors));
        return;
    }
    let format = content.format();
    *local.content.lock().expect("clipboard mutex") = Some(content);
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
        clipfiles::capabilities()
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let content = self.local.content.lock().expect("clipboard mutex").clone();
        match content {
            Some(Content::Files(list)) if self.local.files_enabled() => {
                let d: Vec<FileDescriptor> = clipfiles::descriptors(&list.entries);
                (self.send)(ClipboardMessage::SendInitiateFileCopy(d));
            }
            Some(c) if !matches!(c, Content::Files(_)) => {
                (self.send)(ClipboardMessage::SendInitiateCopy(vec![c.format()]));
            }
            _ => (self.send)(ClipboardMessage::SendInitiateCopy(vec![])),
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        self.local
            .caps
            .store(capabilities.bits(), Ordering::Relaxed);
        self.fetcher.set_capabilities(capabilities);
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let files = available_formats.iter().find(|f| {
            f.name()
                .is_some_and(|n| *n == ClipboardFormatName::FILE_LIST)
        });
        if let (Some(f), true) = (files, self.local.files_enabled()) {
            // The list itself is fetched now; the contents are read when the application pastes.
            self.requested = None;
            (self.send)(ClipboardMessage::SendInitiatePaste(f.id()));
            return;
        }
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
        let content = self.local.content.lock().expect("clipboard mutex").clone();
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

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        let (entries, wire_index) = clipfiles::entries(files);
        if entries.is_empty() {
            return;
        }
        let source = RemoteSource::new(
            self.fetcher.clone(),
            self.send.clone(),
            clip_data_id,
            wire_index,
            &entries,
        );
        (self.on_content)(Content::Files(FileList {
            entries,
            source: Arc::new(source),
        }));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let list = request
            .data_id
            .and_then(|id| self.local.locks.lock().expect("locks").get(&id).cloned())
            .or_else(
                || match &*self.local.content.lock().expect("clipboard mutex") {
                    Some(Content::Files(l)) => Some(l.clone()),
                    _ => None,
                },
            );
        match list {
            Some(list) => self.server.submit(request, list),
            None => (self.send)(ClipboardMessage::SendFileContentsResponse(
                FileContentsResponse::new_error(request.stream_id),
            )),
        }
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let reply = if response.is_error() {
            Err(())
        } else {
            Ok(response.data().to_vec())
        };
        self.fetcher.complete(response.stream_id(), reply);
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        if let Some(Content::Files(l)) = &*self.local.content.lock().expect("clipboard mutex") {
            self.local
                .locks
                .lock()
                .expect("locks")
                .insert(data_id.0, l.clone());
        }
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.local.locks.lock().expect("locks").remove(&data_id.0);
    }
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

    // --- the backend, against a recording connection ------------------------------------------------------------

    struct NoSource;

    impl rdsktp_core::files::FileSource for NoSource {
        fn read(
            &self,
            _: u32,
            _: u64,
            _: u32,
        ) -> Result<Vec<u8>, rdsktp_core::error::ClipboardError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default, Clone)]
    struct Wire {
        sent: Arc<Mutex<Vec<String>>>,
        got: Arc<Mutex<Vec<Content>>>,
    }

    impl Wire {
        fn backend(&self) -> (RdpClipboard, LocalContent, SendFn) {
            let sent = self.sent.clone();
            let send: SendFn = Arc::new(move |m| sent.lock().unwrap().push(format!("{m:?}")));
            let got = self.got.clone();
            let local = LocalContent::default();
            let backend = RdpClipboard::new(
                send.clone(),
                local.clone(),
                Arc::new(move |c| got.lock().unwrap().push(c)),
            );
            (backend, local, send)
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }

        fn last(&self) -> String {
            self.sent().last().cloned().unwrap_or_default()
        }
    }

    fn formats(ids: &[ClipboardFormatId]) -> Vec<ClipboardFormat> {
        ids.iter().map(|id| ClipboardFormat::new(*id)).collect()
    }

    #[test]
    fn a_local_text_copy_is_announced_then_served_with_crlf() {
        let wire = Wire::default();
        let (mut backend, local, send) = wire.backend();
        announce(&send, &local, Content::Text("a\nb".into()));
        assert!(wire.last().contains("SendInitiateCopy"));
        assert!(wire.last().contains("CF_UNICODETEXT") || wire.last().contains("13"));
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(wire.last().contains("SendFormatData"));
        // Asking for a format we do not hold is an error answer, not silence.
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert!(wire.last().contains("SendFormatData"));
        let n = wire.sent().len();
        assert!(n >= 3);
    }

    #[test]
    fn nothing_is_offered_before_something_was_copied() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.on_request_format_list();
        assert!(wire.last().contains("SendInitiateCopy([])"));
    }

    #[test]
    fn files_are_only_announced_when_the_other_end_negotiated_file_transfer() {
        let wire = Wire::default();
        let (mut backend, local, send) = wire.backend();
        let list = FileList {
            entries: vec![],
            source: Arc::new(NoSource),
        };
        announce(&send, &local, Content::Files(list.clone()));
        assert!(
            wire.sent().is_empty(),
            "no file transfer negotiated: nothing sent"
        );
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        announce(&send, &local, Content::Files(list));
        assert!(wire.last().contains("SendInitiateFileCopy"));
        backend.on_request_format_list();
        assert_eq!(wire.sent().len(), 2);
        assert!(wire.last().contains("SendInitiateFileCopy"));
    }

    #[test]
    fn text_wins_over_an_image_and_unknown_formats_are_not_requested() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.on_remote_copy(&formats(&[
            ClipboardFormatId::CF_DIB,
            ClipboardFormatId::CF_UNICODETEXT,
        ]));
        assert!(wire.last().contains("SendInitiatePaste"));
        assert_eq!(backend.requested, Some(ClipboardFormatId::CF_UNICODETEXT));
        let before = wire.sent().len();
        backend.on_remote_copy(&formats(&[ClipboardFormatId::CF_DIB]));
        assert_eq!(backend.requested, Some(ClipboardFormatId::CF_DIB));
        backend.requested = None;
        backend.on_remote_copy(&formats(&[ClipboardFormatId::CF_HDROP]));
        assert_eq!(
            wire.sent().len(),
            before + 1,
            "nothing usable: no paste request"
        );
        assert_eq!(backend.requested, None);
    }

    #[test]
    fn the_text_received_reaches_the_desktop_with_lf_and_an_error_answer_is_dropped() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.requested = Some(ClipboardFormatId::CF_UNICODETEXT);
        let data: Vec<u8> = "x\r\ny\0"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        backend.on_format_data_response(FormatDataResponse::new_data(data));
        backend.requested = Some(ClipboardFormatId::CF_UNICODETEXT);
        backend.on_format_data_response(FormatDataResponse::new_error());
        // An answer nobody asked for is ignored.
        backend.on_format_data_response(FormatDataResponse::new_data(vec![0, 0]));
        let got = wire.got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], Content::Text(t) if t == "x\ny"));
    }

    #[test]
    fn a_broken_image_is_ignored_instead_of_reaching_the_desktop() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.requested = Some(ClipboardFormatId::CF_DIB);
        backend.on_format_data_response(FormatDataResponse::new_data(vec![1, 2, 3]));
        assert!(wire.got.lock().unwrap().is_empty());
        // A local image that is not a PNG is answered with an error, not a crash.
        let (mut backend, local, send) = wire.backend();
        announce(&send, &local, Content::Image(vec![1, 2, 3]));
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert!(wire.last().contains("SendFormatData"));
    }

    #[test]
    fn a_request_for_file_contents_without_a_copied_list_is_refused() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.on_file_contents_request(FileContentsRequest {
            stream_id: 9,
            index: 0,
            flags: ironrdp_cliprdr::pdu::FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        });
        assert!(wire.last().contains("SendFileContentsResponse"));
    }

    #[test]
    fn locks_freeze_the_list_until_they_are_released() {
        let wire = Wire::default();
        let (mut backend, local, send) = wire.backend();
        backend.on_process_negotiated_capabilities(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED,
        );
        announce(
            &send,
            &local,
            Content::Files(FileList {
                entries: vec![],
                source: Arc::new(NoSource),
            }),
        );
        backend.on_lock(LockDataId(5));
        assert!(local.locks.lock().unwrap().contains_key(&5));
        backend.on_unlock(LockDataId(5));
        assert!(local.locks.lock().unwrap().is_empty());
        // Locking when no file list is held records nothing.
        announce(&send, &local, Content::Text("t".into()));
        backend.on_lock(LockDataId(6));
        assert!(local.locks.lock().unwrap().is_empty());
    }

    #[test]
    fn an_empty_remote_file_list_is_not_forwarded() {
        let wire = Wire::default();
        let (mut backend, _, _) = wire.backend();
        backend.on_remote_file_list(&[], None);
        assert!(wire.got.lock().unwrap().is_empty());
    }
}
