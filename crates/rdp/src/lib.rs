//! Adaptateur primaire RDP : traduit IronRDP en appels aux ports de `rdsktp-core`.
//! Seul crate qui connaît IronRDP côté serveur.

pub mod clipboard;
mod pump;
mod server;
mod sound;

pub use clipboard::TextClipboard;
pub use server::{run, Ports, ServerConfig};
