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
    /// Native size of the last frame of each output.
    natives: Vec<Option<(u32, u32)>>,
    /// Several outputs scaled individually (the compositor refused the layout).
    multi: Option<Vec<Slot>>,
}

/// One output of a scaled multi-output layout.
#[derive(Clone, Copy)]
struct Slot {
    /// Rectangle of the output in the client's desktop.
    rect: (i32, i32, u32, u32),
    /// Position of the native output in the compositor's desktop (outputs side by side).
    origin: (i32, i32),
    map: Option<ScaleMap>,
}

impl Slot {
    fn distance(&self, x: i32, y: i32) -> i64 {
        let (rx, ry, rw, rh) = self.rect;
        let dx = i64::from((rx - x).max(x - (rx + rw as i32 - 1)).max(0));
        let dy = i64::from((ry - y).max(y - (ry + rh as i32 - 1)).max(0));
        dx * dx + dy * dy
    }

    fn to_source(self, x: i32, y: i32) -> (i32, i32) {
        let (local_x, local_y) = (x - self.rect.0, y - self.rect.1);
        let (sx, sy) = self
            .map
            .map_or((local_x, local_y), |m| m.to_source(local_x, local_y));
        (self.origin.0 + sx, self.origin.1 + sy)
    }
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

    fn take_cursor(&mut self) -> Option<rdsktp_core::cursor::CursorUpdate> {
        self.inner.take_cursor()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        self.state.lock().expect("scaling mutex").multi = None;
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

    /// One output keeps the scaling fallback. Several outputs go to the source first; when it refuses
    /// (KWin 6.3: fixed, identical outputs), each output is scaled to its requested size, provided a
    /// frame told its native size. The native outputs are assumed side by side, left to right.
    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        if let [o] = layout.outputs.as_slice() {
            return self.resize(o.width, o.height);
        }
        let refused = match self.inner.apply_layout(layout) {
            Ok(()) => {
                let mut s = self.state.lock().expect("scaling mutex");
                s.map = None;
                s.multi = None;
                return Ok(());
            }
            Err(e) => e,
        };
        let mut s = self.state.lock().expect("scaling mutex");
        let mut origin = 0;
        let mut slots = Vec::new();
        for o in &layout.outputs {
            let Some(native) = s.natives.get(o.id.0 as usize).copied().flatten() else {
                return Err(refused);
            };
            let map = if native == (o.width, o.height) {
                None
            } else {
                Some(ScaleMap::fit(native, (o.width, o.height)).ok_or_else(|| {
                    CaptureError::Unavailable("cannot scale an empty output".into())
                })?)
            };
            slots.push(Slot {
                rect: (o.x, o.y, o.width, o.height),
                origin: (origin, 0),
                map,
            });
            origin += native.0 as i32;
        }
        tracing::info!("{refused}: {} outputs scaled individually", slots.len());
        s.map = None;
        s.multi = Some(slots);
        Ok(())
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
            let index = output.0 as usize;
            if s.natives.len() <= index {
                s.natives.resize(index + 1, None);
            }
            s.natives[index] = Some((frame.width, frame.height));
            let map = match &s.multi {
                Some(slots) => slots.get(index).and_then(|slot| slot.map),
                None => s.map,
            };
            // A frame of another size than the mapping's source (real resize in flight): not scaled.
            map.filter(|m| m.src == (frame.width, frame.height))
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
        let (x, y) = {
            let s = self.state.lock().expect("scaling mutex");
            match &s.multi {
                Some(slots) => slots
                    .iter()
                    .min_by_key(|slot| slot.distance(x, y))
                    .map_or((x, y), |slot| slot.to_source(x, y)),
                None => s.map.map_or((x, y), |m| m.to_source(x, y)),
            }
        };
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

    /// Two 8x4 outputs side by side, left half red / right half blue, refuse any layout.
    struct FixedPair;
    impl CaptureSource for FixedPair {
        fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
            Fixed.recv_latest(output)
        }
    }

    #[test]
    fn refused_layout_scales_each_output_and_maps_the_pointer() {
        let spy = Spy::default();
        let (mut cap, mut input) = wrap(Box::new(FixedPair), Box::new(spy.clone()));
        let layout = Layout {
            outputs: vec![
                rdsktp_core::layout::OutputInfo {
                    id: OutputId(0),
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                    primary: true,
                },
                rdsktp_core::layout::OutputInfo {
                    id: OutputId(1),
                    x: 4,
                    y: 0,
                    width: 4,
                    height: 2,
                    primary: false,
                },
            ],
        };
        // Native sizes unknown until each output delivered a frame.
        assert!(cap.apply_layout(&layout).is_err());
        for i in 0..2 {
            cap.recv_latest(OutputId(i)).unwrap();
        }
        cap.apply_layout(&layout).unwrap();
        for i in 0..2 {
            let f = cap.recv_latest(OutputId(i)).unwrap().unwrap();
            assert_eq!((f.width, f.height), (4, 2));
        }
        // Last pixel of the second output lands in the second native output (x 8..16).
        input.pointer_move(7, 1).unwrap();
        input.pointer_move(0, 0).unwrap();
        assert_eq!(spy.0.lock().unwrap().as_slice(), &[(15, 3), (0, 0)]);
        // A layout the source accepts, or a single output, drops the individual scaling.
        cap.apply_layout(&Layout::side_by_side(1, 8, 4)).unwrap();
        input.pointer_move(5, 2).unwrap();
        assert_eq!(spy.0.lock().unwrap().last(), Some(&(5, 2)));
    }
}
