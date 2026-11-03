//! Status probe, `recover_transport` (§6.3) with its classification, and the
//! postflight gate.

use silvercraft_usb::Transport;

use crate::EpsonError;
use crate::codec::{ACK, CAN, CLEAN_IDLE, ESC_AT, ESC_F, MAX_READ, Status, hex};
use crate::io::{DRAIN_TIMEOUT_CAP, Io, RECOVERY_TIMEOUT_CAP, STATUS_TIMEOUT_CAP};

/// Read an ESC/I packet: 4-byte header; a non-`02` header is a payload-only
/// reply; a non-zero length payload is read and ACKed (probe path only).
pub(crate) fn read_esci_packet<T: Transport>(io: &mut Io<T>, fallback_size: usize, read_payload: bool) -> Result<Vec<u8>, EpsonError> {
    let header = io.read_exact(4, "ESC/I packet header")?;
    if header.first() != Some(&0x02) {
        // Payload-only reply: drain the rest of a B8/'R' block, then ACK.
        let mut payload = header.clone();
        if header.starts_with(b"B8") || header.contains(&b'R') {
            while let Ok(more) = io.read(64, "payload-only reply") {
                if more.is_empty() {
                    break;
                }
                payload.extend_from_slice(&more);
                if payload.len() > 0x10000 {
                    break;
                }
            }
            let _ = io.write(&[ACK], "payload ACK");
        }
        return Ok(if read_payload { payload } else { header });
    }
    let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if len == 0 {
        return Ok(header);
    }
    if len > (fallback_size * 8).max(65536) {
        return Err(EpsonError::Protocol(format!("unexpectedly large ESC/I payload: {len} bytes")));
    }
    let payload = io.read_exact(len, "ESC/I packet payload")?;
    io.write(&[ACK], "ESC/I payload ACK")?;
    let mut out = header;
    if read_payload {
        out.extend_from_slice(&payload);
    }
    Ok(out)
}

/// The one-shot status probe: `W 1B 46 ; R 4` (+ payload/ACK if the header announces one),
/// with the timeout capped at 1.5 s.
pub(crate) fn status_probe<T: Transport>(io: &mut Io<T>) -> Result<Status, EpsonError> {
    let saved = io.timeout();
    io.set_timeout(saved.min(STATUS_TIMEOUT_CAP));
    let r = io.write(&ESC_F, "ESC F status").and_then(|()| read_esci_packet(io, 4, false));
    io.set_timeout(saved);
    r.map(|raw| Status { raw })
}

/// One step of a recovery run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryStep {
    /// Step name (`drain-before-cancel`, `cancel`, `initialize`, `status`, ...).
    pub name: String,
    /// What was sent.
    pub command: String,
    /// Bytes received.
    pub response: Vec<u8>,
    /// Error text, if the step failed.
    pub error: Option<String>,
}

impl RecoveryStep {
    fn ok(name: &str, command: &str, response: Vec<u8>) -> Self {
        Self { name: name.into(), command: command.into(), response, error: None }
    }
    fn err(name: &str, command: &str, response: Vec<u8>, e: impl ToString) -> Self {
        Self { name: name.into(), command: command.into(), response, error: Some(e.to_string()) }
    }
}

/// Recovery outcome class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryClass {
    /// Final status `02 12 00 00` and no step errors or live packets.
    CleanIdle,
    /// A step returned a lone `02` with an error: power-cycle if it repeats.
    PartialNativeTail,
    /// Some response ends with the live-packet terminator `15`.
    LivePacketRemnant,
    /// The final status failed or was empty.
    MissingFinalStatus,
    /// Final status read but not clean idle.
    NotCleanIdle,
}

/// Recovery parameters (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryOptions {
    /// Read size (4096 default; a GUI used 640).
    pub read_size: usize,
    /// Send ESC @ (unless a drained native transfer tail was observed).
    pub initialize: bool,
    /// Return right away if the first status is clean idle.
    pub fast_if_idle: bool,
    /// Drain reads (at least 8 are always made on nusb/libusb).
    pub max_drain_reads: usize,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        Self { read_size: 4096, initialize: true, fast_if_idle: false, max_drain_reads: 1 }
    }
}

