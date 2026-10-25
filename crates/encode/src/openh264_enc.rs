use std::path::PathBuf;

use crate::color;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    UsageType, VuiConfig,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use oxydesk_core::error::EncodeError;
use oxydesk_core::frame::{Frame, FrameData, PixelFormat};
use oxydesk_core::ports::{EncodedFrame, FrameEncoder};
use oxydesk_core::ColorDepth;

#[derive(Debug, Clone)]
pub struct OpenH264Config {
    /// Path of libopenh264; otherwise `OXYDESK_OPENH264_LIB`, otherwise the usual locations.
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
    // Fedora / RHEL (lib64) and Debian multiarch, x86_64 then aarch64.
    "/usr/lib64/libopenh264.so.8",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
    "/usr/lib/aarch64-linux-gnu/libopenh264.so.8",
    "/usr/lib64/libopenh264.so",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so",
    "/usr/lib/aarch64-linux-gnu/libopenh264.so",
];

fn locate(cfg: &OpenH264Config) -> Result<PathBuf, EncodeError> {
    if let Some(p) = &cfg.library {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("OXYDESK_OPENH264_LIB") {
        return Ok(p.into());
    }
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| EncodeError::Backend("libopenh264 not found (install openh264 / libopenh264-8 or set OXYDESK_OPENH264_LIB)".into()))
}

/// Software H.264 encoder, Annex-B stream, no B-frames, low latency.
pub struct OpenH264Encoder {
    inner: Encoder,
    yuv: Vec<u8>,
    depth: ColorDepth,
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
            .skip_frames(true) // without it the codec cannot hold a bitrate; a skipped frame comes out empty
            .intra_frame_period(period)
            // The pictures are sRGB, BT.709 matrix, full range (see `color`): say so, clients with a hardware decoder
            // otherwise guess limited range or BT.601.
            .vui(VuiConfig::srgb());
        let inner = Encoder::with_api_config(api, config)
            .map_err(|e| EncodeError::Backend(e.to_string()))?;
        Ok(Self {
            inner,
            yuv: Vec::new(),
            depth: ColorDepth::default(),
        })
    }

    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn request_keyframe(&mut self) {
        self.force_keyframe();
    }

    fn set_color_depth(&mut self, depth: ColorDepth) {
        self.depth = depth;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        use openh264_sys2::{SBitrateInfo, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL};
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bitrate_bps.min(i32::MAX as u32) as i32,
        };
        // SAFETY: `info` is the structure the option expects and outlives the call; the encoder is ours.
        // A refusal keeps the previous bitrate: nothing to do about it.
        let _status = unsafe {
            self.inner.raw_api().set_option(
                ENCODER_OPTION_BITRATE,
                std::ptr::addr_of_mut!(info).cast::<std::ffi::c_void>(),
            )
        };
    }

    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        let data = match &frame.data {
            FrameData::Shm(d) => d,
            FrameData::DmaBuf(dma) => {
                // No GPU path here: copy the pixels out of the buffer and encode them as SHM.
                let need = frame.stride as usize * frame.height as usize;
                let copy = oxydesk_core::dmabuf::with_pixels(frame, dma, |p| p[..need].to_vec())?;
                let shm = Frame {
                    output: frame.output,
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride,
                    format: frame.format,
                    data: FrameData::Shm(copy),
                };
                return self.encode(&shm);
            }
            FrameData::Encoded { .. } => {
                return Err(EncodeError::Unsupported("already-encoded frame".into()))
            }
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let stride = frame.stride as usize;
        if data.len() < stride * h || stride < w * 4 {
            return Err(EncodeError::Unsupported(
                "buffer too short for stride/height".into(),
            ));
        }
        let bgra = frame.format == PixelFormat::Bgra8;
        let depth = self.depth;
        self.yuv.resize(w * h * 3 / 2, 0);
        color::fill_i420(
            |x, y| {
                let p = &data[y * stride + x * 4..][..3];
                let (r, g, b) = if bgra {
                    (i32::from(p[2]), i32::from(p[1]), i32::from(p[0]))
                } else {
                    (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]))
                };
                depth.quantize(r, g, b)
            },
            w,
            h,
            &mut self.yuv,
        );
        let yuv = YUVBuffer::from_vec(std::mem::take(&mut self.yuv), w, h);
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
    use oxydesk_core::layout::OutputId;

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
