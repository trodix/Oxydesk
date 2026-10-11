//! `InputSink` port adapters.

pub mod scancode;
pub mod xkb;

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::KwinFakeInput;
