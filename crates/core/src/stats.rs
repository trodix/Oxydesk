//! Measures of the video path (`OXYDESK_STATS=1`): series of values summarised over a window, and the gaps between events.
//!
//! Smoothness is made of the regularity of frames, not only of their number: the summary of a gap series gives the
//! median, the 95th percentile and the worst gap, which is what the eye sees as a stutter.

use std::fmt;
use std::time::{Duration, Instant};

/// Whether the video path is measured (`OXYDESK_STATS` set to anything but empty or `0`).
pub fn enabled() -> bool {
    std::env::var("OXYDESK_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// How often a window is reported.
pub const WINDOW: Duration = Duration::from_secs(2);

/// Most values kept per window: beyond that, the oldest are not remembered (a window holds a few hundred at most).
const KEPT: usize = 4096;

/// Values recorded during a window.
#[derive(Debug, Default, Clone)]
pub struct Series {
    values: Vec<f32>,
    sum: f64,
    count: u64,
}

impl Series {
    pub fn record(&mut self, value: f32) {
        self.count += 1;
        self.sum += f64::from(value);
        if self.values.len() < KEPT {
            self.values.push(value);
        }
    }

    pub fn record_duration(&mut self, d: Duration) {
        self.record(d.as_secs_f32() * 1000.0);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// The figures of the window, then starts a new one.
    pub fn take(&mut self) -> Summary {
        let mut values = std::mem::take(&mut self.values);
        let (sum, count) = (
            std::mem::take(&mut self.sum),
            std::mem::take(&mut self.count),
        );
        values.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f32| {
            values
                .get(((values.len() as f32 - 1.0) * q).round() as usize)
                .copied()
        };
        Summary {
            count,
            mean: if count == 0 {
                0.0
            } else {
                (sum / count as f64) as f32
            },
            p50: at(0.5).unwrap_or(0.0),
            p95: at(0.95).unwrap_or(0.0),
            max: values.last().copied().unwrap_or(0.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub count: u64,
    pub mean: f32,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
}

impl fmt::Display for Summary {
    /// `n=58 mean 17.2 p50 16.7 p95 33.4 max 50.1`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "n={} mean {:.1} p50 {:.1} p95 {:.1} max {:.1}",
            self.count, self.mean, self.p50, self.p95, self.max
        )
    }
}

/// The gaps in milliseconds between the events of a window.
#[derive(Debug, Default, Clone)]
pub struct Gaps {
    last: Option<Instant>,
    gaps: Series,
}

impl Gaps {
    /// An event happens at `now`.
    pub fn event(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now) {
            self.gaps
                .record_duration(now.saturating_duration_since(last));
        }
    }

    /// Events per second over `window`, and the gaps between them.
    pub fn take(&mut self, window: Duration) -> (f32, Summary) {
        let summary = self.gaps.take();
        // n gaps are n + 1 events of which the first belongs to the previous window.
        (
            summary.count as f32 / window.as_secs_f32().max(0.001),
            summary,
        )
    }
}

/// When a window is over: `due` is true at most once per `WINDOW`.
#[derive(Debug, Clone)]
pub struct Window {
    start: Instant,
}

impl Window {
    pub fn new(now: Instant) -> Self {
        Self { start: now }
    }

    /// The length of the window that just ended, if it did; a new one starts.
    pub fn due(&mut self, now: Instant) -> Option<Duration> {
        let elapsed = now.saturating_duration_since(self.start);
        (elapsed >= WINDOW).then(|| {
            self.start = now;
            elapsed
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_gives_its_median_its_95th_percentile_and_its_worst() {
        let mut s = Series::default();
        for v in 1..=100 {
            s.record(v as f32);
        }
        let sum = s.take();
        assert_eq!(sum.count, 100);
        assert_eq!(sum.mean, 50.5);
        assert_eq!(sum.p50, 51.0);
        assert_eq!(sum.p95, 95.0);
        assert_eq!(sum.max, 100.0);
        // A new window starts empty.
        assert_eq!(s.take().count, 0);
        assert_eq!(s.take().max, 0.0);
    }

    #[test]
    fn a_stutter_shows_in_the_percentile_and_the_maximum_not_in_the_mean() {
        let mut s = Series::default();
        for _ in 0..55 {
            s.record(16.7);
        }
        for _ in 0..5 {
            s.record(120.0);
        }
        let sum = s.take();
        assert!((sum.p50 - 16.7).abs() < 0.01);
        assert_eq!(sum.p95, 120.0);
        assert_eq!(sum.max, 120.0);
        assert!(sum.mean < 30.0);
    }

    #[test]
    fn the_gaps_between_events_are_measured_and_the_rate_follows() {
        let t0 = Instant::now();
        let mut g = Gaps::default();
        for i in 0..=10u64 {
            g.event(t0 + Duration::from_millis(20 * i));
        }
        g.event(t0 + Duration::from_millis(400));
        let (rate, sum) = g.take(Duration::from_secs(1));
        assert_eq!(sum.count, 11);
        assert_eq!(rate, 11.0);
        assert!((sum.p50 - 20.0).abs() < 0.5);
        assert!((sum.max - 200.0).abs() < 0.5);
    }

    #[test]
    fn a_window_is_due_once_per_period() {
        let t0 = Instant::now();
        let mut w = Window::new(t0);
        assert_eq!(w.due(t0 + Duration::from_secs(1)), None);
        let first = w.due(t0 + Duration::from_millis(2100)).expect("due");
        assert!(first >= WINDOW);
        assert_eq!(w.due(t0 + Duration::from_millis(2200)), None);
        assert!(w.due(t0 + Duration::from_millis(4200)).is_some());
    }

    #[test]
    fn the_summary_reads_on_one_line() {
        let mut s = Series::default();
        s.record(10.0);
        assert_eq!(
            s.take().to_string(),
            "n=1 mean 10.0 p50 10.0 p95 10.0 max 10.0"
        );
    }
}
