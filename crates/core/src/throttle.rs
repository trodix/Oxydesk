//! Limits password guessing in login mode: too many failures from one address, or against one account,
//! block further attempts for a while. Pure logic: the caller supplies the clock.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// Failures from one address within `window` before it is blocked.
    pub max_ip_failures: u32,
    /// Failures against one account within `window` before it is blocked. Higher than the per-address
    /// limit: blocking an account lets anyone who knows its name lock its owner out for `lockout`.
    pub max_user_failures: u32,
    /// Failures older than this are forgotten.
    pub window: Duration,
    /// How long a blocked address or account stays blocked.
    pub lockout: Duration,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            max_ip_failures: 5,
            max_user_failures: 10,
            window: Duration::from_secs(300),
            lockout: Duration::from_secs(300),
        }
    }
}

/// Entries kept per table; beyond it the oldest are dropped (an attacker cannot grow memory without bound).
const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Default)]
struct Entry {
    failures: u32,
    first: Option<Instant>,
    blocked_until: Option<Instant>,
}

impl Entry {
    /// Forgets what is too old to count.
    fn refresh(&mut self, now: Instant, window: Duration) {
        let lockout_over = self.blocked_until.is_some_and(|t| t <= now);
        let window_over = self.blocked_until.is_none()
            && self.first.is_some_and(|f| now.duration_since(f) > window);
        if lockout_over || window_over {
            *self = Entry::default();
        }
    }

    fn fail(&mut self, now: Instant, max: u32, cfg: &ThrottleConfig) {
        self.refresh(now, cfg.window);
        self.first.get_or_insert(now);
        self.failures += 1;
        if self.failures >= max {
            self.blocked_until = Some(now + cfg.lockout);
        }
    }
}

#[derive(Debug)]
pub struct LoginThrottle {
    cfg: ThrottleConfig,
    ips: HashMap<IpAddr, Entry>,
    users: HashMap<String, Entry>,
}

fn trim<K: Clone + Eq + std::hash::Hash>(
    map: &mut HashMap<K, Entry>,
    now: Instant,
    window: Duration,
) {
    if map.len() < MAX_ENTRIES {
        return;
    }
    map.retain(|_, e| {
        e.refresh(now, window);
        e.failures > 0
    });
    while map.len() >= MAX_ENTRIES {
        let oldest = map
            .iter()
            .min_by_key(|(_, e)| e.first)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => map.remove(&k),
            None => break,
        };
    }
}

/// Accounts are compared without regard to case or surrounding spaces.
fn user_key(user: &str) -> String {
    user.trim().to_lowercase()
}

impl LoginThrottle {
    pub fn new(cfg: ThrottleConfig) -> Self {
        Self {
            cfg,
            ips: HashMap::new(),
            users: HashMap::new(),
        }
    }

    fn remaining(entry: Option<&Entry>, now: Instant) -> Option<Duration> {
        entry?
            .blocked_until?
            .checked_duration_since(now)
            .filter(|d| !d.is_zero())
    }

    /// Remaining block time of this address, if it is blocked.
    pub fn ip_blocked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        Self::remaining(self.ips.get(&ip), now)
    }

    /// Remaining block time for an attempt by `user` from `ip`, if either is blocked.
    pub fn blocked(&self, ip: Option<IpAddr>, user: &str, now: Instant) -> Option<Duration> {
        let by_ip = ip.and_then(|ip| self.ip_blocked(ip, now));
        let by_user = Self::remaining(self.users.get(&user_key(user)), now);
        by_ip.max(by_user)
    }

    /// A refused password. Returns `true` when this failure blocked the address or the account.
    pub fn record_failure(&mut self, ip: Option<IpAddr>, user: &str, now: Instant) -> bool {
        let mut newly_blocked = false;
        if let Some(ip) = ip {
            trim(&mut self.ips, now, self.cfg.window);
            let e = self.ips.entry(ip).or_default();
            let was = e.blocked_until.is_some();
            e.fail(now, self.cfg.max_ip_failures, &self.cfg);
            newly_blocked |= !was && e.blocked_until.is_some();
        }
        trim(&mut self.users, now, self.cfg.window);
        let e = self.users.entry(user_key(user)).or_default();
        let was = e.blocked_until.is_some();
        e.fail(now, self.cfg.max_user_failures, &self.cfg);
        newly_blocked |= !was && e.blocked_until.is_some();
        newly_blocked
    }

    /// A good login clears the account's failures and the address's.
    pub fn record_success(&mut self, ip: Option<IpAddr>, user: &str) {
        self.users.remove(&user_key(user));
        if let Some(ip) = ip {
            self.ips.remove(&ip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    const B: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

    fn cfg() -> ThrottleConfig {
        ThrottleConfig {
            max_ip_failures: 3,
            max_user_failures: 5,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(120),
        }
    }

    #[test]
    fn an_address_is_blocked_after_too_many_failures_then_released() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        assert!(!t.record_failure(Some(A), "a", t0));
        assert!(!t.record_failure(Some(A), "b", t0));
        assert!(t.record_failure(Some(A), "c", t0));
        assert_eq!(t.ip_blocked(A, t0), Some(Duration::from_secs(120)));
        assert_eq!(t.ip_blocked(B, t0), None);
        assert!(t.blocked(Some(A), "other", t0).is_some());
        assert_eq!(t.ip_blocked(A, t0 + Duration::from_secs(121)), None);
        // the slate is clean after the lockout
        assert!(!t.record_failure(Some(A), "a", t0 + Duration::from_secs(121)));
    }

    #[test]
    fn an_account_is_blocked_from_any_address_with_its_own_higher_limit() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        for i in 0..5 {
            // a different address each time: only the account accumulates
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 1, 0, i));
            t.record_failure(Some(ip), "Alice", t0);
        }
        assert!(
            t.blocked(Some(B), "alice ", t0).is_some(),
            "case and spaces ignored"
        );
        assert!(t.blocked(Some(B), "bob", t0).is_none());
    }

    #[test]
    fn failures_older_than_the_window_are_forgotten() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        t.record_failure(Some(A), "a", t0);
        t.record_failure(Some(A), "a", t0);
        assert!(!t.record_failure(Some(A), "a", t0 + Duration::from_secs(61)));
        assert!(t.ip_blocked(A, t0 + Duration::from_secs(61)).is_none());
    }

    #[test]
    fn a_success_clears_the_counters() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        t.record_failure(Some(A), "a", t0);
        t.record_failure(Some(A), "a", t0);
        t.record_success(Some(A), "a");
        assert!(!t.record_failure(Some(A), "a", t0));
        assert!(!t.record_failure(Some(A), "a", t0));
    }

    #[test]
    fn tables_stay_bounded() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        for i in 0..(MAX_ENTRIES as u32 * 2) {
            t.record_failure(
                None,
                &format!("user{i}"),
                t0 + Duration::from_millis(i.into()),
            );
        }
        assert!(t.users.len() <= MAX_ENTRIES);
    }

    #[test]
    fn without_an_address_only_the_account_counts() {
        let mut t = LoginThrottle::new(cfg());
        let t0 = Instant::now();
        for _ in 0..5 {
            t.record_failure(None, "x", t0);
        }
        assert!(t.blocked(None, "x", t0).is_some());
    }
}
