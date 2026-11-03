//! Time source, so polling loops and button timings are testable without sleeping.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Monotonic time and sleeping.
pub trait Clock {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
    /// Block for `d`.
    fn sleep(&self, d: Duration);
}

/// The real clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A virtual clock: `sleep` advances time instantly. Clones share the time.
#[derive(Debug, Clone, Default)]
pub struct FakeClock {
    nanos: Arc<AtomicU64>,
    slept: Arc<AtomicU64>,
}

impl FakeClock {
    /// A clock at t = 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance time without counting it as a sleep.
    pub fn advance(&self, d: Duration) {
        let n = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(n, Ordering::SeqCst);
    }

    /// Total time spent in `sleep`.
    pub fn total_slept(&self) -> Duration {
        Duration::from_nanos(self.slept.load(Ordering::SeqCst))
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
    fn sleep(&self, d: Duration) {
        let n = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(n, Ordering::SeqCst);
        self.slept.fetch_add(n, Ordering::SeqCst);
    }
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now(&self) -> Duration {
        (**self).now()
    }
    fn sleep(&self, d: Duration) {
        (**self).sleep(d)
    }
}
