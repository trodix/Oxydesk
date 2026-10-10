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
    /// Déjà encodé en H.264 Annex-B par l'agent de session (voir `PassthroughEncoder`).
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
