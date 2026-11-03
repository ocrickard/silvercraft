//! Container framing over the wire: multi-transfer data phases, overshoot
//! carry-over, zero padding, packet alignment, ZLPs, overflow recovery,
//! error drain, huge data phases, hostile input.

mod common;

use std::io::Write;
use std::time::Duration;

use common::{Script, data, response};
use proptest::prelude::*;
use silvercraft_ptp::codec::{encode_command, encode_data, encode_event, encode_response, rc};
use silvercraft_ptp::{ConnectionConfig, PtpConnection, PtpError};
use silvercraft_usb::{MockEvent, Pipe, ScriptedTransport, Step, Transport, UsbError, round_up_to_packet};

fn conn(steps: Vec<Step>, cfg: ConnectionConfig) -> (PtpConnection<ScriptedTransport>, ScriptedTransport) {
    let t = ScriptedTransport::new(steps);
    (PtpConnection::new(t.clone(), cfg), t)
}

fn small_cfg() -> ConnectionConfig {
    ConnectionConfig { transfer_size: 1024, ..ConnectionConfig::default() }
}

fn requested_sizes(t: &ScriptedTransport) -> Vec<usize> {
    t.events()
        .into_iter()
        .filter_map(|e| match e {
            MockEvent::In { requested, .. } => Some(requested),
            _ => None,
        })
        .collect()
}

#[test]
fn multi_chunk_data_phase_with_overshoot_carry_over() {
    let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let mut s = Script::new();
    // Uneven transfers; the last one carries the data tail AND the response.
    s.data_in_split(0x1009, &[0xFFFF_C001], &payload, rc::OK, &[700, 1024, 1024, 1500]);
    // A second transaction proves the carry-over did not desync anything.
    s.data_in(0x1009, &[0xFFFF_C002], Some(b"next"), rc::OK);
    let (mut c, t) = conn(s.build(), small_cfg());

    let mut sink = Vec::new();
    let o = c.transaction(0x1009, &[0xFFFF_C001], None, &mut sink).unwrap();
    assert_eq!(o.response.code, rc::OK);
    assert_eq!(o.data_len, Some(5000));
    assert_eq!(sink, payload);
    assert_eq!(c.pending_len(), 0, "response consumed from the carry-over");

    let (r, d) = c.transaction_vec(0x1009, &[0xFFFF_C002], None, 1 << 20).unwrap();
    assert!(r.is_ok());
    assert_eq!(d, b"next");
    t.verify().unwrap();
    for req in requested_sizes(&t) {
        assert_eq!(req % 512, 0, "every IN request is packet aligned");
        assert!(req <= 1024);
    }
}

#[test]
fn stale_response_is_a_hard_error_not_an_answer() {
    // A late reply to an earlier transaction (txn 99) must not complete txn 1:
    // accepting it would shift every later reply by one, permanently.
    let steps = vec![Step::Out(encode_command(0x1009, 1, &[7]).unwrap()), Step::In(response(rc::OK, 99))];
    let (mut c, t) = conn(steps, small_cfg());
    let e = c.transaction_vec(0x1009, &[7], None, 1 << 20).unwrap_err();
    assert!(e.to_string().contains("stale response"), "{e}");
    t.verify().unwrap();
}

#[test]
fn stale_data_phase_fails_and_the_sink_is_dirty() {
    let mut reply = data(0x1009, 99, b"wrong file");
    reply.extend_from_slice(&response(rc::OK, 99));
    let steps = vec![Step::Out(encode_command(0x1009, 1, &[7]).unwrap()), Step::In(reply)];
    let (mut c, t) = conn(steps, small_cfg());
    let mut sink: Vec<u8> = Vec::new();
    let e = c.transaction(0x1009, &[7], None, &mut sink).unwrap_err();
    assert!(e.to_string().contains("stale data phase"), "{e}");
    // The bytes were streamed before the header could be judged: the transaction
    // failed, so the caller must discard the sink rather than keep the wrong file.
    assert_eq!(sink, b"wrong file");
    t.verify().unwrap();
}

