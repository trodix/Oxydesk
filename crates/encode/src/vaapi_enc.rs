//! Hardware H.264 encoder through VA-API (`cros-libva`), Annex-B stream, no B-frames, low latency.
//!
//! One slice per frame, one reference frame (the previous reconstruction), CBR rate control.
//! SPS/PPS and slice headers are written by `h264_headers` and passed as packed headers (required by radeonsi). The BGRA/RGBA -> NV12 conversion runs on
//! the CPU (BT.601 limited range, same matrix as the software backend) and the NV12 image is
//! uploaded to a VA surface.
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::h264_headers;
use cros_libva as va;
use rdsktp_core::error::EncodeError;
use rdsktp_core::frame::{DmaBuf, Frame, FrameData, PixelFormat};
use rdsktp_core::ports::{EncodedFrame, FrameEncoder};
use va::{
    BufferType, Config, Context, Display, EncCodedBuffer, EncMiscParameter,
    EncMiscParameterFrameRate, EncMiscParameterHRD, EncMiscParameterRateControl,
    EncPictureParameter, EncPictureParameterBufferH264, EncSequenceParameter,
    EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
    H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields, Image, MappedCodedBuffer, Picture,
    PictureH264, RcFlags, Surface,
};

#[derive(Debug, Clone)]
pub struct VaapiConfig {
    /// DRM render node; `None` = first node where VA-API initializes.
    pub device: Option<PathBuf>,
    pub bitrate_bps: u32,
    pub fps: f32,
}

impl Default for VaapiConfig {
    fn default() -> Self {
        Self {
            device: None,
            bitrate_bps: 8_000_000,
            fps: 30.0,
        }
    }
}

fn backend<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> EncodeError + '_ {
    move |e| EncodeError::Backend(format!("VA-API {what}: {e}"))
}

const SLICE_P: u8 = 0;
const SLICE_I: u8 = 2;
const LOG2_MAX_FRAME_NUM: u32 = 8;
// VAEncPackedHeaderType values (va_enc_h264/va.h).
const PACKED_SEQUENCE: u32 = 1;
const PACKED_SLICE: u32 = 3;

/// Hardware H.264 encoder (see the module documentation).
pub struct VaapiEncoder {
    // `Rc` fields below are only ever cloned inside this struct (see the `Send` impl).
    display: Rc<Display>,
    context: Rc<Context>,
    _config: Config,
    coded: EncCodedBuffer,
    /// Source surface; `None` after an error (recreated on the next frame).
    input: Option<Surface<()>>,
    recon: [Surface<()>; 2],
    width: u32,
    height: u32,
    aligned_w: u32,
    aligned_h: u32,
    profile: va::VAProfile::Type,
    bitrate_bps: u32,
    fps: u32,
    force_idr: bool,
    /// Index in `recon` of the last reconstruction, valid when `frame_num > 0` or after an IDR.
    last_recon: usize,
    frame_num: u32,
    idr_pic_id: u16,
    scratch: Vec<u8>,
    nv12: Vec<u8>,
    /// Video-processing context (DMA-BUF import + BGRx -> NV12 on the GPU), created on first use.
    vpp: Option<Vpp>,
    /// The GPU import path failed once: DMA-BUF frames are read through the CPU from then on.
    vpp_failed: bool,
    gpu_frames: u64,
}

struct Vpp {
    context: Rc<Context>,
    _config: Config,
}

/// Imported DMA-BUF (`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`). Owns a duplicate of the descriptor,
/// closed when the VA surface is dropped.
struct DmaImport {
    fd: std::os::fd::OwnedFd,
    size: u32,
    modifier: u64,
    drm_format: u32,
    width: u32,
    height: u32,
    offset: u32,
    pitch: u32,
}

impl va::ExternalBufferDescriptor for DmaImport {
    const MEMORY_TYPE: va::MemoryType = va::MemoryType::DrmPrime2;
    type DescriptorAttribute = va::VADRMPRIMESurfaceDescriptor;

