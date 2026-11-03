//! Time source. The real driver uses [`SystemClock`]; tests and the simulator use a
//! [`VirtualClock`] so 30 s timeouts and lamp warm-ups run instantly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Monotonic time plus sleeping.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
    /// Block for `d`.
    fn sleep(&self, d: Duration);
    /// Account for a blocking transport call that just spent `d` waiting (a timed-out
    /// slice). The system clock ignores this (the time really passed); a virtual clock
    /// advances by `d`.
    fn charge(&self, d: Duration);
}

/// Wall-clock time.
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
    fn charge(&self, _d: Duration) {}
}

/// A shared, manually advanced clock (cheap to clone; clones share time).
#[derive(Debug, Clone, Default)]
pub struct VirtualClock {
    nanos: Arc<AtomicU64>,
}

impl VirtualClock {
    /// A clock at time zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward.
    pub fn advance(&self, d: Duration) {
        let n = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let _ = self.nanos.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_add(n)));
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
    fn charge(&self, d: Duration) {
        self.advance(d);
    }
}
