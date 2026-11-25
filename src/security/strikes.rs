//! In-memory failure counters with a reset window ("strikes").
//!
//! Each key counts consecutive failures. The count resets to zero when the
//! caller clears it (a success) or when no failure was recorded for the
//! `ttl` passed to [`Strikes::count`] and [`Strikes::strike`]. State is per
//! process and is not persisted: a restart only forgives.
//!
//! Memory is bounded like [`super::limiter::Limiter`]: when the table is full,
//! stale entries are dropped first, then the half with the oldest failure.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Maximum tracked keys before eviction.
const DEFAULT_CAPACITY: usize = 100_000;

/// Consecutive failure counts per key.
pub struct Strikes {
    capacity: usize,
    state: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Default for Strikes {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl Strikes {
    /// Creates a table that tracks at most `capacity` keys (minimum 2).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(2),
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u32, Instant)>> {
        // The map holds plain counters, so a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Failures of `key` whose latest one is less than `ttl` old.
    pub fn count(&self, key: &str, ttl: Duration) -> u32 {
        self.count_at(key, ttl, Instant::now())
    }

    /// Records one failure of `key`. A count older than `ttl` starts over.
    pub fn strike(&self, key: &str, ttl: Duration) {
        self.strike_at(key, ttl, Instant::now());
    }

    /// Forgets `key`, for example after a success.
    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    fn count_at(&self, key: &str, ttl: Duration, now: Instant) -> u32 {
        match self.lock().get(key) {
            Some((count, last)) if now.saturating_duration_since(*last) < ttl => *count,
            _ => 0,
        }
    }

    fn strike_at(&self, key: &str, ttl: Duration, now: Instant) {
        let mut state = self.lock();
        let count = match state.get(key) {
            Some((count, last)) if now.saturating_duration_since(*last) < ttl => *count,
            _ => 0,
        };
        if !state.contains_key(key) && state.len() >= self.capacity {
            state.retain(|_, (_, last)| now.saturating_duration_since(*last) < ttl);
            if state.len() >= self.capacity {
                let mut times: Vec<Instant> = state.values().map(|(_, t)| *t).collect();
                times.sort_unstable();
                let cutoff = times[times.len() / 2];
                state.retain(|_, (_, last)| *last > cutoff);
            }
        }
        state.insert(key.to_owned(), (count.saturating_add(1), now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(1800);

    #[test]
    fn strikes_accumulate_reset_on_clear_and_expire_after_ttl() {
        let s = Strikes::default();
        let t0 = Instant::now();
        s.strike_at("ip", TTL, t0);
        s.strike_at("ip", TTL, t0 + Duration::from_secs(60));
        assert_eq!(s.count_at("ip", TTL, t0 + Duration::from_secs(61)), 2);
        // The window runs from the latest failure.
        assert_eq!(s.count_at("ip", TTL, t0 + Duration::from_secs(1859)), 2);
        assert_eq!(s.count_at("ip", TTL, t0 + Duration::from_secs(1861)), 0);
        // A failure after the window starts a new count.
        s.strike_at("ip", TTL, t0 + Duration::from_secs(4000));
        assert_eq!(s.count_at("ip", TTL, t0 + Duration::from_secs(4001)), 1);
        s.clear("ip");
        assert_eq!(s.count_at("ip", TTL, t0 + Duration::from_secs(4001)), 0);
        assert_eq!(s.count_at("other", TTL, t0), 0);
    }

    #[test]
    fn memory_stays_bounded() {
        let s = Strikes::with_capacity(4);
        let t0 = Instant::now();
        for i in 0..20u64 {
            s.strike_at(&format!("k{i}"), TTL, t0 + Duration::from_secs(i));
        }
        assert!(s.lock().len() <= 4);
        assert_eq!(s.count_at("k19", TTL, t0 + Duration::from_secs(20)), 1);
    }
}
