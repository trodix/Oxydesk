//! Colour depth chosen by the client (mstsc "Colors": 15, 16, 24 or 32 bits).
//!
//! The graphics pipeline carries 32-bit pictures whatever the client asked for, so the server reduces the pictures to the
//! depth the user selected: the same picture, with the same banding as a server that really sends 16-bit bitmaps.

/// Number of distinct values kept per channel, expressed as bits, for each depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDepth {
    /// 24 and 32 bits: 8 bits per channel, nothing to reduce.
    #[default]
    TrueColor,
    /// 16 bits: 5 red, 6 green, 5 blue.
    Rgb565,
    /// 15 bits: 5 bits per channel.
    Rgb555,
    /// 8 bits (and 4 bits, which no client asks for any more): 3 red, 3 green, 2 blue.
    Rgb332,
}

impl ColorDepth {
    /// From the bits per pixel announced by the client (`0` or an unknown value: true colour).
    pub fn from_bits(bits: u8) -> Self {
        match bits {
            4 | 8 => Self::Rgb332,
            15 => Self::Rgb555,
            16 => Self::Rgb565,
            _ => Self::TrueColor,
        }
    }

    /// Bits per pixel, as announced by the client.
    pub fn bits(self) -> u8 {
        match self {
            Self::TrueColor => 32,
            Self::Rgb565 => 16,
            Self::Rgb555 => 15,
            Self::Rgb332 => 8,
        }
    }

    /// `true` when the pictures must be reduced.
    pub fn is_reduced(self) -> bool {
        self != Self::TrueColor
    }

    /// The colour a client at this depth shows for `(r, g, b)` (0..=255 per channel): each channel is cut to its bits and
    /// spread back over 0..=255, so that white stays white.
    pub fn quantize(self, r: i32, g: i32, b: i32) -> (i32, i32, i32) {
        let (rb, gb, bb) = match self {
            Self::TrueColor => return (r, g, b),
            Self::Rgb565 => (5, 6, 5),
            Self::Rgb555 => (5, 5, 5),
            Self::Rgb332 => (3, 3, 2),
        };
        (reduce(r, rb), reduce(g, gb), reduce(b, bb))
    }
}

impl ColorDepth {
    /// Reduces in place a buffer of BGRA (or BGRX) pixels; the alpha byte is left alone.
    pub fn reduce_bgra(self, pixels: &mut [u8]) {
        if !self.is_reduced() {
            return;
        }
        #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs a newer MSRV
        for px in pixels.chunks_exact_mut(4) {
            let (r, g, b) = self.quantize(i32::from(px[2]), i32::from(px[1]), i32::from(px[0]));
            px[0] = b as u8;
            px[1] = g as u8;
            px[2] = r as u8;
        }
    }
}

fn reduce(v: i32, bits: u32) -> i32 {
    let max = (1 << bits) - 1;
    let level = v.clamp(0, 255) >> (8 - bits);
    (level * 255 + max / 2) / max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bits_announced_by_the_client_select_the_depth() {
        assert_eq!(ColorDepth::from_bits(32), ColorDepth::TrueColor);
        assert_eq!(ColorDepth::from_bits(24), ColorDepth::TrueColor);
        assert_eq!(ColorDepth::from_bits(16), ColorDepth::Rgb565);
        assert_eq!(ColorDepth::from_bits(15), ColorDepth::Rgb555);
        assert_eq!(ColorDepth::from_bits(8), ColorDepth::Rgb332);
        assert_eq!(ColorDepth::from_bits(4), ColorDepth::Rgb332);
        assert_eq!(ColorDepth::from_bits(0), ColorDepth::TrueColor);
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Rgb565,
            ColorDepth::Rgb555,
            ColorDepth::Rgb332,
        ] {
            assert_eq!(ColorDepth::from_bits(depth.bits()), depth);
        }
    }

    #[test]
    fn true_colour_is_left_alone() {
        assert_eq!(ColorDepth::TrueColor.quantize(1, 2, 3), (1, 2, 3));
        assert!(!ColorDepth::TrueColor.is_reduced());
    }

    #[test]
    fn black_and_white_survive_every_depth() {
        for depth in [ColorDepth::Rgb565, ColorDepth::Rgb555, ColorDepth::Rgb332] {
            assert_eq!(depth.quantize(0, 0, 0), (0, 0, 0), "{depth:?}");
            assert_eq!(depth.quantize(255, 255, 255), (255, 255, 255), "{depth:?}");
            assert!(depth.is_reduced());
        }
    }

    #[test]
    fn sixteen_bits_keeps_six_bits_of_green_and_five_of_the_others() {
        // 5 bits: 32 levels, a step of about 8; 6 bits: 64 levels, a step of about 4.
        assert_eq!(ColorDepth::Rgb565.quantize(100, 100, 100), (99, 101, 99));
        assert_eq!(ColorDepth::Rgb565.quantize(7, 3, 7), (0, 0, 0));
        assert_eq!(ColorDepth::Rgb565.quantize(8, 4, 8), (8, 4, 8));
    }

    #[test]
    fn fifteen_bits_keeps_five_bits_everywhere() {
        assert_eq!(ColorDepth::Rgb555.quantize(100, 100, 100), (99, 99, 99));
    }

    #[test]
    fn eight_bits_keeps_three_three_two() {
        assert_eq!(ColorDepth::Rgb332.quantize(255, 128, 0), (255, 146, 0));
    }

    #[test]
    fn a_reduced_colour_is_stable() {
        for depth in [ColorDepth::Rgb565, ColorDepth::Rgb555, ColorDepth::Rgb332] {
            for v in 0..=255 {
                let once = depth.quantize(v, v, v);
                assert_eq!(
                    depth.quantize(once.0, once.1, once.2),
                    once,
                    "{depth:?} {v}"
                );
            }
        }
    }

    #[test]
    fn a_bgra_buffer_is_reduced_in_place_and_keeps_its_alpha() {
        let mut px = [100, 100, 100, 7, 255, 255, 255, 255, 1, 2, 3];
        ColorDepth::Rgb555.reduce_bgra(&mut px);
        assert_eq!(px, [99, 99, 99, 7, 255, 255, 255, 255, 1, 2, 3]); // the trailing partial pixel is untouched
        let mut same = [100, 100, 100, 7];
        ColorDepth::TrueColor.reduce_bgra(&mut same);
        assert_eq!(same, [100, 100, 100, 7]);
    }

    #[test]
    fn values_out_of_range_are_clamped() {
        assert_eq!(ColorDepth::Rgb565.quantize(-5, 300, 255), (0, 255, 255));
    }
}
