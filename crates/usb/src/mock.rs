//! A scripted fake [`Transport`] for tests: expected writes are asserted
//! byte for byte, reads are served from canned device replies.
//!
//! The script is a queue of [`Step`]s consumed in order. A read when the next
//! step is not a read behaves like a real idle device: it times out without
//! consuming anything (that is what lets "drain the pipe" logic terminate). A
//! write when the next step is not the matching expected write is recorded as
//! a mismatch and fails with [`UsbError::Script`]; [`ScriptedTransport::verify`]
//! reports it afterwards.
//!
//! [`Step::In`] models one device transfer that ends on a short packet. When
//! the host requests fewer bytes than the step holds (the request is rounded up
//! to the max packet size first), the remainder stays queued for the next read,
//! exactly like bytes left in the device's FIFO.
//!
//! The transport is a cheap handle over shared state (`Clone`): keep one clone
//! in the test to inspect the traffic after the code under test has consumed
//! (or dropped) the other.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::{Pipe, Transport, UsbError, round_up_to_packet};

/// One scripted exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The host must write exactly these bytes to bulk OUT.
    Out(Vec<u8>),
    /// The host writes something to bulk OUT; the bytes are not checked.
    OutAny,
    /// The host must write exactly `len` bytes starting with `prefix`; the rest is not
    /// checked. Replays of logs whose large writes were recorded truncated (`HEX..+N`).
    OutPrefix {
        /// The bytes that were recorded.
        prefix: Vec<u8>,
        /// Total length of the write.
        len: usize,
    },
    /// The device answers the next bulk IN read with these bytes.
    In(Vec<u8>),
    /// The next bulk IN read fails with this error.
    InErr(UsbError),
    /// The device answers the next interrupt IN read with these bytes.
    Interrupt(Vec<u8>),
    /// The next bulk OUT write fails with this error.
    OutErr(UsbError),
}

/// What happened on the fake wire, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEvent {
    /// Host wrote bytes to bulk OUT.
    Out(Vec<u8>),
    /// Host read bytes from bulk IN (requested size, bytes returned).
    In { requested: usize, data: Vec<u8> },
    /// A bulk IN read failed.
    InErr(UsbError),
    /// Host read from interrupt IN.
    Interrupt(Vec<u8>),
    /// An interrupt IN read timed out.
    InterruptTimeout,
    /// Host cleared a halt.
    ClearHalt(Pipe),
    /// Host reset the port.
    Reset,
}

#[derive(Debug, Default)]
struct State {
    script: VecDeque<Step>,
    events: Vec<MockEvent>,
    mismatches: Vec<String>,
    reset_count: usize,
}

/// Scripted fake transport. See the module docs.
#[derive(Debug, Clone)]
pub struct ScriptedTransport {
    state: Arc<Mutex<State>>,
    in_packet: usize,
    out_packet: usize,
    has_interrupt: bool,
    fail_reset: bool,
}

