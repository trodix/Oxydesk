use std::time::{Duration, Instant};

use crate::contract::*;
use crate::report::Report;
use crate::session::{Frame, Session};
use crate::stats::percentile;

pub struct Config {
    pub width: u32,
    pub height: u32,
    pub key_samples: usize,
    pub max_key_latency_ms: f64,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            key_samples: 20,
            max_key_latency_ms: 250.0,
            timeout: Duration::from_secs(10),
        }
    }
}

fn close_to(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.abs_diff(*y) <= TOLERANCE)
}

fn marker(f: &Frame) -> Option<[u8; 3]> {
    f.block_mean(MARKER_POS.0, MARKER_POS.1, BLOCK)
}

fn heartbeat_on(f: &Frame) -> Option<bool> {
    f.block_mean(HEARTBEAT_POS.0, HEARTBEAT_POS.1, BLOCK)
        .map(|m| m.iter().map(|&c| c as u32).sum::<u32>() > 3 * 128)
}

pub fn run_all(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    let size = match s.connect() {
        Ok(size) => {
            r.check("connect", true, format!("{}x{}", size.0, size.1));
            Some(size)
        }
        Err(e) => {
            r.check("connect", false, e);
            None
        }
    };
    let Some(size) = size else {
        for n in [
            "first_frame",
            "frames_flow",
            "key_to_pixel",
            "clipboard_roundtrip",
            "clipboard_roundtrip_large",
        ] {
            r.check(n, false, "non exécuté : connexion échouée");
        }
        return;
    };
    first_frame(s, cfg, size, r);
    frames_flow(s, cfg, r);
    key_to_pixel(s, cfg, r);
    clipboard(s, cfg, "clipboard_roundtrip", &short_text(), r);
    clipboard(s, cfg, "clipboard_roundtrip_large", &large_text(), r);
    s.close();
}

fn first_frame(s: &mut dyn Session, cfg: &Config, size: (u32, u32), r: &mut Report) {
    match s.next_frame(cfg.timeout) {
        Err(e) => r.check("first_frame", false, e),
        Ok(f) => {
            let expected = (cfg.width, cfg.height);
            let non_black = f.rgba.chunks(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
            let ok = (f.width, f.height) == expected && size == expected && non_black;
            r.check(
                "first_frame",
                ok,
                format!(
                    "frame {}x{}, négocié {}x{}, attendu {}x{}, non noire: {}",
                    f.width, f.height, size.0, size.1, expected.0, expected.1, non_black
                ),
            );
        }
    }
}

fn frames_flow(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    let start = Instant::now();
    let (mut frames, mut transitions, mut last) = (0u32, 0u32, None);
    while transitions < 10 && start.elapsed() < cfg.timeout {
        let Ok(f) = s.next_frame(cfg.timeout) else {
            break;
        };
        frames += 1;
        if let Some(on) = heartbeat_on(&f) {
            if last.is_some_and(|l| l != on) {
                transitions += 1;
            }
            last = Some(on);
        }
    }
    let secs = start.elapsed().as_secs_f64().max(1e-6);
    r.metric("fps", frames as f64 / secs);
    r.check(
        "frames_flow",
        transitions >= 10,
        format!("{transitions} alternances sur {frames} frames"),
    );
}

fn key_to_pixel(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    let mut lat = Vec::new();
    for i in 0..cfg.key_samples {
        let (key, color) = (KEYS[i % 8], PALETTE[i % 8]);
        let t0 = Instant::now();
        if let Err(e) = s.send_key(key, true) {
            r.check("key_to_pixel", false, e);
            return;
        }
        let mut seen = false;
        while t0.elapsed() < cfg.timeout {
            match s.next_frame(cfg.timeout) {
                Ok(f) if marker(&f).is_some_and(|m| close_to(m, color)) => {
                    seen = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    r.check("key_to_pixel", false, e);
                    return;
                }
            }
        }
        if !seen {
            r.check(
                "key_to_pixel",
                false,
                format!("échantillon {i} : marqueur jamais apparu"),
            );
            return;
        }
        lat.push(t0.elapsed().as_secs_f64() * 1000.0);
        let _ = s.send_key(key, false);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (p50, p95, max) = (
        percentile(&lat, 50.0).unwrap_or(f64::NAN),
        percentile(&lat, 95.0).unwrap_or(f64::NAN),
        percentile(&lat, 100.0).unwrap_or(f64::NAN),
    );
    r.metric("key_to_pixel_p50_ms", p50);
    r.metric("key_to_pixel_p95_ms", p95);
    r.metric("key_to_pixel_max_ms", max);
    r.check(
        "key_to_pixel",
        !lat.is_empty() && p95 <= cfg.max_key_latency_ms,
        format!(
            "p95 {:.1} ms (seuil {:.1} ms, {} échantillons)",
            p95,
            cfg.max_key_latency_ms,
            lat.len()
        ),
    );
}

fn short_text() -> String {
    "Café déjà vu — 10 € 😀 日本".to_string()
}

fn large_text() -> String {
    let unit = "Écrit à la main : €uro, naïve, 😀 — ligne\n";
    let mut s = String::new();
    while s.len() < 64 * 1024 {
        s.push_str(unit);
    }
    s
}

fn clipboard(s: &mut dyn Session, cfg: &Config, name: &str, text: &str, r: &mut Report) {
    if let Err(e) = s.set_clipboard_text(text) {
        r.check(name, false, e);
        return;
    }
    match s.next_clipboard_text(cfg.timeout) {
        Ok(Some(got)) if got == text => {
            r.check(name, true, format!("{} octets identiques", text.len()))
        }
        Ok(Some(got)) => r.check(
            name,
            false,
            format!(
                "texte différent ({} octets reçus, {} attendus)",
                got.len(),
                text.len()
            ),
        ),
        Ok(None) => r.check(name, false, "aucun texte reçu avant le timeout"),
        Err(e) => r.check(name, false, e),
    }
}
