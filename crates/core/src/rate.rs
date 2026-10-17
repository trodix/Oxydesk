//! Video rate regulation from what the client acknowledges (EGFX frame acknowledgements): the bitrate follows
//! the capacity of the link, and the number of frames in flight follows its latency.
//!
//! Pure domain logic: the pump feeds `NetSample`s (cumulative counters) and applies the `RateTarget`s that come back.
//!
//! - **Queueing** is the difference between the smoothed round-trip time and the best one seen. On a link that
//!   carries what is sent it stays near zero; when frames are bigger than the link can take, they wait and it
//!   grows. Then the bitrate goes down by a quarter (or more); it comes back by a tenth after a few calm updates.
//! - **Window**: to keep the link busy, a frame must be sent before the previous one is acknowledged: about
//!   `base rtt × fps` frames are in flight. The EGFX default (3) is kept as a floor.

/// Bitrate of each H.264 stream when nothing says otherwise: the ceiling of the regulation.
pub const DEFAULT_BITRATE_BPS: u32 = 8_000_000;
/// Frames in flight allowed by default by the EGFX server (floor of the window).
pub const DEFAULT_IN_FLIGHT: u32 = 3;
/// Ceiling of the window: beyond it frames only pile up latency.
pub const MAX_IN_FLIGHT: u32 = 12;
/// Lowest bitrate regulation goes down to.
pub const MIN_BITRATE_BPS: u32 = 2_000_000;

/// Updates with fewer new round-trip samples than this carry no information.
const MIN_NEW_SAMPLES: u64 = 3;
/// Window while the link is congested: a single frame on the way keeps the delay down.
const CONGESTED_IN_FLIGHT: u32 = 1;
/// Samples before the best round trip is trusted to size the window (the first frames carry a keyframe).
const WARMUP_SAMPLES: u64 = 20;
/// Queueing (ms) tolerated before the bitrate goes down: at least this, or half the best round trip.
const QUEUE_FLOOR_MS: f32 = 30.0;
/// The relative tolerance never exceeds this: the best round trip seen may itself come from the congestion of the first keyframe.
const QUEUE_CEIL_MS: f32 = 60.0;
const CALM_CEIL_MS: f32 = 25.0;
/// Queueing (ms) under which the link is calm: at least this, or a fifth of the best round trip.
const CALM_FLOOR_MS: f32 = 20.0;
/// Calm updates in a row before the bitrate goes up.
const CALM_BEFORE_RISE: u32 = 4;
/// Updates ignored after a decrease: the smoothed round trip still carries the congestion that caused it.
const HOLD_AFTER_DECREASE: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateConfig {
    pub fps: f32,
    /// Ceiling of the bitrate (the configured one) and its starting value.
    pub max_bitrate_bps: u32,
    pub min_bitrate_bps: u32,
}

impl RateConfig {
    pub fn new(fps: f32, max_bitrate_bps: u32) -> Self {
        Self {
            fps,
            max_bitrate_bps,
            min_bitrate_bps: MIN_BITRATE_BPS.min(max_bitrate_bps),
        }
    }
}

/// What the connection has shown since it started (cumulative counters, like the EGFX statistics).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetSample {
    /// Smoothed round trip, frame sent to frame acknowledged.
    pub avg_rtt_ms: f32,
    /// Best round trip seen.
    pub min_rtt_ms: f32,
    pub rtt_samples: u64,
    /// Frames sent / dropped because the client had too many in flight.
    pub sent: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateTarget {
    pub bitrate_bps: u32,
    pub max_in_flight: u32,
}

pub struct RateController {
    cfg: RateConfig,
    bitrate_bps: u32,
    max_in_flight: u32,
    last: NetSample,
    calm: u32,
    hold: u32,
}

impl RateController {
    pub fn new(cfg: RateConfig) -> Self {
        Self {
            cfg,
            bitrate_bps: cfg.max_bitrate_bps,
            max_in_flight: DEFAULT_IN_FLIGHT,
            last: NetSample::default(),
            calm: 0,
            hold: 0,
        }
    }

    pub fn target(&self) -> RateTarget {
        RateTarget {
            bitrate_bps: self.bitrate_bps,
            max_in_flight: self.max_in_flight,
        }
    }

