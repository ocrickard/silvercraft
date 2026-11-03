//! Cooperative cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cooperative stop flag (`Arc<AtomicBool>`), cheap to clone and safe to trigger from
/// any thread or a signal handler. The driver checks it between raster chunks and in
/// every ≤200 ms transport slice; a triggered scan ends with the hardware-clean abort
/// (drain, CAN, postflight status), never a hard kill.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A fresh, untriggered token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    /// Clear the request (for reusing a token for the next scan).
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}
