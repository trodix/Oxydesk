//! Transfer function of the pictures the compositor hands over.
//!
//! KWin composes in linear light and encodes each output with the transfer function of its colour profile, which for the
//! default "sRGB" profile is a pure gamma 2.2. RDP clients decode the stream as sRGB (piecewise): shown as is, the dark
//! tones are lifted (13 becomes 21) and the picture looks washed out. Converting the capture to sRGB gives back what the
//! applications drew, and what the physical screen shows.

use std::str::FromStr;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// The pictures are already sRGB (synthetic desktop, test sources): left alone.
    #[default]
    Srgb,
    /// Pure gamma 2.2, as KWin encodes its captures: converted to sRGB.
    Gamma22,
}

impl FromStr for Transfer {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "srgb" => Ok(Self::Srgb),
            "gamma22" => Ok(Self::Gamma22),
            other => Err(format!("unknown transfer function: {other} (srgb|gamma22)")),
        }
    }
}

fn srgb_encode(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

impl Transfer {
    /// The value a channel takes in the sRGB picture, for each value of the capture.
    pub fn table(self) -> &'static [u8; 256] {
        static IDENTITY: [u8; 256] = {
            let mut t = [0u8; 256];
            let mut i = 0;
            while i < 256 {
                t[i] = i as u8;
                i += 1;
            }
            t
        };
        static GAMMA22: OnceLock<[u8; 256]> = OnceLock::new();
        match self {
            Self::Srgb => &IDENTITY,
            Self::Gamma22 => GAMMA22.get_or_init(|| {
                let mut t = [0u8; 256];
                for (i, v) in t.iter_mut().enumerate() {
                    let linear = (i as f64 / 255.0).powf(2.2);
                    *v = (srgb_encode(linear) * 255.0).round() as u8;
                }
                t
            }),
        }
    }

    /// `true` when the pictures must be converted.
    pub fn is_converted(self) -> bool {
        self != Self::Srgb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_is_the_identity() {
        let t = Transfer::Srgb.table();
        assert!((0..=255).all(|i| t[i] as usize == i));
        assert!(!Transfer::Srgb.is_converted());
    }

    #[test]
    fn gamma22_brings_the_lifted_dark_tones_back() {
        // A GitHub dark page (13,17,23 once drawn) reads 21,24,29 in a gamma 2.2 capture.
        let t = Transfer::Gamma22.table();
        assert_eq!([t[21], t[24], t[29]], [13, 17, 23]);
        assert_eq!([t[6], t[12], t[17]], [1, 4, 9]);
    }

    #[test]
    fn gamma22_keeps_the_ends_and_stays_monotonic() {
        let t = Transfer::Gamma22.table();
        assert_eq!((t[0], t[255]), (0, 255));
        assert!(t.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn it_parses_from_its_name() {
        assert_eq!("gamma22".parse(), Ok(Transfer::Gamma22));
        assert_eq!("srgb".parse(), Ok(Transfer::Srgb));
        assert!("bt709".parse::<Transfer>().is_err());
    }
}