    /// Takes the counters of a new measurement; the new target if it differs from the previous one.
    pub fn update(&mut self, s: &NetSample) -> Option<RateTarget> {
        // Counters restart with a connection.
        if s.rtt_samples < self.last.rtt_samples || s.sent < self.last.sent {
            self.last = NetSample::default();
        }
        let new_samples = s.rtt_samples - self.last.rtt_samples;
        let sent = s.sent - self.last.sent;
        let dropped = s.dropped - self.last.dropped;
        let before = self.target();
        // Frames refused while nothing is acknowledged: the link is stalled.
        let stalled = new_samples == 0 && dropped >= MIN_NEW_SAMPLES && sent <= dropped;
        if new_samples < MIN_NEW_SAMPLES && !stalled {
            return None;
        }
        self.last = *s;

        let best = s.min_rtt_ms.max(0.0);
        let queue = if stalled {
            f32::MAX
        } else {
            (s.avg_rtt_ms - best).max(0.0)
        };
        // Dropped frames say nothing here: the window we shrink ourselves makes the back-pressure drop them.
        let congested = queue > QUEUE_FLOOR_MS.max((best * 0.5).min(QUEUE_CEIL_MS));
        let calm = queue < CALM_FLOOR_MS.max((best * 0.2).min(CALM_CEIL_MS));

        if self.hold > 0 {
            self.hold -= 1;
        } else if congested {
            let lower = (f64::from(self.bitrate_bps) * 0.75) as u32;
            self.bitrate_bps = lower.max(self.cfg.min_bitrate_bps);
            self.calm = 0;
            self.hold = HOLD_AFTER_DECREASE;
        } else if calm {
            self.calm += 1;
            if self.calm >= CALM_BEFORE_RISE {
                let higher = (f64::from(self.bitrate_bps) * 1.1) as u32;
                self.bitrate_bps = higher.min(self.cfg.max_bitrate_bps);
                self.calm = 0;
            }
        } else {
            self.calm = 0;
        }

        if congested {
            // The best round trip seen may itself come from a congested moment: do not widen the window now.
            self.max_in_flight = CONGESTED_IN_FLIGHT;
        } else if calm && best > 0.0 && s.rtt_samples >= WARMUP_SAMPLES {
            let busy = (best / 1000.0 * self.cfg.fps).ceil() as u32 + 1;
            self.max_in_flight = busy.clamp(DEFAULT_IN_FLIGHT, MAX_IN_FLIGHT);
        }
        let after = self.target();
        (after != before).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link of `capacity_bps` and `delay_ms` of latency carrying frames of `bitrate / 8 / fps` bytes.
    struct Link {
        capacity_bps: f64,
        delay_ms: f64,
        backlog_bytes: f64,
        smoothed: f32,
        best: f32,
        samples: u64,
        sent: u64,
        dropped: u64,
    }

    impl Link {
        fn new(capacity_bps: f64, delay_ms: f64) -> Self {
            Self {
                capacity_bps,
                delay_ms,
                backlog_bytes: 0.0,
                smoothed: 0.0,
                best: f32::MAX,
                samples: 0,
                sent: 0,
                dropped: 0,
            }
        }

        /// Half a second of the stream at `fps`, with `window` frames in flight at most.
        fn half_second(&mut self, bitrate_bps: u32, fps: f32, window: u32) {
            let frame = f64::from(bitrate_bps) / 8.0 / f64::from(fps);
            for _ in 0..(fps / 2.0) as u32 {
                let in_flight = (self.backlog_bytes / frame).ceil() as u32;
                if in_flight >= window {
                    self.dropped += 1;
                } else {
                    self.backlog_bytes += frame;
                    self.sent += 1;
                    // The frame leaves once what is before it has: serialization + latency.
                    let rtt = (self.backlog_bytes * 8.0 / self.capacity_bps * 1000.0
                        + self.delay_ms) as f32;
                    self.smoothed = if self.samples == 0 {
                        rtt
                    } else {
                        self.smoothed * 0.9 + rtt * 0.1
                    };
                    self.best = self.best.min(rtt);
                    self.samples += 1;
                }
                self.backlog_bytes =
                    (self.backlog_bytes - self.capacity_bps / 8.0 / f64::from(fps)).max(0.0);
            }
        }

        fn sample(&self) -> NetSample {
            NetSample {
                avg_rtt_ms: self.smoothed,
                min_rtt_ms: self.best,
                rtt_samples: self.samples,
                sent: self.sent,
                dropped: self.dropped,
            }
        }
    }

    /// Runs `seconds` of simulation, one update per half second; the bitrate at the end and the lowest window seen.
    fn run(link: &mut Link, ctl: &mut RateController, seconds: u32) -> RateTarget {
        for _ in 0..seconds * 2 {
            let t = ctl.target();
            link.half_second(t.bitrate_bps, ctl.cfg.fps, t.max_in_flight);
            ctl.update(&link.sample());
        }
        ctl.target()
    }

    fn controller(max: u32) -> RateController {
        RateController::new(RateConfig::new(60.0, max))
    }

    #[test]
    fn a_steady_wifi_round_trip_above_a_stale_best_one_lets_the_bitrate_rise() {
        // Seen on Wi-Fi: best round trip 3.3 ms (an early lucky sample), steady 15.5 ms afterwards, nothing queued.
        let mut ctl = controller(8_000_000);
        let s = |n: u64| NetSample {
            avg_rtt_ms: 15.5,
            min_rtt_ms: 3.3,
            rtt_samples: 100 * n,
            sent: 100 * n,
            dropped: 0,
        };
        ctl.bitrate_bps = 2_500_000;
        for n in 1..=12 {
            ctl.update(&s(n));
        }
        assert!(ctl.target().bitrate_bps > 2_500_000, "{:?}", ctl.target());
    }

    #[test]
    fn a_fast_link_keeps_the_full_bitrate_and_the_default_window() {
        let mut link = Link::new(1e9, 2.0);
        let mut ctl = controller(8_000_000);
        let t = run(&mut link, &mut ctl, 30);
        assert_eq!(t.bitrate_bps, 8_000_000);
        assert_eq!(t.max_in_flight, DEFAULT_IN_FLIGHT);
    }

    #[test]
    fn a_slow_link_brings_the_bitrate_down_to_its_capacity() {
        let mut link = Link::new(3e6, 5.0);
        let mut ctl = controller(8_000_000);
        let t = run(&mut link, &mut ctl, 60);
        assert!(
            (1_500_000..=3_600_000).contains(&t.bitrate_bps),
            "{} bit/s for a 3 Mbit/s link",
            t.bitrate_bps
        );
    }

    #[test]
    fn the_bitrate_comes_back_when_the_link_recovers() {
        let mut link = Link::new(2e6, 5.0);
        let mut ctl = controller(8_000_000);
        let low = run(&mut link, &mut ctl, 40).bitrate_bps;
        assert!(low < 3_000_000, "{low}");
        link.capacity_bps = 1e9;
        let high = run(&mut link, &mut ctl, 120).bitrate_bps;
        assert!(high > low * 2, "{low} -> {high}");
    }

    #[test]
    fn the_bitrate_never_leaves_its_bounds() {
        let mut link = Link::new(100_000.0, 5.0);
        let mut ctl = controller(8_000_000);
        let t = run(&mut link, &mut ctl, 120);
        assert_eq!(t.bitrate_bps, MIN_BITRATE_BPS);
        let mut ctl = controller(500_000);
        assert_eq!(ctl.target().bitrate_bps, 500_000);
        let mut fast = Link::new(1e9, 1.0);
        assert!(run(&mut fast, &mut ctl, 60).bitrate_bps <= 500_000);
    }

    #[test]
    fn a_long_latency_widens_the_window() {
        let mut link = Link::new(1e9, 120.0);
        let mut ctl = controller(8_000_000);
        let t = run(&mut link, &mut ctl, 10);
        // 120 ms at 60 fps: about 7 frames in the air.
        assert!(
            (7..=MAX_IN_FLIGHT).contains(&t.max_in_flight),
            "{}",
            t.max_in_flight
        );
        assert_eq!(
            t.bitrate_bps, 8_000_000,
            "latency alone does not lower the bitrate"
        );
    }

    #[test]
    fn a_stalled_link_lowers_the_bitrate_without_any_acknowledgement() {
        let mut ctl = controller(8_000_000);
        let s = NetSample {
            avg_rtt_ms: 10.0,
            min_rtt_ms: 5.0,
            rtt_samples: 100,
            sent: 100,
            dropped: 0,
        };
        ctl.update(&s);
        let stalled = NetSample {
            sent: 100 + 2,
            dropped: 40,
            ..s
        };
        let t = ctl.update(&stalled).expect("lowered");
        assert!(t.bitrate_bps < 8_000_000);
    }

    #[test]
    fn counters_restarting_with_a_new_connection_are_not_a_burst() {
        let mut ctl = controller(8_000_000);
        let mut link = Link::new(1e9, 2.0);
        run(&mut link, &mut ctl, 10);
        // New connection: all counters back to zero, nothing to conclude yet.
        assert_eq!(ctl.update(&NetSample::default()), None);
        assert_eq!(ctl.target().bitrate_bps, 8_000_000);
    }

    #[test]
    fn too_few_samples_decide_nothing() {
        let mut ctl = controller(8_000_000);
        let s = NetSample {
            avg_rtt_ms: 400.0,
            min_rtt_ms: 5.0,
            rtt_samples: 2,
            sent: 2,
            dropped: 0,
        };
        assert_eq!(ctl.update(&s), None);
    }

    #[test]
    fn frames_dropped_by_our_own_window_do_not_lower_the_bitrate() {
        let mut rc = RateController::new(RateConfig::new(30.0, 8_000_000));
        let sample = |n: u64| NetSample {
            avg_rtt_ms: 6.0,
            min_rtt_ms: 5.0,
            rtt_samples: 100 + n * 10,
            sent: 10 + n * 10,
            dropped: 200 + n * 200,
        };
        for n in 0..10 {
            rc.update(&sample(n));
        }
        assert_eq!(rc.target().bitrate_bps, 8_000_000);
    }
}
