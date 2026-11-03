//! Handle sources. The protocol opens a fresh handle for every logical operation and several
//! protocol behaviours depend on those boundaries (priming cycles, visible handle → IR
//! handle), so the session asks a [`HandleOpener`] for a new transport each time and drops
//! it (release + close) when the step ends.

use std::sync::Arc;
use std::time::Duration;

use silvercraft_usb::{DeviceFilter, FoundDevice, NusbTransport, OpenOptions, Transport, discover};

use crate::codec::{EP_IN, EP_OUT, INTERFACE, PRODUCT_ID, VENDOR_ID};
use crate::{Clock, EpsonError, SystemClock};

/// Opens one handle (open + claim) per call; dropping the handle closes it.
pub trait HandleOpener {
    /// The transport of one open handle.
    type Handle: Transport;
    /// Open and claim a fresh handle.
    fn open(&mut self) -> Result<Self::Handle, EpsonError>;
    /// The time source (virtual for tests and the simulator).
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(SystemClock::new())
    }
    /// An id of this *enumeration* of the device — it must change whenever the device
    /// re-enumerates (power cycle, replug). `None`: unknown, so calibration is tracked
    /// for the lifetime of the [`crate::Session`] only.
    fn enumeration_id(&self) -> Option<String> {
        None
    }
}

/// A V800/V850 seen on the bus. Holding one does not open the device.
#[derive(Debug, Clone)]
pub struct FoundScanner {
    device: FoundDevice,
}

impl FoundScanner {
    /// The underlying USB record.
    pub fn usb(&self) -> &FoundDevice {
        &self.device
    }
    /// Product string, if the OS cached one.
    pub fn product(&self) -> Option<&str> {
        self.device.product.as_deref()
    }
    /// Serial string, if any.
    pub fn serial(&self) -> Option<&str> {
        self.device.serial.as_deref()
    }
    /// Bus id and address (the address changes on every re-enumeration).
    pub fn location(&self) -> String {
        format!("{}-{}", self.device.bus_id, self.device.address)
    }
    /// legacy-style device id.
    pub fn id(&self) -> String {
        format!("epson-direct:{VENDOR_ID:04x}:{PRODUCT_ID:04x}:{}", self.location())
    }
}

/// Enumerate attached V800/V850 scanners from cached OS descriptors. Never opens, claims,
/// resets or talks to a device (§1.1).
pub fn discover_scanners() -> Result<Vec<FoundScanner>, EpsonError> {
    let filter = DeviceFilter { vendor_id: Some(VENDOR_ID), product_id: Some(PRODUCT_ID), interface_class: None };
    Ok(discover(&filter).map_err(EpsonError::from)?.into_iter().map(|device| FoundScanner { device }).collect())
}

/// Opens handles on a real scanner through `nusb` (Linux/macOS). Interface 0, kernel driver
/// detached, bulk OUT 0x01 / IN 0x82. No SET_CONFIGURATION, no control transfers, no reset.
#[derive(Debug, Clone)]
pub struct NusbOpener {
    found: FoundScanner,
}

impl NusbOpener {
    /// An opener for `found` (does no I/O).
    pub fn new(found: FoundScanner) -> Self {
        Self { found }
    }
}

/// Whether this build can reach the scanner. Windows needs a usbscan.sys transport
/// (`CreateFileW` + overlapped I/O), which is not implemented: replacing Epson's driver with
/// WinUSB is not supported.
pub fn platform_supported() -> Result<(), EpsonError> {
    // Every desktop platform is reachable through nusb. On Windows that needs the scanner bound
    // to WinUSB instead of Epson's usbscan.sys (see `windows_open_hint`); a usbscan backend is
    // not implemented.
    Ok(())
}

/// What to tell a Windows user whose scanner could not be opened: Epson's own driver
/// (usbscan.sys) owns it, and nusb needs WinUSB.
pub const WINDOWS_OPEN_HINT: &str = "On Windows, SilverCraft talks to the scanner through WinUSB: install the WinUSB driver for \
\"EPSON Perfection V800/V850\" (e.g. with Zadig). Epson Scan stops working while WinUSB is installed; reinstall Epson's driver \
to switch back.";

fn windows_hint(e: EpsonError) -> EpsonError {
    if cfg!(windows) { EpsonError::Unsupported(format!("{e}. {WINDOWS_OPEN_HINT}")) } else { e }
}

impl HandleOpener for NusbOpener {
    type Handle = NusbTransport;

    fn open(&mut self) -> Result<NusbTransport, EpsonError> {
        platform_supported()?;
        // Revalidate the snapshot before opening it: the scanner may have been
        // power-cycled or replugged since discovery, and a stale record can open
        // successfully against a *new* enumeration whose carriage is uncalibrated.
        // Require the same enumeration to still be on the bus and open the fresh
        // record. (Discovery reads cached OS descriptors; it does no device I/O.)
        let key = self.found.device.enumeration_key();
        let fresh = discover_scanners()?.into_iter().find(|s| s.device.enumeration_key() == key);
        let Some(fresh) = fresh else {
            return Err(EpsonError::Disconnected(
                "the scanner is no longer attached as it was (power-cycled or unplugged); refresh the device list".into(),
            ));
        };
        self.found = fresh;
        let opts =
            OpenOptions { interface_class: None, claim_attempts: 3, claim_retry_delay: Duration::from_millis(400), detach_kernel_driver: true };
        let t = NusbTransport::open(&self.found.device, &opts).map_err(|e| windows_hint(EpsonError::from(e)))?;
        let s = t.selection();
        if s.interface != INTERFACE || s.bulk_out.address != EP_OUT || s.bulk_in.address != EP_IN {
            // Dropping releases the claim; nothing was sent.
            return Err(EpsonError::Protocol(format!(
                "unexpected pipes: interface {} OUT 0x{:02x} IN 0x{:02x} (expected {INTERFACE}, 0x{EP_OUT:02x}, 0x{EP_IN:02x})",
                s.interface, s.bulk_out.address, s.bulk_in.address
            )));
        }
        Ok(t)
    }

    fn enumeration_id(&self) -> Option<String> {
        Some(self.found.device.enumeration_key())
    }
}

/// Hands out clones of one transport as "handles" (tests, replays, the simulator's raw
/// transport). Handle boundaries are not visible on the wire, so clones sharing state are
/// exactly what a scripted fake needs.
#[derive(Debug, Clone)]
pub struct CloneOpener<T: Transport + Clone> {
    transport: T,
    clock: Arc<dyn Clock>,
    opens: usize,
}

impl<T: Transport + Clone> CloneOpener<T> {
    /// Wrap `transport` with the given clock.
    pub fn new(transport: T, clock: Arc<dyn Clock>) -> Self {
        Self { transport, clock, opens: 0 }
    }
    /// How many handles were opened.
    pub fn opens(&self) -> usize {
        self.opens
    }
}

impl std::fmt::Debug for dyn Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock")
    }
}

impl<T: Transport + Clone> HandleOpener for CloneOpener<T> {
    type Handle = T;
    fn open(&mut self) -> Result<T, EpsonError> {
        self.opens += 1;
        Ok(self.transport.clone())
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
}
