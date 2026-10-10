use std::time::{Duration, Instant};

use crate::contract::*;
use crate::report::Report;
use crate::session::{Frame, MonitorRequest, Session};
use crate::stats::percentile;

pub struct Config {
    pub width: u32,
    pub height: u32,
    /// Number of side-by-side screens, each `width`×`height` (M5).
    pub outputs: u32,
    pub key_samples: usize,
    pub max_key_latency_ms: f64,
    pub timeout: Duration,
    /// Runs the `multi_resize` scenario (dynamic number of monitors through Display Control).
    pub multi_resize: bool,
    /// `multi_resize` also looks for the synthetic identity blocks (not on a real desktop).
    pub identity_blocks: bool,
    /// `multi_resize` skips the layouts with more monitors than this (compositor with a fixed number of outputs).
    pub max_monitors: usize,
    /// How long to retry connecting after a connection loss.
    pub reconnect_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            outputs: 1,
            multi_resize: false,
            identity_blocks: true,
            max_monitors: usize::MAX,
            key_samples: 20,
            max_key_latency_ms: 250.0,
            timeout: Duration::from_secs(10),
            reconnect_timeout: Duration::from_secs(20),
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
            "audio_tone",
            "reconnect",
        ]
        .into_iter()
        .chain((cfg.outputs > 1).then_some("multi_screen"))
        {
            r.check(n, false, "not run: connection failed");
        }
        return;
    };
    first_frame(s, cfg, size, r);
    if cfg.outputs > 1 {
        multi_screen(s, cfg, r);
    }
    frames_flow(s, cfg, r);
    key_to_pixel(s, cfg, r);
    clipboard(s, cfg, "clipboard_roundtrip", &short_text(), r);
    clipboard(s, cfg, "clipboard_roundtrip_large", &large_text(), r);
    audio_tone(s, cfg, r);
    if cfg.multi_resize {
        multi_resize(s, cfg, r);
    }
    reconnect(s, cfg, r);
    s.close();
}

/// Monitors side by side, the first one primary; sizes are `(width, height)` per monitor.
fn row(sizes: &[(u32, u32)]) -> Vec<MonitorRequest> {
    let mut x = 0;
    sizes
        .iter()
        .enumerate()
        .map(|(i, &(width, height))| {
            let m = MonitorRequest {
                x,
                y: 0,
                width,
                height,
                primary: i == 0,
            };
            x += width as i32;
            m
        })
        .collect()
}

/// Changes the number and sizes of the monitors (2 → 3 → 1 → 2 outputs): after each Display Control layout the
/// desktop must take the bounding box and the screens k ≥ 1 must show their identity block (`PALETTE[k]` at
/// (64, 0) of their own area) at the requested position.
fn multi_resize(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    // The last step restores the configured layout: the server keeps the last one across connections.
    let configured = vec![(cfg.width, cfg.height); cfg.outputs.max(1) as usize];
    let steps = [
        ("multi_resize_two", row(&[(800, 600), (640, 480)])),
        (
            "multi_resize_add_third",
            row(&[(800, 600), (640, 480), (1024, 576)]),
        ),
        ("multi_resize_remove_to_one", row(&[(1024, 576)])),
        ("multi_resize_add_again", row(&[(1024, 576), (800, 600)])),
        ("multi_resize_restore", row(&configured)),
    ];
    for (name, monitors) in steps {
        if monitors.len() > cfg.max_monitors {
            continue;
        }
        let width = monitors
            .iter()
            .map(|m| m.x as u32 + m.width)
            .max()
            .unwrap_or(0);
        let height = monitors.iter().map(|m| m.height).max().unwrap_or(0);
        if let Err(e) = s.set_monitors(&monitors) {
            r.check(name, false, e);
            continue;
        }
        let start = Instant::now();
        let mut detail = String::from("no frame");
        let mut ok = false;
        while !ok && start.elapsed() < cfg.timeout {
            let Ok(f) = s.next_frame(cfg.timeout) else {
                break;
            };
            let seen: Vec<Option<[u8; 3]>> = monitors
                .iter()
                .enumerate()
                .skip(1)
                .map(|(k, m)| {
                    f.block_mean(m.x as u32 + 2 * BLOCK, m.y as u32, BLOCK)
                        .filter(|&c| close_to(c, PALETTE[k % PALETTE.len()]))
                })
                .collect();
            ok = (f.width, f.height) == (width, height)
                && (!cfg.identity_blocks || seen.iter().all(Option::is_some));
            detail = format!(
                "desktop {}x{}, expected {width}x{height}, identity blocks matching: {}/{}",
                f.width,
                f.height,
                seen.iter().filter(|m| m.is_some()).count(),
                seen.len()
            );
        }
        r.check(name, ok, detail);
    }
}

