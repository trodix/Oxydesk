//! Adaptateurs `FrameEncoder`.

#[cfg(feature = "openh264")]
mod openh264_enc;

#[cfg(feature = "openh264")]
pub use openh264_enc::{OpenH264Config, OpenH264Encoder};

mod h264_headers;

#[cfg(any(feature = "vaapi", feature = "openh264"))]
mod auto;
#[cfg(any(feature = "vaapi", feature = "openh264"))]
pub use auto::{AutoEncoder, EncoderChoice};

#[cfg(feature = "vaapi")]
mod vaapi_enc;

#[cfg(feature = "vaapi")]
pub use vaapi_enc::{default_device, VaapiConfig, VaapiEncoder};
