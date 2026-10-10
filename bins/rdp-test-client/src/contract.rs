//! Contract with the test server (synthetic frame source).

/// Size of the marker / heartbeat blocks, in pixels.
pub const BLOCK: u32 = 32;
/// Marker position (top left).
pub const MARKER_POS: (u32, u32) = (0, 0);
/// Heartbeat position.
pub const HEARTBEAT_POS: (u32, u32) = (32, 0);
/// Per-channel tolerance (H.264 is lossy).
pub const TOLERANCE: u8 = 48;

/// Scancodes (set 1): 1..8.
pub const KEYS: [u16; 8] = [0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
/// Widely separated RGB colors, one per key.
pub const PALETTE: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 128, 0],
    [128, 0, 255],
];
pub const BLACK: [u8; 3] = [0, 0, 0];
pub const HEARTBEAT_ON: [u8; 3] = [255, 255, 255];
pub const HEARTBEAT_OFF: [u8; 3] = [48, 48, 48];
