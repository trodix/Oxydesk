//! Outgoing ports (hexagonal). Adapters live in other crates.

use crate::error::*;
use crate::frame::Frame;
use crate::layout::{Layout, OutputId};

/// Frame source. `recv_latest` only returns the most recent one (drop, no queue).
pub trait CaptureSource: Send {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError>;
    /// The consumer may sleep on `wake` instead of polling: the source notifies it on every new
    /// frame. No effect by default (polled sources only).
    fn set_waker(&mut self, _wake: std::sync::Arc<crate::wake::Wake>) {}
    /// The consumer wants uncompressed pixels (`true`) or can take the source's own compressed stream
    /// (`false`, the default): a source that encodes by itself (the login agent) switches mode. No effect on
    /// sources that always deliver pixels.
    fn set_raw_pixels(&mut self, _raw: bool) {}
    /// The pointer shape if it changed since the last call. The shape is not part of the frames: the client
    /// draws it at its own pointer position. `None` by default (sources that do not know the shape).
    fn take_cursor(&mut self) -> Option<crate::cursor::CursorUpdate> {
        None
    }
    /// Resizes the (single) captured desktop. Frames after the call have the new size.
    fn resize(&mut self, _width: u32, _height: u32) -> Result<(), CaptureError> {
        Err(CaptureError::ResizeUnsupported)
    }
    /// This source is dropped from the layout: its output is no longer wanted (a compositor output that
    /// cannot be destroyed is disabled). Dropping the source itself releases the rest.
    fn retire(&mut self) {}
    /// Moves this source's output to `(x, y)` in the desktop. Sources without a position (single
    /// output, synthetic) ignore it.
    fn place(&mut self, _x: i32, _y: i32) -> Result<(), CaptureError> {
        Ok(())
    }
    /// Applies a whole layout (number, sizes and positions of the outputs). By default only a
    /// single-output layout is supported, through `resize`.
    fn apply_layout(&mut self, layout: &crate::layout::Layout) -> Result<(), CaptureError> {
        match layout.outputs.as_slice() {
            [o] => self.resize(o.width, o.height),
            _ => Err(CaptureError::ResizeUnsupported),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EncodedFrame {
    pub output: Option<OutputId>,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub trait FrameEncoder: Send {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError>;
    /// The next `encode` must produce a standalone keyframe (new client).
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
    /// `scancode`: RDP scancode (set 1), prefix 0xE000 for extended keys.
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError>;
    /// Absolute position in the desktop virtual space.
    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError>;
    fn pointer_button(&mut self, _button: PointerButton, _pressed: bool) -> Result<(), InputError> {
        Ok(())
    }
    /// Wheel in RDP units (120 = one notch), `dy` > 0 = upward.
    fn wheel(&mut self, _dx: i32, _dy: i32) -> Result<(), InputError> {
        Ok(())
    }
}

pub trait ClipboardBackend: Send {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;
    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError>;
    /// Image (PNG file) to offer as the desktop selection. Ignored by default.
    fn set_image(&mut self, _png: &[u8]) -> Result<(), ClipboardError> {
        Ok(())
    }
    /// New image selection of the desktop (PNG file), if any.
    fn poll_image(&mut self) -> Result<Option<Vec<u8>>, ClipboardError> {
        Ok(None)
    }
}

/// 16-bit little-endian interleaved PCM format requested from an audio source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioParams {
    /// Bytes per frame (one sample per channel).
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * 2
    }

    /// Bytes corresponding to `ms` milliseconds.
    pub fn bytes_for_ms(&self, ms: u32) -> usize {
        (self.sample_rate as usize * ms as usize / 1000) * self.frame_bytes()
    }
}

/// PCM audio stream being captured.
pub trait AudioSource: Send {
    /// Blocks until `buf` is filled (multiple of `frame_bytes`); error if the stream is closed.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), AudioError>;
}

/// Opens audio streams in the format negotiated with the client.
pub trait AudioProvider: Send + Sync {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError>;
}

/// Creates the encoder of an output added by a layout change.
pub type EncoderFactory = Box<dyn Fn(crate::layout::OutputId) -> Box<dyn FrameEncoder> + Send>;

/// Ports of a remote desktop opened for a user.
pub struct DesktopPorts {
    pub capture: Box<dyn CaptureSource>,
    /// One encoder per output, in layout order.
    pub encoders: Vec<Box<dyn FrameEncoder>>,
    /// Encoders for outputs added later; without it the number of outputs cannot change.
    pub make_encoder: Option<EncoderFactory>,
    pub input: Box<dyn InputSink>,
    pub clipboard: Box<dyn ClipboardBackend>,
    pub audio: Option<std::sync::Arc<dyn AudioProvider>>,
}

/// Authenticates a user and opens (or joins) their desktop session.
/// Blocking: to be called outside the async runtime.
pub trait SessionBroker: Send + Sync {
    fn login(&self, username: &str, password: &str) -> Result<DesktopPorts, LoginError>;
}

pub trait OutputManager: Send {
    fn layout(&self) -> Layout;
    fn set_layout(&mut self, layout: Layout) -> Result<(), String>;
}

pub trait SessionProvider: Send {
    /// Starts (or joins) the graphical session; fails clearly if the environment is incompatible.
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self);
}
