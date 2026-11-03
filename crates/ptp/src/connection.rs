//! Container I/O over a [`Transport`]: packet-aligned reads, carry-over of
//! overshoot bytes, zero-padding strip, streamed data phases, ZLPs, drain on
//! error, and the command/data/response transaction choreography.
//!
//! Follows the established transaction choreography, with
//! these deliberate changes:
//! - data phases stream to a `Write` sink with no 64 MiB cap (uncompressed A1
//!   RAWs are ~100 MB), and a data `length` of 0xFFFFFFFF reads to the short
//!   packet;
//! - an all-zero read counts as an empty read instead of failing at once;
//! - a ZLP follows any OUT container that is an exact multiple of the OUT
//!   max packet size (PIMA 15740 USB still-image class rule; historically never
//!   sent a container that large so never needed it);
//! - events arriving on the bulk pipe are skipped (bounded) instead of
//!   being parsed as the response.

use std::io::Write;
use std::time::Duration;

use silvercraft_usb::{Pipe, Transport, UsbError, round_up_to_packet};

use crate::PtpError;
use crate::codec::{ContainerKind, Event, HEADER_LEN, Header, LENGTH_UNKNOWN, Response, encode_command, encode_data, parse_event, parse_response};

/// Tunables (hardware-validated defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionConfig {
    /// Bulk read/write timeout (5 s).
    pub timeout: Duration,
    /// Bulk IN request size for streaming data phases (rounded to the packet size).
    pub transfer_size: usize,
    /// Consecutive empty reads tolerated per container (8).
    pub empty_read_limit: u32,
    /// Largest command/response/event container accepted (they are tiny;
    /// anything larger is treated as garbage).
    pub max_control_container: u32,
    /// Largest data phase accepted, in bytes (`u64::MAX` = no cap).
    pub max_data_len: u64,
    /// Drain: per-read timeout (100 ms).
    pub drain_timeout: Duration,
    /// Drain: read size (64 KiB).
    pub drain_read: usize,
    /// Drain: give up after this many non-empty reads (bounds a babbling device).
    pub drain_max_reads: u32,
    /// Send a ZLP after OUT containers that are a multiple of the packet size.
    pub send_zlp: bool,
    /// Bulk-pipe events skipped while waiting for a response.
    pub max_skipped_events: u32,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(5000),
            transfer_size: 1024 * 1024,
            empty_read_limit: 8,
            max_control_container: 64 * 1024,
            max_data_len: u64::MAX,
            drain_timeout: Duration::from_millis(100),
            drain_read: 64 * 1024,
            drain_max_reads: 8192,
            send_zlp: true,
            max_skipped_events: 16,
        }
    }
}

/// Result of one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnOutcome {
    /// The response container.
    pub response: Response,
    /// Bytes written to the sink by an IN data phase (`None` when the device
    /// sent no data phase).
    pub data_len: Option<u64>,
}

/// A PTP container connection over a USB transport. Owns the transaction
/// counter and the carry-over buffer.
#[derive(Debug)]
pub struct PtpConnection<T: Transport> {
    transport: T,
    cfg: ConnectionConfig,
    pending: Vec<u8>,
    txn: u32,
}

/// What `read_container` produced.
struct Container {
    header: Header,
    /// Payload for non-data containers; empty for data (streamed to the sink).
    small: Vec<u8>,
    /// Bytes streamed to the sink (data containers).
    streamed: u64,
}

fn sink_err(e: std::io::Error) -> PtpError {
    PtpError::Sink(e.to_string())
}

impl<T: Transport> PtpConnection<T> {
    /// Wrap a claimed transport. Does no I/O.
    pub fn new(transport: T, cfg: ConnectionConfig) -> Self {
        Self { transport, cfg, pending: Vec::new(), txn: 0 }
    }

    /// Configuration.
    pub fn config(&self) -> &ConnectionConfig {
        &self.cfg
    }

    /// Mutable configuration (e.g. to shorten the timeout for CloseSession).
    pub fn config_mut(&mut self) -> &mut ConnectionConfig {
        &mut self.cfg
    }

    /// Borrow the transport.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Mutably borrow the transport.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Unwrap the transport.
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Last transaction id used.
    pub fn last_txn(&self) -> u32 {
        self.txn
    }

