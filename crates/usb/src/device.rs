//! The real transport over `nusb`, and device discovery.
//!
//! Nothing here runs in unit tests against hardware: the descriptor logic is
//! factored into pure functions ([`select_pipes`], [`DeviceFilter::matches`])
//! that are tested on synthetic descriptors.

use std::time::Duration;

use nusb::MaybeFuture;
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, In, Interrupt, Out, TransferError};

use crate::{Pipe, Transport, UsbError, round_up_to_packet};

/// Largest single IN request we hand to the OS (nusb buffers are u32-sized;
/// usbfs also has per-URB limits, so keep transfers modest).
const MAX_TRANSFER: usize = 4 * 1024 * 1024;

/// Which devices [`discover`] reports. `None` fields match anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceFilter {
    /// USB vendor id.
    pub vendor_id: Option<u16>,
    /// USB product id.
    pub product_id: Option<u16>,
    /// Some interface of the device must have this class.
    pub interface_class: Option<u8>,
}

impl DeviceFilter {
    /// Pure matching rule, separated from enumeration for testing.
    pub fn matches(&self, vendor_id: u16, product_id: u16, interface_classes: &[u8]) -> bool {
        self.vendor_id.is_none_or(|v| v == vendor_id)
            && self.product_id.is_none_or(|p| p == product_id)
            && self.interface_class.is_none_or(|c| interface_classes.contains(&c))
    }
}

/// A device found by [`discover`]. Holding one does not open the device.
#[derive(Debug, Clone)]
pub struct FoundDevice {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Manufacturer string from the cached descriptor, if any.
    pub manufacturer: Option<String>,
    /// Product string, if any.
    pub product: Option<String>,
    /// Serial number string, if any.
    pub serial: Option<String>,
    /// Bus id (platform-specific text).
    pub bus_id: String,
    /// Device address on the bus.
    pub address: u8,
    /// Interface number of the first interface matching the filter's class
    /// (or the first interface when the filter has no class).
    pub interface_number: Option<u8>,
    /// Negotiated speed, when the OS reports it.
    pub speed: Option<String>,
    info: nusb::DeviceInfo,
}

impl FoundDevice {
    /// The underlying `nusb` enumeration record.
    pub fn info(&self) -> &nusb::DeviceInfo {
        &self.info
    }

    /// A key for this *enumeration* of the device: the platform device id plus the
    /// serial. On macOS the id is the IOKit registry id, which changes on every
    /// re-enumeration (power cycle / replug). On Linux/Windows it is bus + address /
    /// device instance, which *usually* changes but can be reassigned identically —
    /// callers must treat a match as "probably the same enumeration", never proof,
    /// and revalidate by re-discovering (see the Epson opener).
    pub fn enumeration_key(&self) -> String {
        format!("{:?}:{}", self.info.id(), self.serial.as_deref().unwrap_or(""))
    }
}

/// Enumerate USB devices matching `filter`. Reads cached OS descriptors only;
/// does not open, claim or talk to any device.
pub fn discover(filter: &DeviceFilter) -> Result<Vec<FoundDevice>, UsbError> {
    let devices = nusb::list_devices().wait().map_err(map_nusb_error)?;
    let mut out = Vec::new();
    for info in devices {
        let classes: Vec<u8> = info.interfaces().map(|i| i.class()).collect();
        if !filter.matches(info.vendor_id(), info.product_id(), &classes) {
            continue;
        }
        let interface_number = match filter.interface_class {
            Some(c) => info.interfaces().find(|i| i.class() == c).map(|i| i.interface_number()),
            None => info.interfaces().next().map(|i| i.interface_number()),
        };
        out.push(FoundDevice {
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            manufacturer: info.manufacturer_string().map(str::to_string),
            product: info.product_string().map(str::to_string),
            serial: info.serial_number().map(str::to_string),
            bus_id: info.bus_id().to_string(),
            address: info.device_address(),
            interface_number,
            speed: info.speed().map(|s| format!("{s:?}")),
            info,
        });
    }
    Ok(out)
}

/// Kind of an endpoint, as far as pipe selection cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// Bulk.
    Bulk,
    /// Interrupt.
    Interrupt,
    /// Control or isochronous (ignored).
    Other,
}

/// A plain-data endpoint descriptor (address bit 7 set = IN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDesc {
    /// bEndpointAddress.
    pub address: u8,
    /// Transfer type.
    pub kind: EndpointKind,
    /// wMaxPacketSize (payload bytes).
    pub max_packet: usize,
}

impl EndpointDesc {
    fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
}

/// A plain-data interface descriptor (alternate setting 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDesc {
    /// bInterfaceNumber.
    pub number: u8,
    /// bInterfaceClass.
    pub class: u8,
    /// Endpoints.
    pub endpoints: Vec<EndpointDesc>,
}