/// Result of one `recover_transport` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Every step, in order.
    pub steps: Vec<RecoveryStep>,
    /// Classification.
    pub class: RecoveryClass,
    /// The last `status` step's response.
    pub final_status: Vec<u8>,
    /// Final status is `02 12 00 00`.
    pub clean_idle: bool,
    /// Some error / live packet / not clean.
    pub requires_followup_recovery: bool,
    /// `clean_idle && !requires_followup_recovery`.
    pub safe_to_retry_native_start: bool,
    /// Class is [`RecoveryClass::PartialNativeTail`].
    pub requires_power_cycle_if_repeated: bool,
    /// Bytes discarded by the drain steps.
    pub drained_bytes: usize,
}

fn looks_like_live_zero_packet(r: &[u8]) -> bool {
    r.last() == Some(&0x15) && r[..r.len() - 1].iter().all(|&b| b == 0)
}

fn is_native_state(r: &[u8]) -> bool {
    r.len() == 4
}

/// True after draining the observed native transfer packet / ACK / tail sequence: skip ESC @.
pub fn shows_drained_native_transfer_tail(steps: &[RecoveryStep]) -> bool {
    let live = steps.iter().any(|s| looks_like_live_zero_packet(&s.response));
    let ack = steps.iter().any(|s| s.response == [ACK]);
    let tail = steps.iter().any(|s| is_native_state(&s.response));
    live && ack && tail
}

/// Classify recovery steps.
pub fn classify(steps: Vec<RecoveryStep>) -> RecoveryReport {
    let responses: Vec<&Vec<u8>> = steps.iter().map(|s| &s.response).filter(|r| !r.is_empty()).collect();
    let live_terminator = responses.iter().any(|r| r.last() == Some(&0x15));
    let error_count = steps.iter().filter(|s| s.error.is_some()).count();
    let partial_tail = steps.iter().any(|s| s.response == [0x02] && s.error.is_some());
    let status_error = steps.iter().any(|s| s.name == "status" && s.error.is_some());
    let final_status = steps.iter().rev().find(|s| s.name == "status").map(|s| s.response.clone()).unwrap_or_default();
    let clean_idle = final_status == CLEAN_IDLE;
    let requires_followup_recovery = error_count > 0 || live_terminator || !clean_idle;
    let class = if clean_idle && !requires_followup_recovery {
        RecoveryClass::CleanIdle
    } else if partial_tail {
        RecoveryClass::PartialNativeTail
    } else if live_terminator {
        RecoveryClass::LivePacketRemnant
    } else if status_error || final_status.is_empty() {
        RecoveryClass::MissingFinalStatus
    } else {
        RecoveryClass::NotCleanIdle
    };
    let drained_bytes = steps.iter().filter(|s| s.name.starts_with("drain")).map(|s| s.response.len()).sum();
    RecoveryReport {
        steps,
        class,
        final_status,
        clean_idle,
        requires_followup_recovery,
        safe_to_retry_native_start: clean_idle && !requires_followup_recovery,
        requires_power_cycle_if_repeated: class == RecoveryClass::PartialNativeTail,
        drained_bytes,
    }
}

