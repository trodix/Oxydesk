//! Primary RDP adapter: translates IronRDP into calls to the `rdsktp-core` ports.
//! The only crate that knows IronRDP on the server side.

pub mod clipboard;
mod pump;
mod raw;
mod server;
mod sound;

pub use clipboard::TextClipboard;
pub use server::{run, run_with_broker, Ports, ServerConfig};
