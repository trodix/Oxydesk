//! Server domain and ports. No heavy technical dependency here:
//! adapters (capture, encoding, RDP, ...) implement these traits.

pub mod cursor;
pub mod dmabuf;
pub mod drive;
pub mod error;
pub mod files;
pub mod frame;
pub mod keyboard;
pub mod layout;
pub mod pixels;
pub mod ports;
pub mod printer;
pub mod rate;
pub mod scale;
pub mod throttle;
pub mod wake;

pub use cursor::{CursorResolver, CursorShape, CursorUpdate};
pub use drive::{DriveEntry, DriveHandle, DriveHost, DriveSpace, DriveStat, OpenMode, RemoteDrive};
pub use error::{
    AudioError, CaptureError, ClipboardError, DriveError, EncodeError, InputError, LoginError,
    PrintError,
};
pub use files::{FileEntry, FileExchange, FileList, FileSource, RangeRequest, RangeSender};
pub use frame::{DmaBuf, Frame, FrameData, PixelFormat};
pub use keyboard::{klid_to_xkb, XkbLayout};
pub use layout::{CoordinateMapper, Layout, OutputId, OutputInfo};
pub use printer::{
    document_only_driver, printer_name, resolve_format, ModeError, PrintFormat, PrintJob,
    PrintLanguage, PrintMode, PrinterHost, PrinterInfo, PrinterModes, RemotePrinter, XpsConverter,
};
pub use rate::{NetSample, RateConfig, RateController, RateTarget, DEFAULT_BITRATE_BPS};
pub use scale::ScaleMap;
pub use throttle::{LoginThrottle, ThrottleConfig};