impl ScriptedTransport {
    /// A fake with 512-byte bulk packets (USB 2 high speed, like the ILCE-1 on
    /// this machine's hub chain) and an interrupt endpoint.
    pub fn new(script: impl IntoIterator<Item = Step>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State { script: script.into_iter().collect(), ..State::default() })),
            in_packet: 512,
            out_packet: 512,
            has_interrupt: true,
            fail_reset: false,
        }
    }

    /// Set both bulk max packet sizes (0 is treated as 1).
    pub fn with_packet_size(mut self, packet: usize) -> Self {
        self.in_packet = packet.max(1);
        self.out_packet = packet.max(1);
        self
    }

    /// Present (or hide) the interrupt endpoint.
    pub fn with_interrupt(mut self, present: bool) -> Self {
        self.has_interrupt = present;
        self
    }

    /// Make `reset()` fail.
    pub fn with_failing_reset(mut self) -> Self {
        self.fail_reset = true;
        self
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, UsbError> {
        self.state.lock().map_err(|_| UsbError::Script("mock state poisoned".into()))
    }

    /// Append more steps to the script.
    pub fn push(&self, steps: impl IntoIterator<Item = Step>) {
        if let Ok(mut s) = self.lock() {
            s.script.extend(steps);
        }
    }

    /// Everything that crossed the fake wire so far.
    pub fn events(&self) -> Vec<MockEvent> {
        self.lock().map(|s| s.events.clone()).unwrap_or_default()
    }

    /// All bytes the host wrote, one entry per write.
    pub fn writes(&self) -> Vec<Vec<u8>> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                MockEvent::Out(b) => Some(b),
                _ => None,
            })
            .collect()
    }

    /// Steps not yet consumed.
    pub fn remaining(&self) -> Vec<Step> {
        self.lock().map(|s| s.script.iter().cloned().collect()).unwrap_or_default()
    }

    /// How many port resets were requested.
    pub fn reset_count(&self) -> usize {
        self.lock().map(|s| s.reset_count).unwrap_or(0)
    }

    /// `Ok` when every step was consumed and no write mismatched.
    pub fn verify(&self) -> Result<(), String> {
        let s = self.lock().map_err(|e| e.to_string())?;
        if !s.mismatches.is_empty() {
            return Err(s.mismatches.join("\n"));
        }
        if !s.script.is_empty() {
            return Err(format!("{} scripted steps not consumed: {:?}", s.script.len(), s.script));
        }
        Ok(())
    }
}

fn hex_preview(b: &[u8]) -> String {
    let mut out = String::new();
    for byte in b.iter().take(48) {
        out.push_str(&format!("{byte:02x}"));
    }
    if b.len() > 48 {
        out.push_str(&format!("..(+{} B)", b.len() - 48));
    }
    out
}

impl Transport for ScriptedTransport {
    fn bulk_out(&mut self, data: &[u8], _timeout: Duration) -> Result<(), UsbError> {
        let mut s = self.lock()?;
        s.events.push(MockEvent::Out(data.to_vec()));
        match s.script.front().cloned() {
            Some(Step::Out(expected)) => {
                s.script.pop_front();
                if expected == data {
                    Ok(())
                } else {
                    let msg = format!("write mismatch: expected {} got {}", hex_preview(&expected), hex_preview(data));
                    s.mismatches.push(msg.clone());
                    Err(UsbError::Script(msg))
                }
            }
            Some(Step::OutAny) => {
                s.script.pop_front();
                Ok(())
            }
            Some(Step::OutPrefix { prefix, len }) => {
                s.script.pop_front();
                if data.len() == len && data.starts_with(&prefix) {
                    Ok(())
                } else {
                    let msg = format!(
                        "write mismatch: expected {len} bytes starting {} got {} bytes {}",
                        hex_preview(&prefix),
                        data.len(),
                        hex_preview(data)
                    );
                    s.mismatches.push(msg.clone());
                    Err(UsbError::Script(msg))
                }
            }
            Some(Step::OutErr(e)) => {
                s.script.pop_front();
                Err(e)
            }
            other => {
                let msg = format!("unexpected write {} (next step {:?})", hex_preview(data), other);
                s.mismatches.push(msg.clone());
                Err(UsbError::Script(msg))
            }
        }
    }

    fn bulk_in(&mut self, max_len: usize, _timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let requested = round_up_to_packet(max_len, self.in_packet);
        let mut s = self.lock()?;
        match s.script.front_mut() {
            Some(Step::In(data)) => {
                let out = if data.len() > requested {
                    let rest = data.split_off(requested);
                    std::mem::replace(data, rest)
                } else {
                    let d = std::mem::take(data);
                    s.script.pop_front();
                    d
                };
                s.events.push(MockEvent::In { requested, data: out.clone() });
                Ok(out)
            }
            Some(Step::InErr(_)) => {
                let err = match s.script.pop_front() {
                    Some(Step::InErr(e)) => e,
                    _ => UsbError::Timeout,
                };
                s.events.push(MockEvent::InErr(err.clone()));
                Err(err)
            }
            _ => {
                s.events.push(MockEvent::InErr(UsbError::Timeout));
                Err(UsbError::Timeout)
            }
        }
    }

