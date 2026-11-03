//! Traffic recording, for building replayable fixtures during hardware bring-up.
//!
//! [`RecordingTransport`] wraps any [`Transport`] and appends one line per
//! operation to a writer:
//!
//! ```text
//! out 0c000000010002100100000000
//! in 512 0c0000000300012001000000
//! in-err timeout
//! int 10000000040001c2...
//! clear-halt bulk-in
//! reset
//! ```
//!
//! [`parse_recording`] turns such a log back into a [`Step`] script for
//! [`crate::ScriptedTransport`]. Lines starting with `#` are comments. A payload may also be
//! written `z:LEN` (LEN zero bytes): fixtures built from real traffic replace image data
//! with it, keeping the transfer lengths the protocol depends on without the content.

use std::io::Write;
use std::time::Duration;

use crate::{Pipe, Step, Transport, UsbError};

/// A transport wrapper that logs every operation. Logging failures never
/// affect the I/O itself; the first one is kept in [`RecordingTransport::log_error`].
#[derive(Debug)]
pub struct RecordingTransport<T, W: Write> {
    inner: T,
    log: W,
    log_error: Option<String>,
    payload_limit: Option<usize>,
}

impl<T: Transport, W: Write> RecordingTransport<T, W> {
    /// Wrap `inner`, logging to `log`.
    pub fn new(inner: T, log: W) -> Self {
        Self { inner, log, log_error: None, payload_limit: None }
    }

    /// Truncate logged payloads to `limit` bytes (marking the cut with `..+N`).
    /// A truncated log is not replayable; use it only to keep logs of large
    /// downloads small.
    pub fn with_payload_limit(mut self, limit: usize) -> Self {
        self.payload_limit = Some(limit);
        self
    }

    /// Write a free-form comment line.
    pub fn comment(&mut self, text: &str) {
        let line = format!("# {}", text.replace('\n', " "));
        self.line(&line);
    }

    /// The first logging failure, if any.
    pub fn log_error(&self) -> Option<&str> {
        self.log_error.as_deref()
    }

    /// Unwrap into the inner transport and the log writer.
    pub fn into_parts(self) -> (T, W) {
        (self.inner, self.log)
    }

    /// Borrow the inner transport.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    fn line(&mut self, s: &str) {
        let r = writeln!(self.log, "{s}").and_then(|()| self.log.flush());
        if let Err(e) = r
            && self.log_error.is_none()
        {
            self.log_error = Some(e.to_string());
        }
    }

    fn hex(&self, data: &[u8]) -> String {
        let shown = match self.payload_limit {
            Some(l) if data.len() > l => &data[..l],
            _ => data,
        };
        let mut s = String::with_capacity(shown.len() * 2 + 16);
        for b in shown {
            s.push_str(&format!("{b:02x}"));
        }
        if shown.len() < data.len() {
            s.push_str(&format!("..+{}", data.len() - shown.len()));
        }
        if s.is_empty() {
            s.push('-');
        }
        s
    }
}

/// Stable token for an error in a traffic log.
fn error_token(e: &UsbError) -> String {
    match e {
        UsbError::Timeout => "timeout".into(),
        UsbError::Stall => "stall".into(),
        UsbError::Overflow => "overflow".into(),
        UsbError::Disconnected => "disconnected".into(),
        other => format!("io:{}", other.to_string().replace('\n', " ")),
    }
}

fn parse_error_token(s: &str) -> UsbError {
    match s {
        "timeout" => UsbError::Timeout,
        "stall" => UsbError::Stall,
        "overflow" => UsbError::Overflow,
        "disconnected" => UsbError::Disconnected,
        other => UsbError::Io(other.strip_prefix("io:").unwrap_or(other).to_string()),
    }
}

