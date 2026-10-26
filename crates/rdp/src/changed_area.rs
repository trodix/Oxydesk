//! What changed between two pictures: bitmap updates (RemoteFX) only carry the part of the screen that moved.

/// Snapping the rectangle to this grid keeps the tiles of the encoder aligned from one update to the next.
const GRID: usize = 16;

/// A rectangle of pixels: `x`, `y`, `width`, `height`.
pub type Rect = (usize, usize, usize, usize);

/// The smallest rectangle (on a 16-pixel grid) holding every pixel that differs between two BGRA pictures of
/// `width` x `height`, `None` when they are the same.
pub fn changed_rect(prev: &[u8], cur: &[u8], width: usize, height: usize) -> Option<Rect> {
    let stride = width * 4;
    debug_assert!(prev.len() >= stride * height && cur.len() >= stride * height);
    let (mut top, mut bottom) = (usize::MAX, 0);
    let (mut left, mut right) = (usize::MAX, 0);
    for y in 0..height {
        let (a, b) = (&prev[y * stride..][..stride], &cur[y * stride..][..stride]);
        if a == b {
            continue;
        }
        top = top.min(y);
        bottom = y;
        let (pa, pb) = (a.as_chunks::<4>().0, b.as_chunks::<4>().0);
        let first = pa.iter().zip(pb).position(|(p, q)| p != q);
        let last = pa.iter().zip(pb).rposition(|(p, q)| p != q);
        if let (Some(first), Some(last)) = (first, last) {
            left = left.min(first);
            right = right.max(last);
        }
    }
    if top == usize::MAX {
        return None;
    }
    let (x0, y0) = (left / GRID * GRID, top / GRID * GRID);
    let x1 = ((right + 1).div_ceil(GRID) * GRID).min(width);
    let y1 = ((bottom + 1).div_ceil(GRID) * GRID).min(height);
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// The pixels of `rect` in a BGRA picture of `width` pixels, rows packed one after the other.
pub fn crop(pixels: &[u8], width: usize, rect: Rect) -> Vec<u8> {
    let (x, y, w, h) = rect;
    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        out.extend_from_slice(&pixels[(row * width + x) * 4..][..w * 4]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 100;
    const H: usize = 60;

    fn picture() -> Vec<u8> {
        vec![0; W * H * 4]
    }

    fn paint(p: &mut [u8], x: usize, y: usize) {
        p[(y * W + x) * 4 + 1] = 255;
    }

    #[test]
    fn the_same_picture_has_no_changed_area() {
        assert_eq!(changed_rect(&picture(), &picture(), W, H), None);
    }

    #[test]
    fn a_pixel_gives_the_cell_that_holds_it() {
        let mut cur = picture();
        paint(&mut cur, 37, 21);
        assert_eq!(changed_rect(&picture(), &cur, W, H), Some((32, 16, 16, 16)));
    }

    #[test]
    fn two_distant_pixels_give_their_bounding_box() {
        let mut cur = picture();
        paint(&mut cur, 3, 2);
        paint(&mut cur, 90, 50);
        // Clamped to the picture: 90 -> cell 80..96, 50 -> cell 48..64 cut at 60.
        assert_eq!(changed_rect(&picture(), &cur, W, H), Some((0, 0, 96, 60)));
    }

    #[test]
    fn the_rectangle_stops_at_the_edge_of_the_picture() {
        let mut cur = picture();
        paint(&mut cur, W - 1, H - 1);
        assert_eq!(changed_rect(&picture(), &cur, W, H), Some((96, 48, 4, 12)));
    }

    #[test]
    fn a_change_in_the_alpha_byte_alone_is_a_change() {
        let mut cur = picture();
        cur[(5 * W + 7) * 4 + 3] = 9;
        assert!(changed_rect(&picture(), &cur, W, H).is_some());
    }

    #[test]
    fn crop_packs_the_rows_of_the_rectangle() {
        let mut p = picture();
        paint(&mut p, 10, 4);
        let out = crop(&p, W, (8, 4, 4, 2));
        assert_eq!(out.len(), 4 * 2 * 4);
        // Pixel (10, 4) is the third of the first row of the crop.
        assert_eq!(out[2 * 4 + 1], 255);
        assert_eq!(out.iter().filter(|&&b| b == 255).count(), 1);
    }
}
