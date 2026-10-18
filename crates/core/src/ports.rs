//! Outgoing ports (hexagonal). Adapters live in other crates.

use crate::error::*;
use crate::files::FileList;
use crate::frame::Frame;
use crate::layout::{Layout, OutputId};

/// Moves the windows of a desktop that is not ours, when some of its monitors stop being shown to the client.
pub trait WindowMover: Send {
    /// Brings every application window that is on another output onto the outputs named in `keep`.
    fn gather(&mut self, keep: &[String]) -> Result<(), CaptureError>;
    /// The client's extra monitors `extra` (virtual outputs of the session) have just appeared. Each has a virtual
    /// desktop of its own (created if missing); the windows parked on it come to the monitor.
    fn claim(&mut self, _extra: &[String]) -> Result<(), CaptureError> {
        Ok(())
    }
    /// The client's extra monitors `extra` are going. Their windows go to the real monitor `home`. With `keep` they
    /// are parked on the virtual desktop of their monitor, which stays; without it that desktop is removed.
    fn park(&mut self, _extra: &[String], _home: &str, _keep: bool) -> Result<(), CaptureError> {
        Ok(())
    }
    /// Removes the virtual desktops of the client's extra monitors whose monitor no longer exists (left over from an
    /// earlier run, or from screens that were plugged back in), their windows going to the first desktop.
    fn sweep(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
    /// Puts back, where they were, the windows that `gather` moved (the ones the user has not moved since).
    fn restore(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

/// Frame source. `recv_latest` only returns the most recent one (drop, no queue).
pub trait CaptureSource: Send {
    /// The client left: gives the desktop back as it was before the client changed it. Nothing by default.
    fn release(&mut self) {}
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
    /// Name of this source's output in the compositor, when it has one.
    fn output_name(&self) -> Option<String> {
        None
    }
    /// Moves several outputs (by name) in one single change: the compositor keeps the outputs adjacent, so moving
    /// them one by one cannot swap two positions. Sources that cannot do it refuse (`ResizeUnsupported`).
    fn place_group(&mut self, _moves: &[(String, i32, i32)]) -> Result<(), CaptureError> {
        Err(CaptureError::ResizeUnsupported)
    }
    /// Makes this source's output the primary one of the desktop (where the shell puts its panel). Sources without
    /// such a notion ignore it.
    fn set_primary(&mut self) -> Result<(), CaptureError> {
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
    /// New target bitrate of the stream (the link carries less or more than before). Ignored by default.
    fn set_bitrate(&mut self, _bitrate_bps: u32) {}
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
    /// Layout announced by the client: the session adopts it so that the keys give the same characters.
    fn set_keyboard_layout(&mut self, _layout: &crate::XkbLayout) -> Result<(), InputError> {
        Ok(())
    }
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
    /// Files copied on the remote side, to be offered as the desktop selection (pasted on demand:
    /// the contents are read through the list's source only when an application reads them).
    fn set_files(&mut self, _list: FileList) -> Result<(), ClipboardError> {
        Ok(())
    }
    /// New file selection of the desktop (files copied in a file manager), if any.
    fn poll_files(&mut self) -> Result<Option<FileList>, ClipboardError> {
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
    /// Where the drives redirected by the client are made available; without it they are refused.
    pub drives: Option<std::sync::Arc<dyn crate::drive::DriveHost>>,
    /// Where the printers redirected by the client are made available; without it they are refused.
    pub printers: Option<std::sync::Arc<dyn crate::printer::PrinterHost>>,
}

/// Authenticates a user and opens (or joins) their desktop session.
/// `outputs` is the number of monitors the client announced at connection (`None`: the server's default); a session
/// that is created for this login starts with that many outputs, an existing one keeps its own.
/// Blocking: to be called outside the async runtime.
pub trait SessionBroker: Send + Sync {
    fn login(
        &self,
        username: &str,
        password: &str,
        outputs: Option<u32>,
    ) -> Result<DesktopPorts, LoginError>;

    /// Rejoins the running session of `username`, who proved who they are by an earlier login (automatic
    /// reconnection after a network loss): no password, and a session is never started. `Denied` when there is
    /// none to rejoin.
    fn attach(&self, username: &str, outputs: Option<u32>) -> Result<DesktopPorts, LoginError> {
        let _ = (username, outputs);
        Err(LoginError::Denied)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CaptureError, ClipboardError, InputError, LoginError};
    use crate::frame::Frame;
    use crate::layout::{Layout, OutputId};

    /// The minimum an adapter must provide: everything else comes from the defaults under test.
    struct Bare;

    impl CaptureSource for Bare {
        fn recv_latest(&mut self, _: OutputId) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }
    }

    impl InputSink for Bare {
        fn key(&mut self, _: u16, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn pointer_move(&mut self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
    }

    impl ClipboardBackend for Bare {
        fn set_text(&mut self, _: &str) -> Result<(), ClipboardError> {
            Ok(())
        }
        fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
            Ok(None)
        }
    }

    impl SessionBroker for Bare {
        fn login(&self, _: &str, _: &str, _: Option<u32>) -> Result<DesktopPorts, LoginError> {
            Err(LoginError::Denied)
        }
    }

    #[test]
    fn a_capture_without_extras_cannot_resize_but_accepts_placement() {
        let mut c = Bare;
        assert!(matches!(
            c.resize(800, 600),
            Err(CaptureError::ResizeUnsupported)
        ));
        assert!(c.place(10, 10).is_ok());
        assert!(c.set_primary().is_ok());
        assert!(c.take_cursor().is_none());
        assert!(c.output_name().is_none());
        assert!(matches!(
            c.place_group(&[("a".into(), 0, 0)]),
            Err(CaptureError::ResizeUnsupported)
        ));
        c.retire();
        c.set_raw_pixels(true);
    }

    #[test]
    fn a_single_output_layout_goes_through_resize_and_several_are_refused() {
        let mut c = Bare;
        // `resize` is unsupported by default: the single output layout reports it, it is not silently accepted.
        assert!(matches!(
            c.apply_layout(&Layout::side_by_side(1, 800, 600)),
            Err(CaptureError::ResizeUnsupported)
        ));
        assert!(matches!(
            c.apply_layout(&Layout::side_by_side(2, 800, 600)),
            Err(CaptureError::ResizeUnsupported)
        ));
    }

    #[test]
    fn input_and_clipboard_extras_are_optional() {
        let mut i = Bare;
        assert!(i.pointer_button(PointerButton::Left, true).is_ok());
        assert!(i.wheel(0, 120).is_ok());
        let mut c = Bare;
        assert!(c.set_image(&[1]).is_ok());
        assert_eq!(c.poll_image().unwrap(), None);
        assert!(c.poll_files().unwrap().is_none());
    }

    #[test]
    fn a_broker_without_reconnection_never_rejoins_a_session() {
        assert!(matches!(
            Bare.attach("alice", Some(2)),
            Err(LoginError::Denied)
        ));
    }

    #[test]
    fn audio_sizes_follow_the_format() {
        let stereo = AudioParams {
            sample_rate: 44_100,
            channels: 2,
        };
        assert_eq!(stereo.frame_bytes(), 4);
        assert_eq!(stereo.bytes_for_ms(1000), 44_100 * 4);
        assert_eq!(stereo.bytes_for_ms(0), 0);
        let mono = AudioParams {
            sample_rate: 8000,
            channels: 1,
        };
        assert_eq!(mono.bytes_for_ms(20), 160 * 2);
    }
}
