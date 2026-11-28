//! In-memory keyed rate limiter (GCRA, the "virtual scheduling" form of a
//! token bucket).
//!
//! Each key stores one instant, its theoretical arrival time (TAT). A rule of
//! `limit` events per `period` allows a full burst of `limit` events, then
//! one more every `period / limit`. State is per process and is not persisted.
//! A restart only ever relaxes limits.
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

/// In-memory keyed rate limiter with a bounded number of tracked keys.
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
    /// Creates a limiter that tracks at most `capacity` keys (minimum 2).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(2),
            state: Mutex::default(),
        }
    }

    /// Consumes one event for `key`, or returns the seconds to wait. A
    /// `limit` of 0 means unlimited.
    pub fn acquire(&self, key: &str, limit: u32, period: Duration) -> Result<(), u64> {
        self.try_acquire(key, limit, period, 1).map_err(seconds)
    }

    /// Consumes `n` events at once (a weight such as a token count), or
    /// returns how long to wait until they fit. `n` is capped at `limit`.
    /// A `limit` of 0 means unlimited.
    pub fn try_acquire(
        &self,
        key: &str,
        limit: u32,
        period: Duration,
        n: u32,
    ) -> Result<(), Duration> {
        self.acquire_at(key, limit, period, n, Instant::now())
    }

    /// Returns the seconds until `key` may act again, or `None` when it is not
    /// blocked. This does not consume an event. A `limit` of 0 never blocks.
    pub fn blocked(&self, key: &str, limit: u32, period: Duration) -> Option<u64> {
        self.blocked_for(key, limit, period, 1).map(seconds)
    }

    /// Like [`Limiter::blocked`] for `n` events at once, with the exact wait.
    pub fn blocked_for(&self, key: &str, limit: u32, period: Duration, n: u32) -> Option<Duration> {
        self.blocked_at(key, limit, period, n, Instant::now())
    }

    /// Consumes one event unconditionally, for example after a failed login.
    ///
    /// The debt is capped at one full period, so a lockout never outlives it.
    pub fn record(&self, key: &str, limit: u32, period: Duration) {
        self.record_at(key, limit, period, 1, Instant::now());
    }

    /// Consumes `n` events unconditionally (for example tokens charged after a
    /// request completed). The debt is capped at one full period.
    pub fn record_n(&self, key: &str, limit: u32, period: Duration, n: u64) {
        self.record_at(
            key,
            limit,
            period,
            u32::try_from(n).unwrap_or(u32::MAX),
            Instant::now(),
        );
    }

    /// The state of `key` without consuming anything.
    pub fn remaining(&self, key: &str, limit: u32, period: Duration) -> Remaining {
        self.remaining_at(key, limit, period, Instant::now())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        // The map holds plain instants, so a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn acquire_at(
        &self,
        key: &str,
        limit: u32,
        period: Duration,
        n: u32,
        now: Instant,
    ) -> Result<(), Duration> {
        if limit == 0 || n == 0 {
            return Ok(());
        }
        let cost = (period / limit) * n.min(limit);
        let mut state = self.lock();
        let tat = state.get(key).copied().unwrap_or(now).max(now);
        let wait = (tat + cost).saturating_duration_since(now);
        if wait > period {
            return Err(wait - period);
        }
        self.store(&mut state, key, tat + cost, now);
        Ok(())
    }

    fn blocked_at(
        &self,
        key: &str,
        limit: u32,
        period: Duration,
        n: u32,
        now: Instant,
    ) -> Option<Duration> {
        if limit == 0 || n == 0 {
            return None;
        }
        let cost = (period / limit) * n.min(limit);
        let tat = self.lock().get(key).copied()?.max(now);
        let wait = (tat + cost).saturating_duration_since(now);
        (wait > period).then(|| wait - period)
    }

    fn record_at(&self, key: &str, limit: u32, period: Duration, n: u32, now: Instant) {
        if limit == 0 || n == 0 {
            return;
        }
        let cost = (period / limit) * n.min(limit);
        let mut state = self.lock();
        let tat = state.get(key).copied().unwrap_or(now).max(now);
        let next = (tat + cost).min(now + period);
        self.store(&mut state, key, next, now);
    }

    fn remaining_at(&self, key: &str, limit: u32, period: Duration, now: Instant) -> Remaining {
        if limit == 0 {
            return Remaining {
                limit,
                remaining: 0,
                reset: Duration::ZERO,
            };
        }
        let interval = period / limit;
        let used = self
            .lock()
            .get(key)
            .map_or(Duration::ZERO, |tat| tat.saturating_duration_since(now))
            .min(period);
        let used_units = used.as_nanos().div_ceil(interval.as_nanos().max(1));
        let remaining = u32::try_from(u128::from(limit).saturating_sub(used_units)).unwrap_or(0);
        let reset = if remaining == 0 {
            // Time until a single event fits again.
            (used + interval).saturating_sub(period)
        } else {
            used
        };
        Remaining {
            limit,
            remaining,
            reset,
        }
    }

    /// Writes `tat` for `key`. A full table drops idle entries first, then the
    /// earliest-TAT half if still full.
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

