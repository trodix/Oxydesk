use crate::layout::OutputId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
}

/// A single-plane DMA-BUF owned by the frame (closed on drop). The content may be overwritten by the
/// producer once newer frames arrive, so consume it promptly.
#[derive(Debug)]
pub struct DmaBuf {
    pub fd: std::os::fd::OwnedFd,
    /// Byte offset of the first row inside the buffer.
    pub offset: u32,
    /// DRM format modifier (0 = linear).
    pub modifier: u64,
}

/// Frame data: shared memory or DMA-BUF (GPU path).
#[derive(Debug)]
pub enum FrameData {
    Shm(Vec<u8>),
    DmaBuf(DmaBuf),
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
