//! Backend selection: hardware (VA-API) when available, software (openh264) otherwise.
//!
//! The VA-API encoder needs the frame size at creation, so the choice is made lazily on the first
//! frame. `Auto` falls back to software when VA-API cannot be initialized; `Vaapi` and `OpenH264`
//! never fall back.
use std::str::FromStr;

use oxydesk_core::error::EncodeError;
use oxydesk_core::frame::Frame;
use oxydesk_core::ports::{EncodedFrame, FrameEncoder};
use oxydesk_core::ColorDepth;

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
    size: (u32, u32),
    depth: ColorDepth,
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
            size: (0, 0),
            depth: ColorDepth::default(),
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
        let mut enc = enc;
        enc.set_color_depth(self.depth);
        self.inner = Some(enc);
        self.name = name;
        self.size = (frame.width, frame.height);
        Ok(())
    }
}

impl FrameEncoder for AutoEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        // Desktop resize: the codec is created again for the new size (it starts on a keyframe).
        if self.inner.is_none() || self.size != (frame.width, frame.height) {
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

    fn set_color_depth(&mut self, depth: ColorDepth) {
        // Remembered for an encoder created later (resize).
        self.depth = depth;
        if let Some(e) = self.inner.as_mut() {
            e.set_color_depth(depth);
        }
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        // Remembered for an encoder created later (resize).
        self.bitrate_bps = bitrate_bps;
        if let Some(e) = self.inner.as_mut() {
            e.set_bitrate(bitrate_bps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::frame::{FrameData, PixelFormat};
    use oxydesk_core::layout::OutputId;

    fn frame(w: u32, h: u32) -> Frame {
        Frame {
            output: OutputId(0),
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra8,
            data: FrameData::Shm(vec![90; (w * h * 4) as usize]),
        }
    }

    #[test]
    fn the_choice_is_read_from_its_name_and_unknown_names_list_the_valid_ones() {
        assert_eq!("auto".parse(), Ok(EncoderChoice::Auto));
        assert_eq!("vaapi".parse(), Ok(EncoderChoice::Vaapi));
        assert_eq!("openh264".parse(), Ok(EncoderChoice::OpenH264));
        let e = "x264".parse::<EncoderChoice>().unwrap_err();
        assert!(e.contains("x264") && e.contains("auto|vaapi|openh264"));
        assert_eq!(EncoderChoice::default(), EncoderChoice::Auto);
    }

    #[test]
    fn only_a_gpu_encoder_asks_for_dmabuf_buffers() {
        assert!(!EncoderChoice::OpenH264.wants_dmabuf());
        assert!(EncoderChoice::Vaapi.wants_dmabuf());
    }

    #[test]
    fn nothing_is_created_before_the_first_frame() {
        let mut enc = AutoEncoder::new(EncoderChoice::OpenH264, 30.0, 1_000_000);
        assert_eq!(enc.name(), "none");
        // Requests made early are remembered, not lost nor fatal.
        enc.request_keyframe();
        assert!(enc.keyframe_pending);
        enc.set_bitrate(2_000_000);
        assert_eq!(enc.bitrate_bps, 2_000_000);
        assert!(enc.inner.is_none());
    }

    #[cfg(not(feature = "vaapi"))]
    #[test]
    fn a_forced_backend_that_is_missing_fails_instead_of_falling_back() {
        let mut enc = AutoEncoder::new(EncoderChoice::Vaapi, 30.0, 1_000_000);
        assert!(matches!(
            enc.encode(&frame(64, 64)),
            Err(EncodeError::Unsupported(_))
        ));
        assert_eq!(enc.name(), "none");
    }

    #[test]
    fn the_codec_is_created_again_when_the_desktop_size_changes() {
        let mut enc = AutoEncoder::new(EncoderChoice::OpenH264, 30.0, 1_000_000);
        if enc.encode(&frame(64, 64)).is_err() {
            eprintln!("libopenh264 missing: test skipped");
            return;
        }
        assert_eq!(enc.name(), "openh264");
        assert_eq!(enc.size, (64, 64));
        assert!(!enc.keyframe_pending);
        enc.request_keyframe();
        enc.set_bitrate(500_000);
        assert!(enc.encode(&frame(64, 64)).is_ok());
        let out = enc.encode(&frame(128, 96)).expect("new size");
        assert_eq!(enc.size, (128, 96));
        assert!(!out.data.is_empty(), "a new codec starts on a keyframe");
    }

    #[test]
    fn a_keyframe_asked_before_the_first_frame_is_applied_to_it() {
        let mut enc = AutoEncoder::new(EncoderChoice::OpenH264, 30.0, 1_000_000);
        enc.request_keyframe();
        if enc.encode(&frame(64, 64)).is_err() {
            eprintln!("libopenh264 missing: test skipped");
            return;
        }
        assert!(!enc.keyframe_pending, "consumed by the first encode");
    }
}
