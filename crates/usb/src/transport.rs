//! The transport trait.

use std::time::Duration;

use crate::UsbError;

/// One of the pipes a [`Transport`] exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pipe {
    /// Bulk IN (device to host).
    BulkIn,
    /// Bulk OUT (host to device).
    BulkOut,
    /// Interrupt IN (events).
    InterruptIn,
}

impl Pipe {
    /// Short stable name, used in traffic logs.
    pub fn name(self) -> &'static str {
        match self {
            Pipe::BulkIn => "bulk-in",
            Pipe::BulkOut => "bulk-out",
            Pipe::InterruptIn => "interrupt-in",
        }
    }

    /// Inverse of [`Pipe::name`].
    pub fn from_name(s: &str) -> Option<Pipe> {
        match s {
            "bulk-in" => Some(Pipe::BulkIn),
            "bulk-out" => Some(Pipe::BulkOut),
            "interrupt-in" => Some(Pipe::InterruptIn),
            _ => None,
        }
    }
}

/// Blocking bulk/interrupt USB I/O on one claimed interface.
///
/// Semantics every implementation follows:
///
/// - `bulk_in` performs one IN transfer of at most `max_len` bytes. The
///   transfer ends early on a short packet, so the result may be shorter
///   (including empty, for a zero-length packet). Implementations round the
///   request up to a multiple of the max packet size, so callers that want
///   packet-aligned requests should do the rounding themselves with
///   [`round_up_to_packet`] to know exactly how much may arrive.
/// - `bulk_out` with an empty slice sends a zero-length packet.
/// - Timeouts with no data are `Err(UsbError::Timeout)`; a timeout after a
///   partial transfer returns the partial bytes.
pub trait Transport {
    /// Write `data` to the bulk OUT endpoint.
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), UsbError>;
    /// Read one bulk IN transfer of up to `max_len` bytes.
    fn bulk_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError>;
    /// Read one interrupt IN transfer of up to `max_len` bytes.
    fn interrupt_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError>;
    /// Whether an interrupt IN endpoint is available.
    fn has_interrupt_in(&self) -> bool;
    /// Max packet size of the bulk IN endpoint (never 0).
    fn bulk_in_max_packet(&self) -> usize;
    /// Max packet size of the bulk OUT endpoint (never 0).
    fn bulk_out_max_packet(&self) -> usize;
    /// Clear a halt (STALL) condition on `pipe`.
    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), UsbError>;
    /// USB port reset. After a reset the device re-enumerates and this
    /// transport is no longer usable: drop it and open a fresh one.
    fn reset(&mut self) -> Result<(), UsbError>;

    /// Write `data` to bulk OUT and report how many bytes the device accepted.
    ///
    /// `Ok(n)` with `n < data.len()` means the transfer timed out after `n`
    /// bytes went out (possibly 0): the caller may resume with `data[n..]` and
    /// must never resend `data[..n]`. Implementations that cannot tell how
    /// much of a timed-out transfer was sent return `Err(UsbError::Timeout)`
    /// (progress unknown; the default does exactly that by delegating to
    /// [`Transport::bulk_out`]).
    fn bulk_out_counted(&mut self, data: &[u8], timeout: Duration) -> Result<usize, UsbError> {
        self.bulk_out(data, timeout).map(|()| data.len())
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), UsbError> {
        (**self).bulk_out(data, timeout)
    }
    fn bulk_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        (**self).bulk_in(max_len, timeout)
    }
    fn interrupt_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        (**self).interrupt_in(max_len, timeout)
    }
    fn has_interrupt_in(&self) -> bool {
        (**self).has_interrupt_in()
    }
    fn bulk_in_max_packet(&self) -> usize {
        (**self).bulk_in_max_packet()
    }
    fn bulk_out_max_packet(&self) -> usize {
        (**self).bulk_out_max_packet()
    }
    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), UsbError> {
        (**self).clear_halt(pipe)
    }
    fn reset(&mut self) -> Result<(), UsbError> {
        (**self).reset()
    }
    fn bulk_out_counted(&mut self, data: &[u8], timeout: Duration) -> Result<usize, UsbError> {
        (**self).bulk_out_counted(data, timeout)
    }
}

/// Round `len` up to a non-zero multiple of `packet` (a `packet` of 0 is treated as 1).
///
/// Saturates instead of overflowing.
pub fn round_up_to_packet(len: usize, packet: usize) -> usize {
    let packet = packet.max(1);
    let len = len.max(1);
    match len.checked_add(packet - 1) {
        Some(v) => v / packet * packet,
        None => usize::MAX / packet * packet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python_cases() {
        assert_eq!(round_up_to_packet(1337, 512), 1536);
        assert_eq!(round_up_to_packet(512, 1024), 1024);
        assert_eq!(round_up_to_packet(1337, 1024), 2048);
        assert_eq!(round_up_to_packet(1024, 1024), 1024);
        assert_eq!(round_up_to_packet(0, 512), 512);
        assert_eq!(round_up_to_packet(5, 0), 5);
        assert_eq!(round_up_to_packet(usize::MAX, 512) % 512, 0);
    }

    #[test]
    fn pipe_names_roundtrip() {
        for p in [Pipe::BulkIn, Pipe::BulkOut, Pipe::InterruptIn] {
            assert_eq!(Pipe::from_name(p.name()), Some(p));
        }
        assert_eq!(Pipe::from_name("nope"), None);
    }
}
