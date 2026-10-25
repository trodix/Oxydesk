//! RGB to YCbCr conversion shared by the H.264 backends.
//!
//! RDP clients decode the AVC stream of the graphics pipeline as BT.709 with the full 0..255 range (the matrix used by
//! Windows servers and FreeRDP, [MS-RDPEGFX] 3.3.8.3). A BT.601 or limited-range encoding is accepted but displayed
//! washed out: black comes out at 16 and the chroma loses about 12 % of its amplitude.

/// BT.709 full-range luma of an RGB pixel (0..=255 per channel).
pub fn luma(r: i32, g: i32, b: i32) -> u8 {
    clamp((54 * r + 183 * g + 19 * b + 128) >> 8)
}

/// BT.709 full-range (Cb, Cr) of an RGB pixel.
pub fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    (
        clamp(((-29 * r - 99 * g + 128 * b + 128) >> 8) + 128),
        clamp(((128 * r - 116 * g - 12 * b + 128) >> 8) + 128),
    )
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Fills the planar I420 buffer `yuv` (`w * h * 3 / 2` bytes, even `w` and `h`) from `px(x, y) -> (r, g, b)`; the chroma
/// is the average of each 2x2 block.
pub fn fill_i420(px: impl Fn(usize, usize) -> (i32, i32, i32), w: usize, h: usize, yuv: &mut [u8]) {
    let (y_plane, chroma_planes) = yuv.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma_planes.split_at_mut(w * h / 4);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            y_plane[y * w + x] = luma(r, g, b);
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px(cx * 2 + dx, cy * 2 + dy);
                r += pr;
                g += pg;
                b += pb;
            }
            let (cb, cr) = chroma((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
            u_plane[cy * (w / 2) + cx] = cb;
            v_plane[cy * (w / 2) + cx] = cr;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_and_white_use_the_full_range() {
        assert_eq!(luma(0, 0, 0), 0);
        assert_eq!(luma(255, 255, 255), 255);
        assert_eq!(chroma(0, 0, 0), (128, 128));
        assert_eq!(chroma(255, 255, 255), (128, 128));
    }

    #[test]
    fn greys_have_no_chroma() {
        for v in [1, 64, 127, 200, 254] {
            assert_eq!(chroma(v, v, v), (128, 128), "grey {v}");
        }
    }

    #[test]
    fn primaries_follow_bt709() {
        // Reference values: Y = 0.2126 R + 0.7152 G + 0.0722 B, Cb = (B - Y) / 1.8556 + 128, Cr = (R - Y) / 1.5748 + 128.
        assert_eq!(luma(255, 0, 0), 54);
        assert_eq!(chroma(255, 0, 0), (99, 255));
        assert_eq!(luma(0, 255, 0), 182);
        assert_eq!(chroma(0, 255, 0), (29, 12));
        assert_eq!(luma(0, 0, 255), 19);
        assert_eq!(chroma(0, 0, 255), (255, 116));
    }

    #[test]
    fn i420_planes_are_laid_out_y_then_u_then_v() {
        let (w, h) = (4, 2);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        fill_i420(
            |x, _| if x < 2 { (255, 0, 0) } else { (0, 0, 255) },
            w,
            h,
            &mut yuv,
        );
        assert_eq!(&yuv[..8], &[54, 54, 19, 19, 54, 54, 19, 19]);
        assert_eq!(&yuv[8..10], &[99, 255]); // U
        assert_eq!(&yuv[10..12], &[255, 116]); // V
    }
}