fn first_frame(s: &mut dyn Session, cfg: &Config, size: (u32, u32), r: &mut Report) {
    match s.next_frame(cfg.timeout) {
        Err(e) => r.check("first_frame", false, e),
        Ok(f) => {
            let expected = (cfg.width * cfg.outputs, cfg.height);
            let non_black = f.rgba.chunks(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
            let ok = (f.width, f.height) == expected && size == expected && non_black;
            r.check(
                "first_frame",
                ok,
                format!(
                    "frame {}x{}, negotiated {}x{}, expected {}x{}, non-black: {}",
                    f.width, f.height, size.0, size.1, expected.0, expected.1, non_black
                ),
            );
        }
    }
}

/// Each screen k ≥ 1 displays `PALETTE[k]` at (64, 0) of its own frame of reference: the announced desktop must
/// juxtapose them at abscissa k·width.
fn multi_screen(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    let start = Instant::now();
    let mut detail = String::new();
    while start.elapsed() < cfg.timeout {
        let Ok(f) = s.next_frame(cfg.timeout) else {
            break;
        };
        let seen: Vec<Option<[u8; 3]>> = (1..cfg.outputs)
            .map(|k| f.block_mean(k * cfg.width + 2 * BLOCK, 0, BLOCK))
            .collect();
        if seen
            .iter()
            .enumerate()
            .all(|(i, m)| m.is_some_and(|m| close_to(m, PALETTE[(i + 1) % PALETTE.len()])))
        {
            r.check(
                "multi_screen",
                true,
                format!(
                    "{} screens identified in a {}x{} desktop",
                    cfg.outputs, f.width, f.height
                ),
            );
            return;
        }
        detail = format!(
            "desktop {}x{}, identity blocks read: {seen:?}",
            f.width, f.height
        );
    }
    r.check(
        "multi_screen",
        false,
        format!("screens not identified ({detail})"),
    );
}

/// The server plays `TONE_HZ`: we must receive ≥ 1 s of continuous PCM (at real-time pace) whose
/// energy is carried by that frequency.
fn audio_tone(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    const WINDOW_SECS: f64 = 1.5;
    const SKIP_SECS: f64 = 0.3; // stream priming
                                // Packets accumulated during the other scenarios are stale: restart from a fresh stream.
    while let Ok(Some(c)) = s.next_audio(Duration::ZERO) {
        if c.arrived.elapsed() < Duration::from_millis(5) {
            break;
        }
    }
    let start = Instant::now();
    let (mut samples, mut format) = (Vec::new(), None);
    let (mut first, mut last_at) = (None, None);
    let mut total = 0usize;
    while start.elapsed() < cfg.timeout {
        match s.next_audio(Duration::from_secs(2)) {
            Ok(Some(c)) => {
                let now = c.arrived;
                format = Some((c.sample_rate, c.channels));
                let mono = c.left();
                if first.is_none() {
                    first = Some(now);
                } else {
                    total += mono.len();
                }
                if first.is_some_and(|f| now.duration_since(f).as_secs_f64() >= SKIP_SECS) {
                    samples.extend(mono);
                }
                last_at = Some(now);
                if first
                    .is_some_and(|f| now.duration_since(f).as_secs_f64() >= WINDOW_SECS + SKIP_SECS)
                {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                r.check("audio_tone", false, e);
                return;
            }
        }
    }
    let (Some((rate, channels)), Some(first), Some(last_at)) = (format, first, last_at) else {
        r.check(
            "audio_tone",
            false,
            "no audio packet received (RDPSND channel not negotiated?)",
        );
        return;
    };
    let wall = last_at.duration_since(first).as_secs_f64().max(1e-6);
    let received = total as f64 / f64::from(rate);
    let continuity = received / wall;
    let ratio = crate::audio::tone_ratio(&samples, rate, crate::audio::TONE_HZ);
    r.metric("audio_tone_ratio", ratio);
    r.metric("audio_continuity", continuity);
    let enough = samples.len() as f64 >= f64::from(rate) * 1.0;
    r.check(
        "audio_tone",
        enough && ratio > 0.5 && continuity > 0.9,
        format!("{rate} Hz, {channels} channels, tone {:.0} Hz: ratio {ratio:.2}, continuity {continuity:.2}, {:.1} s analyzed", crate::audio::TONE_HZ, samples.len() as f64 / f64::from(rate)),
    );
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
        format!("{transitions} alternations over {frames} frames"),
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
                format!("sample {i}: marker never appeared"),
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
            "p95 {:.1} ms (threshold {:.1} ms, {} samples)",
            p95,
            cfg.max_key_latency_ms,
            lat.len()
        ),
    );
}

