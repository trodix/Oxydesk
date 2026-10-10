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
    /// Le prochain `encode` doit produire une image clé autonome (nouveau client).
    fn request_keyframe(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

pub trait InputSink: Send {
    /// `scancode` : scancode RDP (set 1), préfixe 0xE000 pour les touches étendues.
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError>;
    /// Position absolue dans l'espace virtuel du bureau.
    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError>;
    fn pointer_button(&mut self, _button: PointerButton, _pressed: bool) -> Result<(), InputError> {
        Ok(())
    }
    /// Molette en unités RDP (120 = un cran), `dy` > 0 = vers le haut.
    fn wheel(&mut self, _dx: i32, _dy: i32) -> Result<(), InputError> {
        Ok(())
    }
}

pub trait ClipboardBackend: Send {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;
    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError>;
}

/// Format PCM 16 bits little-endian entrelacé demandé à une source audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioParams {
    /// Octets par trame (un échantillon par canal).
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * 2
    }

    /// Octets correspondant à `ms` millisecondes.
    pub fn bytes_for_ms(&self, ms: u32) -> usize {
        (self.sample_rate as usize * ms as usize / 1000) * self.frame_bytes()
    }
}

/// Flux audio PCM en cours de capture.
pub trait AudioSource: Send {
    /// Bloque jusqu'à avoir rempli `buf` (multiple de `frame_bytes`) ; erreur si le flux est fermé.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), AudioError>;
}

/// Ouvre des flux audio au format négocié avec le client.
pub trait AudioProvider: Send + Sync {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError>;
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
