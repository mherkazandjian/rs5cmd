//! Shared async bandwidth limiter (token bucket via virtual scheduling).
//!
//! A [`RateLimiter`] enforces an *aggregate* byte-rate cap across any number of
//! concurrent tasks (the worker pool). It is hand-rolled on top of `tokio`
//! primitives only (no external rate-limiting crate) so the project keeps
//! building offline.
//!
//! The implementation uses "virtual scheduling": a single `Instant` cursor
//! (`next_time`, behind a [`tokio::sync::Mutex`]) tracks the earliest moment the
//! next byte may flow. Each [`RateLimiter::acquire`] call advances the cursor by
//! `n / rate` seconds and then sleeps (outside the lock) until its assigned
//! start time. Because the cursor is shared, the cap is aggregate — not
//! per-worker — and because the sleep happens outside the lock, throttled tasks
//! do not block each other's scheduling.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

/// Aggregate byte-rate limiter, shared across the worker pool via [`Arc`].
#[derive(Debug)]
pub struct RateLimiter {
    /// Allowed bytes per second (must be > 0).
    rate: u64,
    /// Earliest instant the next chunk may start, shared across all tasks.
    next_time: Mutex<Instant>,
    /// Maximum burst (bytes). Bounds how far `next_time` may lag behind `now`,
    /// capping the instantaneous burst granted after an idle period.
    burst: u64,
}

impl RateLimiter {
    /// Creates a limiter capped at `rate` bytes/second, wrapped in an `Arc` so
    /// it can be cheaply cloned into every worker task. Assumes `rate > 0`
    /// (callers simply do not build a limiter for the "no limit" case).
    pub fn new(rate: u64) -> Arc<Self> {
        // Allow a burst of up to one second's worth of bytes (but at least a
        // 5 MiB part) so a single large chunk is never starved.
        let burst = rate.max(5 * 1024 * 1024);
        Arc::new(RateLimiter {
            rate,
            next_time: Mutex::new(Instant::now()),
            burst,
        })
    }

    /// The configured rate in bytes/second.
    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Reserves capacity for `n` bytes and sleeps until they may be transferred.
    ///
    /// Returns immediately for `n == 0`. Summed across all concurrent callers,
    /// the time spent sleeping keeps aggregate throughput at or below `rate`.
    pub async fn acquire(&self, n: u64) {
        if n == 0 || self.rate == 0 {
            return;
        }

        // Time this many bytes "costs" at the configured rate, in nanoseconds
        // (integer math to avoid float drift): cost = n / rate seconds.
        let cost = Duration::from_nanos(div_rate_to_nanos(n, self.rate));

        let start = {
            let mut next = self.next_time.lock().await;
            let now = Instant::now();

            // Earliest allowed start for this reservation.
            let mut start = if *next > now { *next } else { now };

            // Bound how far we may have fallen behind, so an idle period grants
            // only a limited burst rather than an unbounded catch-up.
            let max_lag = Duration::from_nanos(div_rate_to_nanos(self.burst, self.rate));
            if now > start + max_lag {
                start = now - max_lag;
            }

            *next = start + cost;
            start
        };

        let now = Instant::now();
        if start > now {
            sleep(start - now).await;
        }
    }
}

/// Computes `bytes / rate` seconds expressed in nanoseconds, using 128-bit
/// intermediate math and saturating to `u64::MAX` to avoid overflow/panic.
fn div_rate_to_nanos(bytes: u64, rate: u64) -> u64 {
    let nanos = bytes as u128 * 1_000_000_000u128 / rate as u128;
    nanos.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant as StdInstant;

    /// acquire(N) at rate R should make aggregate throughput take ~N/R seconds.
    #[tokio::test]
    async fn acquire_sleeps_proportional_to_size_over_rate() {
        // 1000 bytes/sec; transfer 500 bytes in 5 chunks of 100 => ~0.5s total.
        let rl = RateLimiter::new(1000);
        let start = StdInstant::now();
        for _ in 0..5 {
            rl.acquire(100).await;
        }
        let elapsed = start.elapsed();
        // Expected ~0.5s; allow generous slack for CI scheduling noise.
        assert!(
            elapsed.as_millis() >= 350,
            "expected >= ~0.5s of throttling, got {:?}",
            elapsed
        );
        assert!(
            elapsed.as_millis() < 2000,
            "throttling took unexpectedly long: {:?}",
            elapsed
        );
    }

    /// The cap must be aggregate across concurrent tasks, not per-task.
    #[tokio::test]
    async fn aggregate_cap_across_concurrent_tasks() {
        // 2000 bytes/sec; 4 tasks each pushing 500 bytes => 2000 bytes total
        // => ~1.0s aggregate even though the tasks run concurrently.
        let rl = RateLimiter::new(2000);
        let start = StdInstant::now();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let rl = rl.clone();
            handles.push(tokio::spawn(async move {
                rl.acquire(500).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 700,
            "aggregate cap not enforced across tasks, got {:?}",
            elapsed
        );
    }

    /// Zero-byte acquisitions must be no-ops (never sleep, never hang).
    #[tokio::test]
    async fn zero_is_noop() {
        let rl = RateLimiter::new(1000);
        let start = StdInstant::now();
        rl.acquire(0).await;
        assert!(start.elapsed().as_millis() < 50);
    }
}
