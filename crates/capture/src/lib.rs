//! `CaptureSource` adapters. For now: synthetic source (test mode).

mod synthetic;

pub use synthetic::{
    SyntheticDesktop, SyntheticSource, BLOCK, CORNER_COLOR, HEARTBEAT_POS, KEYS, MARKER_POS,
    PALETTE,
};

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::KwinCapture;

mod multi;
pub use multi::MultiCapture;
