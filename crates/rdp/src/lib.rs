//! Primary RDP adapter: translates IronRDP into calls to the `oxydesk-core` ports.
//! The only crate that knows IronRDP on the server side.

mod changed_area;
pub mod clipboard;
mod clipfiles;
mod codec;
pub mod dib;
pub mod drive;
mod greeter;
mod greeter_theme;
pub mod printer;
mod progressive;
mod pump;
mod raw;
mod reconnect;
mod server;
mod sound;

pub use clipboard::{Content, RdpClipboard};
pub use codec::Codec;
pub use greeter::GreeterEncoder;
pub use greeter_theme::Theme as GreeterTheme;
pub use server::{run, run_with_broker, Ports, ServerConfig};
