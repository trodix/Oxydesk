use oxydesk_core::dmabuf::with_pixels;
use oxydesk_core::error::EncodeError;
use oxydesk_core::frame::{Frame, FrameData};
use oxydesk_core::ScaleMap;

/// Frame fitted into `map.dst`: bilinear resampling of the desktop inside `map.rect`, black elsewhere.
/// The result is a compact SHM frame of the same pixel format.
pub fn scale_frame(frame: &Frame, map: &ScaleMap) -> Result<Frame, EncodeError> {
    let scale = |pixels: &[u8]| scale_pixels(pixels, frame, map);
    let data = match &frame.data {
        FrameData::Shm(d) => scale(d)?,
        FrameData::DmaBuf(dma) => with_pixels(frame, dma, scale)??,
        FrameData::Encoded { .. } => {
            return Err(EncodeError::Unsupported("already-encoded frame".into()))
        }
    };
    Ok(Frame {
        output: frame.output,
        width: map.dst.0,
        height: map.dst.1,
        stride: map.dst.0 * 4,
        format: frame.format,
        data: FrameData::Shm(data),
    })
}

fn scale_pixels(src: &[u8], frame: &Frame, map: &ScaleMap) -> Result<Vec<u8>, EncodeError> {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let stride = frame.stride as usize;
    if (frame.width, frame.height) != map.src || stride < sw * 4 || src.len() < stride * sh {
        return Err(EncodeError::Unsupported(
            "frame does not match the scaling source".into(),
        ));
    }
    let (dw, dh) = (map.dst.0 as usize, map.dst.1 as usize);
    let (rx, ry, rw, rh) = {
        let r = map.rect;
        (r.0 as usize, r.1 as usize, r.2 as usize, r.3 as usize)
    };
    let mut out = [0u8, 0, 0, 255].repeat(dw * dh);
    // Source coordinate (8 fractional bits) of each destination column / row; the last one hits the last pixel.
    let axis = |len: usize, native: usize| -> Vec<(usize, u32)> {
        (0..len)
            .map(|i| {
                let f = (i * (native - 1) * 256) / (len - 1).max(1);
                ((f >> 8).min(native - 1), (f & 255) as u32)
            })
            .collect()
    };
    let (xs, ys) = (axis(rw, sw), axis(rh, sh));
    for (j, &(y0, wy)) in ys.iter().enumerate() {
        let y1 = (y0 + 1).min(sh - 1);
        let (row0, row1) = (&src[y0 * stride..], &src[y1 * stride..]);
        let dst_row = &mut out[((ry + j) * dw + rx) * 4..][..rw * 4];
        for (i, &(x0, wx)) in xs.iter().enumerate() {
            let x1 = (x0 + 1).min(sw - 1);
            for c in 0..3 {
                let top =
                    u32::from(row0[x0 * 4 + c]) * (256 - wx) + u32::from(row0[x1 * 4 + c]) * wx;
                let bottom =
                    u32::from(row1[x0 * 4 + c]) * (256 - wx) + u32::from(row1[x1 * 4 + c]) * wx;
                dst_row[i * 4 + c] = ((top * (256 - wy) + bottom * wy + 32768) >> 16) as u8;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxydesk_core::frame::PixelFormat;
    use oxydesk_core::layout::OutputId;

    fn solid(w: u32, h: u32, px: [u8; 4], stride: u32) -> Frame {
        let mut d = vec![0; (stride * h) as usize];
        for row in d.chunks_mut(stride as usize) {
            for p in row[..(w * 4) as usize].chunks_mut(4) {
                p.copy_from_slice(&px);
            }
        }
        Frame {
            output: OutputId(0),
            width: w,
            height: h,
            stride,
            format: PixelFormat::Bgra8,
            data: FrameData::Shm(d),
        }
    }

    #[test]
    fn letterboxes_with_black_bars_and_padded_stride() {
        let f = solid(16, 8, [10, 20, 30, 255], 16 * 4 + 8);
        let map = ScaleMap::fit((16, 8), (8, 8)).unwrap(); // rect: 8x4 at y=2
        let out = scale_frame(&f, &map).unwrap();
        let FrameData::Shm(d) = &out.data else {
            panic!()
        };
        assert_eq!((out.width, out.height, out.stride), (8, 8, 32));
        let px = |x: usize, y: usize| &d[(y * 8 + x) * 4..][..4];
        assert_eq!(px(0, 0), &[0, 0, 0, 255]);
        assert_eq!(px(0, 7), &[0, 0, 0, 255]);
        assert_eq!(px(0, 2), &[10, 20, 30, 255]);
        assert_eq!(px(7, 5), &[10, 20, 30, 255]);
    }

    #[test]
    fn rejects_a_frame_of_another_size() {
        let f = solid(16, 8, [0; 4], 64);
        let map = ScaleMap::fit((32, 16), (8, 8)).unwrap();
        assert!(scale_frame(&f, &map).is_err());
    }
}
