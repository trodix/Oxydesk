//! Adaptateurs `CaptureSource`. Pour l'instant : source synthétique (mode test).

mod synthetic;

pub use synthetic::{SyntheticSource, BLOCK, HEARTBEAT_POS, MARKER_POS, PALETTE};
