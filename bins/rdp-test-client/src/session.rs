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
