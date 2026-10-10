//! Ports sortants (hexagonale). Les adaptateurs vivent dans d'autres crates.

use crate::error::*;
use crate::frame::Frame;
use crate::layout::{Layout, OutputId};

/// Source de frames. `recv_latest` ne retourne que la plus récente (drop, pas de file).
pub trait CaptureSource: Send {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError>;
}

#[derive(Debug, Clone, Default)]
pub struct EncodedFrame {
    pub output: Option<OutputId>,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub trait FrameEncoder: Send {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError>;
}

pub trait InputSink: Send {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError>;
    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError>;
}

pub trait ClipboardBackend: Send {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;
    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError>;
}

pub trait AudioSource: Send {
    /// Remplit `buf` de PCM 16 bits ; retourne le nombre d'octets écrits.
    fn read(&mut self, buf: &mut [u8]) -> usize;
}

pub trait OutputManager: Send {
    fn layout(&self) -> Layout;
    fn set_layout(&mut self, layout: Layout) -> Result<(), String>;
}

pub trait SessionProvider: Send {
    /// Démarre (ou rejoint) la session graphique ; échoue clairement si l'environnement est incompatible.
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self);
}