/// `recover_transport` on a fresh handle (§6.3). Step errors are recorded, never raised.
pub(crate) fn recover_on_handle<T: Transport>(io: &mut Io<T>, opts: &RecoveryOptions) -> RecoveryReport {
    io.set_honor_cancel(false);
    let base = io.timeout().min(RECOVERY_TIMEOUT_CAP);
    io.set_timeout(base);
    let mut steps: Vec<RecoveryStep> = Vec::new();

    if opts.fast_if_idle {
        match io.write(&ESC_F, "ESC F").and_then(|()| read_esci_packet(io, opts.read_size, true)) {
            Ok(r) => {
                let clean = r == CLEAN_IDLE;
                steps.push(RecoveryStep::ok("status", "ESC F", r));
                if clean {
                    return classify(steps);
                }
            }
            Err(e) => steps.push(RecoveryStep::err("status", "ESC F", vec![], e)),
        }
    }

    let drain = |io: &mut Io<T>, steps: &mut Vec<RecoveryStep>, label: &str| {
        let count = opts.max_drain_reads.max(1).max(8);
        let size = opts.read_size.max(MAX_READ);
        io.set_timeout(base.min(DRAIN_TIMEOUT_CAP));
        for i in 0..count {
            let name = if i == 0 { label.to_string() } else { format!("{label}-{}", i + 1) };
            match io.read(size, "recovery drain") {
                Ok(r) => {
                    let empty = r.is_empty();
                    steps.push(RecoveryStep::ok(&name, "raw read", r));
                    if empty {
                        break;
                    }
                }
                // An idle pipe ends the drain: record it as an empty read, not an error
                //.
                Err(e) if e.is_timeout() => {
                    steps.push(RecoveryStep::ok(&name, "raw read", vec![]));
                    break;
                }
                Err(e) => {
                    steps.push(RecoveryStep::err(&name, "raw read", vec![], e));
                    break;
                }
            }
        }
        io.set_timeout(base);
    };

    drain(io, &mut steps, "drain-before-cancel");
    match io.write(&[CAN], "CAN").and_then(|()| io.read(opts.read_size.clamp(1, 64), "CAN reply")) {
        Ok(r) => steps.push(RecoveryStep::ok("cancel", "CAN", r)),
        Err(e) => steps.push(RecoveryStep::err("cancel", "CAN", vec![], e)),
    }
    drain(io, &mut steps, "drain-after-cancel");

    if opts.initialize && shows_drained_native_transfer_tail(&steps) {
        steps.push(RecoveryStep::ok("initialize-skipped-after-native-transfer-tail", "ESC @ skipped", vec![]));
    } else if opts.initialize {
        match io.write(&ESC_AT, "ESC @").and_then(|()| io.read(1, "ESC @ ack")) {
            Ok(r) if r == [ACK] => steps.push(RecoveryStep::ok("initialize", "ESC @", r)),
            Ok(r) if r == [0x02] => match io.read_exact(3, "ESC @ native tail") {
                Ok(more) => {
                    let mut all = r;
                    all.extend_from_slice(&more);
                    steps.push(RecoveryStep::ok("initialize-queued-native-tail", "ESC @ tail drain", all));
                }
                Err(e) => steps.push(RecoveryStep::err(
                    "initialize-queued-native-tail",
                    "ESC @ tail drain",
                    r,
                    format!("native tail drain after ESC @ timed out after leading 0x02: {e}"),
                )),
            },
            Ok(r) => steps.push(RecoveryStep::err("initialize", "ESC @", vec![], format!("expected ACK, got {}", hex(&r)))),
            Err(e) => steps.push(RecoveryStep::err("initialize", "ESC @", vec![], e)),
        }
    }

    match io.write(&ESC_F, "ESC F").and_then(|()| read_esci_packet(io, opts.read_size, true)) {
        Ok(r) => steps.push(RecoveryStep::ok("status", "ESC F", r)),
        Err(e) => steps.push(RecoveryStep::err("status", "ESC F", vec![], e)),
    }
    classify(steps)
}

/// Postflight gate outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostflightGate {
    /// Clean idle: another scan may start.
    pub safe_to_continue: bool,
    /// Run recovery before the next scan.
    pub requires_recovery: bool,
    /// Escalate to a power cycle if this repeats.
    pub requires_power_cycle_if_repeated: bool,
    /// Reason tag.
    pub reason: &'static str,
    /// Status bytes seen.
    pub status_hex: String,
}