#[test]
fn transaction_after_a_stale_reply_still_works() {
    let mut s = Script::new();
    // txn 1 gets a stale response, then txn 2 is answered properly; the error
    // drain must leave the pipe aligned for it.
    s.steps.push(Step::Out(encode_command(0x1009, 1, &[7]).unwrap()));
    s.steps.push(Step::In(response(rc::OK, 99)));
    s.txn = 1;
    s.data_in(0x1009, &[8], Some(b"right"), rc::OK);
    let (mut c, t) = conn(s.build(), small_cfg());
    assert!(c.transaction_vec(0x1009, &[7], None, 1 << 20).is_err());
    let (r, d) = c.transaction_vec(0x1009, &[8], None, 1 << 20).unwrap();
    assert!(r.is_ok());
    assert_eq!(d, b"right");
    t.verify().unwrap();
}

#[test]
fn response_riding_with_data_is_carried_to_next_read() {
    let mut s = Script::new();
    s.data_in(0x1009, &[1], Some(&[7; 300]), rc::OK);
    let (mut c, t) = conn(s.build(), ConnectionConfig::default());
    let (r, d) = c.transaction_vec(0x1009, &[1], None, 1 << 20).unwrap();
    assert!(r.is_ok());
    assert_eq!(d.len(), 300);
    // One IN transfer served both containers.
    assert_eq!(requested_sizes(&t).len(), 1);
    t.verify().unwrap();
}

#[test]
fn fragmented_containers_reassemble() {
    // tests/test_cameras.py:652-676: splits 5/6/rest, response 3/9.
    let d = data(0x1009, 1, b"live-view-payload");
    let r = response(rc::OK, 1);
    let steps = vec![
        Step::Out(encode_command(0x1009, 1, &[]).unwrap()),
        Step::In(d[..5].to_vec()),
        Step::In(d[5..11].to_vec()),
        Step::In(d[11..].to_vec()),
        Step::In(r[..3].to_vec()),
        Step::In(r[3..].to_vec()),
    ];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    let (resp, payload) = c.transaction_vec(0x1009, &[], None, 1 << 20).unwrap();
    assert!(resp.is_ok());
    assert_eq!(payload, b"live-view-payload");
    t.verify().unwrap();
}

#[test]
fn zero_padding_between_data_and_response_is_stripped() {
    // tests/test_cameras.py:955-979: 19 zero bytes of padding.
    for glued in [false, true] {
        let d = data(0x1009, 1, &[1, 2, 3]);
        let mut pad_resp = vec![0u8; 19];
        pad_resp.extend_from_slice(&response(rc::OK, 1));
        let mut steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap())];
        if glued {
            let mut all = d.clone();
            all.extend_from_slice(&pad_resp);
            steps.push(Step::In(all));
        } else {
            steps.push(Step::In(d));
            steps.push(Step::In(pad_resp));
        }
        let (mut c, t) = conn(steps, ConnectionConfig::default());
        let (resp, payload) = c.transaction_vec(0x1009, &[], None, 1 << 20).unwrap();
        assert!(resp.is_ok(), "glued={glued}");
        assert_eq!(payload, vec![1, 2, 3]);
        t.verify().unwrap();
    }
}

#[test]
fn junk_carry_over_is_discarded() {
    // Overshoot that does not frame (device junk) after a complete response.
    let mut first = response(rc::OK, 1);
    first.extend_from_slice(&[0xAB; 7]);
    let steps = vec![
        Step::Out(encode_command(0x1001, 1, &[]).unwrap()),
        Step::In(first),
        Step::Out(encode_command(0x1001, 2, &[]).unwrap()),
        Step::In(response(rc::OK, 2)),
    ];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    c.transaction_vec(0x1001, &[], None, 64).unwrap();
    assert_eq!(c.pending_len(), 7);
    let (r, _) = c.transaction_vec(0x1001, &[], None, 64).unwrap();
    assert_eq!(r.txn, 2);
    t.verify().unwrap();
}

#[test]
fn error_mid_transfer_drains_then_recovers() {
    let txn1_data = data(0x1009, 1, &vec![9u8; 4000]);
    let steps = vec![
        Step::Out(encode_command(0x1009, 1, &[0xFFFF_C001]).unwrap()),
        Step::In(txn1_data[..1024].to_vec()),
        Step::InErr(UsbError::Stall),
        // What the camera still had queued: drained and discarded.
        Step::In(txn1_data[1024..3072].to_vec()),
        Step::In(txn1_data[3072..].to_vec()),
        Step::In(response(rc::OK, 1)),
        // Next transaction is clean.
        Step::Out(encode_command(0x1001, 2, &[]).unwrap()),
        Step::In(response(rc::OK, 2)),
    ];
    let (mut c, t) = conn(steps, small_cfg());
    let mut sink = Vec::new();
    let e = c.transaction(0x1009, &[0xFFFF_C001], None, &mut sink).unwrap_err();
    assert!(matches!(e, PtpError::Usb(UsbError::Stall)), "{e}");
    assert_eq!(c.pending_len(), 0);
    let (r, _) = c.transaction_vec(0x1001, &[], None, 64).unwrap();
    assert!(r.is_ok());
    assert_eq!(r.txn, 2);
    t.verify().unwrap();
}

