//! Uncompressed pixels for the clients that cannot decode H.264: EGFX without AVC (`AVC_DISABLED`) and
//! clients without EGFX at all (legacy bitmap updates). Last resort, far heavier than H.264.

use rdsktp_core::frame::{Frame, FrameData, PixelFormat};

/// Compact BGRA rows (`width * 4` bytes per row), the layout of the EGFX `XRGB` surfaces and of
/// `PixelFormat::BgrA32`. `None` for a frame already encoded by an agent (no pixels to read).
pub(crate) fn to_bgra(frame: &Frame) -> Result<Option<Vec<u8>>, String> {
    let (w, h, stride) = (
        frame.width as usize,
        frame.height as usize,
        frame.stride as usize,
    );
    let convert = |pixels: &[u8]| -> Result<Vec<u8>, String> {
        if stride < w * 4 || pixels.len() < stride * h {
            return Err("buffer too short for stride/height".into());
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for row in pixels.chunks(stride).take(h) {
            match frame.format {
                PixelFormat::Bgra8 => out.extend_from_slice(&row[..w * 4]),
                PixelFormat::Rgba8 => {
                    #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs a newer MSRV
                    for px in row[..w * 4].chunks_exact(4) {
                        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                    }
                }
            }
        }
        Ok(out)
    };
    match &frame.data {
        FrameData::Shm(data) => convert(data).map(Some),
        FrameData::DmaBuf(dma) => rdsktp_core::dmabuf::with_pixels(frame, dma, convert)
            .map_err(|e| e.to_string())?
            .map(Some),
        FrameData::Encoded { .. } => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdsktp_core::layout::OutputId;

    fn frame(format: PixelFormat, stride: u32, data: Vec<u8>) -> Frame {
        Frame {
            output: OutputId(0),
            width: 2,
            height: 2,
            stride,
            format,
            data: FrameData::Shm(data),
        }
    }

    #[test]
    fn bgra_is_copied_without_the_row_padding() {
        let rows = [
            [1, 2, 3, 4, 5, 6, 7, 8, 0, 0],
            [9, 10, 11, 12, 13, 14, 15, 16, 0, 0],
        ];
        let f = frame(PixelFormat::Bgra8, 10, rows.concat());
        assert_eq!(to_bgra(&f).unwrap().unwrap(), (1..=16).collect::<Vec<u8>>());
    }

    #[test]
    fn rgba_is_swapped_to_bgra() {
        let f = frame(
            PixelFormat::Rgba8,
            8,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4, 5, 6, 7, 8],
        );
        assert_eq!(to_bgra(&f).unwrap().unwrap()[..8], [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn short_buffer_and_encoded_frames() {
        assert!(to_bgra(&frame(PixelFormat::Bgra8, 8, vec![0; 8])).is_err());
        let mut f = frame(PixelFormat::Bgra8, 8, vec![]);
        f.data = FrameData::Encoded {
            keyframe: true,
            data: vec![1],
        };
        assert_eq!(to_bgra(&f).unwrap(), None);
    }
}
