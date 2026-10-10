use crate::layout::OutputId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
}

/// Données d'une frame : mémoire partagée ou DMA-BUF (chemin GPU futur).
#[derive(Debug)]
pub enum FrameData {
    Shm(Vec<u8>),
    DmaBuf(i32),
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
