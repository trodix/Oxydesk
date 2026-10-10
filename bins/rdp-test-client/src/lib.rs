//! RDP test client: scenarios with deterministic oracles, writes `client.json`.

pub mod audio;
pub mod contract;
#[cfg(feature = "ironrdp")]
pub mod ironrdp_backend;
pub mod mock;
pub mod report;
pub mod scenarios;
pub mod session;
pub mod stats;