/// Classify the postflight `ESC F` result.
pub fn postflight_gate(result: &Result<Status, EpsonError>) -> PostflightGate {
    match result {
        Ok(s) if s.is_clean_idle() => PostflightGate {
            safe_to_continue: true,
            requires_recovery: false,
            requires_power_cycle_if_repeated: false,
            reason: "postflight-clean-idle",
            status_hex: s.hex(),
        },
        Ok(s) if s.is_partial_native_tail() => PostflightGate {
            safe_to_continue: false,
            requires_recovery: true,
            requires_power_cycle_if_repeated: true,
            reason: "postflight-partial-native-tail",
            status_hex: s.hex(),
        },
        Ok(s) => PostflightGate {
            safe_to_continue: false,
            requires_recovery: true,
            requires_power_cycle_if_repeated: false,
            reason: "postflight-not-clean-idle",
            status_hex: s.hex(),
        },
        Err(e) => PostflightGate {
            safe_to_continue: false,
            requires_recovery: true,
            requires_power_cycle_if_repeated: e.is_timeout(),
            reason: if e.is_timeout() { "postflight-status-timeout" } else { "postflight-status-error" },
            status_hex: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VirtualClock;
    use silvercraft_usb::{MockEvent, ScriptedTransport, Step};
    use std::sync::Arc;
    use std::time::Duration;

    fn step(name: &str, r: &[u8], err: bool) -> RecoveryStep {
        RecoveryStep { name: name.into(), command: String::new(), response: r.to_vec(), error: err.then(|| "x".to_string()) }
    }

    #[test]
    fn classification_cases() {
        assert_eq!(classify(vec![step("cancel", &[6], false), step("status", &CLEAN_IDLE, false)]).class, RecoveryClass::CleanIdle);
        let r = classify(vec![step("initialize-queued-native-tail", &[2], true), step("status", &CLEAN_IDLE, false)]);
        assert_eq!(r.class, RecoveryClass::PartialNativeTail);
        assert!(r.requires_power_cycle_if_repeated && !r.safe_to_retry_native_start);
        assert_eq!(
            classify(vec![step("drain-before-cancel", &[0, 0, 0x15], false), step("status", &CLEAN_IDLE, false)]).class,
            RecoveryClass::LivePacketRemnant
        );
        assert_eq!(classify(vec![step("status", &[], true)]).class, RecoveryClass::MissingFinalStatus);
        assert_eq!(classify(vec![]).class, RecoveryClass::MissingFinalStatus);
        assert_eq!(classify(vec![step("status", &[2, 0x12, 1, 0], false)]).class, RecoveryClass::NotCleanIdle);
        assert_eq!(classify(vec![step("status", &[2, 0x92, 1, 0], false)]).class, RecoveryClass::NotCleanIdle);
    }

    #[test]
    fn drained_tail_detection() {
        let steps = vec![
            step("drain-before-cancel", &[0, 0, 0, 0, 0x15], false),
            step("cancel", &[6], false),
            step("drain-after-cancel", &[2, 0x12, 1, 0], false),
        ];
        assert!(shows_drained_native_transfer_tail(&steps));
        assert!(!shows_drained_native_transfer_tail(&steps[..2]));
    }

    fn io(t: ScriptedTransport, timeout_ms: u64) -> Io<ScriptedTransport> {
        Io::new(t, Duration::from_millis(timeout_ms), Arc::new(VirtualClock::new()), None)
    }

    #[test]
    fn libusb_recovery_drains_full_queued_raster_before_cancel() {
        // Port of test_libusb_recovery_drains_full_queued_raster_before_cancel.
        let t = ScriptedTransport::new([
            Step::In(vec![b'x'; 5 * 131072]),
            Step::Out(vec![CAN]),
            Step::In(vec![ACK]),
            Step::Out(ESC_AT.to_vec()),
            Step::In(vec![ACK]),
            Step::Out(ESC_F.to_vec()),
            Step::In(CLEAN_IDLE.to_vec()),
        ]);
        let mut io = io(t.clone(), 30000);
        let r = recover_on_handle(&mut io, &RecoveryOptions::default());
        assert!(r.clean_idle, "{r:?}");
        assert_eq!(r.class, RecoveryClass::CleanIdle);
        assert_eq!(r.drained_bytes, 5 * 131072);
        let sizes: Vec<usize> =
            t.events().iter().filter_map(|e| if let MockEvent::In { requested, .. } = e { Some(*requested) } else { None }).collect();
        assert_eq!(&sizes[..5], &[131072; 5]);
        assert_eq!(t.writes(), vec![vec![CAN], ESC_AT.to_vec(), ESC_F.to_vec()]);
        t.verify().unwrap();
    }

    #[test]
    fn recovery_records_errors_and_never_raises() {
        let t = ScriptedTransport::new([]);
        let mut io = io(t, 30000);
        let r = recover_on_handle(&mut io, &RecoveryOptions::default());
        assert!(!r.clean_idle);
        assert_ne!(r.class, RecoveryClass::CleanIdle);
    }

    #[test]
    fn fast_if_idle_returns_after_clean_status() {
        let t = ScriptedTransport::new([Step::Out(ESC_F.to_vec()), Step::In(CLEAN_IDLE.to_vec())]);
        let mut io = io(t.clone(), 30000);
        let r = recover_on_handle(&mut io, &RecoveryOptions { fast_if_idle: true, ..Default::default() });
        assert!(r.clean_idle);
        assert_eq!(r.steps.len(), 1);
        t.verify().unwrap();
    }

    #[test]
    fn initialize_queued_native_tail_is_read() {
        let t = ScriptedTransport::new([
            Step::Out(vec![CAN]),
            Step::In(vec![ACK]),
            Step::Out(ESC_AT.to_vec()),
            Step::In(vec![0x02]),
            Step::In(vec![0x12, 0x00, 0x00]),
            Step::Out(ESC_F.to_vec()),
            Step::In(CLEAN_IDLE.to_vec()),
        ]);
        let mut io = io(t.clone(), 30000);
        let r = recover_on_handle(&mut io, &RecoveryOptions::default());
        assert!(r.steps.iter().any(|s| s.name == "initialize-queued-native-tail" && s.response == [2, 0x12, 0, 0]));
        assert!(r.clean_idle);
        t.verify().unwrap();
    }

    #[test]
    fn status_probe_bytes_and_payload_ack() {
        let t = ScriptedTransport::new([Step::Out(ESC_F.to_vec()), Step::In(CLEAN_IDLE.to_vec())]);
        let mut io = io(t.clone(), 30000);
        assert!(status_probe(&mut io).unwrap().is_clean_idle());
        t.verify().unwrap();
        // A header announcing a payload: read it and ACK (probe path).
        let t = ScriptedTransport::new([Step::Out(ESC_F.to_vec()), Step::In(vec![2, 0x12, 2, 0, 0xAA, 0xBB]), Step::Out(vec![ACK])]);
        let mut io = io_t(t.clone());
        assert_eq!(status_probe(&mut io).unwrap().raw, vec![2, 0x12, 2, 0]);
        t.verify().unwrap();
    }

    fn io_t(t: ScriptedTransport) -> Io<ScriptedTransport> {
        io(t, 30000)
    }

    #[test]
    fn postflight_gate_cases() {
        assert!(postflight_gate(&Ok(Status { raw: CLEAN_IDLE.to_vec() })).safe_to_continue);
        let g = postflight_gate(&Ok(Status { raw: vec![2] }));
        assert!(g.requires_power_cycle_if_repeated && g.requires_recovery);
        let g = postflight_gate(&Ok(Status { raw: vec![2, 0x12, 1, 0] }));
        assert!(g.requires_recovery && !g.requires_power_cycle_if_repeated);
        let g = postflight_gate(&Err(EpsonError::Timeout { what: "x".into() }));
        assert!(g.requires_power_cycle_if_repeated);
        assert!(!postflight_gate(&Err(EpsonError::Protocol("x".into()))).requires_power_cycle_if_repeated);
    }
}
