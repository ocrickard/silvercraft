//! Camera model table and the live entry point.
//!
//! Matching is VID 0x054C plus any class-6 (still image) interface, like
//! the original libusb path: the PID is for naming only, so other Alpha bodies
//! are attempted (one body can even have different PIDs per USB mode).

use silvercraft_usb::{DeviceFilter, FoundDevice, NusbTransport, OpenOptions, UsbError, discover};

use super::camera::{HandshakeReport, SonyCamera, SonyOptions};
use crate::PtpError;
use crate::clock::SystemClock;
use crate::connection::ConnectionConfig;
use crate::session::Session;

/// Sony's USB vendor id.
pub const SONY_VENDOR_ID: u16 = 0x054C;
/// USB still-image (PTP) interface class.
pub const PTP_INTERFACE_CLASS: u8 = 0x06;

/// A known camera body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraModel {
    /// USB product id (PC Remote mode).
    pub product_id: u16,
    /// Model name.
    pub name: &'static str,
    /// Whether the full flow was validated on this body.
    pub verified: bool,
}

/// Bodies we know by PID. Only the ILCE-1 is known.
pub const KNOWN_MODELS: &[CameraModel] = &[CameraModel { product_id: 0x0D1C, name: "ILCE-1", verified: true }];

/// Look up a known model.
pub fn model_for(product_id: u16) -> Option<&'static CameraModel> {
    KNOWN_MODELS.iter().find(|m| m.product_id == product_id)
}

/// Display name: "Sony ILCE-1" or "Sony camera (PID 0x0e00)".
pub fn model_name(product_id: u16) -> String {
    match model_for(product_id) {
        Some(m) => format!("Sony {}", m.name),
        None => format!("Sony camera (PID 0x{product_id:04x})"),
    }
}

/// Filter: Sony VID + a class-6 interface, any PID.
pub fn sony_filter() -> DeviceFilter {
    DeviceFilter { vendor_id: Some(SONY_VENDOR_ID), product_id: None, interface_class: Some(PTP_INTERFACE_CLASS) }
}

/// List attached Sony PTP cameras. Reads OS descriptor caches only.
pub fn discover_sony_cameras() -> Result<Vec<FoundDevice>, UsbError> {
    discover(&sony_filter())
}

/// Live bring-up entry point: claim `found`, open a session (with
/// reset-and-retry-once recovery, re-finding the device by serial after the
/// reset), and run the SDIO handshake. **Talks to real hardware.**
pub fn connect_sony(found: &FoundDevice, opts: SonyOptions) -> Result<(SonyCamera<NusbTransport, SystemClock>, HandshakeReport), PtpError> {
    let clock = SystemClock::new();
    let open_opts = OpenOptions::default();
    let serial = found.serial.clone();
    let mut first = Some(found.clone());
    let connect = || -> Result<NusbTransport, PtpError> {
        let dev = match first.take() {
            Some(d) => d,
            None => discover_sony_cameras()?
                .into_iter()
                .find(|d| serial.is_none() || d.serial == serial)
                .ok_or_else(|| PtpError::Camera("camera did not come back after USB reset".into()))?,
        };
        Ok(NusbTransport::open(&dev, &open_opts)?)
    };
    let session = Session::open_with_recovery(connect, ConnectionConfig::default(), &clock)?;
    SonyCamera::connect(session, clock, opts)
}
