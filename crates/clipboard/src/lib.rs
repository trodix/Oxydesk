//! Adaptateurs du port `ClipboardBackend`.

#[cfg(feature = "wayland")]
mod wayland;
#[cfg(feature = "wayland")]
pub use wayland::WaylandClipboard;
