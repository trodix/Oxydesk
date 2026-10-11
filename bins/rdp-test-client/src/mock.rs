use std::collections::VecDeque;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::contract::*;
use crate::session::{Frame, MonitorRequest, Session};

/// Faults that can be injected to test the scenarios.
#[derive(Debug, Clone)]
pub struct MockFaults {
    pub broken_clipboard: bool,
    pub key_delay: Duration,
    pub desktop: (u32, u32),
    /// Frequency emitted by the simulated audio.
    pub tone_hz: f64,
    /// The server refuses any connection after the first one.
    pub no_reconnect: bool,
}

impl Default for MockFaults {
    fn default() -> Self {
        Self {
            broken_clipboard: false,
            key_delay: Duration::ZERO,
            desktop: (1920, 1080),
            tone_hz: crate::audio::TONE_HZ,
            no_reconnect: false,
        }
    }
}

/// Simulates the test server: marker, heartbeat, clipboard echo.
pub struct MockSession {
    faults: MockFaults,
    marker: [u8; 3],
    pending: Option<(Instant, [u8; 3])>,
    beat: bool,
    clip: VecDeque<String>,
    clip_images: VecDeque<Vec<u8>>,
    audio_pos: u64,
    connections: u32,
    monitors: Vec<MonitorRequest>,
}

impl MockSession {
    pub fn new(faults: MockFaults) -> Self {
        Self {
            faults,
            marker: BLACK,
            pending: None,
            beat: false,
            clip: VecDeque::new(),
            clip_images: VecDeque::new(),
            audio_pos: 0,
            connections: 0,
            monitors: Vec::new(),
        }
    }
}

