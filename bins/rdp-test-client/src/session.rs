use std::time::Duration;

/// Bureau complet décodé, RGBA 8 bits.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    /// Moyenne RGB d'un bloc carré de `size` pixels en (x, y).
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

/// Session RDP bloquante (mock ou backend IronRDP).
pub trait Session {
    /// Se connecte et retourne la taille de bureau négociée.
    fn connect(&mut self) -> Result<(u32, u32), String>;
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame, String>;
    fn send_key(&mut self, scancode: u16, pressed: bool) -> Result<(), String>;
    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String>;
    fn next_clipboard_text(&mut self, timeout: Duration) -> Result<Option<String>, String>;
    /// Prochain paquet audio (`None` si rien avant le délai).
    fn next_audio(&mut self, _timeout: Duration) -> Result<Option<crate::audio::AudioChunk>, String> {
        Err("audio non supporté par ce backend".into())
    }
    fn close(&mut self);
}

/// Backend IronRDP : à écrire en M2. Échoue toujours, jamais de faux succès.
pub struct UnimplementedSession;

impl Session for UnimplementedSession {
    fn connect(&mut self) -> Result<(u32, u32), String> {
        Err("backend IronRDP non implémenté (prévu en M2) ; utiliser --mock pour le mock".into())
    }
    fn next_frame(&mut self, _: Duration) -> Result<Frame, String> {
        Err("non connecté".into())
    }
    fn send_key(&mut self, _: u16, _: bool) -> Result<(), String> {
        Err("non connecté".into())
    }
    fn set_clipboard_text(&mut self, _: &str) -> Result<(), String> {
        Err("non connecté".into())
    }
    fn next_clipboard_text(&mut self, _: Duration) -> Result<Option<String>, String> {
        Err("non connecté".into())
    }
    fn close(&mut self) {}
}