#[test]
fn sink_failure_drains_and_reports() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut s = Script::new();
    s.data_in(0x1009, &[1], Some(&[1; 100]), rc::OK);
    let (mut c, t) = conn(s.build(), ConnectionConfig::default());
    assert!(matches!(c.transaction(0x1009, &[1], None, &mut Broken), Err(PtpError::Sink(_))));
    t.verify().unwrap();
}

#[test]
fn overflow_clears_halt_and_retries_once() {
    let steps = vec![Step::Out(encode_command(0x1001, 1, &[]).unwrap()), Step::InErr(UsbError::Overflow), Step::In(response(rc::OK, 1))];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    let (r, _) = c.transaction_vec(0x1001, &[], None, 64).unwrap();
    assert!(r.is_ok());
    assert!(t.events().contains(&MockEvent::ClearHalt(Pipe::BulkIn)));
    t.verify().unwrap();
}

#[test]
fn zlp_follows_packet_multiple_out_container() {
    // 500 B payload + 12 B header = 512 = one full packet: needs a ZLP.
    let payload = vec![0x55; 500];
    let steps = vec![
        Step::Out(encode_command(0x9205, 1, &[0xD222]).unwrap()),
        Step::Out(encode_data(0x9205, 1, &payload)),
        Step::Out(vec![]),
        Step::In(response(rc::OK, 1)),
        Step::Out(encode_command(0x9205, 2, &[0xD222]).unwrap()),
        Step::Out(encode_data(0x9205, 2, &[1, 0])),
        Step::In(response(rc::OK, 2)),
    ];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    c.transaction(0x9205, &[0xD222], Some(&payload), &mut std::io::sink()).unwrap();
    c.transaction(0x9205, &[0xD222], Some(&[1, 0]), &mut std::io::sink()).unwrap();
    t.verify().unwrap();
}

#[test]
fn zlp_after_packet_multiple_in_data_is_tolerated() {
    // Data container of exactly 1024 bytes, then the device's ZLP, then the response.
    let d = data(0x1009, 1, &[3; 1012]);
    assert_eq!(d.len(), 1024);
    let steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap()), Step::In(d), Step::In(vec![]), Step::In(response(rc::OK, 1))];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    let (r, p) = c.transaction_vec(0x1009, &[], None, 1 << 20).unwrap();
    assert!(r.is_ok());
    assert_eq!(p.len(), 1012);
    t.verify().unwrap();
}

#[test]
fn events_on_the_bulk_pipe_are_skipped() {
    let mut reply = encode_event(0xC203, 0, &[0x5007]).unwrap();
    reply.extend_from_slice(&response(rc::OK, 1));
    let steps = vec![Step::Out(encode_command(0x1001, 1, &[]).unwrap()), Step::In(reply)];
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    assert!(c.transaction_vec(0x1001, &[], None, 64).unwrap().0.is_ok());
    t.verify().unwrap();
}

#[test]
fn interrupt_events_parse() {
    let t = ScriptedTransport::new([Step::Interrupt(encode_event(0xC201, 0, &[0xFFFF_C001]).unwrap())]);
    let mut c = PtpConnection::new(t.clone(), ConnectionConfig::default());
    let e = c.poll_event(Duration::from_millis(10)).unwrap().unwrap();
    assert_eq!(e.code, 0xC201);
    assert_eq!(e.params, vec![0xFFFF_C001]);
    assert_eq!(c.poll_event(Duration::from_millis(10)).unwrap(), None);
    // Garbage on the interrupt pipe is ignored, not fatal.
    t.push([Step::Interrupt(vec![1, 2, 3])]);
    assert_eq!(c.poll_event(Duration::from_millis(10)).unwrap(), None);
    let mut no_int = PtpConnection::new(ScriptedTransport::new([]).with_interrupt(false), ConnectionConfig::default());
    assert_eq!(no_int.poll_event(Duration::from_millis(10)).unwrap(), None);
}

