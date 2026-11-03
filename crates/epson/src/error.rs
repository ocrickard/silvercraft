//! Driver errors.

use silvercraft_usb::UsbError;

/// User-facing text for [`EpsonError::PowerCycleRequired`].
pub const POWER_CYCLE_MESSAGE: &str = "The scanner stopped responding and software recovery could not clear it. \
Turn the scanner off, wait a few seconds, turn it back on, then refresh the device list.";

/// An Epson V800/V850 driver failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EpsonError {
    /// The USB transport failed (other than a timeout, which is [`EpsonError::Timeout`]).
    #[error("Epson USB error: {0}")]
    Usb(UsbError),
    /// A read or write did not finish within its deadline. `what` names the step.
    #[error("Epson USB {what} timed out")]
    Timeout {
        /// The protocol step that timed out.
        what: String,
    },
    /// The caller's [`crate::CancelToken`] was triggered; the scanner was cleanly aborted.
    #[error("Epson scan cancelled")]
    Cancelled,
    /// The device answered something the protocol does not allow (hostile or unexpected reply).
    #[error("Epson protocol error: {0}")]
    Protocol(String),
    /// The device answered NAK (0x15) where an ACK was required.
    #[error("Epson scanner rejected {0} (NAK)")]
    Nak(String),
    /// A request that would hard-lock the firmware or is otherwise invalid; refused before any I/O.
    #[error("invalid scan request: {0}")]
    InvalidRequest(String),
    /// A targeted (non-origin) scan was requested before the origin prescan established the
    /// carriage calibration this power cycle, and `auto_calibrate` was off. No I/O was done.
    #[error(
        "the scanner carriage has not been calibrated this power cycle: run the bed prescan first \
         (a targeted scan on an uncalibrated carriage hard-locks the scanner)"
    )]
    NotCalibrated,
    /// The visible pass of a dual pass did not complete, so the infrared pass was not started.
    #[error("visible scan did not complete; infrared capture was cancelled ({0})")]
    VisibleIncomplete(String),
    /// The raster that came back cannot form the requested image.
    #[error("Epson raster error: {0}")]
    Raster(String),
    /// The scanner is wedged; only a physical power cycle recovers it.
    #[error("power-cycle required: {reason}. {POWER_CYCLE_MESSAGE}")]
    PowerCycleRequired {
        /// What was observed.
        reason: String,
    },
    /// The scanner is not reachable through this backend (e.g. Windows usbscan).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The scanner went away (unplugged or powered off) or was not found.
    #[error("The Epson scanner disconnected. Refresh the device list. ({0})")]
    Disconnected(String),
}

impl EpsonError {
    /// Text suitable for a GUI dialog.
    pub fn user_message(&self) -> String {
        match self {
            EpsonError::PowerCycleRequired { .. } => POWER_CYCLE_MESSAGE.to_string(),
            EpsonError::Usb(UsbError::PermissionDenied(m)) => {
                format!("Permission denied opening the scanner ({m}). Allow this user access to the scanner with the system scanner/udev rules.")
            }
            EpsonError::Usb(UsbError::Busy(m)) => format!("The scanner is busy ({m}). Close other applications using the scanner."),
            other => other.to_string(),
        }
    }

    /// True for [`EpsonError::PowerCycleRequired`].
    pub fn requires_power_cycle(&self) -> bool {
        matches!(self, EpsonError::PowerCycleRequired { .. })
    }

    pub(crate) fn is_timeout(&self) -> bool {
        matches!(self, EpsonError::Timeout { .. })
    }
}

impl From<UsbError> for EpsonError {
    fn from(e: UsbError) -> Self {
        match e {
            UsbError::Timeout => EpsonError::Timeout { what: "transfer".into() },
            UsbError::Disconnected => EpsonError::Disconnected("device disconnected".into()),
            UsbError::NotFound(m) => EpsonError::Disconnected(m),
            other => EpsonError::Usb(other),
        }
    }
}
