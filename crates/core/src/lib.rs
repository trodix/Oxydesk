//! Server domain and ports. No heavy technical dependency here:
//! adapters (capture, encoding, RDP, ...) implement these traits.

pub mod cursor;
pub mod dmabuf;
pub mod error;
pub mod frame;
pub mod layout;
pub mod pixels;
pub mod ports;
pub mod scale;
pub mod throttle;
pub mod wake;

pub use cursor::{CursorResolver, CursorShape, CursorUpdate};
pub use error::{AudioError, CaptureError, ClipboardError, EncodeError, InputError, LoginError};
pub use frame::{DmaBuf, Frame, FrameData, PixelFormat};
pub use layout::{CoordinateMapper, Layout, OutputId, OutputInfo};
pub use scale::ScaleMap;
pub use throttle::{LoginThrottle, ThrottleConfig};
