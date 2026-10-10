//! Domaine et ports du serveur. Aucune dépendance technique lourde ici :
//! les adaptateurs (capture, encodage, RDP, ...) implémentent ces traits.

pub mod error;
pub mod frame;
pub mod layout;
pub mod ports;

pub use error::{CaptureError, ClipboardError, EncodeError, InputError};
pub use frame::{Frame, FrameData, PixelFormat};
pub use layout::{CoordinateMapper, Layout, OutputId, OutputInfo};