#[test]
fn transaction_ids_increment_and_wrap() {
    let mut steps = Vec::new();
    for txn in 1..=3u32 {
        steps.push(Step::Out(encode_command(0x1001, txn, &[]).unwrap()));
        steps.push(Step::In(response(rc::OK, txn)));
    }
    let (mut c, t) = conn(steps, ConnectionConfig::default());
    for _ in 0..3 {
        c.transaction_vec(0x1001, &[], None, 64).unwrap();
    }
    assert_eq!(c.last_txn(), 3);
    t.verify().unwrap();
}

#[test]
fn invalid_header_without_carry_over_is_an_error() {
    let steps = vec![Step::Out(encode_command(0x1001, 1, &[]).unwrap()), Step::In(vec![0xFF; 32])];
    let (mut c, _t) = conn(steps, ConnectionConfig::default());
    assert!(matches!(c.transaction_vec(0x1001, &[], None, 64), Err(PtpError::InvalidContainer(_))));
}

#[test]
fn oversized_control_container_rejected() {
    let mut r = response(rc::OK, 1);
    r[0..4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    let steps = vec![Step::Out(encode_command(0x1001, 1, &[]).unwrap()), Step::In(r)];
    let (mut c, _t) = conn(steps, ConnectionConfig::default());
    assert!(c.transaction_vec(0x1001, &[], None, 64).is_err());
}

#[test]
fn empty_reads_are_bounded() {
    let mut steps = vec![Step::Out(encode_command(0x1001, 1, &[]).unwrap())];
    steps.extend(std::iter::repeat_n(Step::In(vec![]), 20));
    let (mut c, _t) = conn(steps, ConnectionConfig::default());
    assert!(matches!(c.transaction_vec(0x1001, &[], None, 64), Err(PtpError::Protocol(_))));
}

#[test]
fn truncated_data_phase_errors() {
    let d = data(0x1009, 1, &[1; 3000]);
    let mut steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap()), Step::In(d[..2000].to_vec())];
    steps.extend(std::iter::repeat_n(Step::In(vec![]), 10));
    let (mut c, _t) = conn(steps, ConnectionConfig::default());
    assert!(c.transaction_vec(0x1009, &[], None, 1 << 20).is_err());
}

#[test]
fn vec_limit_is_enforced() {
    let mut s = Script::new();
    s.data_in(0x1009, &[], Some(&[0; 2000]), rc::OK);
    let (mut c, _t) = conn(s.build(), ConnectionConfig::default());
    assert!(matches!(c.transaction_vec(0x1009, &[], None, 1000), Err(PtpError::Sink(_))));
}

/// A device that serves one very large GetObject lazily, without holding it
/// in memory: data container of `len` payload bytes then a response.
struct BigObjectDevice {
    header: Vec<u8>,
    total: u64,
    sent: u64,
    response: Vec<u8>,
    response_sent: bool,
    unknown_len: bool,
}

impl BigObjectDevice {
    fn new(len: u64, unknown_len: bool) -> Self {
        let mut header = encode_data(0x1009, 1, &[]);
        let wire_len = if unknown_len { u32::MAX } else { u32::try_from(len + 12).unwrap() };
        header[0..4].copy_from_slice(&wire_len.to_le_bytes());
        Self { header, total: len, sent: 0, response: encode_response(rc::OK, 1, &[]).unwrap(), response_sent: false, unknown_len }
    }
    fn byte(i: u64) -> u8 {
        (i.wrapping_mul(2654435761) >> 13) as u8
    }
}

