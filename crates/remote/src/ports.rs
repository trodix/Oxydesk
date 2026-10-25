//! Proxies of the desktop ports to the agent.

use std::sync::Arc;

use oxydesk_agent_proto::{Rect, ToAgent};
use oxydesk_core::error::{AudioError, CaptureError, ClipboardError, EncodeError, InputError};
use oxydesk_core::frame::{Frame, FrameData};
use oxydesk_core::layout::{Layout, OutputId};
use oxydesk_core::ports::*;

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
    fn set_waker(&mut self, wake: Arc<oxydesk_core::wake::Wake>) {
        self.0.set_waker(wake);
    }

    fn set_raw_pixels(&mut self, raw: bool) {
        self.0.set_raw_pixels(raw);
    }

    fn take_cursor(&mut self) -> Option<oxydesk_core::CursorUpdate> {
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

    fn set_color_depth(&mut self, depth: oxydesk_core::ColorDepth) {
        // One message for all outputs, like the bitrate.
        if self.output.0 == 0 {
            self.link.set_color_depth(depth.bits());
        }
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
    fn set_keyboard_layout(&mut self, layout: &oxydesk_core::XkbLayout) -> Result<(), InputError> {
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
    fn set_files(&mut self, list: oxydesk_core::FileList) -> Result<(), ClipboardError> {
        if self.0.set_clipboard_files(list) {
            Ok(())
        } else {
            Err(ClipboardError::Backend("agent unreachable".into()))
        }
    }
    fn poll_files(&mut self) -> Result<Option<oxydesk_core::FileList>, ClipboardError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_agent_proto::{read_msg, write_msg, FromAgent};
    use std::os::unix::net::UnixStream;

    fn pair() -> (Arc<AgentLink>, UnixStream) {
        let (server, mut agent) = UnixStream::pair().unwrap();
        write_msg(
            &mut agent,
            &FromAgent::Hello {
                outputs: 2,
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        (AgentLink::connect(server, 2).unwrap(), agent)
    }

    fn received(agent: &mut UnixStream) -> ToAgent {
        agent
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        read_msg::<_, ToAgent>(agent).unwrap().expect("a message")
    }

    #[test]
    fn one_encoder_per_output_and_a_factory_for_the_next_ones() {
        let (link, _agent) = pair();
        let ports = desktop_ports(link, 2);
        assert_eq!(ports.encoders.len(), 2);
        assert!(ports.make_encoder.is_some());
        assert!(ports.audio.is_some() && ports.drives.is_some() && ports.printers.is_some());
    }

    #[test]
    fn input_events_reach_the_agent_with_the_protocol_codes() {
        let (link, mut agent) = pair();
        let mut input = RemoteInput(link);
        input.key(0xe01d, true).unwrap();
        assert_eq!(
            received(&mut agent),
            ToAgent::Key {
                scancode: 0xe01d,
                pressed: true
            }
        );
        input.pointer_move(-5, 12).unwrap();
        assert_eq!(received(&mut agent), ToAgent::PointerMove { x: -5, y: 12 });
        for (button, code) in [
            (PointerButton::Left, 0),
            (PointerButton::Right, 1),
            (PointerButton::Middle, 2),
            (PointerButton::Back, 3),
            (PointerButton::Forward, 4),
        ] {
            input.pointer_button(button, false).unwrap();
            assert_eq!(
                received(&mut agent),
                ToAgent::PointerButton {
                    button: code,
                    pressed: false
                }
            );
        }
        input.wheel(0, -120).unwrap();
        assert_eq!(received(&mut agent), ToAgent::Wheel { dx: 0, dy: -120 });
        let layout = oxydesk_core::klid_to_xkb(0x040c).expect("French layout");
        input.set_keyboard_layout(&layout).unwrap();
        assert!(matches!(
            received(&mut agent),
            ToAgent::KeyboardLayout { .. }
        ));
    }

    #[test]
    fn input_fails_once_the_agent_is_gone() {
        let (link, agent) = pair();
        drop(agent);
        let mut input = RemoteInput(link.clone());
        // The first write after the close may still be buffered: it must fail within a few attempts.
        let failed = (0..50).any(|_| input.key(0x1e, true).is_err());
        assert!(failed);
        assert!(link.is_dead());
    }

    #[test]
    fn the_clipboard_goes_to_the_agent_and_text_comes_back() {
        let (link, mut agent) = pair();
        let mut clip = RemoteClipboard(link);
        clip.set_text("héllo").unwrap();
        assert_eq!(received(&mut agent), ToAgent::ClipboardSet("héllo".into()));
        clip.set_image(&[1, 2, 3]).unwrap();
        assert_eq!(
            received(&mut agent),
            ToAgent::ClipboardImageSet(vec![1, 2, 3])
        );
        assert_eq!(clip.poll_text().unwrap(), None);
        assert_eq!(clip.poll_image().unwrap(), None);
        assert!(clip.poll_files().unwrap().is_none());
    }

    #[test]
    fn a_frame_already_encoded_passes_through_and_pixels_are_refused() {
        let (link, _agent) = pair();
        let mut enc = PassthroughEncoder {
            link,
            output: OutputId(1),
        };
        let frame = |data| Frame {
            output: OutputId(1),
            width: 64,
            height: 64,
            stride: 256,
            format: oxydesk_core::frame::PixelFormat::Bgra8,
            data,
        };
        let out = enc
            .encode(&frame(FrameData::Encoded {
                keyframe: true,
                data: vec![0, 0, 0, 1, 9],
            }))
            .unwrap();
        assert_eq!(out.output, Some(OutputId(1)));
        assert!(out.keyframe);
        assert_eq!(out.data, vec![0, 0, 0, 1, 9]);
        assert!(matches!(
            enc.encode(&frame(FrameData::Shm(vec![0; 64 * 256]))),
            Err(EncodeError::Unsupported(_))
        ));
    }

    #[test]
    fn the_colour_depth_is_sent_once_for_all_outputs_and_only_when_it_changes() {
        let (link, mut agent) = pair();
        let mut first = PassthroughEncoder {
            link: link.clone(),
            output: OutputId(0),
        };
        let mut second = PassthroughEncoder {
            link,
            output: OutputId(1),
        };
        first.set_color_depth(oxydesk_core::ColorDepth::TrueColor); // what the agent starts with: nothing to say
        second.set_color_depth(oxydesk_core::ColorDepth::Rgb565); // only the first output speaks for all
        first.set_color_depth(oxydesk_core::ColorDepth::Rgb565);
        first.set_color_depth(oxydesk_core::ColorDepth::Rgb565); // unchanged: not sent again
        first.request_keyframe();
        let a = received(&mut agent);
        let b = received(&mut agent);
        assert!(
            format!("{a:?}").contains("SetColorDepth") && format!("{a:?}").contains("16"),
            "got {a:?}"
        );
        assert!(
            format!("{b:?}").to_lowercase().contains("keyframe"),
            "got {b:?}"
        );
    }

    #[test]
    fn the_bitrate_is_sent_once_for_all_outputs_and_keyframes_per_output() {
        let (link, mut agent) = pair();
        let mut first = PassthroughEncoder {
            link: link.clone(),
            output: OutputId(0),
        };
        let mut second = PassthroughEncoder {
            link,
            output: OutputId(1),
        };
        second.set_bitrate(1_000_000); // ignored: only the first output speaks for all
        first.set_bitrate(2_000_000);
        second.request_keyframe();
        let a = received(&mut agent);
        let b = received(&mut agent);
        assert!(format!("{a:?}").contains("2000000"), "got {a:?}");
        assert!(
            format!("{b:?}").to_lowercase().contains("keyframe"),
            "got {b:?}"
        );
    }

    #[test]
    fn the_capture_reports_a_closed_agent_but_not_an_idle_one() {
        let (link, agent) = pair();
        let mut capture = RemoteCapture(link.clone());
        assert!(matches!(capture.recv_latest(OutputId(0)), Ok(None)));
        link.send(&ToAgent::Wheel { dx: 0, dy: 0 });
        drop(agent);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !link.is_dead() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(link.is_dead(), "the reader notices the closed stream");
        assert!(matches!(
            capture.recv_latest(OutputId(0)),
            Err(CaptureError::Closed)
        ));
    }

    #[test]
    fn a_resize_the_agent_does_not_confirm_is_reported_as_unavailable() {
        let (link, agent) = pair();
        // An agent that answers "no".
        std::thread::spawn(move || {
            let mut agent = agent;
            while let Ok(Some(msg)) = read_msg::<_, ToAgent>(&mut agent) {
                if matches!(msg, ToAgent::Resize { .. }) {
                    write_msg(&mut agent, &FromAgent::Resized { ok: false }).unwrap();
                }
            }
        });
        let mut capture = RemoteCapture(link);
        assert!(matches!(
            capture.resize(1024, 576),
            Err(CaptureError::Unavailable(m)) if m.contains("1024x576")
        ));
    }
}
