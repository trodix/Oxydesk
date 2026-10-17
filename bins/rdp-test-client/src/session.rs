use std::time::Duration;

/// Full decoded desktop, 8-bit RGBA.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    /// RGB average of a square block of `size` pixels at (x, y).
    pub fn block_mean(&self, x: u32, y: u32, size: u32) -> Option<[u8; 3]> {
        if x + size > self.width || y + size > self.height {
            return None;
        }
        let mut sum = [0u64; 3];
        for row in y..y + size {
            for col in x..x + size {
                let i = ((row * self.width + col) * 4) as usize;
                for (c, acc) in sum.iter_mut().enumerate() {
                    *acc += self.rgba[i + c] as u64;
                }
            }
        }
        let n = (size * size) as u64;
        Some([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8])
    }
}

impl Frame {
    /// The desktop as an RGB PNG (alpha dropped), e.g. for documentation screenshots.
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        if self.rgba.len() != (self.width * self.height * 4) as usize {
            return Err("frame buffer does not match its size".into());
        }
        let rgb: Vec<u8> = self
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.width, self.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .and_then(|mut w| w.write_image_data(&rgb))
            .map_err(|e| e.to_string())?;
        Ok(out)
    }
}

/// One monitor of a Display Control layout, in client coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRequest {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Blocking RDP session (mock or IronRDP backend).
pub trait Session {
    /// Connects and returns the negotiated desktop size.
    fn connect(&mut self) -> Result<(u32, u32), String>;
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame, String>;
    fn send_key(&mut self, scancode: u16, pressed: bool) -> Result<(), String>;
    /// Synchronize event of the client: what a real client sends when it regains the focus.
    fn send_sync(&mut self) -> Result<(), String> {
        Err("synchronize event not supported by this backend".into())
    }
    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String>;
    fn next_clipboard_text(&mut self, timeout: Duration) -> Result<Option<String>, String>;
    /// Puts an image (PNG file) on the client's clipboard.
    fn set_clipboard_image(&mut self, _png: &[u8]) -> Result<(), String> {
        Err("image clipboard not supported by this backend".into())
    }
    /// Image (PNG file) received from the server's clipboard.
    fn next_clipboard_image(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        Err("image clipboard not supported by this backend".into())
    }
    /// Puts files on the client's clipboard (their contents are read by range when the server pastes).
    fn set_clipboard_files(&mut self, _list: rdsktp_core::FileList) -> Result<(), String> {
        Err("file clipboard not supported by this backend".into())
    }
    /// Files received from the server's clipboard.
    fn next_clipboard_files(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<rdsktp_core::FileList>, String> {
        Err("file clipboard not supported by this backend".into())
    }
    /// Next audio packet (`None` if nothing before the timeout).
    fn next_audio(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<crate::audio::AudioChunk>, String> {
        Err("audio not supported by this backend".into())
    }
    /// Sends a whole monitor layout (Display Control). The next frames carry the new desktop.
    fn set_monitors(&mut self, _monitors: &[MonitorRequest]) -> Result<(), String> {
        Err("Display Control not supported by this backend".into())
    }
    fn close(&mut self);
    /// Abrupt loss of the link: drops the connection without any RDP disconnect (default: `close`).
    fn abort(&mut self) {
        self.close();
    }
}

/// IronRDP backend: to be written in M2. Always fails, never a false success.
pub struct UnimplementedSession;

impl Session for UnimplementedSession {
    fn connect(&mut self) -> Result<(u32, u32), String> {
        Err("IronRDP backend not implemented (planned for M2); use --mock for the mock".into())
    }
    fn next_frame(&mut self, _: Duration) -> Result<Frame, String> {
        Err("not connected".into())
    }
    fn send_key(&mut self, _: u16, _: bool) -> Result<(), String> {
        Err("not connected".into())
    }
    fn set_clipboard_text(&mut self, _: &str) -> Result<(), String> {
        Err("not connected".into())
    }
    fn next_clipboard_text(&mut self, _: Duration) -> Result<Option<String>, String> {
        Err("not connected".into())
    }
    fn close(&mut self) {}
}

#[cfg(test)]
mod png_tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_png_without_alpha() {
        let f = Frame {
            width: 2,
            height: 1,
            rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
        };
        let bytes = f.to_png().unwrap();
        let mut r = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..6], &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_truncated_buffer_is_refused() {
        let f = Frame {
            width: 2,
            height: 2,
            rgba: vec![0; 4],
        };
        assert!(f.to_png().is_err());
    }
}
