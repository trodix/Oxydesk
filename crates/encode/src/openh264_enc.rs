use std::path::PathBuf;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::OpenH264API;
use rdsktp_core::error::EncodeError;
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::ports::{EncodedFrame, FrameEncoder};

#[derive(Debug, Clone)]
pub struct OpenH264Config {
    /// Chemin de libopenh264 ; sinon `RDSKTP_OPENH264_LIB`, sinon emplacements usuels.
    pub library: Option<PathBuf>,
    pub bitrate_bps: u32,
    pub fps: f32,
    /// Période d'IDR en frames (0 = automatique).
    pub idr_period: u32,
}

impl Default for OpenH264Config {
    fn default() -> Self {
        Self {
            library: None,
            bitrate_bps: 8_000_000,
            fps: 30.0,
            idr_period: 0,
        }
    }
}

const CANDIDATES: &[&str] = &[
    "/usr/lib64/libopenh264.so.8",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
    "/usr/lib64/libopenh264.so",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so",
];

fn locate(cfg: &OpenH264Config) -> Result<PathBuf, EncodeError> {
    if let Some(p) = &cfg.library {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("RDSKTP_OPENH264_LIB") {
        return Ok(p.into());
    }
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| EncodeError::Backend("libopenh264 introuvable (installer openh264 / libopenh264-8 ou définir RDSKTP_OPENH264_LIB)".into()))
}

/// Encodeur H.264 logiciel, flux Annex-B, sans B-frames, faible latence.
pub struct OpenH264Encoder {
    inner: Encoder,
}

impl OpenH264Encoder {
    pub fn new(cfg: &OpenH264Config) -> Result<Self, EncodeError> {
        let path = locate(cfg)?;
        // SAFETY: le chemin est choisi par l'administrateur / les emplacements système ; la
        // vérification de hash du crate refuserait les builds de distribution (Fedora/Debian).
        let api = unsafe { OpenH264API::from_blob_path_unchecked(&path) }
            .map_err(|e| EncodeError::Backend(format!("chargement de {} : {e}", path.display())))?;
        let period = if cfg.idr_period == 0 {
            IntraFramePeriod::auto()
        } else {
            IntraFramePeriod::from_num_frames(cfg.idr_period)
        };
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(cfg.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(cfg.fps))
            .complexity(Complexity::Low)
            .skip_frames(false)
            .intra_frame_period(period);
        let inner = Encoder::with_api_config(api, config)
            .map_err(|e| EncodeError::Backend(e.to_string()))?;
        Ok(Self { inner })
    }

    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        let FrameData::Shm(data) = &frame.data else {
            return Err(EncodeError::Unsupported(
                "DmaBuf non géré par le backend logiciel".into(),
            ));
        };
        if frame.format != PixelFormat::Rgba8 || frame.stride != frame.width * 4 {
            return Err(EncodeError::Unsupported("RGBA compact attendu".into()));
        }
        let rgba = RgbaSliceU8::new(data, (frame.width as usize, frame.height as usize));
        let yuv = YUVBuffer::from_rgba8_source(rgba);
        let stream = self
            .inner
            .encode(&yuv)
            .map_err(|e| EncodeError::Backend(e.to_string()))?;
        Ok(EncodedFrame {
            output: Some(frame.output),
            keyframe: matches!(
                stream.frame_type(),
                openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
            ),
            data: stream.to_vec(),
        })
    }
}
