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
    /// Path of libopenh264; otherwise `RDSKTP_OPENH264_LIB`, otherwise the usual locations.
    pub library: Option<PathBuf>,
    pub bitrate_bps: u32,
    pub fps: f32,
    /// IDR period in frames (0 = automatic).
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
        .ok_or_else(|| EncodeError::Backend("libopenh264 not found (install openh264 / libopenh264-8 or set RDSKTP_OPENH264_LIB)".into()))
}

/// Software H.264 encoder, Annex-B stream, no B-frames, low latency.
pub struct OpenH264Encoder {
    inner: Encoder,
    scratch: Vec<u8>,
}

impl OpenH264Encoder {
    pub fn new(cfg: &OpenH264Config) -> Result<Self, EncodeError> {
        let path = locate(cfg)?;
        // SAFETY: the path is chosen by the administrator / system locations; the
        // crate's hash check would reject distribution builds (Fedora/Debian).
        let api = unsafe { OpenH264API::from_blob_path_unchecked(&path) }
            .map_err(|e| EncodeError::Backend(format!("loading {}: {e}", path.display())))?;
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
        Ok(Self { inner, scratch: Vec::new() })
    }

    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn request_keyframe(&mut self) {
        self.force_keyframe();
    }

    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        let data = match &frame.data {
            FrameData::Shm(d) => d,
            FrameData::DmaBuf(dma) => {
                // No GPU path here: copy the pixels out of the buffer and encode them as SHM.
                let need = frame.stride as usize * frame.height as usize;
                let copy = crate::dmabuf::with_pixels(frame, dma, |p| p[..need].to_vec())?;
                let shm = Frame { output: frame.output, width: frame.width, height: frame.height, stride: frame.stride, format: frame.format, data: FrameData::Shm(copy) };
                return self.encode(&shm);
            }
            FrameData::Encoded { .. } => return Err(EncodeError::Unsupported("already-encoded frame".into())),
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let compact = frame.format == PixelFormat::Rgba8 && frame.stride as usize == w * 4;
        let rgba = if compact {
            RgbaSliceU8::new(data, (w, h))
        } else {
            // BGRA and/or non-compact stride (PipeWire): normalization to compact RGBA.
            let stride = frame.stride as usize;
            if data.len() < stride * h || stride < w * 4 {
                return Err(EncodeError::Unsupported("buffer too short for stride/height".into()));
            }
            self.scratch.clear();
            self.scratch.reserve(w * h * 4);
            for row in data.chunks_exact(stride).take(h) {
                for px in row[..w * 4].chunks_exact(4) {
                    match frame.format {
                        PixelFormat::Bgra8 => self.scratch.extend_from_slice(&[px[2], px[1], px[0], 255]),
                        PixelFormat::Rgba8 => self.scratch.extend_from_slice(px),
                    }
                }
            }
            RgbaSliceU8::new(&self.scratch, (w, h))
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use rdsktp_core::layout::OutputId;

    #[test]
    fn bgra_with_padded_stride_encodes() {
        let Ok(mut enc) = OpenH264Encoder::new(&OpenH264Config::default()) else {
            eprintln!("libopenh264 missing: test skipped");
            return;
        };
        let (w, h, stride) = (64u32, 64u32, 64 * 4 + 32);
        let frame = Frame {
            output: OutputId(0),
            width: w,
            height: h,
            stride,
            format: PixelFormat::Bgra8,
            data: FrameData::Shm(vec![128; (stride * h) as usize]),
        };
        let out = enc.encode(&frame).unwrap();
        assert!(out.keyframe && !out.data.is_empty());
    }
}
