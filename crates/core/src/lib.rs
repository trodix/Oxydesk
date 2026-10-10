//! Server domain and ports. No heavy technical dependency here:
//! adapters (capture, encoding, RDP, ...) implement these traits.

pub mod error;
pub mod frame;
pub mod layout;
pub mod ports;
pub mod scale;
pub mod wake;

pub use error::{AudioError, CaptureError, ClipboardError, EncodeError, InputError, LoginError};
pub use frame::{DmaBuf, Frame, FrameData, PixelFormat};
pub use layout::{CoordinateMapper, Layout, OutputId, OutputInfo};
pub use scale::ScaleMap;
