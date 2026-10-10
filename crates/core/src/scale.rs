//! Geometry of the server-side scaling fallback: the desktop keeps its native size, the client sees
//! it fitted (aspect ratio kept, black bars) into the size it asked for.

/// Mapping between the native desktop (`src`) and the size announced to the client (`dst`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleMap {
    pub src: (u32, u32),
    pub dst: (u32, u32),
    /// Rectangle of `dst` showing the desktop: `(x, y, width, height)`, even dimensions.
    pub rect: (u32, u32, u32, u32),
}

impl ScaleMap {
    /// Largest centered rectangle of `dst` with the aspect ratio of `src`.
    pub fn fit(src: (u32, u32), dst: (u32, u32)) -> Option<Self> {
        if src.0 == 0 || src.1 == 0 || dst.0 < 2 || dst.1 < 2 {
            return None;
        }
        let (sw, sh, dw, dh) = (
            u64::from(src.0),
            u64::from(src.1),
            u64::from(dst.0),
            u64::from(dst.1),
        );
        // Compare dw/sw with dh/sh without floating point.
        let (w, h) = if dw * sh <= dh * sw {
            (dw, (sh * dw / sw).max(2))
        } else {
            ((sw * dh / sh).max(2), dh)
        };
        let (w, h) = ((w & !1) as u32, (h & !1) as u32);
        Some(Self {
            src,
            dst,
            rect: ((dst.0 - w) / 2, (dst.1 - h) / 2, w, h),
        })
    }

    /// Position of a client pointer in the native desktop (bounded to the visible rectangle).
    pub fn to_source(&self, x: i32, y: i32) -> (i32, i32) {
        let (rx, ry, rw, rh) = self.rect;
        let map = |p: i32, origin: u32, len: u32, native: u32| {
            let rel = i64::from(p).clamp(i64::from(origin), i64::from(origin + len) - 1)
                - i64::from(origin);
            (rel * (i64::from(native) - 1) / (i64::from(len) - 1).max(1)) as i32
        };
        (map(x, rx, rw, self.src.0), map(y, ry, rh, self.src.1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_ratio_fills_the_client() {
        let m = ScaleMap::fit((1920, 1080), (1024, 576)).unwrap();
        assert_eq!(m.rect, (0, 0, 1024, 576));
    }

    #[test]
    fn narrower_ratio_gets_horizontal_bars_and_even_sizes() {
        let m = ScaleMap::fit((1920, 1080), (1000, 1000)).unwrap();
        assert_eq!(m.rect, (0, 219, 1000, 562));
        assert!(m.rect.2 & 1 == 0 && m.rect.3 & 1 == 0);
    }

    #[test]
    fn wider_ratio_gets_vertical_bars() {
        let m = ScaleMap::fit((1920, 1080), (2000, 800)).unwrap();
        assert_eq!(m.rect, (289, 0, 1422, 800));
    }

    #[test]
    fn pointer_maps_back_and_is_bounded() {
        let m = ScaleMap::fit((1920, 1080), (1000, 1000)).unwrap();
        assert_eq!(m.to_source(0, 219), (0, 0));
        assert_eq!(m.to_source(500, 500), (960, 540));
        assert_eq!(m.to_source(5000, 5000), (1919, 1079));
        assert_eq!(m.to_source(-5, -5), (0, 0)); // in the bars: bounded to the edge
    }

    #[test]
    fn degenerate_sizes_are_rejected() {
        assert!(ScaleMap::fit((0, 10), (10, 10)).is_none());
        assert!(ScaleMap::fit((10, 10), (1, 10)).is_none());
    }
}
