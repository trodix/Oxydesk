//! Adaptateurs `AudioProvider` : tonalité synthétique (tests) et capture PipeWire/PulseAudio (voir ADR 007).

mod pulse;
mod tone;

pub use pulse::PulseCapture;
pub use tone::{ToneProvider, TONE_HZ};
