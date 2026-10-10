use rdsktp_core::error::CaptureError;
use rdsktp_core::frame::{Frame, FrameData, PixelFormat};
use rdsktp_core::layout::OutputId;
use rdsktp_core::ports::CaptureSource;

// Contrat avec bins/rdp-test-client (src/contract.rs) : garder les deux synchronisés.
pub const BLOCK: u32 = 32;
pub const MARKER_POS: (u32, u32) = (0, 0);
pub const HEARTBEAT_POS: (u32, u32) = (32, 0);
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

/// Source de frames synthétique : fond animé (bruit de mouvement pour charger l'encodeur),
/// bloc marqueur et bloc heartbeat conformes au contrat du client de test.
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

    /// Pire cas pour l'encodeur : bruit pseudo-aléatoire sur tout le fond (change à chaque frame).
    pub fn with_noise(mut self, noise: bool) -> Self {
        self.noise = noise;
        self
    }

    /// Appui sur la touche `index` (0..8) : le marqueur prend la couleur correspondante.
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
                // Motif diagonal qui défile : contenu changeant sur tout l'écran.
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
        assert_eq!(pixel(&s.render(), 5, 5), PALETTE[2], "la couleur persiste");
    }

    #[test]
    fn heartbeat_alternates_every_frame() {
        let mut s = SyntheticSource::new(256, 128);
        let a = pixel(&s.render(), 40, 5);
        let b = pixel(&s.render(), 40, 5);
        assert_ne!(a, b);
    }
}