/// The state of one key, see [`Limiter::remaining`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remaining {
    /// The configured limit (0 when unlimited).
    pub limit: u32,
    /// Events that can be consumed right now.
    pub remaining: u32,
    /// Time until one more event fits when none remains, otherwise until the
    /// key is fully replenished.
    pub reset: Duration,
}

/// Whole seconds, rounded up, at least one.
pub fn seconds(d: Duration) -> u64 {
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
            l.acquire_at("k", 3, MINUTE, 1, t0).unwrap();
        }
        let wait = l.acquire_at("k", 3, MINUTE, 1, t0).unwrap_err();
        assert_eq!(wait, Duration::from_secs(20));
        assert!(
            l.acquire_at("k", 3, MINUTE, 1, t0 + Duration::from_secs(19))
                .is_err()
        );
        l.acquire_at("k", 3, MINUTE, 1, t0 + Duration::from_secs(21))
            .unwrap();
        // Keys are independent.
        l.acquire_at("other", 3, MINUTE, 1, t0).unwrap();
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
            assert!(l.blocked_at("ip", 2, MINUTE, 1, t0).is_none());
        }
        l.record_at("ip", 2, MINUTE, 1, t0);
        assert!(l.blocked_at("ip", 2, MINUTE, 1, t0).is_none());
        l.record_at("ip", 2, MINUTE, 1, t0);
        assert_eq!(
            l.blocked_at("ip", 2, MINUTE, 1, t0),
            Some(Duration::from_secs(30))
        );
        // Further failures cannot extend the lockout beyond one period.
        for _ in 0..50 {
            l.record_at("ip", 2, MINUTE, 1, t0);
        }
        assert!(l.blocked_at("ip", 2, MINUTE, 1, t0).unwrap() <= MINUTE);
        assert!(
            l.blocked_at("ip", 2, MINUTE, 1, t0 + Duration::from_secs(61))
                .is_none()
        );
    }

    #[test]
    fn table_size_is_bounded() {
        let l = Limiter::with_capacity(100);
        let t0 = Instant::now();
        for i in 0..10_000 {
            l.record_at(
                &format!("ip-{i}"),
                5,
                MINUTE,
                1,
                t0 + Duration::from_millis(i),
            );
            assert!(l.len() <= 100);
        }
        // Idle entries are reclaimed before live ones are evicted.
        let l = Limiter::with_capacity(10);
        for i in 0..10 {
            l.record_at(&format!("old-{i}"), 5, MINUTE, 1, t0);
        }
        l.record_at("new", 5, MINUTE, 1, t0 + Duration::from_secs(120));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn weighted_events_and_the_remaining_view() {
        let l = Limiter::default();
        let t0 = Instant::now();
        let day = Duration::from_secs(86_400);
        // 864 units a day, one every 100 s: a weighted record spends many at once.
        assert_eq!(l.remaining_at("t", 864, day, t0).remaining, 864);
        l.record_at("t", 864, day, 400, t0);
        let r = l.remaining_at("t", 864, day, t0);
        assert_eq!((r.limit, r.remaining), (864, 464));
        assert_eq!(r.reset, Duration::from_secs(400 * 100));
        // A weight larger than what is left is refused with the exact wait.
        assert!(l.blocked_at("t", 864, day, 464, t0).is_none());
        assert_eq!(
            l.blocked_at("t", 864, day, 465, t0),
            Some(Duration::from_secs(100))
        );
        assert_eq!(
            l.acquire_at("t", 864, day, 500, t0),
            Err(Duration::from_secs(3600))
        );
        l.acquire_at("t", 864, day, 464, t0).unwrap();
        let r = l.remaining_at("t", 864, day, t0);
        assert_eq!(r.remaining, 0);
        assert_eq!(r.reset, Duration::from_secs(100));
        // The peek never consumes.
        for _ in 0..10 {
            assert_eq!(l.remaining_at("t", 864, day, t0).remaining, 0);
        }
        // Debt is capped at one period, so the key recovers.
        l.record_at("t", 864, day, u32::MAX, t0);
        assert!(l.blocked_at("t", 864, day, 1, t0).unwrap() <= day);
        assert_eq!(
            l.remaining_at("t", 864, day, t0 + day + Duration::from_secs(1))
                .remaining,
            864
        );
        // Unlimited keys report nothing and never block.
        assert_eq!(l.remaining_at("u", 0, day, t0).limit, 0);
        assert!(l.blocked_at("u", 0, day, 5, t0).is_none());
    }
}
