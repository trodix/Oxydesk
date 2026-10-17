//! Primary RDP adapter: translates IronRDP into calls to the `rdsktp-core` ports.
//! The only crate that knows IronRDP on the server side.

pub mod clipboard;
mod clipfiles;
pub mod dib;
pub mod drive;
pub mod printer;
mod pump;
mod raw;
mod reconnect;
mod server;
mod sound;

pub use clipboard::{Content, RdpClipboard};
pub use server::{run, run_with_broker, Ports, ServerConfig};
