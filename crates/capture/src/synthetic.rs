use std::sync::{Arc, Mutex};

use rdsktp_core::error::{CaptureError, ClipboardError, InputError};
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::{CaptureSource, ClipboardBackend, InputSink};

// Contract with bins/rdp-test-client (src/contract.rs): keep both in sync.
pub const BLOCK: u32 = 32;
pub const MARKER_POS: (u32, u32) = (0, 0);
pub const HEARTBEAT_POS: (u32, u32) = (32, 0);
pub const KEYS: [u16; 8] = [0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
pub const PALETTE: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 128, 0],
    [128, 0, 255],
];
const HEARTBEAT_ON: [u8; 3] = [255, 255, 255];
const HEARTBEAT_OFF: [u8; 3] = [48, 48, 48];

/// Synthetic frame source: animated background (motion noise to load the encoder),
/// marker block and heartbeat block compliant with the test client contract.
pub struct SyntheticSource {
    width: u32,
    height: u32,
    tick: u64,
    marker: Option<usize>,
    noise: bool,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(
            width.is_multiple_of(2)
                && height.is_multiple_of(2)
                && width >= 2 * BLOCK + 32
                && height >= BLOCK
        );
        Self {
            width,
            height,
            tick: 0,
            marker: None,
            noise: false,
        }
    }

    /// Worst case for the encoder: pseudo-random noise over the whole background (changes every frame).
    pub fn with_noise(mut self, noise: bool) -> Self {
        self.noise = noise;
        self
    }

    /// Press of key `index` (0..8): the marker takes the corresponding color.
    pub fn set_marker(&mut self, index: usize) {
        self.marker = Some(index % PALETTE.len());
    }

    pub fn render(&mut self) -> Frame {
        self.tick += 1;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut px = vec![255u8; w * h * 4];
        let t = self.tick as usize;
        for y in 0..h {
            let row = &mut px[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                // Scrolling diagonal pattern: content changing over the whole screen.
                let v = (((x + t * 4) ^ (y + t * 2)) & 0xff) as u8;
                row[x * 4] = v / 2 + 40;
                row[x * 4 + 1] = (v ^ 0x55) / 2 + 40;
                row[x * 4 + 2] = 90;
            }
        }
        if self.noise {
            let mut state = (self.tick as u32).wrapping_mul(2_654_435_761) | 1;
            for p in px.as_chunks_mut::<4>().0 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                p[0] = state as u8;
                p[1] = (state >> 8) as u8;
                p[2] = (state >> 16) as u8;
            }
        }
        let marker = self.marker.map_or([0, 0, 0], |i| PALETTE[i]);
        fill(&mut px, w, MARKER_POS, marker);
        let beat = if self.tick.is_multiple_of(2) {
            HEARTBEAT_ON
        } else {
            HEARTBEAT_OFF
        };
        fill(&mut px, w, HEARTBEAT_POS, beat);
        Frame {
            output: OutputId(0),
            width: self.width,
            height: self.height,
            stride: self.width * 4,
            format: PixelFormat::Rgba8,
            data: FrameData::Shm(px),
        }
    }
}

fn fill(px: &mut [u8], w: usize, pos: (u32, u32), c: [u8; 3]) {
    for y in pos.1 as usize..(pos.1 + BLOCK) as usize {
        for x in pos.0 as usize..(pos.0 + BLOCK) as usize {
            let i = (y * w + x) * 4;
            px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
}

impl CaptureSource for SyntheticSource {
    fn recv_latest(&mut self, _output: OutputId) -> Result<Option<Frame>, CaptureError> {
        Ok(Some(self.render()))
    }
}

/// Complete test double of a desktop: capture + input + "echo" clipboard.
/// Clones share the same state (test server contract, see the test client).
#[derive(Clone)]
pub struct SyntheticDesktop {
    inner: Arc<Mutex<DesktopState>>,
}

struct DesktopState {
    source: SyntheticSource,
    echo: Option<String>,
}

impl SyntheticDesktop {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DesktopState {
                source: SyntheticSource::new(width, height),
                echo: None,
            })),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DesktopState> {
        self.inner.lock().expect("synthetic desktop mutex")
    }
}

impl CaptureSource for SyntheticDesktop {
    fn recv_latest(&mut self, _output: OutputId) -> Result<Option<Frame>, CaptureError> {
        Ok(Some(self.state().source.render()))
    }
}

impl InputSink for SyntheticDesktop {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        if pressed {
            if let Some(i) = KEYS.iter().position(|&k| k == scancode) {
                self.state().source.set_marker(i);
            }
        }
        Ok(())
    }

    fn pointer_move(&mut self, _x: i32, _y: i32) -> Result<(), InputError> {
        Ok(())
    }
}

impl ClipboardBackend for SyntheticDesktop {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.state().echo = Some(text.to_string());
        Ok(())
    }

    fn poll_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(self.state().echo.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(f: &Frame, x: usize, y: usize) -> [u8; 3] {
        let FrameData::Shm(d) = &f.data else { panic!() };
        let i = (y * f.width as usize + x) * 4;
        [d[i], d[i + 1], d[i + 2]]
    }

    #[test]
    fn marker_starts_black_then_follows_key() {
        let mut s = SyntheticSource::new(256, 128);
        assert_eq!(pixel(&s.render(), 5, 5), [0, 0, 0]);
        s.set_marker(2);
        assert_eq!(pixel(&s.render(), 5, 5), PALETTE[2]);
        assert_eq!(pixel(&s.render(), 5, 5), PALETTE[2], "the color persists");
    }

    #[test]
    fn desktop_key_sets_marker_and_clipboard_echoes() {
        let mut d = SyntheticDesktop::new(256, 128);
        d.key(KEYS[3], true).unwrap();
        let f = d.recv_latest(OutputId(0)).unwrap().unwrap();
        assert_eq!(pixel(&f, 5, 5), PALETTE[3]);
        d.set_text("é€😀").unwrap();
        assert_eq!(d.poll_text().unwrap().as_deref(), Some("é€😀"));
        assert_eq!(d.poll_text().unwrap(), None);
    }

    #[test]
    fn heartbeat_alternates_every_frame() {
        let mut s = SyntheticSource::new(256, 128);
        let a = pixel(&s.render(), 40, 5);
        let b = pixel(&s.render(), 40, 5);
        assert_ne!(a, b);
    }
}