    #[allow(clippy::field_reassign_with_default)] // nested C arrays are clearer assigned in place
    fn va_surface_attribute(&mut self) -> Self::DescriptorAttribute {
        use std::os::fd::AsRawFd;
        let mut d = va::VADRMPRIMESurfaceDescriptor::default();
        d.fourcc = va::VA_FOURCC_BGRX;
        d.width = self.width;
        d.height = self.height;
        d.num_objects = 1;
        d.objects[0].fd = self.fd.as_raw_fd();
        d.objects[0].size = self.size;
        d.objects[0].drm_format_modifier = self.modifier;
        d.num_layers = 1;
        d.layers[0].drm_format = self.drm_format;
        d.layers[0].num_planes = 1;
        d.layers[0].object_index[0] = 0;
        d.layers[0].offset[0] = self.offset;
        d.layers[0].pitch[0] = self.pitch;
        d
    }
}

// DRM fourcc codes (little-endian words): XRGB8888 = bytes B,G,R,x; XBGR8888 = bytes R,G,B,x.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;

// SAFETY: `Display`, `Context` and every VA object are held in `Rc`s whose clones all live inside
// this struct (surfaces, buffers, pictures are created and dropped within `encode`). Moving the whole
// struct to another thread therefore moves every reference count with it, and libva calls are
// never made concurrently because `FrameEncoder::encode` takes `&mut self`.
unsafe impl Send for VaapiEncoder {}

fn align16(v: u32) -> u32 {
    (v + 15) & !15
}

