//! Adaptateurs `CaptureSource`. Pour l'instant : source synthétique (mode test).

mod synthetic;

pub use synthetic::{
    SyntheticDesktop, SyntheticSource, BLOCK, HEARTBEAT_POS, KEYS, MARKER_POS, PALETTE,
};

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::KwinCapture;
