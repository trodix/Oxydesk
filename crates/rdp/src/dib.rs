//! Image conversion between the desktop clipboard (PNG) and CLIPRDR (CF_DIB: a BITMAPINFOHEADER
//! followed by the pixels, without the BITMAPFILEHEADER).

/// Cap on the decoded pixels (RGBA bytes) accepted in either direction.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

const HEADER_LEN: usize = 40;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// CF_DIB (24 or 32 bits per pixel, uncompressed) -> PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, String> {
    let truncated = || "truncated DIB".to_string();
    let header = le32(dib, 0).ok_or_else(truncated)? as usize;
    if header < HEADER_LEN || header > dib.len() {
        return Err(format!("unsupported DIB header ({header} bytes)"));
    }
    let width = le32(dib, 4).ok_or_else(truncated)? as i32;
    let height = le32(dib, 8).ok_or_else(truncated)? as i32;
    let bpp = u16::from_le_bytes(dib[14..16].try_into().expect("2 bytes"));
    let compression = le32(dib, 16).ok_or_else(truncated)?;
    if width <= 0 || height == 0 {
        return Err("empty DIB".into());
    }
    if !matches!(compression, BI_RGB | BI_BITFIELDS) || !matches!(bpp, 24 | 32) {
        return Err(format!(
            "unsupported DIB ({bpp} bpp, compression {compression})"
        ));
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    if w.saturating_mul(h).saturating_mul(4) > MAX_IMAGE_BYTES {
        return Err("DIB too large".into());
    }
    let mut start = header;
    let mut alpha_mask = 0xFF00_0000u32;
    if compression == BI_BITFIELDS {
        // Only the usual BGR(A) layout is supported; the masks are inside the V4/V5 header or follow a 40-byte one.
        let (r, g, b) = (le32(dib, 40), le32(dib, 44), le32(dib, 48));
        if header == HEADER_LEN {
            start += 12;
        }
        if (r, g, b) != (Some(0x00FF_0000), Some(0x0000_FF00), Some(0x0000_00FF)) {
            return Err("unsupported DIB channel masks".into());
        }
        if header > HEADER_LEN {
            alpha_mask = le32(dib, 52).unwrap_or(0);
        }
    } else {
        alpha_mask = 0;
    }
    let bytes_pp = usize::from(bpp) / 8;
    let stride = (w * usize::from(bpp)).div_ceil(32) * 4;
    let pixels = dib.get(start..).ok_or_else(truncated)?;
    if pixels.len() < stride * h {
        return Err(truncated());
    }
    let bottom_up = height > 0;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let src = if bottom_up { h - 1 - row } else { row };
        let line = &pixels[src * stride..src * stride + w * bytes_pp];
        for px in line.chunks_exact(bytes_pp) {
            let a = if bytes_pp == 4 && alpha_mask != 0 {
                px[3]
            } else {
                255
            };
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    // A 32-bit BI_RGB DIB has no defined alpha (usually zero): treated as opaque (done above).
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// PNG -> CF_DIB (24 bits per pixel, bottom-up; transparency flattened over white because
/// Windows applications do not agree on the meaning of a DIB's alpha).
pub fn png_to_dib(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let (w, h) = {
        let i = reader.info();
        (i.width as usize, i.height as usize)
    };
    if w == 0 || h == 0 || w.saturating_mul(h).saturating_mul(4) > MAX_IMAGE_BYTES {
        return Err("PNG empty or too large".into());
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return Err("unexpanded palette".into()),
    };
    let stride = (w * 24).div_ceil(32) * 4;
    let mut dib = Vec::with_capacity(HEADER_LEN + stride * h);
    dib.extend_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&24u16.to_le_bytes());
    dib.extend_from_slice(&BI_RGB.to_le_bytes());
    dib.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    dib.extend_from_slice(&[0; 16]); // resolution, palette
    for row in (0..h).rev() {
        let line = &buf[row * frame.line_size..row * frame.line_size + w * channels];
        let start = dib.len();
        for px in line.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                4 => (px[0], px[1], px[2], px[3]),
                3 => (px[0], px[1], px[2], 255),
                2 => (px[0], px[0], px[0], px[1]),
                _ => (px[0], px[0], px[0], 255),
            };
            let over_white =
                |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
            dib.extend_from_slice(&[over_white(b), over_white(g), over_white(r)]);
        }
        dib.resize(start + stride, 0);
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(color: png::ColorType, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(color);
        e.set_depth(png::BitDepth::Eight);
        let mut wr = e.write_header().unwrap();
        wr.write_image_data(data).unwrap();
        wr.finish().unwrap();
        out
    }

    fn decode(data: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut r = png::Decoder::new(data).read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let f = r.next_frame(&mut buf).unwrap();
        (f.width, f.height, buf[..f.buffer_size()].to_vec())
    }

    #[test]
    fn opaque_image_survives_the_roundtrip_with_odd_width() {
        // 3x2 (row padding), distinct pixels.
        let rgb: Vec<u8> = (0..18).map(|i| i * 13).collect();
        let dib = png_to_dib(&png_of(png::ColorType::Rgb, 3, 2, &rgb)).unwrap();
        assert_eq!(dib.len(), 40 + 12 * 2);
        let (w, h, px) = decode(&dib_to_png(&dib).unwrap());
        assert_eq!((w, h), (3, 2));
        let expected: Vec<u8> = rgb
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        assert_eq!(px, expected);
    }

    #[test]
    fn transparency_is_flattened_over_white() {
        let dib = png_to_dib(&png_of(png::ColorType::Rgba, 1, 1, &[10, 20, 30, 0])).unwrap();
        assert_eq!(&dib[40..43], &[255, 255, 255]);
    }

    #[test]
    fn top_down_32_bit_dib_is_read_with_opaque_alpha() {
        let mut dib = Vec::new();
        for v in [40u32, 2, (-1i32) as u32] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]); // B G R x
        let (w, h, px) = decode(&dib_to_png(&dib).unwrap());
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, [3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(dib_to_png(&[1, 2, 3]).is_err());
        assert!(png_to_dib(b"not a png").is_err());
        let mut dib = png_to_dib(&png_of(png::ColorType::Rgb, 2, 2, &[0; 12])).unwrap();
        dib.truncate(50);
        assert!(dib_to_png(&dib).is_err());
    }
}