impl VaapiEncoder {
    /// `width`/`height`: visible size of the frames that will be encoded (fixed for the stream).
    pub fn new(cfg: &VaapiConfig, width: u32, height: u32) -> Result<Self, EncodeError> {
        let display = match &cfg.device {
            Some(p) => Display::open_drm_display(p).map_err(backend(&p.display().to_string()))?,
            None => Display::open()
                .ok_or_else(|| EncodeError::Backend("VA-API: no usable DRM render node".into()))?,
        };
        let (aligned_w, aligned_h) = (align16(width), align16(height));

        let profiles = display
            .query_config_profiles()
            .map_err(backend("profiles"))?;
        let profile = [
            va::VAProfile::VAProfileH264High,
            va::VAProfile::VAProfileH264Main,
            va::VAProfile::VAProfileH264ConstrainedBaseline,
        ]
        .into_iter()
        .find(|p| {
            profiles.contains(p)
                && display
                    .query_config_entrypoints(*p)
                    .is_ok_and(|e| e.contains(&va::VAEntrypoint::VAEntrypointEncSlice))
        })
        .ok_or_else(|| {
            EncodeError::Unsupported("VA-API: the driver has no H.264 encoder".into())
        })?;

        let mut attrs = vec![
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType::VAConfigAttribRTFormat,
                value: va::VA_RT_FORMAT_YUV420,
            },
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType::VAConfigAttribRateControl,
                value: va::VA_RC_CBR,
            },
        ];
        // Keep only the attributes the driver supports (otherwise vaCreateConfig fails).
        let mut probe = attrs.clone();
        display
            .get_config_attributes(profile, va::VAEntrypoint::VAEntrypointEncSlice, &mut probe)
            .map_err(backend("config attributes"))?;
        if probe[0].value & va::VA_RT_FORMAT_YUV420 == 0 || probe[1].value & va::VA_RC_CBR == 0 {
            return Err(EncodeError::Unsupported(
                "VA-API: YUV420 / CBR not supported by the encoder".into(),
            ));
        }
        attrs.truncate(2);
        let config = display
            .create_config(attrs, profile, va::VAEntrypoint::VAEntrypointEncSlice)
            .map_err(backend("config"))?;

        let mut surfaces = display
            .create_surfaces(
                va::VA_RT_FORMAT_YUV420,
                Some(va::VA_FOURCC_NV12),
                aligned_w,
                aligned_h,
                None,
                vec![(); 3],
            )
            .map_err(backend("surfaces"))?;
        let context = display
            .create_context(&config, aligned_w, aligned_h, Some(&surfaces), true)
            .map_err(backend("context"))?;
        let coded = context
            .create_enc_coded((aligned_w * aligned_h) as usize + 0x10000)
            .map_err(backend("coded buffer"))?;
        let r1 = surfaces.pop().expect("3 surfaces");
        let r0 = surfaces.pop().expect("3 surfaces");
        let input = surfaces.pop().expect("3 surfaces");

        Ok(Self {
            display,
            context,
            _config: config,
            coded,
            input: Some(input),
            recon: [r0, r1],
            width,
            height,
            aligned_w,
            aligned_h,
            profile,
            bitrate_bps: cfg.bitrate_bps,
            fps: cfg.fps.round().max(1.0) as u32,
            force_idr: true,
            last_recon: 1,
            frame_num: 0,
            idr_pic_id: 0,
            scratch: Vec::new(),
            nv12: Vec::new(),
            vpp: None,
            vpp_failed: false,
            gpu_frames: 0,
        })
    }

    /// Convert to NV12 (aligned size, edges replicated) into `self.nv12` (tightly packed).
    fn convert(&mut self, frame: &Frame, data: &[u8]) -> Result<(), EncodeError> {
        let (w, h, stride) = (
            frame.width as usize,
            frame.height as usize,
            frame.stride as usize,
        );
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(EncodeError::Unsupported(format!(
                "frame size {}x{} differs from the encoder's {}x{}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        if stride < w * 4 || data.len() < stride * h {
            return Err(EncodeError::Unsupported(
                "buffer too short for stride/height".into(),
            ));
        }
        let (aw, ah) = (self.aligned_w as usize, self.aligned_h as usize);
        let bgra = frame.format == PixelFormat::Bgra8;
        let px = |x: usize, y: usize| -> (i32, i32, i32) {
            let o = y.min(h - 1) * stride + x.min(w - 1) * 4;
            let p = &data[o..o + 3];
            if bgra {
                (p[2] as i32, p[1] as i32, p[0] as i32)
            } else {
                (p[0] as i32, p[1] as i32, p[2] as i32)
            }
        };
        self.nv12.resize(aw * ah * 3 / 2, 0);
        let (y_plane, uv_plane) = self.nv12.split_at_mut(aw * ah);
        for y in 0..ah {
            let row = &mut y_plane[y * aw..(y + 1) * aw];
            for (x, out) in row.iter_mut().enumerate() {
                let (r, g, b) = px(x, y);
                *out = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            }
        }
        for cy in 0..ah / 2 {
            for cx in 0..aw / 2 {
                let (mut r, mut g, mut b) = (0, 0, 0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (pr, pg, pb) = px(cx * 2 + dx, cy * 2 + dy);
                    r += pr;
                    g += pg;
                    b += pb;
                }
                let (r, g, b) = (r / 4, g / 4, b / 4);
                let o = cy * aw + cx * 2;
                uv_plane[o] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                uv_plane[o + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
        Ok(())
    }

    fn upload(&mut self, surface: &Surface<()>) -> Result<(), EncodeError> {
        let (aw, ah) = (self.aligned_w as usize, self.aligned_h as usize);
        let mut image = match Image::derive_from(surface, (self.aligned_w, self.aligned_h)) {
            Ok(i) => i,
            Err(_) => {
                let fmt = self
                    .display
                    .query_image_formats()
                    .map_err(backend("image formats"))?
                    .into_iter()
                    .find(|f| f.fourcc == va::VA_FOURCC_NV12)
                    .ok_or_else(|| {
                        EncodeError::Unsupported("VA-API: no NV12 image format".into())
                    })?;
                Image::create_from(
                    surface,
                    fmt,
                    (self.aligned_w, self.aligned_h),
                    (self.aligned_w, self.aligned_h),
                )
                .map_err(backend("image"))?
            }
        };
        let (pitch_y, pitch_uv) = (
            image.image().pitches[0] as usize,
            image.image().pitches[1] as usize,
        );
        let (off_y, off_uv) = (
            image.image().offsets[0] as usize,
            image.image().offsets[1] as usize,
        );
        let dst: &mut [u8] = image.as_mut();
        for y in 0..ah {
            dst[off_y + y * pitch_y..off_y + y * pitch_y + aw]
                .copy_from_slice(&self.nv12[y * aw..(y + 1) * aw]);
        }
        for y in 0..ah / 2 {
            let src = aw * ah + y * aw;
            dst[off_uv + y * pitch_uv..off_uv + y * pitch_uv + aw]
                .copy_from_slice(&self.nv12[src..src + aw]);
        }
        Ok(())
    }

    fn level_idc(&self) -> u8 {
        let mbs = (self.aligned_w / 16) * (self.aligned_h / 16);
        if mbs <= 8192 {
            42
        } else {
            51
        }
    }

    fn stream_params(&self) -> h264_headers::StreamParams {
        let profile_idc = match self.profile {
            va::VAProfile::VAProfileH264High => 100,
            va::VAProfile::VAProfileH264Main => 77,
            _ => 66,
        };
        h264_headers::StreamParams {
            profile_idc,
            level_idc: self.level_idc(),
            mb_width: self.aligned_w / 16,
            mb_height: self.aligned_h / 16,
            crop_right: self.aligned_w - self.width,
            crop_bottom: self.aligned_h - self.height,
            log2_max_frame_num: LOG2_MAX_FRAME_NUM,
        }
    }

    /// Adds a packed header (parameter + data buffers). `bits` defaults to the whole data.
    fn add_packed(
        &self,
        pic: &mut Picture<va::PictureNew, Surface<()>>,
        kind: u32,
        data: Vec<u8>,
        bits: Option<usize>,
    ) -> Result<(), EncodeError> {
        let param = va::VAEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length: bits.unwrap_or(data.len() * 8) as u32,
            has_emulation_bytes: 1,
            ..Default::default()
        };
        for t in [
            BufferType::EncPackedHeaderParameter(param),
            BufferType::EncPackedHeaderData(data),
        ] {
            pic.add_buffer(
                self.context
                    .create_buffer(t)
                    .map_err(backend("packed header"))?,
            );
        }
        Ok(())
    }

    fn ref_pic(&self, idx: usize, frame_num: u32) -> PictureH264 {
        let poc = (frame_num * 2) as i32;
        PictureH264::new(
            self.recon[idx].id(),
            frame_num,
            va::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            poc,
            poc,
        )
    }

    fn invalid_pic() -> PictureH264 {
        PictureH264::new(va::VA_INVALID_SURFACE, 0, va::VA_PICTURE_H264_INVALID, 0, 0)
    }

    /// The NV12 surface that receives the next source image (recreated after an error).
    fn take_input(&mut self) -> Result<Surface<()>, EncodeError> {
        match self.input.take() {
            Some(s) => Ok(s),
            None => Ok(self
                .display
                .create_surfaces(
                    va::VA_RT_FORMAT_YUV420,
                    Some(va::VA_FOURCC_NV12),
                    self.aligned_w,
                    self.aligned_h,
                    None,
                    vec![()],
                )
                .map_err(backend("surfaces"))?
                .pop()
                .expect("1 surface")),
        }
    }

    /// Imports the DMA-BUF into VA-API and converts it to NV12 with the video-processing engine.
    #[allow(clippy::field_reassign_with_default)] // C struct with raw pointers filled in place
    fn convert_gpu(&mut self, frame: &Frame, dma: &DmaBuf) -> Result<Surface<()>, EncodeError> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(EncodeError::Unsupported(
                "frame size differs from the encoder's".into(),
            ));
        }
        if self.vpp.is_none() {
            let entrypoints = self
                .display
                .query_config_entrypoints(va::VAProfile::VAProfileNone)
                .map_err(backend("vpp entrypoints"))?;
            if !entrypoints.contains(&va::VAEntrypoint::VAEntrypointVideoProc) {
                return Err(EncodeError::Unsupported(
                    "VA-API: no video processing entrypoint".into(),
                ));
            }
            let config = self
                .display
                .create_config(
                    vec![],
                    va::VAProfile::VAProfileNone,
                    va::VAEntrypoint::VAEntrypointVideoProc,
                )
                .map_err(backend("vpp config"))?;
            let context = self
                .display
                .create_context(
                    &config,
                    self.aligned_w,
                    self.aligned_h,
                    None::<&Vec<Surface<()>>>,
                    true,
                )
                .map_err(backend("vpp context"))?;
            self.vpp = Some(Vpp {
                context,
                _config: config,
            });
        }
        let context = Rc::clone(&self.vpp.as_ref().expect("created above").context);

        let import = DmaImport {
            fd: dma
                .fd
                .try_clone()
                .map_err(|e| EncodeError::Backend(format!("dup dmabuf: {e}")))?,
            size: u32::try_from(rdsktp_core::dmabuf::size(dma)?).unwrap_or(u32::MAX),
            modifier: dma.modifier,
            drm_format: if frame.format == PixelFormat::Bgra8 {
                DRM_FORMAT_XRGB8888
            } else {
                DRM_FORMAT_XBGR8888
            },
            width: frame.width,
            height: frame.height,
            offset: dma.offset,
            pitch: frame.stride,
        };
        let source = self
            .display
            .create_surfaces(
                va::VA_RT_FORMAT_RGB32,
                None,
                frame.width,
                frame.height,
                None,
                vec![import],
            )
            .map_err(backend("import dmabuf"))?
            .pop()
            .expect("1 surface");
        let output = self.take_input()?;

        let rect = va::VARectangle {
            x: 0,
            y: 0,
            width: frame.width as u16,
            height: frame.height as u16,
        };
        let mut params = va::VAProcPipelineParameterBuffer::default();
        params.surface = source.id();
        params.surface_region = &rect;
        params.surface_color_standard = va::_VAProcColorStandardType_VAProcColorStandardBT601;
        params.output_region = &rect;
        params.output_color_standard = va::_VAProcColorStandardType_VAProcColorStandardBT601;
        params.input_color_properties.color_range = va::VA_SOURCE_RANGE_FULL as u8;
        params.output_color_properties.color_range = va::VA_SOURCE_RANGE_REDUCED as u8;

        let mut pic = Picture::new(0, Rc::clone(&context), output);
        pic.add_buffer(
            context
                .create_buffer(BufferType::ProcPipelineParameter(params))
                .map_err(backend("vpp buffer"))?,
        );
        let pic = pic.begin::<()>().map_err(backend("vpp begin"))?;
        let pic = pic.render().map_err(backend("vpp render"))?;
        let pic = pic.end().map_err(backend("vpp end"))?;
        let pic = pic.sync::<()>().map_err(|(e, _)| backend("vpp sync")(e))?;
        // `rect` and `source` stay alive until here: the driver reads both while processing.
        drop(source);
        pic.take_surface()
            .map_err(|_| EncodeError::Backend("VA-API: vpp surface still referenced".into()))
    }

    fn encode_inner(
        &mut self,
        idr: bool,
        prepared: Option<Surface<()>>,
    ) -> Result<Vec<u8>, EncodeError> {
        let input = match prepared {
            Some(s) => s,
            None => {
                let input = self.take_input()?;
                self.upload(&input)?;
                input
            }
        };

        let (mbw, mbh) = ((self.aligned_w / 16) as u16, (self.aligned_h / 16) as u16);
        if idr {
            self.frame_num = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        let cur = if idr { 0 } else { 1 - self.last_recon };
        let frame_num = self.frame_num;
        let poc = (frame_num * 2) as i32;
        // The packed PPS (see `h264_headers`) is CAVLC without 8x8 transform.
        let (cabac, high) = (false, false);
        let params = self.stream_params();

        let mut pic = Picture::new(u64::from(frame_num), Rc::clone(&self.context), input);

        if idr {
            let crop = (self.aligned_w != self.width || self.aligned_h != self.height).then(|| {
                H264EncFrameCropOffsets {
                    left: 0,
                    right: (self.aligned_w - self.width) / 2,
                    top: 0,
                    bottom: (self.aligned_h - self.height) / 2,
                }
            });
            let seq_fields = H264EncSeqFields::new(1, 1, 0, 0, 1, LOG2_MAX_FRAME_NUM - 4, 2, 0, 0);
            let seq = EncSequenceParameterBufferH264::new(
                0,
                self.level_idc(),
                1 << 30,
                1 << 30,
                1,
                self.bitrate_bps,
                1,
                mbw,
                mbh,
                &seq_fields,
                0,
                0,
                0,
                0,
                0,
                [0; 256],
                crop,
                None,
                0,
                0,
                0,
                1,
                self.fps * 2,
            );
            for t in [
                BufferType::EncSequenceParameter(EncSequenceParameter::H264(seq)),
                BufferType::EncMiscParameter(EncMiscParameter::RateControl(
                    EncMiscParameterRateControl::new(
                        self.bitrate_bps,
                        100,
                        1000,
                        26,
                        10,
                        0,
                        RcFlags::new(1, 0, 0, 0, 0, 0, 0, 0, 0),
                        0,
                        51,
                        0,
                        0,
                    ),
                )),
                BufferType::EncMiscParameter(EncMiscParameter::FrameRate(
                    EncMiscParameterFrameRate::new(self.fps | (1 << 16), 0),
                )),
                BufferType::EncMiscParameter(EncMiscParameter::HRD(EncMiscParameterHRD::new(
                    self.bitrate_bps / 2,
                    self.bitrate_bps,
                ))),
            ] {
                pic.add_buffer(self.context.create_buffer(t).map_err(backend("buffer"))?);
            }
            self.add_packed(
                &mut pic,
                PACKED_SEQUENCE,
                h264_headers::sps_pps(&params),
                None,
            )?;
        }

        let mut refs: [PictureH264; 16] = std::array::from_fn(|_| Self::invalid_pic());
        if !idr {
            refs[0] = self.ref_pic(self.last_recon, frame_num.saturating_sub(1));
        }
        let pic_fields = H264EncPicFields::new(
            u32::from(idr),
            1,
            u32::from(cabac),
            0,
            0,
            0,
            u32::from(high),
            1,
            0,
            0,
            0,
        );
        let pic_param = EncPictureParameterBufferH264::new(
            PictureH264::new(self.recon[cur].id(), frame_num, 0, poc, poc),
            refs,
            self.coded.id(),
            0,
            0,
            0,
            frame_num as u16,
            26,
            0,
            0,
            0,
            0,
            &pic_fields,
        );
        pic.add_buffer(
            self.context
                .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::H264(
                    pic_param,
                )))
                .map_err(backend("buffer"))?,
        );

        let mut list0: [PictureH264; 32] = std::array::from_fn(|_| Self::invalid_pic());
        if !idr {
            list0[0] = self.ref_pic(self.last_recon, frame_num.saturating_sub(1));
        }
        let slice = EncSliceParameterBufferH264::new(
            0,
            u32::from(mbw) * u32::from(mbh),
            va::VA_INVALID_ID,
            if idr { SLICE_I } else { SLICE_P },
            0,
            self.idr_pic_id,
            0,
            0,
            [0, 0],
            0,
            0,
            0,
            0,
            list0,
            std::array::from_fn(|_| Self::invalid_pic()),
            0,
            0,
            0,
            [0; 32],
            [0; 32],
            0,
            [[0; 2]; 32],
            [[0; 2]; 32],
            0,
            [0; 32],
            [0; 32],
            0,
            [[0; 2]; 32],
            [[0; 2]; 32],
            0,
            0,
            0,
            0,
            0,
        );
        pic.add_buffer(
            self.context
                .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::H264(
                    slice,
                )))
                .map_err(backend("buffer"))?,
        );

        let (hdr, bits) =
            h264_headers::slice_header(&params, idr, frame_num, u32::from(self.idr_pic_id));
        self.add_packed(&mut pic, PACKED_SLICE, hdr, Some(bits))?;

        let pic = pic.begin::<()>().map_err(backend("begin"))?;
        let pic = pic.render().map_err(backend("render"))?;
        let pic = pic.end().map_err(backend("end"))?;
        let pic = pic.sync::<()>().map_err(|(e, _)| backend("sync")(e))?;
        self.input = pic.take_surface().ok();

        let mapped = MappedCodedBuffer::new(&self.coded).map_err(backend("map"))?;
        self.scratch.clear();
        for seg in mapped.iter() {
            self.scratch.extend_from_slice(seg.buf);
        }
        drop(mapped);

        self.last_recon = cur;
        self.frame_num = (frame_num + 1) % (1 << LOG2_MAX_FRAME_NUM);
        Ok(std::mem::take(&mut self.scratch))
    }
}

