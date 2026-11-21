//! In-memory keyed rate limiter (GCRA, the "virtual scheduling" form of a
//! token bucket).
//!
//! Each key stores one instant, its theoretical arrival time (TAT). A rule of
//! `limit` events per `period` allows a full burst of `limit` events, then
//! one more every `period / limit`. State is per process and not persisted;
//! a restart only ever relaxes limits.
//!
//! Memory is bounded: when the table is full, idle entries (those whose TAT
//! has passed, which are equivalent to absent ones) are dropped first and, if
//! that is not enough, the half with the earliest TAT is evicted.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Maximum tracked keys before eviction.
const DEFAULT_CAPACITY: usize = 100_000;

pub struct Limiter {
    capacity: usize,
    state: Mutex<HashMap<String, Instant>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl Limiter {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(2),
            state: Mutex::default(),
        }
    }

    /// Consumes one event for `key`, or returns the seconds to wait. A
    /// `limit` of 0 means unlimited.
    pub fn acquire(&self, key: &str, limit: u32, period: Duration) -> Result<(), u64> {
        self.acquire_at(key, limit, period, Instant::now())
    }

    /// Seconds until `key` may act again, without consuming an event.
    pub fn blocked(&self, key: &str, limit: u32, period: Duration) -> Option<u64> {
        self.blocked_at(key, limit, period, Instant::now())
    }

    /// Consumes one event unconditionally (for example a failed login); the
    /// debt is capped at one full period so a lockout never outlives it.
    pub fn record(&self, key: &str, limit: u32, period: Duration) {
        self.record_at(key, limit, period, Instant::now());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        // The map holds plain instants, so a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn acquire_at(&self, key: &str, limit: u32, period: Duration, now: Instant) -> Result<(), u64> {
        if limit == 0 {
            return Ok(());
        }
        let interval = period / limit;
        let mut state = self.lock();
        let tat = state.get(key).copied().unwrap_or(now).max(now);
        let wait = (tat + interval).saturating_duration_since(now);
        if wait > period {
            return Err(seconds(wait - period));
        }
        self.store(&mut state, key, tat + interval, now);
        Ok(())
    }

    fn blocked_at(&self, key: &str, limit: u32, period: Duration, now: Instant) -> Option<u64> {
        if limit == 0 {
            return None;
        }
        let interval = period / limit;
        let tat = self.lock().get(key).copied()?.max(now);
        let wait = (tat + interval).saturating_duration_since(now);
        (wait > period).then(|| seconds(wait - period))
    }

    fn record_at(&self, key: &str, limit: u32, period: Duration, now: Instant) {
        if limit == 0 {
            return;
        }
        let interval = period / limit;
        let mut state = self.lock();
        let tat = state.get(key).copied().unwrap_or(now).max(now);
        let next = (tat + interval).min(now + period);
        self.store(&mut state, key, next, now);
    }

    fn store(&self, state: &mut HashMap<String, Instant>, key: &str, tat: Instant, now: Instant) {
        if state.len() >= self.capacity && !state.contains_key(key) {
            state.retain(|_, tat| *tat > now);
            if state.len() >= self.capacity {
                let mut times: Vec<Instant> = state.values().copied().collect();
                let middle = times.len() / 2;
                let (_, median, _) = times.select_nth_unstable(middle);
                let median = *median;
                state.retain(|_, tat| *tat > median);
            }
        }
        state.insert(key.to_owned(), tat);
    }

    /// Number of tracked keys.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whole seconds, rounded up, at least one.
fn seconds(d: Duration) -> u64 {
    (d.as_secs() + u64::from(d.subsec_nanos() > 0)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn allows_a_burst_then_refills_gradually() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            l.acquire_at("k", 3, MINUTE, t0).unwrap();
        }
        let wait = l.acquire_at("k", 3, MINUTE, t0).unwrap_err();
        assert_eq!(wait, 20);
        assert!(
            l.acquire_at("k", 3, MINUTE, t0 + Duration::from_secs(19))
                .is_err()
        );
        l.acquire_at("k", 3, MINUTE, t0 + Duration::from_secs(21))
            .unwrap();
        // Keys are independent.
        l.acquire_at("other", 3, MINUTE, t0).unwrap();
    }

    #[test]
    fn zero_limit_is_unlimited() {
        let l = Limiter::default();
        for _ in 0..100 {
            l.acquire("k", 0, MINUTE).unwrap();
        }
        assert!(l.is_empty());
    }

    #[test]
    fn blocked_peeks_without_consuming_and_record_locks_out() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..100 {
            assert!(l.blocked_at("ip", 2, MINUTE, t0).is_none());
        }
        l.record_at("ip", 2, MINUTE, t0);
        assert!(l.blocked_at("ip", 2, MINUTE, t0).is_none());
        l.record_at("ip", 2, MINUTE, t0);
        assert_eq!(l.blocked_at("ip", 2, MINUTE, t0), Some(30));
        // Further failures cannot extend the lockout beyond one period.
        for _ in 0..50 {
            l.record_at("ip", 2, MINUTE, t0);
        }
        assert!(l.blocked_at("ip", 2, MINUTE, t0).unwrap() <= 60);
        assert!(
            l.blocked_at("ip", 2, MINUTE, t0 + Duration::from_secs(61))
                .is_none()
        );
    }

    #[test]
    fn table_size_is_bounded() {
        let l = Limiter::with_capacity(100);
        let t0 = Instant::now();
        for i in 0..10_000 {
            l.record_at(&format!("ip-{i}"), 5, MINUTE, t0 + Duration::from_millis(i));
            assert!(l.len() <= 100);
        }
        // Idle entries are reclaimed before live ones are evicted.
        let l = Limiter::with_capacity(10);
        for i in 0..10 {
            l.record_at(&format!("old-{i}"), 5, MINUTE, t0);
        }
        l.record_at("new", 5, MINUTE, t0 + Duration::from_secs(120));
        assert_eq!(l.len(), 1);
    }
}