    /// Restart the counter (new session). The next transaction uses id 1,
    /// matching the established behaviour.
    pub fn reset_txn(&mut self) {
        self.txn = 0;
    }

    /// Bytes carried over for the next container read.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn next_txn(&mut self) -> u32 {
        self.txn = self.txn.wrapping_add(1);
        self.txn
    }

    /// Discard stale IN data so the next transaction starts on a container
    /// boundary. Never fails.
    pub fn drain(&mut self) {
        self.pending.clear();
        let mut reads = 0u32;
        while reads < self.cfg.drain_max_reads {
            match self.transport.bulk_in(self.cfg.drain_read, self.cfg.drain_timeout) {
                Ok(d) if !d.is_empty() => reads += 1,
                _ => break,
            }
        }
        if reads > 0 {
            log::debug!("drained {reads} stale IN transfers");
        }
    }

    fn write_container(&mut self, bytes: &[u8]) -> Result<(), PtpError> {
        self.transport.bulk_out(bytes, self.cfg.timeout)?;
        let mps = self.transport.bulk_out_max_packet().max(1);
        if self.cfg.send_zlp && !bytes.is_empty() && bytes.len().is_multiple_of(mps) {
            self.transport.bulk_out(&[], self.cfg.timeout)?;
        }
        Ok(())
    }

    /// One packet-aligned bulk IN read; on overflow (babble) clear the halt
    /// and retry once.
    fn read_aligned(&mut self, want: usize) -> Result<Vec<u8>, PtpError> {
        let req = round_up_to_packet(want, self.transport.bulk_in_max_packet());
        match self.transport.bulk_in(req, self.cfg.timeout) {
            Err(UsbError::Overflow) => {
                log::warn!("bulk IN overflow; clearing halt and retrying once");
                let _ = self.transport.clear_halt(Pipe::BulkIn);
                Ok(self.transport.bulk_in(req, self.cfg.timeout)?)
            }
            r => Ok(r?),
        }
    }

    fn header_ok(&self, h: &Header) -> bool {
        if (h.length as usize) < HEADER_LEN {
            return false;
        }
        match h.container_kind() {
            Some(ContainerKind::Data) => h.length == LENGTH_UNKNOWN || u64::from(h.length) - 12 <= self.cfg.max_data_len,
            Some(_) => h.length <= self.cfg.max_control_container,
            None => false,
        }
    }

    /// Read one complete container, streaming a
    /// data payload into `sink`.
    fn read_container(&mut self, sink: &mut dyn Write) -> Result<Container, PtpError> {
        let mut buf = std::mem::take(&mut self.pending);
        // Bytes at the front of `buf` that came from the carry-over.
        let mut seeded = buf.len();
        let mut empty_reads = 0u32;
        let header = loop {
            while buf.len() < HEADER_LEN {
                let chunk = self.read_aligned(self.cfg.transfer_size)?;
                if chunk.is_empty() {
                    empty_reads += 1;
                    if empty_reads >= self.cfg.empty_read_limit {
                        return Err(PtpError::Protocol("camera returned an empty PTP response".into()));
                    }
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            let Some(h) = Header::parse(&buf) else { continue };
            if self.header_ok(&h) {
                break h;
            }
            // Zero padding glued to the next container: strip and re-parse.
            match buf.iter().position(|&b| b != 0) {
                Some(0) => {}
                Some(i) => {
                    buf.drain(..i);
                    seeded = seeded.saturating_sub(i);
                    continue;
                }
                None => {
                    // All padding: counts as an empty read.
                    buf.clear();
                    seeded = 0;
                    empty_reads += 1;
                    if empty_reads >= self.cfg.empty_read_limit {
                        return Err(PtpError::Protocol("camera sent only zero padding".into()));
                    }
                    continue;
                }
            }
            if seeded > 0 {
                // The carry-over was device padding / junk, not a container:
                // drop it, keep what was freshly read after it.
                log::debug!("discarding {seeded} carry-over bytes that do not frame");
                buf.drain(..seeded.min(buf.len()));
                seeded = 0;
                continue;
            }
            return Err(PtpError::InvalidContainer(format!("invalid PTP container header: length {} type 0x{:04x}", h.length, h.kind)));
        };

        let is_data = header.container_kind() == Some(ContainerKind::Data);
        if !is_data {
            let len = header.length as usize;
            while buf.len() < len {
                let want = len - buf.len();
                let chunk = self.read_aligned(want)?;
                if chunk.is_empty() {
                    empty_reads += 1;
                    if empty_reads >= self.cfg.empty_read_limit {
                        return Err(PtpError::Protocol(format!("camera stopped mid-container ({} of {len} bytes)", buf.len())));
                    }
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            self.pending = buf.split_off(len);
            let small = buf.split_off(HEADER_LEN);
            return Ok(Container { header, small, streamed: 0 });
        }

        // Data phase: stream to the sink.
        let mut streamed: u64 = 0;
        if header.length == LENGTH_UNKNOWN {
            // >= 4 GiB: everything until a short packet is payload.
            let first = buf.get(HEADER_LEN..).unwrap_or(&[]);
            sink.write_all(first).map_err(sink_err)?;
            streamed += first.len() as u64;
            loop {
                let req = round_up_to_packet(self.cfg.transfer_size, self.transport.bulk_in_max_packet());
                let chunk = self.read_aligned(req)?;
                sink.write_all(&chunk).map_err(sink_err)?;
                streamed += chunk.len() as u64;
                if chunk.len() < req {
                    break;
                }
            }
            return Ok(Container { header, small: Vec::new(), streamed });
        }

        let total = u64::from(header.length) - HEADER_LEN as u64;
        let have = buf.len() - HEADER_LEN;
        let take = (have as u64).min(total) as usize;
        sink.write_all(&buf[HEADER_LEN..HEADER_LEN + take]).map_err(sink_err)?;
        streamed += take as u64;
        if have > take {
            self.pending = buf.split_off(HEADER_LEN + take);
        }
        drop(buf);
        while streamed < total {
            let remaining = total - streamed;
            let want = remaining.min(self.cfg.transfer_size as u64) as usize;
            let chunk = self.read_aligned(want)?;
            if chunk.is_empty() {
                empty_reads += 1;
                if empty_reads >= self.cfg.empty_read_limit {
                    return Err(PtpError::Protocol(format!("camera stopped mid-data-phase ({streamed} of {total} bytes)")));
                }
                continue;
            }
            let use_n = (chunk.len() as u64).min(remaining) as usize;
            sink.write_all(&chunk[..use_n]).map_err(sink_err)?;
            streamed += use_n as u64;
            if use_n < chunk.len() {
                // Overshoot: device padding or the response riding along.
                self.pending = chunk[use_n..].to_vec();
            }
        }
        Ok(Container { header, small: Vec::new(), streamed })
    }

    /// Run one transaction. `data_out` sends a data phase; an IN data phase,
    /// if the device sends one, is streamed into `sink`. The response code is
    /// returned unchecked (Sony's handshake legitimately answers 0xA101).
    ///
    /// On any read failure the IN pipe is drained before the error is
    /// returned, so the next transaction starts aligned. On error the sink may
    /// already hold (partial or stale) bytes and must be discarded by the caller
    /// — notably when a reply carries another transaction's id, which is a hard
    /// protocol error, never accepted as this operation's answer.
    pub fn transaction(&mut self, op: u16, params: &[u32], data_out: Option<&[u8]>, sink: &mut dyn Write) -> Result<TxnOutcome, PtpError> {
        let cmd = encode_command(op, 0, params)?;
        let txn = self.next_txn();
        let mut cmd = cmd;
        cmd[8..12].copy_from_slice(&txn.to_le_bytes());
        log::trace!("PTP txn={txn} op=0x{op:04x} params={params:x?} send={}B", data_out.map_or(0, <[u8]>::len));

        let write = (|| {
            self.write_container(&cmd)?;
            if let Some(d) = data_out {
                self.write_container(&encode_data(op, txn, d))?;
            }
            Ok::<(), PtpError>(())
        })();
        if let Err(e) = write {
            self.drain();
            return Err(e);
        }

        match self.read_reply(op, txn, sink) {
            Ok(o) => {
                log::trace!("PTP txn={txn} resp=0x{:04x} data={:?} pending={}B", o.response.code, o.data_len, self.pending.len());
                Ok(o)
            }
            Err(e) => {
                log::debug!("PTP txn={txn} op=0x{op:04x} failed: {e}; draining");
                self.drain();
                Err(e)
            }
        }
    }

    fn read_reply(&mut self, op: u16, txn: u32, sink: &mut dyn Write) -> Result<TxnOutcome, PtpError> {
        let mut data_len = None;
        let mut skipped = 0u32;
        loop {
            let c = self.read_container(sink)?;
            match c.header.container_kind() {
                Some(ContainerKind::Data) => {
                    if data_len.is_some() {
                        return Err(PtpError::Protocol(format!("two data phases for op 0x{op:04x}")));
                    }
                    // A data phase for another transaction is a stale reply to an
                    // earlier operation. Its bytes are already in the sink, so the
                    // sink is dirty: fail the transaction (the caller drains and
                    // discards) instead of accepting the wrong file. A mismatch of
                    // the operation code is the same desync.
                    if c.header.txn != txn || c.header.code != op {
                        return Err(PtpError::Protocol(format!(
                            "stale data phase for op 0x{op:04x}: got op 0x{:04x} txn {} (expected txn {txn}); sinked bytes must be discarded",
                            c.header.code, c.header.txn
                        )));
                    }
                    data_len = Some(c.streamed);
                }
                Some(ContainerKind::Response) => {
                    let mut bytes = Vec::with_capacity(HEADER_LEN + c.small.len());
                    bytes.extend_from_slice(&[0u8; HEADER_LEN]);
                    bytes[0..4].copy_from_slice(&c.header.length.to_le_bytes());
                    bytes[4..6].copy_from_slice(&c.header.kind.to_le_bytes());
                    bytes[6..8].copy_from_slice(&c.header.code.to_le_bytes());
                    bytes[8..12].copy_from_slice(&c.header.txn.to_le_bytes());
                    bytes.extend_from_slice(&c.small);
                    let response = parse_response(&bytes)?;
                    // A response for another transaction must not complete this
                    // one: every later reply would be off by one, permanently.
                    if response.txn != txn {
                        return Err(PtpError::Protocol(format!(
                            "stale response for op 0x{op:04x}: got txn {} (expected txn {txn}); session desynchronised",
                            response.txn
                        )));
                    }
                    return Ok(TxnOutcome { response, data_len });
                }
                Some(ContainerKind::Event) => {
                    skipped += 1;
                    log::debug!("skipping event 0x{:04x} on the bulk pipe", c.header.code);
                    if skipped > self.cfg.max_skipped_events {
                        return Err(PtpError::Protocol("too many events while waiting for a response".into()));
                    }
                }
                _ => {
                    return Err(PtpError::Protocol(format!("unexpected container type 0x{:04x} in reply to op 0x{op:04x}", c.header.kind)));
                }
            }
        }
    }

    /// Transaction collecting an IN data phase into a `Vec` (bounded by
    /// `limit` bytes; exceeding it is a sink error and the pipe is drained).
    pub fn transaction_vec(&mut self, op: u16, params: &[u32], data_out: Option<&[u8]>, limit: usize) -> Result<(Response, Vec<u8>), PtpError> {
        let mut sink = LimitedVec { buf: Vec::new(), limit };
        let o = self.transaction(op, params, data_out, &mut sink)?;
        Ok((o.response, sink.buf))
    }

    /// Read one event from the interrupt endpoint, waiting up to `timeout`.
    /// `Ok(None)` on timeout, when there is no interrupt endpoint, or when
    /// the bytes do not parse as an event (logged).
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, PtpError> {
        if !self.transport.has_interrupt_in() {
            return Ok(None);
        }
        match self.transport.interrupt_in(64, timeout) {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) => match parse_event(&bytes) {
                Ok(e) => Ok(Some(e)),
                Err(err) => {
                    log::debug!("ignoring malformed event ({} bytes): {err}", bytes.len());
                    Ok(None)
                }
            },
            Err(UsbError::Timeout) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// A `Vec` sink with a size cap.
#[derive(Debug, Default)]
pub struct LimitedVec {
    /// Collected bytes.
    pub buf: Vec<u8>,
    /// Maximum size.
    pub limit: usize,
}

impl Write for LimitedVec {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            return Err(std::io::Error::other(format!("data phase exceeds {} byte limit", self.limit)));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
