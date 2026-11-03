//! USB transport errors.

/// A USB transport failure. `Clone` so scripted fakes can replay errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsbError {
    /// The transfer did not complete within its timeout (nothing was received).
    #[error("USB transfer timed out")]
    Timeout,
    /// The endpoint is halted (STALL); clear the halt before retrying.
    #[error("USB endpoint stalled")]
    Stall,
    /// The device sent more than requested (babble / EOVERFLOW).
    #[error("USB transfer overflow")]
    Overflow,
    /// The device went away.
    #[error("USB device disconnected")]
    Disconnected,
    /// Another driver, process or handle owns the device or interface.
    #[error("USB device or interface busy: {0}")]
    Busy(String),
    /// The OS refused access (udev rules, sandbox).
    #[error("USB permission denied: {0}")]
    PermissionDenied(String),
    /// No such device, interface or endpoint.
    #[error("USB device or interface not found: {0}")]
    NotFound(String),
    /// The operation is not available on this transport or platform.
    #[error("USB operation unsupported: {0}")]
    Unsupported(String),
    /// Any other OS-level failure.
    #[error("USB I/O error: {0}")]
    Io(String),
    /// A scripted fake saw traffic it did not expect.
    #[error("USB script mismatch: {0}")]
    Script(String),
}

impl UsbError {
    /// True for errors where the device is gone and retrying is pointless.
    pub fn is_fatal(&self) -> bool {
        matches!(self, UsbError::Disconnected | UsbError::NotFound(_))
    }
}