/// The pipes chosen on one interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeSelection {
    /// Interface to claim.
    pub interface: u8,
    /// Bulk IN endpoint.
    pub bulk_in: EndpointDesc,
    /// Bulk OUT endpoint.
    pub bulk_out: EndpointDesc,
    /// Interrupt IN endpoint, if present.
    pub interrupt_in: Option<EndpointDesc>,
}

/// Take the first interface of class `class` (any class when `None`) that has
/// both a bulk IN and a bulk OUT endpoint.
pub fn select_pipes(interfaces: &[InterfaceDesc], class: Option<u8>) -> Option<PipeSelection> {
    interfaces.iter().filter(|i| class.is_none_or(|c| c == i.class)).find_map(|i| {
        let bulk_in = i.endpoints.iter().find(|e| e.kind == EndpointKind::Bulk && e.is_in())?;
        let bulk_out = i.endpoints.iter().find(|e| e.kind == EndpointKind::Bulk && !e.is_in())?;
        let interrupt_in = i.endpoints.iter().find(|e| e.kind == EndpointKind::Interrupt && e.is_in()).copied();
        Some(PipeSelection { interface: i.number, bulk_in: *bulk_in, bulk_out: *bulk_out, interrupt_in })
    })
}

/// How [`NusbTransport::open`] claims the device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenOptions {
    /// Required interface class (6 = still image / PTP). `None` takes the
    /// first interface with bulk IN + OUT.
    pub interface_class: Option<u8>,
    /// Claim attempts (3).
    pub claim_attempts: u32,
    /// Pause between claim attempts (0.4 s).
    pub claim_retry_delay: Duration,
    /// Detach a bound kernel driver first (Linux only; no effect elsewhere).
    pub detach_kernel_driver: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self { interface_class: Some(0x06), claim_attempts: 3, claim_retry_delay: Duration::from_millis(400), detach_kernel_driver: true }
    }
}

/// [`Transport`] over a claimed `nusb` interface.
pub struct NusbTransport {
    device: nusb::Device,
    // Held so the claim lives as long as the transport; released on drop.
    _interface: nusb::Interface,
    bulk_in: nusb::Endpoint<Bulk, In>,
    bulk_out: nusb::Endpoint<Bulk, Out>,
    interrupt_in: Option<nusb::Endpoint<Interrupt, In>>,
    selection: PipeSelection,
}

impl std::fmt::Debug for NusbTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NusbTransport").field("selection", &self.selection).finish()
    }
}

fn interface_descs(device: &nusb::Device) -> Result<Vec<InterfaceDesc>, UsbError> {
    let config = device.active_configuration().map_err(|e| UsbError::NotFound(e.to_string()))?;
    let mut out = Vec::new();
    for group in config.interfaces() {
        let alt = group.first_alt_setting();
        let endpoints = alt
            .endpoints()
            .map(|e| EndpointDesc {
                address: e.address(),
                kind: match e.transfer_type() {
                    TransferType::Bulk => EndpointKind::Bulk,
                    TransferType::Interrupt => EndpointKind::Interrupt,
                    _ => EndpointKind::Other,
                },
                max_packet: e.max_packet_size(),
            })
            .collect();
        out.push(InterfaceDesc { number: group.interface_number(), class: alt.class(), endpoints });
    }
    Ok(out)
}

impl NusbTransport {
    /// Open `found`, pick the pipes and claim the interface (with retries and,
    /// on Linux, kernel-driver detach). This is real hardware I/O.
    pub fn open(found: &FoundDevice, opts: &OpenOptions) -> Result<Self, UsbError> {
        let device = found.info.open().wait().map_err(map_nusb_error)?;
        let descs = interface_descs(&device)?;
        let selection =
            select_pipes(&descs, opts.interface_class).ok_or_else(|| UsbError::NotFound("no interface with bulk IN and OUT endpoints".into()))?;

        let attempts = opts.claim_attempts.max(1);
        let mut last = UsbError::NotFound("claim not attempted".into());
        let mut interface = None;
        for attempt in 0..attempts {
            let r = if opts.detach_kernel_driver {
                device.detach_and_claim_interface(selection.interface).wait()
            } else {
                device.claim_interface(selection.interface).wait()
            };
            match r {
                Ok(i) => {
                    interface = Some(i);
                    break;
                }
                Err(e) => {
                    last = map_nusb_error(e);
                    log::warn!("claim interface {} attempt {} failed: {last}", selection.interface, attempt + 1);
                    if attempt + 1 < attempts {
                        std::thread::sleep(opts.claim_retry_delay);
                    }
                }
            }
        }
        let Some(interface) = interface else { return Err(last) };
        let bulk_in = interface.endpoint::<Bulk, In>(selection.bulk_in.address).map_err(map_nusb_error)?;
        let bulk_out = interface.endpoint::<Bulk, Out>(selection.bulk_out.address).map_err(map_nusb_error)?;
        let interrupt_in = match selection.interrupt_in {
            Some(ep) => match interface.endpoint::<Interrupt, In>(ep.address) {
                Ok(e) => Some(e),
                Err(e) => {
                    log::warn!("interrupt endpoint 0x{:02x} unavailable: {e}", ep.address);
                    None
                }
            },
            None => None,
        };
        Ok(Self { device, _interface: interface, bulk_in, bulk_out, interrupt_in, selection })
    }

