//! `CaptureSource` adapters. For now: synthetic source (test mode).

#[cfg(feature = "kwin")]
mod cursor_meta;
mod synthetic;

pub use synthetic::echo_copy;
pub use synthetic::{
    SyntheticDesktop, SyntheticSource, BLOCK, CORNER_COLOR, HEARTBEAT_POS, KEYS, MARKER_POS,
    PALETTE, PROBE_COLOR, PROBE_KEY,
};

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::{enabled_outputs, existing_factory, source_factory, KwinCapture};

mod existing;
pub use existing::{ExistingDesktop, ExistingOutput, Settled};

mod windows;
pub use windows::KwinWindowMover;

mod multi;
pub use multi::{MultiCapture, SourceFactory};