impl<T: Transport, W: Write> Transport for RecordingTransport<T, W> {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), UsbError> {
        let r = self.inner.bulk_out(data, timeout);
        let line = match &r {
            Ok(()) => format!("out {}", self.hex(data)),
            Err(e) => format!("out-err {} {}", error_token(e), self.hex(data)),
        };
        self.line(&line);
        r
    }

    fn bulk_out_counted(&mut self, data: &[u8], timeout: Duration) -> Result<usize, UsbError> {
        let r = self.inner.bulk_out_counted(data, timeout);
        // A partial (timed-out) write logs only the bytes that went out, so a
        // replay sees the same byte stream split at the same point.
        let line = match &r {
            Ok(n) if *n >= data.len() => format!("out {}", self.hex(data)),
            Ok(0) => format!("out-err timeout {}", self.hex(data)),
            Ok(n) => format!("# partial write {n} of {} bytes (timeout)\nout {}", data.len(), self.hex(data.get(..*n).unwrap_or(data))),
            Err(e) => format!("out-err {} {}", error_token(e), self.hex(data)),
        };
        self.line(&line);
        r
    }

    fn bulk_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let r = self.inner.bulk_in(max_len, timeout);
        let line = match &r {
            Ok(d) => format!("in {max_len} {}", self.hex(d)),
            Err(e) => format!("in-err {}", error_token(e)),
        };
        self.line(&line);
        r
    }

    fn interrupt_in(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let r = self.inner.interrupt_in(max_len, timeout);
        let line = match &r {
            Ok(d) => format!("int {}", self.hex(d)),
            Err(e) => format!("int-err {}", error_token(e)),
        };
        self.line(&line);
        r
    }

    fn has_interrupt_in(&self) -> bool {
        self.inner.has_interrupt_in()
    }

    fn bulk_in_max_packet(&self) -> usize {
        self.inner.bulk_in_max_packet()
    }

    fn bulk_out_max_packet(&self) -> usize {
        self.inner.bulk_out_max_packet()
    }

    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), UsbError> {
        let r = self.inner.clear_halt(pipe);
        self.line(&format!("clear-halt {}", pipe.name()));
        r
    }

    fn reset(&mut self) -> Result<(), UsbError> {
        let r = self.inner.reset();
        self.line("reset");
        r
    }
}

/// Largest payload a single `z:LEN` token may expand to (64 MiB).
pub const MAX_ZERO_RUN: usize = 64 << 20;
/// Largest total of `z:LEN` expansions one log may ask for (512 MiB), so a hostile log of
/// many maximal runs is an error rather than an out-of-memory abort.
pub const MAX_ZERO_TOTAL: usize = 512 << 20;

/// A payload field of a log line.
enum Payload {
    /// Every byte is known.
    Exact(Vec<u8>),
    /// Recorded with a payload limit: `prefix` was shown, `len` is the full length.
    Truncated { prefix: Vec<u8>, len: usize },
}

fn decode_hex(s: &str, line_no: usize) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("line {line_no}: odd-length hex"));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| format!("line {line_no}: bad hex"))?;
        out.push(u8::from_str_radix(text, 16).map_err(|_| format!("line {line_no}: bad hex {text:?}"))?);
    }
    Ok(out)
}

/// Parse a payload field: `-` (empty), `z:LEN` (LEN zero bytes), hex, or truncated hex
/// `HEX..+N`. `zero_budget` is what is left of [`MAX_ZERO_TOTAL`].
fn parse_payload(s: &str, line_no: usize, max_run: usize, zero_budget: &mut usize) -> Result<Payload, String> {
    if s == "-" {
        return Ok(Payload::Exact(Vec::new()));
    }
    if let Some(len) = s.strip_prefix("z:") {
        // Digits only: no sign, no whitespace, no exponent; parse failure covers overflow.
        if len.is_empty() || !len.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("line {line_no}: bad zero-run length {len:?}"));
        }
        let n: usize = len.parse().map_err(|_| format!("line {line_no}: zero-run length {len:?} out of range"))?;
        if n > max_run {
            return Err(format!("line {line_no}: zero run of {n} bytes exceeds the {max_run}-byte limit"));
        }
        if n > *zero_budget {
            return Err(format!("line {line_no}: zero runs exceed the total limit"));
        }
        *zero_budget -= n;
        return Ok(Payload::Exact(vec![0; n]));
    }
    if let Some((shown, rest)) = s.split_once("..+") {
        let n: usize = match rest.bytes().all(|b| b.is_ascii_digit()) {
            true => rest.parse().map_err(|_| format!("line {line_no}: bad truncation count {rest:?}"))?,
            false => return Err(format!("line {line_no}: bad truncation count {rest:?}")),
        };
        let prefix = decode_hex(shown, line_no)?;
        let len = prefix.len().checked_add(n).ok_or_else(|| format!("line {line_no}: truncated length overflows"))?;
        return Ok(Payload::Truncated { prefix, len });
    }
    decode_hex(s, line_no).map(Payload::Exact)
}