impl FrameEncoder for VaapiEncoder {
    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        let prepared = match &frame.data {
            FrameData::Shm(data) => {
                self.convert(frame, data)?;
                None
            }
            FrameData::DmaBuf(dma) => {
                let gpu = if self.vpp_failed {
                    None
                } else {
                    self.convert_gpu(frame, dma)
                        .map_err(|e| self.gpu_path_failed(&e))
                        .ok()
                };
                if gpu.is_none() {
                    rdsktp_core::dmabuf::with_pixels(frame, dma, |px| self.convert(frame, px))??;
                }
                gpu
            }
            FrameData::Encoded { .. } => {
                return Err(EncodeError::Unsupported("already-encoded frame".into()))
            }
        };
        if prepared.is_some() && self.gpu_frames == 0 {
            eprintln!("VA-API: DMA-BUF import active (GPU colour conversion)");
        }
        self.gpu_frames += u64::from(prepared.is_some());
        let idr = std::mem::take(&mut self.force_idr);
        match self.encode_inner(idr, prepared) {
            Ok(data) => Ok(EncodedFrame {
                output: Some(frame.output),
                keyframe: idr,
                data,
            }),
            Err(e) => {
                // The reference chain may be broken: restart from an IDR.
                self.force_idr = true;
                Err(e)
            }
        }
    }
}