    fn interrupt_in(&mut self, max_len: usize, _timeout: Duration) -> Result<Vec<u8>, UsbError> {
        if !self.has_interrupt {
            return Err(UsbError::NotFound("no interrupt endpoint".into()));
        }
        let mut s = self.lock()?;
        if let Some(Step::Interrupt(data)) = s.script.front() {
            let mut data = data.clone();
            s.script.pop_front();
            data.truncate(max_len.max(1));
            s.events.push(MockEvent::Interrupt(data.clone()));
            return Ok(data);
        }
        s.events.push(MockEvent::InterruptTimeout);
        Err(UsbError::Timeout)
    }

    fn has_interrupt_in(&self) -> bool {
        self.has_interrupt
    }

    fn bulk_in_max_packet(&self) -> usize {
        self.in_packet
    }

    fn bulk_out_max_packet(&self) -> usize {
        self.out_packet
    }

    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), UsbError> {
        self.lock()?.events.push(MockEvent::ClearHalt(pipe));
        Ok(())
    }

    fn reset(&mut self) -> Result<(), UsbError> {
        let mut s = self.lock()?;
        s.events.push(MockEvent::Reset);
        s.reset_count += 1;
        if self.fail_reset { Err(UsbError::Io("reset failed".into())) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_millis(10);

    #[test]
    fn writes_are_asserted() {
        let mut t = ScriptedTransport::new([Step::Out(vec![1, 2, 3])]);
        t.bulk_out(&[1, 2, 3], T).unwrap();
        t.verify().unwrap();

        let mut t = ScriptedTransport::new([Step::Out(vec![1, 2, 3])]);
        assert!(matches!(t.bulk_out(&[9], T), Err(UsbError::Script(_))));
        assert!(t.verify().is_err());
    }

    #[test]
    fn prefix_writes_check_length_and_prefix() {
        let mut t = ScriptedTransport::new([Step::OutPrefix { prefix: vec![1, 2], len: 3 }]);
        t.bulk_out(&[1, 2, 9], T).unwrap();
        t.verify().unwrap();
        for bad in [&[1u8, 2][..], &[1, 2, 3, 4], &[1, 3, 3]] {
            let mut t = ScriptedTransport::new([Step::OutPrefix { prefix: vec![1, 2], len: 3 }]);
            assert!(matches!(t.bulk_out(bad, T), Err(UsbError::Script(_))), "{bad:?}");
            assert!(t.verify().is_err());
        }
    }

    #[test]
    fn idle_reads_time_out_without_consuming() {
        let mut t = ScriptedTransport::new([Step::Out(vec![1])]);
        assert_eq!(t.bulk_in(512, T), Err(UsbError::Timeout));
        assert_eq!(t.remaining().len(), 1);
    }

    #[test]
    fn long_replies_are_split_at_the_packet_aligned_request() {
        let mut t = ScriptedTransport::new([Step::In(vec![7; 1300])]).with_packet_size(512);
        // 100 rounds up to 512.
        assert_eq!(t.bulk_in(100, T).unwrap().len(), 512);
        assert_eq!(t.bulk_in(4096, T).unwrap().len(), 788);
        assert_eq!(t.bulk_in(4096, T), Err(UsbError::Timeout));
        t.verify().unwrap();
    }

    #[test]
    fn errors_and_interrupts_replay() {
        let mut t = ScriptedTransport::new([Step::InErr(UsbError::Stall), Step::Interrupt(vec![1, 2, 3, 4])]);
        assert_eq!(t.interrupt_in(64, T), Err(UsbError::Timeout));
        assert_eq!(t.bulk_in(64, T), Err(UsbError::Stall));
        assert_eq!(t.interrupt_in(2, T).unwrap(), vec![1, 2]);
        t.clear_halt(Pipe::BulkIn).unwrap();
        t.reset().unwrap();
        assert_eq!(t.reset_count(), 1);
        t.verify().unwrap();
    }

    #[test]
    fn clones_share_state() {
        let t = ScriptedTransport::new([Step::OutAny]);
        let mut moved = t.clone();
        moved.bulk_out(&[5], T).unwrap();
        drop(moved);
        assert_eq!(t.writes(), vec![vec![5]]);
    }
}