/// A device payload must be exact: a replay has to hand the host real bytes.
fn exact(p: Payload, line_no: usize) -> Result<Vec<u8>, String> {
    match p {
        Payload::Exact(b) => Ok(b),
        Payload::Truncated { .. } => Err(format!(
            "line {line_no}: device payload was truncated when recorded; not replayable (scrub it to z:LEN if its content does not matter)"
        )),
    }
}

/// Parse a traffic log written by [`RecordingTransport`] into a replay script.
///
/// Host writes become [`Step::Out`] (asserted on replay), device replies
/// [`Step::In`] / [`Step::InErr`] / [`Step::Interrupt`]. Interrupt timeouts,
/// halts and resets carry no device data and are skipped.
///
/// Payload fields are hex, `-` for an empty transfer, or `z:LEN` for LEN zero bytes (how
/// scrubbed fixtures replace image data; LEN ≤ [`MAX_ZERO_RUN`], all runs together ≤
/// [`MAX_ZERO_TOTAL`]). A host write recorded truncated (`HEX..+N`) becomes a
/// [`Step::OutPrefix`]: its length and shown bytes are checked, the cut-off tail is not. A
/// truncated device payload is an error.
pub fn parse_recording(text: &str) -> Result<Vec<Step>, String> {
    parse_with_limits(text, MAX_ZERO_RUN, MAX_ZERO_TOTAL)
}