    /// The pipes in use.
    pub fn selection(&self) -> PipeSelection {
        self.selection
    }
}

fn map_nusb_error(e: nusb::Error) -> UsbError {
    use nusb::ErrorKind as K;
    let msg = e.to_string();
    match e.kind() {
        K::Disconnected => UsbError::Disconnected,
        K::Busy => UsbError::Busy(msg),
        K::PermissionDenied => UsbError::PermissionDenied(msg),
        K::NotFound => UsbError::NotFound(msg),
        K::Unsupported => UsbError::Unsupported(msg),
        _ => UsbError::Io(msg),
    }
}

/// Map a transfer error. EOVERFLOW (Linux errno 75, macOS errno 84) is babble.
fn map_transfer_error(e: TransferError) -> UsbError {
    match e {
        TransferError::Cancelled => UsbError::Timeout,
        TransferError::Stall => UsbError::Stall,
        TransferError::Disconnected => UsbError::Disconnected,
        TransferError::Unknown(code) if is_overflow_code(code) => UsbError::Overflow,
        other => {
            let msg = other.to_string();
            if msg.to_ascii_lowercase().contains("overflow") { UsbError::Overflow } else { UsbError::Io(msg) }
        }
    }
}

fn is_overflow_code(code: u32) -> bool {
    if cfg!(target_os = "linux") || cfg!(target_os = "android") {
        code == 75
    } else if cfg!(target_os = "macos") {
        code == 84
    } else {
        false
    }
}

/// Result of a counted OUT transfer: a timeout (`Cancelled`) reports the bytes
/// that went out before it, so the caller can resume without resending them.
fn counted_out_result(status: Result<(), TransferError>, actual_len: usize, requested: usize) -> Result<usize, UsbError> {
    match status {
        Ok(()) => Ok(actual_len.min(requested)),
        Err(TransferError::Cancelled) => Ok(actual_len.min(requested)),
        Err(e) => Err(map_transfer_error(e)),
    }
}

fn completion_to_result(c: nusb::transfer::Completion) -> Result<Vec<u8>, UsbError> {
    match c.status {
        Ok(()) => Ok(c.buffer.into_vec()),
        // A timeout after partial data: hand back what arrived so the
        // protocol layer can keep its framing.
        Err(TransferError::Cancelled) if c.actual_len > 0 => Ok(c.buffer.into_vec()),
        Err(e) => Err(map_transfer_error(e)),
    }
}

