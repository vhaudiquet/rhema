//! Injectable time. No threads, tasks, or timers exist in this crate:
//! every deadline is evaluated lazily against a [`Clock`] at call time,
//! which keeps the dispatcher embeddable and timing deterministic in tests.

use std::sync::atomic::{AtomicU64, Ordering};

/// The time seam. Milliseconds since the Unix epoch; monotonicity across
/// calls is the embedder's responsibility (the real clock qualifies).
pub trait Clock: Send + Sync {
    /// The current time in milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// The production clock: the system wall clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// A clock the test drives by hand: starts at a fixed instant and moves only
/// when the test says so.
#[derive(Debug, Default)]
pub struct FixedClock(AtomicU64);

impl FixedClock {
    /// A clock parked at `now_millis`.
    pub fn new(now_millis: u64) -> Self {
        Self(AtomicU64::new(now_millis))
    }

    /// Jumps the clock to `now_millis` (forwards or backwards).
    pub fn set(&self, now_millis: u64) {
        self.0.store(now_millis, Ordering::Relaxed);
    }

    /// Moves the clock forward by `millis`.
    pub fn advance(&self, millis: u64) {
        self.0.fetch_add(millis, Ordering::Relaxed);
    }

    /// Reads the clock without advancing it.
    pub fn now_millis(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        FixedClock::now_millis(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_moves_only_when_told() {
        let clock = FixedClock::new(1_000);
        assert_eq!(clock.now_millis(), 1_000);
        clock.advance(500);
        assert_eq!(clock.now_millis(), 1_500);
        clock.set(100);
        assert_eq!(clock.now_millis(), 100);
    }
}
