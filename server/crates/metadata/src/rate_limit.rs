//! API request-rate limiter for `api.themoviedb.org` (ADR-0026 §7).
//!
//! TMDB ceilings (terms / CDN behaviour):
//! - API host: ~50 requests/second per IP (key ignored).
//! - Image host: ~20 simultaneous connections; no request-rate limit —
//!   that cap is artwork's problem (ADR-0027), not this type.
//!
//! A full-library search pass (~2,500 unique queries in ~12.5 minutes) ran
//! at roughly 3 req/s without 429s. Politeness budget is well inside the
//! 50/s ceiling: [`DEFAULT_REQUESTS_PER_SEC`] with [`DEFAULT_MAX_IN_FLIGHT`].

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Politeness rate for metadata API calls (not a setting).
pub const DEFAULT_REQUESTS_PER_SEC: u32 = 10;

/// Small concurrency cap for in-flight API calls (not a setting).
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// Acquire guard: holds one in-flight permit until drop.
pub struct ApiPermit<'a> {
    limiter: &'a ApiRateLimiter,
}

impl Drop for ApiPermit<'_> {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

#[derive(Debug)]
struct Inner {
    last_start: Option<Instant>,
    in_flight: usize,
}

/// Process-wide (or shared) limiter for metadata HTTP to the API host.
#[derive(Debug)]
pub struct ApiRateLimiter {
    min_interval: Duration,
    max_in_flight: usize,
    state: Mutex<Inner>,
    cv: Condvar,
}

impl ApiRateLimiter {
    pub fn new(requests_per_sec: u32, max_in_flight: usize) -> Arc<Self> {
        let rps = requests_per_sec.max(1);
        let max_in_flight = max_in_flight.max(1);
        Arc::new(Self {
            min_interval: Duration::from_secs_f64(1.0 / f64::from(rps)),
            max_in_flight,
            state: Mutex::new(Inner {
                last_start: None,
                in_flight: 0,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn polite_default() -> Arc<Self> {
        Self::new(DEFAULT_REQUESTS_PER_SEC, DEFAULT_MAX_IN_FLIGHT)
    }

    /// Block until a rate slot and an in-flight permit are available.
    pub fn acquire(&self) -> ApiPermit<'_> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            while guard.in_flight >= self.max_in_flight {
                guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            let wait = match guard.last_start {
                Some(prev) => {
                    let elapsed = prev.elapsed();
                    if elapsed < self.min_interval {
                        Some(self.min_interval - elapsed)
                    } else {
                        None
                    }
                }
                None => None,
            };
            if let Some(d) = wait {
                drop(guard);
                std::thread::sleep(d);
                guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            guard.last_start = Some(Instant::now());
            guard.in_flight += 1;
            return ApiPermit { limiter: self };
        }
    }

    fn release(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.in_flight = guard.in_flight.saturating_sub(1);
        self.cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rate_limits_burst_to_about_configured_rps() {
        let lim = ApiRateLimiter::new(20, 1); // 50ms apart
        let t0 = Instant::now();
        for _ in 0..5 {
            let _p = lim.acquire();
        }
        let elapsed = t0.elapsed();
        // 5 acquires at 20/s → ≥4 intervals ≈ 200ms
        assert!(elapsed >= Duration::from_millis(180), "elapsed {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(800),
            "elapsed {elapsed:?} too slow"
        );
    }

    #[test]
    fn concurrency_cap_blocks_extra_callers() {
        let lim = ApiRateLimiter::new(1000, 2);
        let alive = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..6 {
            let lim = Arc::clone(&lim);
            let alive = Arc::clone(&alive);
            let peak = Arc::clone(&peak);
            handles.push(std::thread::spawn(move || {
                let _p = lim.acquire();
                let n = alive.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(40));
                alive.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }
}
