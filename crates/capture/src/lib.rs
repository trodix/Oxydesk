//! `CaptureSource` adapters. For now: synthetic source (test mode).

mod cursor_meta;
mod synthetic;

pub use synthetic::{
    SyntheticDesktop, SyntheticSource, BLOCK, CORNER_COLOR, HEARTBEAT_POS, KEYS, MARKER_POS,
    PALETTE,
};

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::{source_factory, KwinCapture};

mod multi;
pub use multi::{MultiCapture, SourceFactory};