fn fill(f: &mut Frame, x0: u32, y0: u32, c: [u8; 3]) {
    for y in y0..y0 + BLOCK {
        for x in x0..x0 + BLOCK {
            let i = ((y * f.width + x) * 4) as usize;
            f.rgba[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
}

impl Session for MockSession {
    fn next_audio(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<crate::audio::AudioChunk>, String> {
        sleep(Duration::from_millis(20));
        let rate = 48_000u32;
        let pcm = (0..rate / 50)
            .flat_map(|_| {
                let t = self.audio_pos as f64 / f64::from(rate);
                self.audio_pos += 1;
                let v = (0.5
                    * (2.0 * std::f64::consts::PI * self.faults.tone_hz * t).sin()
                    * 32767.0) as i16;
                let b = v.to_le_bytes();
                [b[0], b[1], b[0], b[1]]
            })
            .collect();
        Ok(Some(crate::audio::AudioChunk {
            arrived: Instant::now(),
            sample_rate: rate,
            channels: 2,
            pcm,
        }))
    }

    fn connect(&mut self) -> Result<(u32, u32), String> {
        self.connections += 1;
        self.monitors.clear();
        if self.faults.no_reconnect && self.connections > 1 {
            return Err("connection refused (mock fault)".into());
        }
        Ok(self.faults.desktop)
    }

    fn next_frame(&mut self, _timeout: Duration) -> Result<Frame, String> {
        sleep(Duration::from_millis(2));
        if let Some((ready, color)) = self.pending {
            if Instant::now() >= ready {
                self.marker = color;
                self.pending = None;
            }
        }
        self.beat = !self.beat;
        let (w, h) = self.faults.desktop;
        // Medium gray background: "non-black" without interfering with the blocks.
        let mut f = Frame {
            width: w,
            height: h,
            rgba: vec![90; (w * h * 4) as usize],
        };
        for (k, m) in self.monitors.iter().enumerate().skip(1) {
            fill(
                &mut f,
                m.x as u32 + 2 * BLOCK,
                m.y as u32,
                PALETTE[k % PALETTE.len()],
            );
        }
        fill(&mut f, MARKER_POS.0, MARKER_POS.1, self.marker);
        fill(
            &mut f,
            HEARTBEAT_POS.0,
            HEARTBEAT_POS.1,
            if self.beat {
                HEARTBEAT_ON
            } else {
                HEARTBEAT_OFF
            },
        );
        Ok(f)
    }

    fn set_monitors(&mut self, monitors: &[MonitorRequest]) -> Result<(), String> {
        let width = monitors
            .iter()
            .map(|m| m.x as u32 + m.width)
            .max()
            .unwrap_or(0);
        let height = monitors.iter().map(|m| m.height).max().unwrap_or(0);
        self.faults.desktop = (width, height);
        self.monitors = monitors.to_vec();
        Ok(())
    }

    fn send_key(&mut self, scancode: u16, pressed: bool) -> Result<(), String> {
        if pressed {
            if let Some(i) = KEYS.iter().position(|&k| k == scancode) {
                self.pending = Some((Instant::now() + self.faults.key_delay, PALETTE[i]));
            }
        }
        Ok(())
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        let echoed = if self.faults.broken_clipboard {
            text.replace('€', "?") + "!"
        } else {
            text.to_string()
        };
        self.clip.push_back(echoed);
        Ok(())
    }

    fn set_clipboard_image(&mut self, png: &[u8]) -> Result<(), String> {
        let mut echoed = png.to_vec();
        if self.faults.broken_clipboard {
            echoed.truncate(echoed.len() / 2);
        }
        self.clip_images.push_back(echoed);
        Ok(())
    }

    fn next_clipboard_image(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        Ok(self.clip_images.pop_front())
    }

    fn next_clipboard_text(&mut self, _timeout: Duration) -> Result<Option<String>, String> {
        Ok(self.clip.pop_front())
    }

    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Report;
    use crate::scenarios::{run_all, Config};

    fn run(faults: MockFaults, cfg: Config) -> Report {
        let mut r = Report::default();
        run_all(&mut MockSession::new(faults), &cfg, &mut r);
        r
    }

    fn fast() -> Config {
        Config {
            timeout: Duration::from_secs(5),
            ..Config::default()
        }
    }

    fn failed(r: &Report) -> Vec<&str> {
        r.checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.name.as_str())
            .collect()
    }

    #[test]
    fn healthy_server_passes_all_nine() {
        let r = run(MockFaults::default(), fast());
        assert_eq!(r.checks.len(), 9);
        assert!(failed(&r).is_empty(), "{:?}", r.checks);
    }

    #[test]
    fn multi_resize_follows_the_requested_layouts() {
        let r = run(
            MockFaults::default(),
            Config {
                multi_resize: true,
                ..fast()
            },
        );
        let names: Vec<_> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"multi_resize_add_third") && names.contains(&"multi_resize_add_again")
        );
        assert!(failed(&r).is_empty(), "{:?}", r.checks);
    }

    #[test]
    fn broken_clipboard_fails_only_clipboard_checks() {
        let r = run(
            MockFaults {
                broken_clipboard: true,
                ..Default::default()
            },
            fast(),
        );
        assert_eq!(
            failed(&r),
            vec![
                "clipboard_roundtrip",
                "clipboard_roundtrip_large",
                "clipboard_image_roundtrip"
            ]
        );
    }

    #[test]
    fn slow_keys_fail_key_to_pixel() {
        let cfg = Config {
            key_samples: 4,
            max_key_latency_ms: 50.0,
            ..fast()
        };
        let r = run(
            MockFaults {
                key_delay: Duration::from_millis(120),
                ..Default::default()
            },
            cfg,
        );
        assert_eq!(failed(&r), vec!["key_to_pixel"]);
    }

    #[test]
    fn wrong_tone_fails_only_audio() {
        let r = run(
            MockFaults {
                tone_hz: 1000.0,
                ..Default::default()
            },
            fast(),
        );
        assert_eq!(failed(&r), vec!["audio_tone"]);
    }

    #[test]
    fn wrong_desktop_size_fails_first_frame() {
        let r = run(
            MockFaults {
                desktop: (1280, 720),
                ..Default::default()
            },
            fast(),
        );
        assert_eq!(failed(&r), vec!["first_frame", "reconnect"]);
    }

    #[test]
    fn refused_reconnection_fails_only_reconnect() {
        let cfg = Config {
            reconnect_timeout: Duration::from_millis(600),
            ..fast()
        };
        let r = run(
            MockFaults {
                no_reconnect: true,
                ..Default::default()
            },
            cfg,
        );
        assert_eq!(failed(&r), vec!["reconnect"]);
    }

    #[test]
    fn json_matches_contract() {
        let r = run(MockFaults::default(), fast());
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        for c in v["checks"].as_array().unwrap() {
            assert!(c["name"].is_string() && c["passed"].is_boolean() && c["detail"].is_string());
        }
        for m in v["metrics"].as_object().unwrap().values() {
            let x = m.as_f64().unwrap();
            assert_eq!(x, (x * 10.0).round() / 10.0);
        }
    }

    #[test]
    fn unimplemented_backend_never_passes() {
        let mut r = Report::default();
        run_all(&mut crate::session::UnimplementedSession, &fast(), &mut r);
        assert!(!r.passed("connect"));
        assert!(r.checks.iter().all(|c| !c.passed));
    }
}