fn parse_with_limits(text: &str, max_run: usize, max_total: usize) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let mut zero_budget = max_total;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let op = parts.next().unwrap_or("");
        match op {
            "out" => steps.push(match parse_payload(parts.next().unwrap_or("-"), line_no, max_run, &mut zero_budget)? {
                Payload::Exact(b) => Step::Out(b),
                Payload::Truncated { prefix, len } => Step::OutPrefix { prefix, len },
            }),
            "out-err" => {
                let tok = parts.next().unwrap_or("io:unknown");
                steps.push(Step::OutErr(parse_error_token(tok)));
            }
            "in" => {
                let _requested = parts.next();
                steps.push(Step::In(exact(parse_payload(parts.next().unwrap_or("-"), line_no, max_run, &mut zero_budget)?, line_no)?));
            }
            "in-err" => steps.push(Step::InErr(parse_error_token(parts.next().unwrap_or("timeout")))),
            "int" => steps.push(Step::Interrupt(exact(parse_payload(parts.next().unwrap_or("-"), line_no, max_run, &mut zero_budget)?, line_no)?)),
            "int-err" | "clear-halt" | "reset" => {}
            other => return Err(format!("line {line_no}: unknown op {other:?}")),
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedTransport;

    const T: Duration = Duration::from_millis(10);

    #[test]
    fn record_then_replay_reproduces_the_exchange() {
        let device = ScriptedTransport::new([Step::Out(vec![0xAA, 0xBB]), Step::In(vec![1, 2, 3]), Step::Interrupt(vec![9, 9])]);
        let mut rec = RecordingTransport::new(device, Vec::<u8>::new());
        rec.comment("handshake");
        rec.bulk_out(&[0xAA, 0xBB], T).unwrap();
        assert_eq!(rec.bulk_in(512, T).unwrap(), vec![1, 2, 3]);
        assert_eq!(rec.bulk_in(512, T), Err(UsbError::Timeout));
        assert_eq!(rec.interrupt_in(64, T).unwrap(), vec![9, 9]);
        rec.bulk_out(&[], T).unwrap_err(); // unexpected ZLP: mismatch, still logged
        rec.clear_halt(Pipe::BulkIn).unwrap();
        let (_dev, log) = rec.into_parts();
        let text = String::from_utf8(log).unwrap();
        assert!(text.contains("out aabb"), "{text}");
        assert!(text.contains("in 512 010203"));
        assert!(text.contains("in-err timeout"));

        let steps = parse_recording(&text).unwrap();
        assert_eq!(steps[0], Step::Out(vec![0xAA, 0xBB]));
        assert_eq!(steps[1], Step::In(vec![1, 2, 3]));
        assert_eq!(steps[2], Step::InErr(UsbError::Timeout));
        assert_eq!(steps[3], Step::Interrupt(vec![9, 9]));

        let mut replay = ScriptedTransport::new(steps[..4].to_vec());
        replay.bulk_out(&[0xAA, 0xBB], T).unwrap();
        assert_eq!(replay.bulk_in(512, T).unwrap(), vec![1, 2, 3]);
        assert_eq!(replay.bulk_in(512, T), Err(UsbError::Timeout));
        assert_eq!(replay.interrupt_in(64, T).unwrap(), vec![9, 9]);
        replay.verify().unwrap();
    }

    #[test]
    fn truncated_and_garbage_logs_are_rejected_not_panicking() {
        assert!(parse_recording("in 512 0102..+99").is_err());
        assert!(parse_recording("out 0").is_err());
        assert!(parse_recording("out zz").is_err());
        assert!(parse_recording("frobnicate").is_err());
        assert!(parse_recording("out é1").is_err());
        assert_eq!(parse_recording("# c\n\nreset\n").unwrap(), vec![]);
    }

    #[test]
    fn zero_runs_expand_to_zero_bytes() {
        let steps = parse_recording("out 1c47\nin 131072 z:5\nin 512 z:0\nint z:2\nout z:3").unwrap();
        assert_eq!(
            steps,
            vec![Step::Out(vec![0x1c, 0x47]), Step::In(vec![0; 5]), Step::In(vec![]), Step::Interrupt(vec![0, 0]), Step::Out(vec![0; 3])]
        );
        // A zero run replays like the recorded bytes would have.
        let mut t = ScriptedTransport::new(parse_recording("in 4096 z:1300").unwrap());
        assert_eq!(t.bulk_in(4096, T).unwrap(), vec![0; 1300]);
        t.verify().unwrap();
        // The cap itself is allowed.
        assert_eq!(parse_recording(&format!("in 512 z:{MAX_ZERO_RUN}")).unwrap(), vec![Step::In(vec![0; MAX_ZERO_RUN])]);
    }

    #[test]
    fn hostile_zero_runs_are_errors_not_allocations() {
        for bad in [
            "in 512 z:".to_string(),
            "in 512 z:-1".into(),
            "in 512 z:+4".into(),
            "in 512 z:1e9".into(),
            "in 512 z:0x10".into(),
            "in 512 z:12ab".into(),
            "in 512 z:18446744073709551616".into(),
            "in 512 z:99999999999999999999999999".into(),
            format!("in 512 z:{}", usize::MAX),
            format!("in 512 z:{}", MAX_ZERO_RUN + 1),
        ] {
            assert!(parse_recording(&bad).is_err(), "{bad}");
        }
        // Many in-limit runs cannot add up to an out-of-memory either: the total is capped
        // before anything past it is allocated (small limits here, same code path).
        let many = "in 512 z:1000\n".repeat(9);
        assert_eq!(parse_with_limits(&many, 1000, 9000).unwrap().len(), 9);
        let e = parse_with_limits(&format!("{many}in 512 z:1"), 1000, 9000).unwrap_err();
        assert!(e.contains("line 10") && e.contains("total"), "{e}");
        const { assert!(MAX_ZERO_RUN < MAX_ZERO_TOTAL) };
    }

    #[test]
    fn truncated_writes_replay_as_prefix_checks() {
        assert_eq!(parse_recording("out 7200..+2").unwrap(), vec![Step::OutPrefix { prefix: vec![0x72, 0], len: 4 }]);
        let mut t = ScriptedTransport::new(parse_recording("out 7200..+2").unwrap());
        t.bulk_out(&[0x72, 0, 1, 2], T).unwrap();
        t.verify().unwrap();
        // Hostile truncation markers.
        for bad in ["out 72..+", "out 72..+x", "out 72..+-1", "out 7..+1", &format!("out 72..+{}", usize::MAX)] {
            assert!(parse_recording(bad).is_err(), "{bad}");
        }
        // Device payloads must be exact.
        assert!(parse_recording("int 0102..+1").is_err());
    }

    #[test]
    fn counted_writes_are_logged() {
        let device = ScriptedTransport::new([Step::Out(vec![1, 2])]);
        let mut rec = RecordingTransport::new(device, Vec::<u8>::new());
        assert_eq!(rec.bulk_out_counted(&[1, 2], T), Ok(2));
        let (_d, log) = rec.into_parts();
        let text = String::from_utf8(log).unwrap();
        assert!(text.contains("out 0102"), "{text}");
        assert_eq!(parse_recording(&text).unwrap(), vec![Step::Out(vec![1, 2])]);
    }

    #[test]
    fn payload_limit_truncates() {
        let device = ScriptedTransport::new([Step::In(vec![0u8; 100])]);
        let mut rec = RecordingTransport::new(device, Vec::<u8>::new()).with_payload_limit(4);
        rec.bulk_in(512, T).unwrap();
        let (_d, log) = rec.into_parts();
        assert!(String::from_utf8(log).unwrap().contains("00000000..+96"));
    }
}
