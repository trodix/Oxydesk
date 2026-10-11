use std::sync::{Arc, Mutex};

use rdsktp_core::error::{CaptureError, ClipboardError, InputError};
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::{Layout, OutputId};
use rdsktp_core::ports::{CaptureSource, ClipboardBackend, InputSink, PointerButton};

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
/// Identity block of the outputs after the first: color `PALETTE[output rank % 8]`.
pub const IDENTITY_POS: (u32, u32) = (64, 0);
/// Block in the bottom-right corner: shows the actual desktop size on the client side.
pub const CORNER_COLOR: [u8; 3] = [255, 0, 255];
/// Pointer block: 32x32 centred on the pointer once it has moved; orange, cyan while the left button is
/// held, purple while the right one is.
pub const POINTER_COLOR: [u8; 3] = [255, 128, 0];
pub const POINTER_LEFT: [u8; 3] = [0, 255, 255];
pub const POINTER_RIGHT: [u8; 3] = [128, 0, 255];
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
    /// Rank of the output: outputs after the first carry an identity block (`IDENTITY_POS`).
    index: u32,
    /// Pointer position (desktop space) and held buttons (bit 0 left, bit 1 right).
    pointer: Option<(i32, i32)>,
    buttons: u8,
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
            index: 0,
            pointer: None,
            buttons: 0,
        }
    }

    /// Rank of the output this source renders (frames carry it; rank > 0 adds an identity block).
    pub fn with_index(mut self, index: u32) -> Self {
        self.index = index;
        self
    }

    /// Worst case for the encoder: pseudo-random noise over the whole background (changes every frame).
    pub fn with_noise(mut self, noise: bool) -> Self {
        self.noise = noise;
        self
    }

    /// Changes the rendered size (even dimensions, large enough for the contract blocks).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        if !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width < 2 * BLOCK + 32
            || height < BLOCK
        {
            return Err(CaptureError::Unavailable(format!(
                "unsupported size {width}x{height}"
            )));
        }
        (self.width, self.height) = (width, height);
        Ok(())
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
        if let Some((x, y)) = self.pointer {
            let max_x = (self.width - BLOCK) as i32;
            let max_y = (self.height - BLOCK) as i32;
            let pos = (
                (x - 16).clamp(0, max_x) as u32,
                (y - 16).clamp(0, max_y) as u32,
            );
            let color = match self.buttons {
                b if b & 1 != 0 => POINTER_LEFT,
                b if b & 2 != 0 => POINTER_RIGHT,
                _ => POINTER_COLOR,
            };
            fill(&mut px, w, pos, color);
        }
        let marker = self.marker.map_or([0, 0, 0], |i| PALETTE[i]);
        fill(&mut px, w, MARKER_POS, marker);
        let beat = if self.tick.is_multiple_of(2) {
            HEARTBEAT_ON
        } else {
            HEARTBEAT_OFF
        };
        fill(&mut px, w, HEARTBEAT_POS, beat);
        if self.index > 0 {
            fill(
                &mut px,
                w,
                IDENTITY_POS,
                PALETTE[self.index as usize % PALETTE.len()],
            );
        }
        fill(
            &mut px,
            w,
            (self.width - BLOCK, self.height - BLOCK),
            CORNER_COLOR,
        );
        Frame {
            output: OutputId(self.index),
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
    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        SyntheticSource::resize(self, width, height)
    }
}

/// Complete test double of a desktop: capture + input + "echo" clipboard.
/// Clones share the same state (test server contract, see the test client).
#[derive(Clone)]
pub struct SyntheticDesktop {
    inner: Arc<Mutex<DesktopState>>,
}

struct DesktopState {
    /// One source per output, in layout order.
    sources: Vec<SyntheticSource>,
    echo: Option<String>,
    /// Files copied by the client, echoed back as the desktop selection.
    image_echo: Option<Vec<u8>>,
    files_echo: Option<rdsktp_core::FileList>,
}

impl SyntheticDesktop {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_outputs(1, width, height)
    }

    /// `n` outputs of the same size.
    pub fn with_outputs(n: u32, width: u32, height: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DesktopState {
                sources: (0..n)
                    .map(|i| SyntheticSource::new(width, height).with_index(i))
                    .collect(),
                echo: None,
                image_echo: None,
                files_echo: None,
            })),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DesktopState> {
        self.inner.lock().expect("synthetic desktop mutex")
    }
}

impl CaptureSource for SyntheticDesktop {
    fn recv_latest(&mut self, output: OutputId) -> Result<Option<Frame>, CaptureError> {
        self.state()
            .sources
            .get_mut(output.0 as usize)
            .map(|s| Some(s.render()))
            .ok_or_else(|| CaptureError::Unavailable(format!("unknown output {}", output.0)))
    }
    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        match self.state().sources.as_mut_slice() {
            [single] => single.resize(width, height),
            _ => Err(CaptureError::ResizeUnsupported),
        }
    }
    fn apply_layout(&mut self, layout: &Layout) -> Result<(), CaptureError> {
        let mut st = self.state();
        // All or nothing: validate the sizes on scratch sources first.
        let mut sources: Vec<SyntheticSource> = Vec::new();
        for (i, o) in layout.outputs.iter().enumerate() {
            sources.push(match st.sources.get(i) {
                Some(old) => {
                    let mut s = SyntheticSource {
                        marker: old.marker,
                        tick: old.tick,
                        pointer: old.pointer,
                        buttons: old.buttons,
                        ..SyntheticSource::new(old.width, old.height).with_index(i as u32)
                    };
                    s.resize(o.width, o.height)?;
                    s
                }
                None => {
                    SyntheticSource::new(o.width.max(96), o.height.max(BLOCK)).with_index(i as u32)
                }
            });
        }
        if sources.is_empty() {
            return Err(CaptureError::Unavailable("empty layout".into()));
        }
        st.sources = sources;
        Ok(())
    }
}

