//! Backend selection: hardware (VA-API) when available, software (openh264) otherwise.
//!
//! The VA-API encoder needs the frame size at creation, so the choice is made lazily on the first
//! frame. `Auto` falls back to software when VA-API cannot be initialized; `Vaapi` and `OpenH264`
//! never fall back.
use std::str::FromStr;

use rdsktp_core::error::EncodeError;
use rdsktp_core::frame::Frame;
use rdsktp_core::ports::{EncodedFrame, FrameEncoder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    #[default]
    Auto,
    Vaapi,
    OpenH264,
}

impl FromStr for EncoderChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "vaapi" => Ok(Self::Vaapi),
            "openh264" => Ok(Self::OpenH264),
            other => Err(format!("unknown encoder '{other}' (auto|vaapi|openh264)")),
        }
    }
}

impl EncoderChoice {
    /// Whether the capture should offer DMA-BUF buffers: only a GPU encoder can use them without a
    /// slow CPU read-back.
    pub fn wants_dmabuf(self) -> bool {
        match self {
            Self::OpenH264 => false,
            Self::Vaapi => true,
            #[cfg(feature = "vaapi")]
            Self::Auto => crate::vaapi_available(),
            #[cfg(not(feature = "vaapi"))]
            Self::Auto => false,
        }
    }
}

pub struct AutoEncoder {
    choice: EncoderChoice,
    fps: f32,
    bitrate_bps: u32,
    inner: Option<Box<dyn FrameEncoder>>,
    name: &'static str,
    keyframe_pending: bool,
}

impl AutoEncoder {
    pub fn new(choice: EncoderChoice, fps: f32, bitrate_bps: u32) -> Self {
        Self {
            choice,
            fps,
            bitrate_bps,
            inner: None,
            name: "none",
            keyframe_pending: false,
        }
    }

    /// Backend in use (`"none"` until the first frame).
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[cfg(feature = "vaapi")]
    fn hardware(&self, frame: &Frame) -> Result<Box<dyn FrameEncoder>, EncodeError> {
        let cfg = crate::VaapiConfig {
            device: None,
            bitrate_bps: self.bitrate_bps,
            fps: self.fps,
        };
        Ok(Box::new(crate::VaapiEncoder::new(
            &cfg,
            frame.width,
            frame.height,
        )?))
    }

    #[cfg(not(feature = "vaapi"))]
    fn hardware(&self, _frame: &Frame) -> Result<Box<dyn FrameEncoder>, EncodeError> {
        Err(EncodeError::Unsupported(
            "built without the `vaapi` feature".into(),
        ))
    }

    #[cfg(feature = "openh264")]
    fn software(&self) -> Result<Box<dyn FrameEncoder>, EncodeError> {
        let cfg = crate::OpenH264Config {
            fps: self.fps,
            bitrate_bps: self.bitrate_bps,
            ..Default::default()
        };
        Ok(Box::new(crate::OpenH264Encoder::new(&cfg)?))
    }

    #[cfg(not(feature = "openh264"))]
    fn software(&self) -> Result<Box<dyn FrameEncoder>, EncodeError> {
        Err(EncodeError::Unsupported(
            "built without the `openh264` feature".into(),
        ))
    }

    fn init(&mut self, frame: &Frame) -> Result<(), EncodeError> {
        let (enc, name) = match self.choice {
            EncoderChoice::Vaapi => (self.hardware(frame)?, "vaapi"),
            EncoderChoice::OpenH264 => (self.software()?, "openh264"),
            EncoderChoice::Auto => match self.hardware(frame) {
                Ok(e) => (e, "vaapi"),
                Err(_) => (self.software()?, "openh264"),
            },
        };
        self.inner = Some(enc);
        self.name = name;
        Ok(())
    }
}

impl FrameEncoder for AutoEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        if self.inner.is_none() {
            self.init(frame)?;
        }
        let inner = self.inner.as_mut().expect("initialized above");
        if std::mem::take(&mut self.keyframe_pending) {
            inner.request_keyframe();
        }
        inner.encode(frame)
    }

    fn request_keyframe(&mut self) {
        match self.inner.as_mut() {
            Some(e) => e.request_keyframe(),
            None => self.keyframe_pending = true,
        }
    }
}
