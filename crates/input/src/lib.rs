//! `InputSink` port adapters.

pub mod scancode;

#[cfg(feature = "kwin")]
mod kwin;
#[cfg(feature = "kwin")]
pub use kwin::KwinFakeInput;
