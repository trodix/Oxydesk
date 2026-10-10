use std::time::{Duration, Instant};

use rdsktp_core::error::AudioError;
use rdsktp_core::ports::{AudioParams, AudioProvider, AudioSource};

/// Fréquence de la tonalité de test ; le client de test la détecte (Goertzel).
pub const TONE_HZ: f32 = 440.0;
const AMPLITUDE: f32 = 0.5;

/// Sinus continu cadencé en temps réel, comme le ferait une carte son.
pub struct ToneProvider;

impl AudioProvider for ToneProvider {
    fn open(&self, params: AudioParams) -> Result<Box<dyn AudioSource>, AudioError> {
        Ok(Box::new(ToneSource { params, produced: 0, started: Instant::now() }))
    }
}

struct ToneSource {
    params: AudioParams,
    /// Trames déjà produites (la phase se déduit de ce compteur : pas de dérive).
    produced: u64,
    started: Instant,
}

impl AudioSource for ToneSource {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), AudioError> {
        let fb = self.params.frame_bytes();
        let rate = f64::from(self.params.sample_rate);
        for frame in buf.chunks_exact_mut(fb) {
            let t = self.produced as f64 / rate;
            let v = (f64::from(AMPLITUDE) * (2.0 * std::f64::consts::PI * f64::from(TONE_HZ) * t).sin() * f64::from(i16::MAX)) as i16;
            for ch in frame.chunks_exact_mut(2) {
                ch.copy_from_slice(&v.to_le_bytes());
            }
            self.produced += 1;
        }
        // Cadence temps réel : on ne livre pas les données avant leur instant de capture.
        let due = self.started + Duration::from_secs_f64(self.produced as f64 / rate);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_paced_stereo_sine() {
        let p = AudioParams { sample_rate: 48_000, channels: 2 };
        let mut src = ToneProvider.open(p).unwrap();
        let t0 = Instant::now();
        let mut buf = vec![0u8; p.bytes_for_ms(100)];
        src.read_exact(&mut buf).unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(95), "doit être cadencé");
        // les deux canaux sont identiques et le signal n'est pas nul
        assert!(buf.chunks_exact(4).all(|f| f[..2] == f[2..]));
        assert!(buf.chunks_exact(2).any(|s| i16::from_le_bytes([s[0], s[1]]).abs() > 10_000));
    }
}
