//! A small fixed-window limiter for the share routes.
//!
//! Share links are the only endpoints a stranger can reach, so they get their
//! own budget instead of borrowing the login limiter. A token is 122 random
//! bits, which already makes guessing pointless; the limit is here so a leaked
//! link cannot be pulled at a rate that starves the library for its owners.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry {
    started: Instant,
    hits: u32,
}

pub struct MinuteLimiter {
    limit: u32,
    window: Duration,
    entries: Mutex<HashMap<String, Entry>>,
    next_sweep: Mutex<Instant>,
}

impl MinuteLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            entries: Mutex::new(HashMap::new()),
            next_sweep: Mutex::new(Instant::now() + window),
        }
    }

    /// Count one request against `key`; `false` means the caller should answer
    /// 429 and look at [`Self::retry_after_secs`].
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            // A poisoned limiter must not become an open door.
            Err(_) => return false,
        };
        // Bound the map: a share link's key is its token, and stale tokens are
        // dropped as soon as their window closes, so this only grows with the
        // number of links used inside one window.
        const MAX_KEYS: usize = 1024;
        if entries.len() >= MAX_KEYS && !entries.contains_key(key) {
            // Only scan when the table is full. Reject rather than evict active
            // budgets (eviction would let a caller reset another key's limit).
            let mut next_sweep = match self.next_sweep.lock() {
                Ok(next_sweep) => next_sweep,
                Err(_) => return false,
            };
            if now >= *next_sweep {
                entries.retain(|_, entry| now.duration_since(entry.started) < self.window);
                *next_sweep = now + self.window;
            }
            if entries.len() >= MAX_KEYS {
                return false;
            }
        }
        let entry = entries.entry(key.to_string()).or_insert(Entry {
            started: now,
            hits: 0,
        });
        if now.duration_since(entry.started) >= self.window {
            entry.started = now;
            entry.hits = 0;
        }
        entry.hits = entry.hits.saturating_add(1);
        entry.hits <= self.limit
    }

    /// Seconds until the current window closes, at least one.
    pub fn retry_after_secs(&self) -> u64 {
        self.window.as_secs().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let limiter = MinuteLimiter::new(3, Duration::from_millis(60));
        assert!(limiter.check("a"));
        assert!(limiter.check("a"));
        assert!(limiter.check("a"));
        assert!(!limiter.check("a"));
        // Another key has its own budget.
        assert!(limiter.check("b"));
    }

    #[test]
    fn the_window_resets() {
        let limiter = MinuteLimiter::new(1, Duration::from_millis(30));
        assert!(limiter.check("t"));
        assert!(!limiter.check("t"));
        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.check("t"));
    }

    #[test]
    fn distinct_keys_cannot_exceed_capacity() {
        let limiter = MinuteLimiter::new(3, Duration::from_secs(60));
        for index in 0..1024 {
            assert!(limiter.check(&index.to_string()));
        }
        assert!(!limiter.check("overflow"));
        assert_eq!(limiter.entries.lock().unwrap().len(), 1024);
        assert!(limiter.check("0"));
    }

    #[test]
    fn reports_a_retry_delay() {
        let limiter = MinuteLimiter::new(1, Duration::from_secs(60));
        assert_eq!(limiter.retry_after_secs(), 60);
    }
}
