//! `ClipboardBackend` port adapters.

#[cfg(feature = "wayland")]
mod wayland;
#[cfg(feature = "wayland")]
pub use wayland::WaylandClipboard;

/// Degraded adapter used when the compositor offers no clipboard protocol: the server keeps running
/// without clipboard sharing.
#[derive(Debug, Default)]
pub struct UnavailableClipboard;

impl rdsktp_core::ports::ClipboardBackend for UnavailableClipboard {
    fn set_text(&mut self, _text: &str) -> Result<(), rdsktp_core::error::ClipboardError> {
        Ok(())
    }
    fn poll_text(&mut self) -> Result<Option<String>, rdsktp_core::error::ClipboardError> {
        Ok(None)
    }
}