fn short_text() -> String {
    "Naïve café — 10 € 😀 日本".to_string()
}

fn large_text() -> String {
    let unit = "Handwritten: €uro, naïve, 😀 — line\n";
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
            r.check(name, true, format!("{} identical bytes", text.len()))
        }
        Ok(Some(got)) => r.check(
            name,
            false,
            format!(
                "different text ({} bytes received, {} expected)",
                got.len(),
                text.len()
            ),
        ),
        Ok(None) => r.check(name, false, "no text received before the timeout"),
        Err(e) => r.check(name, false, e),
    }
}

/// Reconnects on the same server after an abrupt link loss, then after a clean disconnect. The
/// session of the server must survive both: a non-black desktop of the expected size comes back.
/// The server handles one connection at a time and only notices a dead peer after a while, so the
/// first attempts after an abort may be refused: they are retried until the timeout.
fn reconnect(s: &mut dyn Session, cfg: &Config, r: &mut Report) {
    let mut detail = Vec::new();
    let mut ok = true;
    for (how, abrupt) in [("abort", true), ("close", false)] {
        if abrupt {
            s.abort();
        } else {
            s.close();
        }
        let start = Instant::now();
        let outcome = loop {
            match s.connect() {
                Ok(size) => break Ok((size, start.elapsed())),
                Err(e) if start.elapsed() < cfg.reconnect_timeout => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => break Err(e),
            }
        };
        match outcome {
            Err(e) => {
                ok = false;
                detail.push(format!("after {how}: {e}"));
                break;
            }
            Ok((size, took)) => {
                let expected = (cfg.width * cfg.outputs, cfg.height);
                let shown = s.next_frame(cfg.timeout).map(|f| {
                    let non_black = f.rgba.chunks(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
                    (
                        (f.width, f.height) == expected && size == expected && non_black,
                        f,
                    )
                });
                match shown {
                    Ok((true, _)) => {
                        detail.push(format!("after {how}: ok in {} ms", took.as_millis()))
                    }
                    Ok((false, f)) => {
                        ok = false;
                        detail.push(format!(
                            "after {how}: bad desktop {}x{} (negotiated {}x{})",
                            f.width, f.height, size.0, size.1
                        ));
                    }
                    Err(e) => {
                        ok = false;
                        detail.push(format!("after {how}: no image: {e}"));
                    }
                }
            }
        }
    }
    r.check("reconnect", ok, detail.join("; "));
}
