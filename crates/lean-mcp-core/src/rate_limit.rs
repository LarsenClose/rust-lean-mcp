//! Per-category sliding-window rate limiter.
//!
//! Ports the Python `rate_limited` decorator pattern from `server.py`.
//! Each category (e.g. `leansearch`, `loogle`) has its own independent
//! sliding window that tracks request timestamps and rejects requests
//! exceeding the configured limit within the window.
//!
//! # Default limits (from the Python server)
//!
//! | Category           | Max requests | Window (seconds) |
//! |--------------------|-------------|------------------|
//! | `leansearch`       | 3           | 30               |
//! | `loogle`           | 3           | 30               |
//! | `leanfinder`       | 10          | 30               |
//! | `lean_state_search`| 6           | 30               |
//! | `hammer_premise`   | 6           | 30               |

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A sliding-window rate limiter that tracks request timestamps per category.
#[derive(Debug)]
pub struct RateLimiter {
    buckets: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new, empty rate limiter.
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    /// Check if a request is allowed and record it.
    ///
    /// Returns `Ok(())` if the request is within the rate limit, or
    /// `Err(message)` if the limit has been exceeded.
    ///
    /// The sliding window keeps only timestamps within the last `per_seconds`
    /// seconds. If fewer than `max_requests` timestamps remain after pruning,
    /// the new request is recorded and allowed.
    pub fn check_and_record(
        &mut self,
        category: &str,
        max_requests: u32,
        per_seconds: u64,
    ) -> Result<(), String> {
        let window = Duration::from_secs(per_seconds);
        let now = Instant::now();

        let timestamps = self.buckets.entry(category.to_owned()).or_default();

        // Remove expired timestamps outside the sliding window.
        timestamps.retain(|&ts| now.duration_since(ts) < window);

        if timestamps.len() >= max_requests as usize {
            return Err(format!(
                "Tool limit exceeded: {max_requests} requests per {per_seconds} s. \
                 Try again later."
            ));
        }

        timestamps.push(now);
        Ok(())
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    // -- Basic functionality ------------------------------------------------

    #[test]
    fn allows_requests_under_limit() {
        let mut rl = RateLimiter::new();
        assert!(rl.check_and_record("leansearch", 3, 30).is_ok());
        assert!(rl.check_and_record("leansearch", 3, 30).is_ok());
    }

    #[test]
    fn allows_requests_up_to_limit() {
        let mut rl = RateLimiter::new();
        for _ in 0..3 {
            assert!(rl.check_and_record("loogle", 3, 30).is_ok());
        }
    }

    #[test]
    fn rejects_at_limit() {
        let mut rl = RateLimiter::new();
        for _ in 0..3 {
            rl.check_and_record("leansearch", 3, 30).unwrap();
        }
        let err = rl.check_and_record("leansearch", 3, 30).unwrap_err();
        assert_eq!(
            err,
            "Tool limit exceeded: 3 requests per 30 s. Try again later."
        );
    }

    #[test]
    fn error_message_reflects_parameters() {
        let mut rl = RateLimiter::new();
        for _ in 0..10 {
            rl.check_and_record("leanfinder", 10, 30).unwrap();
        }
        let err = rl.check_and_record("leanfinder", 10, 30).unwrap_err();
        assert!(err.contains("10 requests per 30 s"));
    }

    // -- Window expiry ------------------------------------------------------

    #[test]
    fn window_expiry_resets_counter() {
        let mut rl = RateLimiter::new();

        // Manually insert timestamps that are already expired (> 1 s ago).
        let old = Instant::now() - Duration::from_secs(2);
        rl.buckets
            .insert("leansearch".to_owned(), vec![old, old, old]);

        // With a 1-second window the old timestamps should be pruned,
        // so a new request must be allowed.
        assert!(rl.check_and_record("leansearch", 3, 1).is_ok());
    }

    #[test]
    fn partially_expired_window_frees_capacity() {
        let mut rl = RateLimiter::new();

        // Two old timestamps (expired) and one recent one.
        let old = Instant::now() - Duration::from_secs(5);
        let recent = Instant::now();
        rl.buckets
            .insert("loogle".to_owned(), vec![old, old, recent]);

        // With a 2-second window the two old ones are pruned, leaving 1 of 3.
        assert!(rl.check_and_record("loogle", 3, 2).is_ok());
        assert!(rl.check_and_record("loogle", 3, 2).is_ok());
        // Now at 3 -- next should fail.
        let result = rl.check_and_record("loogle", 3, 2);
        assert!(result.is_err());
    }

    // -- Category independence ----------------------------------------------

    #[test]
    fn independent_categories_do_not_interfere() {
        let mut rl = RateLimiter::new();

        // Exhaust leansearch.
        for _ in 0..3 {
            rl.check_and_record("leansearch", 3, 30).unwrap();
        }
        assert!(rl.check_and_record("leansearch", 3, 30).is_err());

        // loogle should still be available.
        assert!(rl.check_and_record("loogle", 3, 30).is_ok());

        // leanfinder should still be available.
        assert!(rl.check_and_record("leanfinder", 10, 30).is_ok());
    }

    // -- Thread safety via Arc<Mutex<RateLimiter>> --------------------------

    #[test]
    fn thread_safety_with_arc_mutex() {
        let rl = Arc::new(Mutex::new(RateLimiter::new()));
        let mut handles = vec![];

        // Spawn 6 threads, each making one request to "hammer_premise" (limit 6).
        for _ in 0..6 {
            let rl = Arc::clone(&rl);
            handles.push(thread::spawn(move || {
                rl.lock().unwrap().check_and_record("hammer_premise", 6, 30)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All 6 should succeed.
        assert!(results.iter().all(|r| r.is_ok()));

        // The 7th request should be rejected.
        let result = rl.lock().unwrap().check_and_record("hammer_premise", 6, 30);
        assert!(result.is_err());
    }

    // -- Default trait ------------------------------------------------------

    #[test]
    fn default_creates_empty_limiter() {
        let rl = RateLimiter::default();
        assert!(rl.buckets.is_empty());
    }
}