impl VaapiEncoder {
    /// Frames converted by the GPU (DMA-BUF import) since creation.
    pub fn gpu_frames(&self) -> u64 {
        self.gpu_frames
    }

    fn gpu_path_failed(&mut self, e: &EncodeError) {
        eprintln!("VA-API: DMA-BUF import unavailable, reading frames on the CPU: {e}");
        self.vpp_failed = true;
    }
}

/// True when a VA-API H.264 encoder can be opened on this machine.
pub fn available() -> bool {
    let Some(display) = Display::open() else {
        return false;
    };
    let Ok(profiles) = display.query_config_profiles() else {
        return false;
    };
    [
        va::VAProfile::VAProfileH264High,
        va::VAProfile::VAProfileH264Main,
        va::VAProfile::VAProfileH264ConstrainedBaseline,
    ]
    .into_iter()
    .any(|p| {
        profiles.contains(&p)
            && display
                .query_config_entrypoints(p)
                .is_ok_and(|e| e.contains(&va::VAEntrypoint::VAEntrypointEncSlice))
    })
}

/// Default DRM render node used when none is configured (informational).
pub fn default_device() -> Option<&'static Path> {
    Path::new("/dev/dri/renderD128")
        .exists()
        .then_some(Path::new("/dev/dri/renderD128"))
}
