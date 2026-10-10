//! `AudioProvider` adapters: synthetic tone (tests) and PipeWire/PulseAudio capture (see ADR 007).

mod pulse;
mod tone;

pub use pulse::PulseCapture;
pub use tone::{ToneProvider, TONE_HZ};