impl InputSink for SyntheticDesktop {
    fn key(&mut self, scancode: u16, pressed: bool) -> Result<(), InputError> {
        if pressed {
            if let Some(i) = KEYS.iter().position(|&k| k == scancode) {
                self.state()
                    .sources
                    .iter_mut()
                    .for_each(|s| s.set_marker(i));
            }
        }
        Ok(())
    }

    fn pointer_move(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        // The pointer lives on the first output (desktop coordinates of the first output).
        if let Some(s) = self.state().sources.first_mut() {
            s.pointer = Some((x, y));
        }
        Ok(())
    }

    fn pointer_button(&mut self, button: PointerButton, pressed: bool) -> Result<(), InputError> {
        let bit = match button {
            PointerButton::Left => 1,
            PointerButton::Right => 2,
            _ => return Ok(()),
        };
        if let Some(s) = self.state().sources.first_mut() {
            if pressed {
                s.buttons |= bit;
            } else {
                s.buttons &= !bit;
            }
        }
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

    fn set_image(&mut self, png: &[u8]) -> Result<(), ClipboardError> {
        self.state().image_echo = Some(png.to_vec());
        Ok(())
    }

    fn poll_image(&mut self) -> Result<Option<Vec<u8>>, ClipboardError> {
        Ok(self.state().image_echo.take())
    }

    fn set_files(&mut self, list: rdsktp_core::FileList) -> Result<(), ClipboardError> {
        self.state().files_echo = Some(list);
        Ok(())
    }

    fn poll_files(&mut self) -> Result<Option<rdsktp_core::FileList>, ClipboardError> {
        Ok(self.state().files_echo.take())
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
    fn resize_changes_frame_size_and_rejects_bad_sizes() {
        let mut s = SyntheticSource::new(256, 128);
        s.resize(512, 200).unwrap();
        let f = s.render();
        assert_eq!((f.width, f.height, f.stride), (512, 200, 2048));
        assert!(s.resize(511, 200).is_err());
        assert!(s.resize(40, 200).is_err());
        assert_eq!(s.render().width, 512, "a refused resize changes nothing");
        assert_eq!(pixel(&f, 512 - 4, 200 - 4), CORNER_COLOR);
    }

    #[test]
    fn apply_layout_adds_resizes_and_removes_outputs() {
        use rdsktp_core::layout::MonitorSpec;
        let spec = |x, w| MonitorSpec {
            x,
            y: 0,
            width: w,
            height: 200,
            primary: x == 0,
        };
        let mut d = SyntheticDesktop::new(256, 128);
        assert!(d.recv_latest(OutputId(1)).is_err());
        let two = Layout::from_monitors(&[spec(0, 300), spec(300, 400)]).unwrap();
        d.apply_layout(&two).unwrap();
        let f1 = d.recv_latest(OutputId(1)).unwrap().unwrap();
        assert_eq!((f1.output, f1.width, f1.height), (OutputId(1), 400, 200));
        assert_eq!(pixel(&f1, IDENTITY_POS.0 as usize, 0), PALETTE[1]);
        let one = Layout::from_monitors(&[spec(0, 500)]).unwrap();
        d.apply_layout(&one).unwrap();
        assert!(d.recv_latest(OutputId(1)).is_err());
        assert_eq!(d.recv_latest(OutputId(0)).unwrap().unwrap().width, 500);
        let bad = Layout::from_monitors(&[spec(0, 20)]).unwrap();
        assert!(d.apply_layout(&bad).is_err());
        assert_eq!(
            d.recv_latest(OutputId(0)).unwrap().unwrap().width,
            500,
            "refused: unchanged"
        );
    }

    #[test]
    fn pointer_block_follows_the_pointer_and_buttons() {
        let mut d = SyntheticDesktop::new(640, 360);
        let at = |d: &mut SyntheticDesktop, x, y| {
            let f = d.recv_latest(OutputId(0)).unwrap().unwrap();
            pixel(&f, x, y)
        };
        assert_ne!(at(&mut d, 300, 200), POINTER_COLOR);
        d.pointer_move(300, 200).unwrap();
        assert_eq!(at(&mut d, 300, 200), POINTER_COLOR);
        d.pointer_button(PointerButton::Left, true).unwrap();
        assert_eq!(at(&mut d, 300, 200), POINTER_LEFT);
        d.pointer_button(PointerButton::Left, false).unwrap();
        d.pointer_button(PointerButton::Right, true).unwrap();
        assert_eq!(at(&mut d, 300, 200), POINTER_RIGHT);
        d.pointer_move(5000, -9).unwrap();
        assert_eq!(
            at(&mut d, 640 - 5, 5),
            POINTER_RIGHT,
            "clamped inside the frame"
        );
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
