//! Wake-up of a consumer by a producer (frame available), to replace a fixed rate with
//! event-driven operation without losing notifications.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Wake {
    generation: Mutex<u64>,
    cv: Condvar,
}

impl Wake {
    pub fn notify(&self) {
        *self.generation.lock().expect("wake mutex") += 1;
        self.cv.notify_all();
    }

    /// Current generation: read *before* polling the sources, then pass to `wait`.
    pub fn generation(&self) -> u64 {
        *self.generation.lock().expect("wake mutex")
    }

    /// Blocks until a notification newer than `seen` (returns immediately if there already was one) or `timeout`.
    pub fn wait(&self, seen: u64, timeout: Duration) {
        let g = self.generation.lock().expect("wake mutex");
        if *g != seen {
            return;
        }
        let _ = self.cv.wait_timeout_while(g, timeout, |g| *g == seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn notification_before_wait_is_not_lost() {
        let w = Wake::default();
        let seen = w.generation();
        w.notify();
        let t = Instant::now();
        w.wait(seen, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn wakes_on_notify_and_times_out_otherwise() {
        let w = Arc::new(Wake::default());
        let seen = w.generation();
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            w2.notify();
        });
        let t = Instant::now();
        w.wait(seen, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
        h.join().unwrap();
        let seen = w.generation();
        let t = Instant::now();
        w.wait(seen, Duration::from_millis(40));
        assert!(t.elapsed() >= Duration::from_millis(35));
    }
}
