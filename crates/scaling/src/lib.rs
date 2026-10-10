//! Fallback for compositors that cannot resize their desktop (KWin 6.3 `--virtual`, see ADR 010).
//!
//! `ScalingCapture` first asks the wrapped source for the real resize; when it refuses, the desktop
//! keeps its native size and the frames are fitted into the size the client asked for (aspect ratio
//! kept, black bars). `ScalingInput` maps the pointer back. Both share the same state.

mod pixels;

use std::sync::{Arc, Mutex};

use rdsktp_core::error::{CaptureError, InputError};
use rdsktp_core::frame::{Frame, FrameData};
use rdsktp_core::layout::{Layout, OutputId};
use rdsktp_core::ports::{CaptureSource, InputSink, PointerButton};
use rdsktp_core::ScaleMap;

pub use pixels::scale_frame;

#[derive(Default)]
struct State {
    /// Native size of the last frame received from the wrapped source.
    native: Option<(u32, u32)>,
    map: Option<ScaleMap>,
}

type Shared = Arc<Mutex<State>>;

/// Wraps a capture source and its input sink so that they share the scaling state.
pub fn wrap(
    capture: Box<dyn CaptureSource>,
    input: Box<dyn InputSink>,
) -> (ScalingCapture, ScalingInput) {
    let state = Shared::default();
    (
        ScalingCapture {
            inner: capture,
            state: state.clone(),
        },
        ScalingInput {
            inner: input,
            state,
        },
    )
}

pub struct ScalingCapture {
    inner: Box<dyn CaptureSource>,
    state: Shared,
}

impl CaptureSource for ScalingCapture {
    fn set_waker(&mut self, wake: Arc<rdsktp_core::wake::Wake>) {
        self.inner.set_waker(wake);
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        let native = self.state.lock().expect("scaling mutex").native;
        if native == Some((width, height)) {
            self.state.lock().expect("scaling mutex").map = None;
            return Ok(());
        }
        let refused = match self.inner.resize(width, height) {
            Ok(()) => {
                self.state.lock().expect("scaling mutex").map = None;
                return Ok(());
            }
            Err(e) => e,
        };
        let Some(map) = native.and_then(|n| ScaleMap::fit(n, (width, height))) else {
            return Err(refused);
        };
        tracing::info!(
            "{refused}: desktop {}x{} scaled to {width}x{height}",
            map.src.0,
            map.src.1
        );
        self.state.lock().expect("scaling mutex").map = Some(map);
        Ok(())
    }

    /// One output keeps the scaling fallback; several outputs go straight to the source (scaling is
    /// still single-output, so a KWin without custom modes refuses them).
    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        match layout.outputs.as_slice() {
            [o] => self.resize(o.width, o.height),
            _ => {
                self.inner.apply_layout(layout)?;
                self.state.lock().expect("scaling mutex").map = None;
                Ok(())
            }
        }
    }

    fn place(&mut self, x: i32, y: i32) -> Result<(), CaptureError> {
        self.inner.place(x, y)
    }

    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        let Some(frame) = self.inner.recv_latest(output)? else {
            return Ok(None);
        };
        let map = {
            let mut s = self.state.lock().expect("scaling mutex");
            s.native = Some((frame.width, frame.height));
            // A frame of another size than the mapping's source (real resize in flight): not scaled.
            s.map.filter(|m| m.src == (frame.width, frame.height))
        };
        match map {
            Some(m) if !matches!(frame.data, FrameData::Encoded { .. }) => scale_frame(&frame, &m)
                .map(Some)
                .map_err(|e| CaptureError::Unavailable(format!("scaling: {e}"))),
            _ => Ok(Some(frame)),
        }
    }
}

pub struct ScalingInput {
    inner: Box<dyn InputSink>,
    state: Shared,
}

impl InputSink for ScalingInput {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        self.inner.key(scancode, pressed)
    }

    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        let map = self.state.lock().expect("scaling mutex").map;
        let (x, y) = map.map_or((x, y), |m| m.to_source(x, y));
        self.inner.pointer_move(x, y)
    }

    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Result<(), InputError> {
        self.inner.pointer_button(button, pressed)
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.inner.wheel(dx, dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdsktp_core::frame::PixelFormat;

    /// 8x4 desktop, left half red / right half blue, refuses any resize.
    struct Fixed;
    impl CaptureSource for Fixed {
        fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
            let mut d = Vec::new();
            for _ in 0..4 {
                for x in 0..8 {
                    d.extend_from_slice(if x < 4 {
                        &[255, 0, 0, 255]
                    } else {
                        &[0, 0, 255, 255]
                    });
                }
            }
            Ok(Some(Frame {
                output,
                width: 8,
                height: 4,
                stride: 32,
                format: PixelFormat::Rgba8,
                data: FrameData::Shm(d),
            }))
        }
    }

    #[derive(Default, Clone)]
    struct Spy(Arc<Mutex<Vec<(i32, i32)>>>);
    impl InputSink for Spy {
        fn key(&mut self, _: u16, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
            self.0.lock().unwrap().push((x, y));
            Ok(())
        }
    }

    #[test]
    fn refused_resize_scales_frames_and_maps_the_pointer() {
        let spy = Spy::default();
        let (mut cap, mut input) = wrap(Box::new(Fixed), Box::new(spy.clone()));
        // Without a first frame the native size is unknown: the refusal is reported.
        assert!(cap.resize(4, 2).is_err());
        let first = cap.recv_latest(OutputId(0)).unwrap().unwrap();
        assert_eq!((first.width, first.height), (8, 4));
        cap.resize(4, 2).unwrap();
        let f = cap.recv_latest(OutputId(0)).unwrap().unwrap();
        assert_eq!((f.width, f.height, f.stride), (4, 2, 16));
        let FrameData::Shm(d) = &f.data else { panic!() };
        assert_eq!(&d[..4], &[255, 0, 0, 255]);
        assert_eq!(&d[12..16], &[0, 0, 255, 255]);
        input.pointer_move(3, 1).unwrap();
        assert_eq!(spy.0.lock().unwrap().as_slice(), &[(7, 3)]);
        // Back to the native size: no scaling, no mapping.
        cap.resize(8, 4).unwrap();
        assert_eq!(cap.recv_latest(OutputId(0)).unwrap().unwrap().width, 8);
        input.pointer_move(3, 1).unwrap();
        assert_eq!(spy.0.lock().unwrap().last(), Some(&(3, 1)));
    }
}
