//! Contrat avec le serveur de test (source de frames synthétique).

/// Taille des blocs marqueur / heartbeat, en pixels.
pub const BLOCK: u32 = 32;
/// Position du marqueur (haut gauche).
pub const MARKER_POS: (u32, u32) = (0, 0);
/// Position du heartbeat.
pub const HEARTBEAT_POS: (u32, u32) = (32, 0);
/// Tolérance par canal (H.264 est avec perte).
pub const TOLERANCE: u8 = 48;

/// Scancodes (set 1) : 1..8.
pub const KEYS: [u16; 8] = [0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
/// Couleurs RGB très séparées, une par touche.
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
