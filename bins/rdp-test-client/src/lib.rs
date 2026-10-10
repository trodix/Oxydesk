//! Client de test RDP : scénarios à oracles déterministes, écrit `client.json`.

pub mod contract;
#[cfg(feature = "ironrdp")]
pub mod ironrdp_backend;
pub mod mock;
pub mod report;
pub mod scenarios;
pub mod session;
pub mod stats;
