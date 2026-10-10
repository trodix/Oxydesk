//! Adaptateurs `FrameEncoder`.

#[cfg(feature = "openh264")]
mod openh264_enc;

#[cfg(feature = "openh264")]
pub use openh264_enc::{OpenH264Config, OpenH264Encoder};