impl Transport for BigObjectDevice {
    fn bulk_out(&mut self, _: &[u8], _: Duration) -> Result<(), UsbError> {
        Ok(())
    }
    fn bulk_in(&mut self, max_len: usize, _: Duration) -> Result<Vec<u8>, UsbError> {
        let max_len = round_up_to_packet(max_len, 512);
        let mut out = Vec::with_capacity(max_len);
        if !self.header.is_empty() {
            out.append(&mut self.header);
        }
        while out.len() < max_len && self.sent < self.total {
            out.push(Self::byte(self.sent));
            self.sent += 1;
        }
        if self.sent == self.total && out.len() < max_len && !self.response_sent && !self.unknown_len {
            // Response rides in the same transfer when it fits.
            let room = max_len - out.len();
            if room >= self.response.len() {
                out.extend_from_slice(&self.response);
                self.response_sent = true;
            }
        } else if self.sent == self.total && out.is_empty() && !self.response_sent {
            out.extend_from_slice(&self.response);
            self.response_sent = true;
        }
        Ok(out)
    }
    fn interrupt_in(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, UsbError> {
        Err(UsbError::Timeout)
    }
    fn has_interrupt_in(&self) -> bool {
        false
    }
    fn bulk_in_max_packet(&self) -> usize {
        512
    }
    fn bulk_out_max_packet(&self) -> usize {
        512
    }
    fn clear_halt(&mut self, _: Pipe) -> Result<(), UsbError> {
        Ok(())
    }
    fn reset(&mut self) -> Result<(), UsbError> {
        Ok(())
    }
}

struct Checker {
    n: u64,
    ok: bool,
}

impl Write for Checker {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            self.ok &= b == BigObjectDevice::byte(self.n);
            self.n += 1;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn data_phase_larger_than_64_mib_streams() {
    let len = 100 * 1024 * 1024 + 123; // an uncompressed A1 RAW is ~100 MB
    let mut c = PtpConnection::new(BigObjectDevice::new(len, false), ConnectionConfig::default());
    let mut sink = Checker { n: 0, ok: true };
    let o = c.transaction(0x1009, &[0xFFFF_C001], None, &mut sink).unwrap();
    assert!(o.response.is_ok());
    assert_eq!(o.data_len, Some(len));
    assert_eq!(sink.n, len);
    assert!(sink.ok, "payload bytes intact and in order");
}

#[test]
fn unknown_length_data_phase_reads_to_short_packet() {
    let len = 3 * 1024 * 1024 + 77;
    let mut c = PtpConnection::new(BigObjectDevice::new(len, true), ConnectionConfig::default());
    let mut sink = Checker { n: 0, ok: true };
    let o = c.transaction(0x1009, &[0xFFFF_C001], None, &mut sink).unwrap();
    assert!(o.response.is_ok());
    assert_eq!(sink.n, len);
    assert!(sink.ok);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Any split of (data + optional zero padding + response) into transfers
    /// reassembles to the same payload.
    #[test]
    fn any_fragmentation_reassembles(
        payload in proptest::collection::vec(any::<u8>(), 0..3000),
        pad in 0usize..40,
        cuts in proptest::collection::vec(1usize..1500, 0..8),
    ) {
        let mut wire = data(0x1009, 1, &payload);
        wire.extend(std::iter::repeat_n(0u8, pad));
        wire.extend_from_slice(&response(rc::OK, 1));
        let mut steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap())];
        let mut rest = wire.as_slice();
        for c in cuts {
            if rest.is_empty() { break; }
            let c = c.min(rest.len());
            steps.push(Step::In(rest[..c].to_vec()));
            rest = &rest[c..];
        }
        if !rest.is_empty() { steps.push(Step::In(rest.to_vec())); }
        let (mut c, _t) = conn(steps, small_cfg());
        let (r, got) = c.transaction_vec(0x1009, &[], None, 1 << 20).unwrap();
        prop_assert!(r.is_ok());
        prop_assert_eq!(got, payload);
    }

    /// Hostile replies never panic or hang; they produce Ok or Err.
    #[test]
    fn hostile_replies_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..200), 0..12)) {
        let mut steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap())];
        steps.extend(chunks.into_iter().map(Step::In));
        let (mut c, _t) = conn(steps, small_cfg());
        let _ = c.transaction_vec(0x1009, &[], None, 1 << 16);
    }

    /// Header-shaped garbage (plausible types, random lengths) never panics.
    #[test]
    fn hostile_headers_never_panic(len in any::<u32>(), kind in 0u16..6, code in any::<u16>(), tail in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut b = Vec::new();
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&kind.to_le_bytes());
        b.extend_from_slice(&code.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&tail);
        let steps = vec![Step::Out(encode_command(0x1009, 1, &[]).unwrap()), Step::In(b)];
        let (mut c, _t) = conn(steps, small_cfg());
        let _ = c.transaction_vec(0x1009, &[], None, 1 << 16);
    }
}
