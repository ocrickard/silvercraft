//! PTP errors.

use silvercraft_usb::UsbError;

/// Anything that can go wrong talking PTP.
#[derive(Debug, thiserror::Error)]
pub enum PtpError {
    /// The USB transport failed.
    #[error(transparent)]
    Usb(#[from] UsbError),
    /// The bytes on the wire do not frame as PTP containers.
    #[error("invalid PTP container: {0}")]
    InvalidContainer(String),
    /// Containers framed fine but arrived in an order PTP does not allow.
    #[error("PTP protocol error: {0}")]
    Protocol(String),
    /// A dataset (DeviceInfo, ObjectInfo, property blob, ...) is malformed.
    #[error("malformed PTP dataset: {0}")]
    Dataset(String),
    /// The device answered an operation with a non-OK response code.
    #[error("PTP operation 0x{op:04x} failed with response 0x{code:04x}")]
    Response {
        /// Operation code.
        op: u16,
        /// Response code.
        code: u16,
    },
    /// Writing the data phase to the caller's sink failed.
    #[error("data sink failed: {0}")]
    Sink(String),
    /// A download started streaming into the caller's sink and then failed;
    /// the sink holds a partial object and must be discarded.
    #[error("object download failed after {bytes} bytes (response 0x{code:04x}); discard the partial file")]
    PartialDownload {
        /// Bytes already written to the sink.
        bytes: u64,
        /// Response code.
        code: u16,
    },
    /// Invalid argument from the caller (not from the device).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The session is not open.
    #[error("PTP session is not open")]
    NotOpen,
    /// A camera-level failure with a user-facing explanation.
    #[error("{0}")]
    Camera(String),
}

impl PtpError {
    /// True when the device is gone: retry loops must stop.
    pub fn is_fatal(&self) -> bool {
        matches!(self, PtpError::Usb(e) if e.is_fatal())
    }
}
