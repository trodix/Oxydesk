//! Deterministic audio oracle: tone detection with the Goertzel algorithm.

/// Frequency of the tone emitted by the test servers (`rdsktp-audio::TONE_HZ`).
pub const TONE_HZ: f64 = 440.0;

/// 16-bit little-endian interleaved PCM packet received through RDPSND.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Instant of reception by the client (not of consumption).
    pub arrived: std::time::Instant,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<u8>,
}

impl AudioChunk {
    /// First channel, normalized to [-1, 1].
    pub fn left(&self) -> Vec<f64> {
        let step = usize::from(self.channels.max(1)) * 2;
        self.pcm.chunks_exact(step).map(|f| f64::from(i16::from_le_bytes([f[0], f[1]])) / 32768.0).collect()
    }
}

/// Share of the signal energy carried by `freq`: 1.0 for a pure sine, ≈ 0 for noise or silence.
pub fn tone_ratio(samples: &[f64], rate: u32, freq: f64) -> f64 {
    let n = samples.len() as f64;
    let energy: f64 = samples.iter().map(|x| x * x).sum();
    if samples.is_empty() || energy < 1e-9 {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (power / (n / 2.0 * energy)).clamp(0.0, 1.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f64> {
        (0..n).map(|i| 0.5 * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin()).collect()
    }

    #[test]
    fn detects_expected_tone_only() {
        assert!(tone_ratio(&sine(440.0, 48_000, 48_000), 48_000, TONE_HZ) > 0.9);
        assert!(tone_ratio(&sine(1000.0, 48_000, 48_000), 48_000, TONE_HZ) < 0.05);
        assert_eq!(tone_ratio(&vec![0.0; 48_000], 48_000, TONE_HZ), 0.0);
    }

    #[test]
    fn left_channel_extraction() {
        let c = AudioChunk { arrived: std::time::Instant::now(), sample_rate: 48_000, channels: 2, pcm: vec![0x00, 0x40, 0xFF, 0xFF] };
        assert_eq!(c.left(), vec![0.5]);
    }
}
