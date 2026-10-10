use crate::layout::OutputId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
}

/// Frame data: shared memory or DMA-BUF (future GPU path).
#[derive(Debug)]
pub enum FrameData {
    Shm(Vec<u8>),
    DmaBuf(i32),
    /// Already encoded as H.264 Annex-B by the session agent (see `PassthroughEncoder`).
    Encoded { keyframe: bool, data: Vec<u8> },
}

#[derive(Debug)]
pub struct Frame {
    pub output: OutputId,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub data: FrameData,
}
