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
        self.pcm
            .chunks_exact(step)
            .map(|f| f64::from(i16::from_le_bytes([f[0], f[1]])) / 32768.0)
            .collect()
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

/// Mono samples (first channel, [-1, 1]) and sample rate of a 16-bit PCM WAV file, as written by the
/// QEMU `wav` audio backend. A `data` chunk size of 0 or past the end (file not finalized) means "to EOF".
pub fn wav_samples(bytes: &[u8]) -> Result<(u32, Vec<f64>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size =
            u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().expect("4 bytes")) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().expect("4 bytes"));
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            if bits != 16 || channels == 0 || rate == 0 {
                return Err(format!(
                    "unsupported format: {channels} ch, {bits} bits, {rate} Hz"
                ));
            }
            let end = if size == 0 || body + size > bytes.len() {
                bytes.len()
            } else {
                body + size
            };
            let step = usize::from(channels) * 2;
            let samples = bytes[body..end]
                .chunks_exact(step)
                .map(|f| f64::from(i16::from_le_bytes([f[0], f[1]])) / 32768.0)
                .collect();
            return Ok((rate, samples));
        }
        pos = body + size + (size & 1);
    }
    Err("no data chunk".into())
}

/// Verdict on a recording: how many 0.5 s windows carry the expected tone (ratio >= 0.5).
/// Returns (passed, detail). At least `min_windows` windows must carry the tone.
pub fn analyze_wav(bytes: &[u8], freq: f64, min_windows: usize) -> (bool, String) {
    let (rate, samples) = match wav_samples(bytes) {
        Ok(v) => v,
        Err(e) => return (false, e),
    };
    let win = (rate / 2) as usize;
    // Zero-crossing frequency estimate per window (diagnostics: a rate mismatch shifts the pitch).
    let mut pitches: Vec<(u32, usize)> = Vec::new();
    for w in samples.chunks_exact(win) {
        let rms = (w.iter().map(|x| x * x).sum::<f64>() / w.len() as f64).sqrt();
        if rms < 0.02 {
            continue;
        }
        let zc = w
            .windows(2)
            .filter(|p| (p[0] < 0.0) != (p[1] < 0.0))
            .count();
        let hz = (zc as f64 / 2.0 / (w.len() as f64 / f64::from(rate)) / 5.0).round() as u32 * 5;
        match pitches.last_mut() {
            Some((f, n)) if *f == hz => *n += 1,
            _ => pitches.push((hz, 1)),
        }
    }
    let pitches = pitches
        .iter()
        .map(|(f, n)| format!("{f}Hz x{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let peak = samples.iter().fold(0f64, |m, x| m.max(x.abs()));
    let (mut tone, mut best, mut total) = (0usize, 0f64, 0usize);
    for w in samples.chunks_exact(win) {
        total += 1;
        let r = tone_ratio(w, rate, freq);
        best = best.max(r);
        if r >= 0.5 {
            tone += 1;
        }
    }
    (
        tone >= min_windows,
        format!("{tone}/{total} half-second windows carry {freq} Hz (best ratio {best:.2}, peak {peak:.3}, {rate} Hz; pitches: {pitches})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| 0.5 * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin())
            .collect()
    }

    #[test]
    fn detects_expected_tone_only() {
        assert!(tone_ratio(&sine(440.0, 48_000, 48_000), 48_000, TONE_HZ) > 0.9);
        assert!(tone_ratio(&sine(1000.0, 48_000, 48_000), 48_000, TONE_HZ) < 0.05);
        assert_eq!(tone_ratio(&vec![0.0; 48_000], 48_000, TONE_HZ), 0.0);
    }

    #[test]
    fn left_channel_extraction() {
        let c = AudioChunk {
            arrived: std::time::Instant::now(),
            sample_rate: 48_000,
            channels: 2,
            pcm: vec![0x00, 0x40, 0xFF, 0xFF],
        };
        assert_eq!(c.left(), vec![0.5]);
    }

    fn wav(rate: u32, pcm: &[i16], declared: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend((36 + pcm.len() as u32 * 4).to_le_bytes());
        v.extend(b"WAVEfmt ");
        v.extend(16u32.to_le_bytes());
        v.extend([1, 0, 2, 0]);
        v.extend(rate.to_le_bytes());
        v.extend((rate * 4).to_le_bytes());
        v.extend([4, 0, 16, 0]);
        v.extend(b"data");
        v.extend(declared.to_le_bytes());
        for s in pcm {
            v.extend(s.to_le_bytes());
            v.extend(s.to_le_bytes());
        }
        v
    }

    fn pcm(freq: f64, secs: usize) -> Vec<i16> {
        (0..44_100 * secs)
            .map(|i| {
                (16_000.0 * (2.0 * std::f64::consts::PI * freq * i as f64 / 44_100.0).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn wav_with_tone_passes_and_other_tone_or_silence_fails() {
        assert!(analyze_wav(&wav(44_100, &pcm(440.0, 3), 0), TONE_HZ, 3).0);
        assert!(!analyze_wav(&wav(44_100, &pcm(1000.0, 3), 0), TONE_HZ, 3).0);
        assert!(!analyze_wav(&wav(44_100, &vec![0; 44_100 * 3], 0), TONE_HZ, 3).0);
        assert!(!analyze_wav(b"junk", TONE_HZ, 3).0);
    }

    #[test]
    fn wav_too_short_fails() {
        assert!(!analyze_wav(&wav(44_100, &pcm(440.0, 1), 0), TONE_HZ, 3).0);
    }
}
