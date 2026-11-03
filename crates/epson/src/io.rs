//! Byte-level I/O on one open handle (§1.3, §1.4, §1.7).
//!
//! Every transfer runs in ≤200 ms slices so cancellation is noticed quickly; a timed-out
//! IN slice keeps whatever partial data arrived (dropping it would break 16-bit sample
//! alignment); IN requests are rounded up to whole 512-byte packets with surplus kept in
//! a FIFO for the next read; a timed-out OUT slice resumes from the unsent remainder and
//! never resends what already went out; no single read exceeds `0x20000` bytes.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use silvercraft_usb::{Transport, UsbError, round_up_to_packet};

use crate::codec::{ACK, MAX_READ, NAK, hex};
use crate::{CancelToken, Clock, EpsonError};

/// Per-call USB slice.
pub const SLICE: Duration = Duration::from_millis(200);
/// Floor for the FS G start-poll read timeout. The first FS G after power-on is silent for
/// up to ~30 s (lamp/carriage bring-up) and abandoning that read wedges the device, so the
/// spec requires ≥ 30 s; we use 60 s for margin (the vendor driver budgets ~120 s, ED:1386).
pub const FS_G_TIMEOUT_FLOOR: Duration = Duration::from_secs(60);
/// Status-probe timeout cap.
pub const STATUS_TIMEOUT_CAP: Duration = Duration::from_millis(1500);
/// Whole-recovery-session timeout cap.
pub const RECOVERY_TIMEOUT_CAP: Duration = Duration::from_millis(2000);
/// Recovery / `drain_input` read timeout cap.
pub const DRAIN_TIMEOUT_CAP: Duration = Duration::from_millis(250);
/// Safe-abort drain timeout cap.
pub const SAFE_ABORT_TIMEOUT_CAP: Duration = Duration::from_millis(2000);
/// Default `drain_input` limit (1 MiB).
pub const DRAIN_LIMIT: usize = 0x100000;

/// One open handle plus its read FIFO.
pub struct Io<T: Transport> {
    t: T,
    pending: VecDeque<u8>,
    timeout: Duration,
    clock: Arc<dyn Clock>,
    cancel: Option<CancelToken>,
    honor_cancel: bool,
}

impl<T: Transport> Io<T> {
    /// Wrap an open transport.
    pub fn new(t: T, timeout: Duration, clock: Arc<dyn Clock>, cancel: Option<CancelToken>) -> Self {
        Self { t, pending: VecDeque::new(), timeout, clock, cancel, honor_cancel: true }
    }

    /// Current per-operation timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Set the per-operation timeout.
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// The clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Stop honouring the cancel token (cleanup writes must go out even after a cancel —
    /// the "clear the abort flag first" rule, §6.5 step 2). Returns the previous setting.
    pub fn set_honor_cancel(&mut self, honor: bool) -> bool {
        std::mem::replace(&mut self.honor_cancel, honor)
    }

