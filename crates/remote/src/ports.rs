//! Proxies of the desktop ports to the agent.

use std::sync::Arc;

use rdsktp_agent_proto::{Rect, ToAgent};
use rdsktp_core::error::{AudioError, CaptureError, ClipboardError, EncodeError, InputError};
use rdsktp_core::frame::{Frame, FrameData};
use rdsktp_core::layout::{Layout, OutputId};
use rdsktp_core::ports::*;

use crate::link::AgentLink;

pub fn desktop_ports(link: Arc<AgentLink>, outputs: u32) -> DesktopPorts {
    DesktopPorts {
        capture: Box::new(RemoteCapture(link.clone())),
        encoders: (0..outputs)
            .map(|i| {
                Box::new(PassthroughEncoder {
                    link: link.clone(),
                    output: OutputId(i),
                }) as Box<dyn FrameEncoder>
            })
            .collect(),
        make_encoder: Some(Box::new({
            let link = link.clone();
            move |output| {
                Box::new(PassthroughEncoder {
                    link: link.clone(),
                    output,
                }) as Box<dyn FrameEncoder>
            }
        })),
        input: Box::new(RemoteInput(link.clone())),
        clipboard: Box::new(RemoteClipboard(link.clone())),
        drives: Some(AgentLink::drive_host(&link)),
        printers: Some(AgentLink::printer_host(&link)),
        audio: Some(Arc::new(RemoteAudio(link))),
    }
}

struct RemoteCapture(Arc<AgentLink>);

impl CaptureSource for RemoteCapture {
    fn set_waker(&mut self, wake: Arc<rdsktp_core::wake::Wake>) {
        self.0.set_waker(wake);
    }

    fn set_raw_pixels(&mut self, raw: bool) {
        self.0.set_raw_pixels(raw);
    }

    fn take_cursor(&mut self) -> Option<rdsktp_core::CursorUpdate> {
        self.0.take_cursor()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        if self
            .0
            .resize(width, height, std::time::Duration::from_secs(10))
        {
            Ok(())
        } else {
            Err(CaptureError::Unavailable(format!(
                "the agent could not resize to {width}x{height}"
            )))
        }
    }

    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        let outputs = layout
            .outputs
            .iter()
            .map(|o| Rect {
                x: o.x,
                y: o.y,
                width: o.width,
                height: o.height,
            })
            .collect();
        if self
            .0
            .set_layout(outputs, std::time::Duration::from_secs(15))
        {
            Ok(())
        } else {
            Err(CaptureError::Unavailable(
                "the agent could not apply the layout".into(),
            ))
        }
    }

    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        match self.0.take_frame(output) {
            Some(f) => Ok(Some(f)),
            None if self.0.is_dead() => Err(CaptureError::Closed),
            None => Ok(None),
        }
    }
}

/// "Encoder" of the frames already encoded by the agent: extracts the H.264 bytes from the frame.
struct PassthroughEncoder {
    link: Arc<AgentLink>,
    output: OutputId,
}

impl FrameEncoder for PassthroughEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, EncodeError> {
        match &frame.data {
            FrameData::Encoded { keyframe, data } => Ok(EncodedFrame {
                output: Some(self.output),
                keyframe: *keyframe,
                data: data.clone(),
            }),
            _ => Err(EncodeError::Unsupported(
                "frame not encoded received from the agent".into(),
            )),
        }
    }

    fn request_keyframe(&mut self) {
        self.link.request_keyframe(self.output);
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        // One message for all outputs: the agent applies it to each of its encoders.
        if self.output.0 == 0 {
            self.link.set_bitrate(bitrate_bps);
        }
    }
}

struct RemoteInput(Arc<AgentLink>);

impl RemoteInput {
    fn send(&self, msg: ToAgent) -> Result<(), InputError> {
        if self.0.send(&msg) {
            Ok(())
        } else {
            Err(InputError::Backend("agent unreachable".into()))
        }
    }
}

impl InputSink for RemoteInput {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        self.send(ToAgent::Key { scancode, pressed })
    }
    fn set_keyboard_layout(&mut self, layout: &rdsktp_core::XkbLayout) -> Result<(), InputError> {
        self.send(ToAgent::KeyboardLayout {
            layout: layout.layout.clone(),
            variant: layout.variant.clone(),
        })
    }
    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        self.send(ToAgent::PointerMove { x, y })
    }
    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Result<(), InputError> {
        let button = match button {
            PointerButton::Left => 0,
            PointerButton::Right => 1,
            PointerButton::Middle => 2,
            PointerButton::Back => 3,
            PointerButton::Forward => 4,
        };
        self.send(ToAgent::PointerButton { button, pressed })
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.send(ToAgent::Wheel { dx, dy })
    }
}

struct RemoteClipboard(Arc<AgentLink>);

impl ClipboardBackend for RemoteClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        if self.0.send(&ToAgent::ClipboardSet(text.to_owned())) {
            Ok(())
        } else {
            Err(ClipboardError::Backend("agent unreachable".into()))
        }
    }
    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(self.0.take_clipboard())
    }
    fn set_image(&mut self, png: &[u8]) -> Result<(), ClipboardError> {
        if self.0.send(&ToAgent::ClipboardImageSet(png.to_vec())) {
            Ok(())
        } else {
            Err(ClipboardError::Backend("agent unreachable".into()))
        }
    }
    fn poll_image(&mut self) -> Result<Option<Vec<u8>>, ClipboardError> {
        Ok(self.0.take_clipboard_image())
    }
    fn set_files(&mut self, list: rdsktp_core::FileList) -> Result<(), ClipboardError> {
        if self.0.set_clipboard_files(list) {
            Ok(())
        } else {
            Err(ClipboardError::Backend("agent unreachable".into()))
        }
    }
    fn poll_files(&mut self) -> Result<Option<rdsktp_core::FileList>, ClipboardError> {
        Ok(self.0.take_clipboard_files())
    }
}

struct RemoteAudio(Arc<AgentLink>);

impl AudioProvider for RemoteAudio {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        self.0.open_audio(params.sample_rate, params.channels);
        Ok(Box::new(RemoteAudioSource(self.0.clone())))
    }
}

struct RemoteAudioSource(Arc<AgentLink>);

impl AudioSource for RemoteAudioSource {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), AudioError> {
        if self.0.read_audio(buf) {
            Ok(())
        } else {
            Err(AudioError::Closed)
        }
    }
}

impl Drop for RemoteAudioSource {
    fn drop(&mut self) {
        self.0.close_audio();
    }
}
