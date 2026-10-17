//! Decoding of the PipeWire cursor metadata (`spa_meta_cursor` followed by a `spa_meta_bitmap`) into the
//! domain's pointer shape. Pure functions over the raw bytes, so they are tested without PipeWire.

use oxydesk_core::cursor::{CursorShape, CursorUpdate, MAX_CURSOR_SIZE};

// enum spa_video_format
const RGBX: u32 = 7;
const BGRX: u32 = 8;
const XRGB: u32 = 9;
const XBGR: u32 = 10;
const RGBA: u32 = 11;
const BGRA: u32 = 12;
const ARGB: u32 = 13;
const ABGR: u32 = 14;

/// `sizeof(struct spa_meta_cursor)`: id, flags, position (2 × i32), hotspot (2 × i32), bitmap_offset.
const CURSOR_HEADER: usize = 28;
/// `sizeof(struct spa_meta_bitmap)`: format, size (2 × u32), stride, offset.
const BITMAP_HEADER: usize = 20;

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    u32_at(b, at).map(|v| v as i32)
}

/// `None`: nothing to report (the metadata only moved the pointer, or it is malformed).
pub fn parse_cursor_meta(meta: &[u8]) -> Option<CursorUpdate> {
    let id = u32_at(meta, 0)?;
    if id == 0 {
        return Some(CursorUpdate::Hidden);
    }
    let (hot_x, hot_y) = (i32_at(meta, 16)?, i32_at(meta, 20)?);
    let bitmap = u32_at(meta, 24)? as usize;
    if bitmap < CURSOR_HEADER || meta.len() < bitmap + BITMAP_HEADER {
        return None; // position only
    }
    let format = u32_at(meta, bitmap)?;
    let (w, h) = (u32_at(meta, bitmap + 4)?, u32_at(meta, bitmap + 8)?);
    let stride = i32_at(meta, bitmap + 12)?;
    let pixels = bitmap.checked_add(u32_at(meta, bitmap + 16)? as usize)?;
    let max = u32::from(MAX_CURSOR_SIZE);
    if w == 0 || h == 0 || w > max || h > max || stride < (w * 4) as i32 {
        return None;
    }
    let (w, h, stride) = (w as usize, h as usize, stride as usize);
    let src = meta.get(pixels..pixels.checked_add(stride.checked_mul(h)?)?)?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in src.chunks_exact(stride) {
        #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs a newer MSRV
        for px in row[..w * 4].chunks_exact(4) {
            let (r, g, b, a) = match format {
                BGRA => (px[2], px[1], px[0], px[3]),
                RGBA => (px[0], px[1], px[2], px[3]),
                ARGB => (px[1], px[2], px[3], px[0]),
                ABGR => (px[3], px[2], px[1], px[0]),
                BGRX => (px[2], px[1], px[0], 255),
                RGBX => (px[0], px[1], px[2], 255),
                XRGB => (px[1], px[2], px[3], 255),
                XBGR => (px[3], px[2], px[1], 255),
                _ => return None,
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    CursorShape::new(
        w as u16,
        h as u16,
        hot_x.max(0) as u16,
        hot_y.max(0) as u16,
        rgba,
    )
    .map(CursorUpdate::Shape)
}

/// Size to request for the cursor metadata so that a bitmap up to `side` × `side` fits.
pub fn meta_size(side: usize) -> usize {
    CURSOR_HEADER + BITMAP_HEADER + side * side * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Metadata of a `w` × `h` bitmap in `format`, hotspot (1, 2), pixels directly after the headers.
    fn meta(id: u32, format: u32, w: u32, h: u32, stride: u32, pixels: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        for v in [id, 0, 5, 6, 1, 2, CURSOR_HEADER as u32] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        for v in [format, w, h, stride, BITMAP_HEADER as u32] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m.extend_from_slice(pixels);
        m
    }

    #[test]
    fn bgra_bitmap_becomes_rgba_with_its_hotspot() {
        let m = meta(1, BGRA, 2, 1, 8, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let Some(CursorUpdate::Shape(s)) = parse_cursor_meta(&m) else {
            panic!("shape expected")
        };
        assert_eq!((s.width, s.height, s.hot_x, s.hot_y), (2, 1, 1, 0));
        assert_eq!(s.rgba, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn row_padding_is_skipped_and_opaque_formats_get_full_alpha() {
        let m = meta(
            1,
            RGBX,
            1,
            2,
            8,
            &[1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9],
        );
        let Some(CursorUpdate::Shape(s)) = parse_cursor_meta(&m) else {
            panic!("shape expected")
        };
        assert_eq!(s.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn argb_and_abgr_orders() {
        for (format, want) in [(ARGB, [20, 30, 40, 10]), (ABGR, [40, 30, 20, 10])] {
            let m = meta(1, format, 1, 1, 4, &[10, 20, 30, 40]);
            let Some(CursorUpdate::Shape(s)) = parse_cursor_meta(&m) else {
                panic!("shape expected")
            };
            assert_eq!(s.rgba, want);
        }
    }

    #[test]
    fn id_zero_hides_the_pointer() {
        assert_eq!(
            parse_cursor_meta(&meta(0, BGRA, 1, 1, 4, &[0; 4])),
            Some(CursorUpdate::Hidden)
        );
    }

    #[test]
    fn position_only_and_malformed_metadata_report_nothing() {
        let mut position_only = meta(1, BGRA, 1, 1, 4, &[0; 4]);
        position_only[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_cursor_meta(&position_only), None);
        assert_eq!(parse_cursor_meta(&[1, 0]), None);
        // pixels cut short, unknown format, oversized bitmap, stride smaller than a row
        assert_eq!(parse_cursor_meta(&meta(1, BGRA, 2, 2, 8, &[0; 8])), None);
        assert_eq!(parse_cursor_meta(&meta(1, 99, 1, 1, 4, &[0; 4])), None);
        assert_eq!(
            parse_cursor_meta(&meta(1, BGRA, 1000, 1, 4000, &[0; 4000])),
            None
        );
        assert_eq!(parse_cursor_meta(&meta(1, BGRA, 2, 1, 4, &[0; 8])), None);
    }

    #[test]
    fn requested_size_fits_the_biggest_bitmap() {
        assert_eq!(meta_size(2), 28 + 20 + 16);
    }
}