    /// Whether cancellation was requested (regardless of honouring).
    pub fn cancel_requested(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.is_cancelled())
    }

    fn check_cancel(&self) -> Result<(), EpsonError> {
        if self.honor_cancel && self.cancel_requested() { Err(EpsonError::Cancelled) } else { Ok(()) }
    }

    fn slice(remaining: Duration) -> Duration {
        remaining.min(SLICE).max(Duration::from_millis(1))
    }

    /// Write all of `data` within the timeout (§1.3).
    pub fn write(&mut self, data: &[u8], what: &str) -> Result<(), EpsonError> {
        let start = self.clock.now();
        let mut written = 0usize;
        while written < data.len() {
            self.check_cancel()?;
            let elapsed = self.clock.now().saturating_sub(start);
            if elapsed >= self.timeout {
                return Err(EpsonError::Timeout { what: format!("write of {what} ({written} of {} bytes sent)", data.len()) });
            }
            let slice = Self::slice(self.timeout - elapsed);
            let rest = data.get(written..).unwrap_or(&[]);
            match self.t.bulk_out_counted(rest, slice) {
                Ok(n) => {
                    // A timed-out slice may have sent a prefix: resume after it, never resend.
                    written += n.min(rest.len());
                    if n < rest.len() {
                        self.clock.charge(slice);
                    }
                }
                Err(UsbError::Timeout) => {
                    // Progress unknown: resending could duplicate command bytes (§6.2 #11).
                    return Err(EpsonError::Timeout { what: format!("write of {what} (progress unknown; not resent)") });
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Read up to `size` bytes (≥1 unless `size == 0`) within the timeout. May return fewer
    /// than requested; surplus from a packet-rounded transfer stays queued.
    pub fn read(&mut self, size: usize, what: &str) -> Result<Vec<u8>, EpsonError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let start = self.clock.now();
        loop {
            self.check_cancel()?;
            if !self.pending.is_empty() {
                let n = size.min(self.pending.len());
                return Ok(self.pending.drain(..n).collect());
            }
            let elapsed = self.clock.now().saturating_sub(start);
            if elapsed >= self.timeout {
                return Err(EpsonError::Timeout { what: format!("read of {what}") });
            }
            let slice = Self::slice(self.timeout - elapsed);
            // Whole packets (512 on the V800): a 1-byte ACK still arrives as a packet.
            let packet = self.t.bulk_in_max_packet().max(1);
            let capacity = round_up_to_packet(size.min(MAX_READ), packet);
            match self.t.bulk_in(capacity, slice) {
                Ok(d) => {
                    if d.is_empty() {
                        // Zero-length packet: no progress; account a little time so a
                        // virtual clock still reaches the deadline.
                        self.clock.charge(Duration::from_millis(1));
                    }
                    self.pending.extend(d);
                }
                Err(UsbError::Timeout) => self.clock.charge(slice),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read exactly `n` bytes in sub-reads of at most `0x20000` (§1.4).
    pub fn read_exact(&mut self, n: usize, what: &str) -> Result<Vec<u8>, EpsonError> {
        let mut out = Vec::with_capacity(n.min(MAX_READ * 8));
        while out.len() < n {
            let chunk = self.read((n - out.len()).min(MAX_READ), what)?;
            if chunk.is_empty() {
                return Err(EpsonError::Protocol(format!("{what}: expected {n} bytes, got {}", out.len())));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Read exactly `n` bytes, appending into `buf` (no intermediate copy of large chunks).
    pub fn read_exact_into(&mut self, buf: &mut Vec<u8>, n: usize, what: &str) -> Result<(), EpsonError> {
        let target = buf.len() + n;
        while buf.len() < target {
            let chunk = self.read((target - buf.len()).min(MAX_READ), what)?;
            if chunk.is_empty() {
                return Err(EpsonError::Protocol(format!("{what}: short read")));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(())
    }

    /// Read one byte and require ACK.
    pub fn ack(&mut self, what: &str) -> Result<(), EpsonError> {
        let r = self.read_exact(1, what)?;
        match r.first() {
            Some(&ACK) => Ok(()),
            Some(&NAK) => Err(EpsonError::Nak(what.to_string())),
            _ => Err(EpsonError::Protocol(format!("{what}: {} != ack", hex(&r)))),
        }
    }

    /// Write a command and require ACK.
    pub fn command_ack(&mut self, data: &[u8], what: &str) -> Result<(), EpsonError> {
        self.write(data, what)?;
        self.ack(what)
    }

    /// Discard queued input (≤ `limit` bytes) with a ≤250 ms per-read timeout, so the
    /// device can accept CAN again. Never fails; returns bytes drained.
    pub fn drain_input(&mut self, limit: usize) -> usize {
        let saved = self.timeout;
        self.timeout = saved.min(DRAIN_TIMEOUT_CAP);
        let mut drained = 0usize;
        while drained < limit {
            match self.read((limit - drained).min(MAX_READ), "drain") {
                Ok(d) if !d.is_empty() => drained += d.len(),
                _ => break,
            }
        }
        self.timeout = saved;
        drained
    }

    /// Bytes queued in the FIFO.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drop the handle wrapper, returning the transport (closing it when dropped).
    pub fn into_inner(self) -> T {
        self.t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VirtualClock;
    use silvercraft_usb::{MockEvent, ScriptedTransport, Step};

    fn io(t: ScriptedTransport, ms: u64) -> (Io<ScriptedTransport>, VirtualClock) {
        let c = VirtualClock::new();
        (Io::new(t, Duration::from_millis(ms), Arc::new(c.clone()), None), c)
    }

    #[test]
    fn surplus_bytes_stay_queued_and_reads_round_to_packets() {
        let t = ScriptedTransport::new([Step::In(vec![6, 2, 18, 0, 0])]);
        let (mut io, _) = io(t.clone(), 1000);
        assert_eq!(io.read_exact(1, "ack").unwrap(), vec![6]);
        assert_eq!(io.pending_len(), 4);
        assert_eq!(io.read_exact(4, "status").unwrap(), vec![2, 18, 0, 0]);
        match &t.events()[0] {
            MockEvent::In { requested, .. } => assert_eq!(*requested, 512),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reads_never_exceed_the_sbp2_block() {
        let t = ScriptedTransport::new([Step::In(vec![1; 300_000])]);
        let (mut io, _) = io(t.clone(), 1000);
        assert_eq!(io.read_exact(300_000, "raster").unwrap().len(), 300_000);
        for e in t.events() {
            if let MockEvent::In { requested, .. } = e {
                assert!(requested <= MAX_READ);
            }
        }
    }

    #[test]
    fn read_timeout_uses_virtual_slices() {
        let (mut io, clock) = io(ScriptedTransport::new([]), 1000);
        let e = io.read(4, "status").unwrap_err();
        assert!(e.is_timeout());
        assert_eq!(clock.now(), Duration::from_millis(1000));
    }

    #[test]
    fn partial_data_from_a_timed_out_slice_is_kept() {
        // Two separate transfers (as if the first slice timed out with 3 bytes).
        let t = ScriptedTransport::new([Step::In(vec![1, 2, 3]), Step::InErr(UsbError::Timeout), Step::In(vec![4, 5])]);
        let (mut io, _) = io(t, 5000);
        assert_eq!(io.read_exact(5, "x").unwrap(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn cancel_is_honoured_unless_cleaning_up() {
        let c = CancelToken::new();
        c.cancel();
        let t = ScriptedTransport::new([Step::Out(vec![0x18]), Step::In(vec![6])]);
        let mut io = Io::new(t.clone(), Duration::from_secs(1), Arc::new(VirtualClock::new()), Some(c));
        assert_eq!(io.write(&[0x18], "CAN"), Err(EpsonError::Cancelled));
        io.set_honor_cancel(false);
        io.write(&[0x18], "CAN").unwrap();
        io.ack("CAN").unwrap();
        t.verify().unwrap();
    }

    #[test]
    fn unknown_progress_write_timeout_is_not_resent() {
        let t = ScriptedTransport::new([Step::OutErr(UsbError::Timeout)]);
        let (mut io, _) = io(t.clone(), 5000);
        assert!(io.write(&[0x1B, 0x40], "ESC @").unwrap_err().is_timeout());
        assert_eq!(t.writes().len(), 1);
    }

    #[test]
    fn ack_variants() {
        let (mut io, _) = io(ScriptedTransport::new([Step::In(vec![0x15]), Step::In(vec![0x02])]), 1000);
        assert!(matches!(io.ack("FS W"), Err(EpsonError::Nak(_))));
        assert!(matches!(io.ack("FS W"), Err(EpsonError::Protocol(_))));
    }

    #[test]
    fn drain_is_bounded_and_restores_timeout() {
        let (mut io, _) = io(ScriptedTransport::new([Step::In(vec![0; 5 * 131072])]), 30_000);
        assert_eq!(io.drain_input(DRAIN_LIMIT), 5 * 131072);
        assert_eq!(io.timeout(), Duration::from_secs(30));
        let (mut io, _) = io_big();
        assert_eq!(io.drain_input(DRAIN_LIMIT), DRAIN_LIMIT);
    }

    fn io_big() -> (Io<ScriptedTransport>, VirtualClock) {
        io(ScriptedTransport::new([Step::In(vec![0; 3 * DRAIN_LIMIT])]), 30_000)
    }
}