impl Transport for NusbTransport {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), UsbError> {
        if self.bulk_out.pending() != 0 {
            self.bulk_out.cancel_all();
            while self.bulk_out.pending() != 0 {
                if self.bulk_out.wait_next_complete(Duration::from_secs(1)).is_none() {
                    return Err(UsbError::Io("stale OUT transfer would not cancel".into()));
                }
            }
        }
        let c = self.bulk_out.transfer_blocking(Buffer::from(data.to_vec()), timeout);
        match c.status {
            Ok(()) => Ok(()),
            Err(e) => Err(map_transfer_error(e)),
        }
    }

    fn bulk_out_counted(&mut self, data: &[u8], timeout: Duration) -> Result<usize, UsbError> {
        if self.bulk_out.pending() != 0 {
            self.bulk_out.cancel_all();
            while self.bulk_out.pending() != 0 {
                if self.bulk_out.wait_next_complete(Duration::from_secs(1)).is_none() {
                    return Err(UsbError::Io("stale OUT transfer would not cancel".into()));
                }
            }
        }
        let c = self.bulk_out.transfer_blocking(Buffer::from(data.to_vec()), timeout);
        counted_out_result(c.status, c.actual_len, data.len())
    }

    fn bulk_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let mps = self.bulk_in.max_packet_size().max(1);
        let req = round_up_to_packet(max_len.min(MAX_TRANSFER), mps);
        if self.bulk_in.pending() != 0 {
            return Err(UsbError::Io("bulk IN transfer already pending".into()));
        }
        completion_to_result(self.bulk_in.transfer_blocking(Buffer::new(req), timeout))
    }

    fn interrupt_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let Some(ep) = self.interrupt_in.as_mut() else {
            return Err(UsbError::NotFound("no interrupt IN endpoint".into()));
        };
        let mps = ep.max_packet_size().max(1);
        let req = round_up_to_packet(max_len.min(64 * 1024), mps);
        if ep.pending() != 0 {
            return Err(UsbError::Io("interrupt transfer already pending".into()));
        }
        completion_to_result(ep.transfer_blocking(Buffer::new(req), timeout))
    }

    fn has_interrupt_in(&self) -> bool {
        self.interrupt_in.is_some()
    }

    fn bulk_in_max_packet(&self) -> usize {
        self.bulk_in.max_packet_size().max(1)
    }

    fn bulk_out_max_packet(&self) -> usize {
        self.bulk_out.max_packet_size().max(1)
    }

    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), UsbError> {
        let r = match pipe {
            Pipe::BulkIn => self.bulk_in.clear_halt().wait(),
            Pipe::BulkOut => self.bulk_out.clear_halt().wait(),
            Pipe::InterruptIn => match self.interrupt_in.as_mut() {
                Some(ep) => ep.clear_halt().wait(),
                None => return Err(UsbError::NotFound("no interrupt IN endpoint".into())),
            },
        };
        r.map_err(map_nusb_error)
    }

    fn reset(&mut self) -> Result<(), UsbError> {
        self.device.reset().wait().map_err(map_nusb_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ilce1_like() -> Vec<InterfaceDesc> {
        // Addendum: class 6/1/1, EP 0x81 bulk IN, 0x02 bulk OUT, 0x83 interrupt IN.
        vec![InterfaceDesc {
            number: 0,
            class: 6,
            endpoints: vec![
                EndpointDesc { address: 0x81, kind: EndpointKind::Bulk, max_packet: 512 },
                EndpointDesc { address: 0x02, kind: EndpointKind::Bulk, max_packet: 512 },
                EndpointDesc { address: 0x83, kind: EndpointKind::Interrupt, max_packet: 64 },
            ],
        }]
    }

    #[test]
    fn selects_ptp_pipes() {
        let s = select_pipes(&ilce1_like(), Some(6)).unwrap();
        assert_eq!(s.interface, 0);
        assert_eq!(s.bulk_in.address, 0x81);
        assert_eq!(s.bulk_out.address, 0x02);
        assert_eq!(s.interrupt_in.unwrap().address, 0x83);
    }

    #[test]
    fn hid_only_accessory_is_rejected() {
        // The 054c:0d68 accessory: one HID interface, interrupt IN only.
        let hid = vec![InterfaceDesc {
            number: 0,
            class: 3,
            endpoints: vec![EndpointDesc { address: 0x81, kind: EndpointKind::Interrupt, max_packet: 8 }],
        }];
        assert_eq!(select_pipes(&hid, Some(6)), None);
        assert_eq!(select_pipes(&hid, None), None);
        let filter = DeviceFilter { vendor_id: Some(0x054C), product_id: None, interface_class: Some(6) };
        assert!(!filter.matches(0x054C, 0x0D68, &[3]));
        assert!(filter.matches(0x054C, 0x0D1C, &[6]));
        assert!(filter.matches(0x054C, 0x0E00, &[8, 6]));
        assert!(!filter.matches(0x04B8, 0x0D1C, &[6]));
    }

    #[test]
    fn skips_class_match_without_bulk_pair() {
        let mut descs = ilce1_like();
        descs.insert(0, InterfaceDesc { number: 1, class: 6, endpoints: vec![] });
        assert_eq!(select_pipes(&descs, Some(6)).unwrap().interface, 0);
    }

    #[test]
    fn counted_out_reports_partial_progress_on_timeout() {
        assert_eq!(counted_out_result(Ok(()), 5, 5), Ok(5));
        assert_eq!(counted_out_result(Err(TransferError::Cancelled), 3, 5), Ok(3));
        assert_eq!(counted_out_result(Err(TransferError::Cancelled), 0, 5), Ok(0));
        assert_eq!(counted_out_result(Err(TransferError::Stall), 2, 5), Err(UsbError::Stall));
        assert_eq!(counted_out_result(Ok(()), 9, 5), Ok(5));
    }

    #[test]
    fn transfer_error_mapping() {
        assert_eq!(map_transfer_error(TransferError::Cancelled), UsbError::Timeout);
        assert_eq!(map_transfer_error(TransferError::Stall), UsbError::Stall);
        assert_eq!(map_transfer_error(TransferError::Disconnected), UsbError::Disconnected);
        if cfg!(target_os = "linux") {
            assert_eq!(map_transfer_error(TransferError::Unknown(75)), UsbError::Overflow);
        }
        assert!(matches!(map_transfer_error(TransferError::Fault), UsbError::Io(_)));
    }
}
