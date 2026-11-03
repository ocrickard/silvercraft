//! The production scan-handle protocol (§3.2–§3.7, §4.2) on one open handle.
//!
//! Wedge-safety by construction:
//! - every scan handle runs under an `AbortGuard` whose `Drop` performs the hardware-clean
//!   abort (§6.5: stop honouring cancel, drain, `CAN`, read 1) unless the raster completed —
//!   so an early `?` return, a NAK, a timeout or even a panic can never leave the pipe
//!   mid-command;
//! - the FS G start-poll read timeout is raised to ≥30 s and cancellation is not honoured
//!   while waiting for the FS G reply (abandoning the silent cold-start read wedges);
//! - every chunk's trailing status byte and the remainder chunk are read;
//! - an IR pass needs an [`IrArmed`] token, which only `extended_handshake` can mint, and only
//!   from a [`CompleteRaster`] proof, which only a complete visible raster produces. "Start IR
//!   after an incomplete visible pass" therefore does not type-check.

use std::time::Duration;

use silvercraft_usb::Transport;

use crate::codec::{
    ACK, CAN, ESC_AT, ESC_D, ESC_D_PROBES, ESC_F, ESC_HASH, ESC_I, ESC_LOWER_F, ESC_LOWER_Z, ESC_Z, ESC_Z_VALUE, FS_F, FS_G, FS_I, FS_S, FS_W,
    MAX_SINGLE_READ_WIDTH_PX, StartResponse, Status, TransferPlan, handshake_payload_from_source, hex, parse_param_block,
};
use crate::io::{DRAIN_LIMIT, FS_G_TIMEOUT_FLOOR, Io, SAFE_ABORT_TIMEOUT_CAP};
use crate::{EpsonError, Pass, Phase};

/// Hardware-clean abort (§6.5). Best effort; never fails. Returns the CAN reply (empty on error).
pub(crate) fn safe_abort<T: Transport>(io: &mut Io<T>) -> Vec<u8> {
    // 1. abort(): nothing is in flight on this thread (transfers are synchronous slices).
    // 2. "clear the abort flag": cleanup I/O must not be cancelled.
    let prev = io.set_honor_cancel(false);
    let saved = io.timeout();
    io.set_timeout(saved.min(SAFE_ABORT_TIMEOUT_CAP));
    // 3. drain queued raster so OUT is not back-pressured, 4. CAN + 1-byte reply.
    io.drain_input(DRAIN_LIMIT);
    let resp = io.write(&[CAN], "CAN").and_then(|()| io.read_exact(1, "CAN reply")).unwrap_or_default();
    io.set_timeout(saved);
    io.set_honor_cancel(prev);
    resp
}

/// Runs `safe_abort` on drop unless defused.
pub(crate) struct AbortGuard<'a, T: Transport> {
    io: &'a mut Io<T>,
    armed: bool,
    response: Option<Vec<u8>>,
}

impl<'a, T: Transport> AbortGuard<'a, T> {
    pub(crate) fn new(io: &'a mut Io<T>) -> Self {
        Self { io, armed: true, response: None }
    }
    pub(crate) fn io(&mut self) -> &mut Io<T> {
        self.io
    }
    /// Abort now; returns the CAN reply.
    pub(crate) fn abort(&mut self) -> Vec<u8> {
        self.armed = false;
        let r = safe_abort(self.io);
        self.response = Some(r.clone());
        r
    }
    /// The operation ended cleanly: no abort.
    pub(crate) fn defuse(&mut self) {
        self.armed = false;
    }
}

impl<T: Transport> Drop for AbortGuard<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            let _ = safe_abort(self.io);
        }
    }
}

/// What one priming cycle read back (recorded only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrimingReplies {
    /// ESC @ reply (not validated, ED:4328).
    pub init: Vec<u8>,
    /// FS F 16 bytes.
    pub fs_f: Vec<u8>,
    /// FS I 80 bytes.
    pub fs_i: Vec<u8>,
    /// ESC I header + payload.
    pub esc_i: Vec<u8>,
}

/// One preview priming cycle (§3.2) on its own fresh handle:
/// `W 1B40 R1 ; W 1C46 R16 ; W 1C49 R80 ; W 1B49 R4 R<len>` — no ACK written.
pub(crate) fn priming_cycle<T: Transport>(io: &mut Io<T>) -> Result<PrimingReplies, EpsonError> {
    // a failed query read is cleaned up (drain + CAN) as well.
    let mut g = AbortGuard::new(io);
    let io = g.io();
    let mut r = PrimingReplies::default();
    io.write(&ESC_AT, "priming ESC @")?;
    r.init = io.read_exact(1, "priming ESC @ ack")?;
    io.write(&FS_F, "priming FS F")?;
    r.fs_f = io.read_exact(16, "priming FS F status")?;
    io.write(&FS_I, "priming FS I")?;
    r.fs_i = io.read_exact(80, "priming FS I identity")?;
    io.write(&ESC_I, "priming ESC I")?;
    let header = io.read_exact(4, "priming ESC I header")?;
    let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
    r.esc_i = header;
    if len > 0 {
        let body = io.read_exact(len, "priming ESC I data")?;
        r.esc_i.extend_from_slice(&body);
    }
    g.defuse();
    Ok(r)
}

/// Proof that a raster was read completely (every planned chunk and trailer). Only
/// `run_scan_handle` can create one; it cannot be forged outside this module:
///
/// ```compile_fail
/// let forged = silvercraft_epson::handle::CompleteRaster { _private: () };
/// ```
#[derive(Debug)]
pub struct CompleteRaster {
    _private: (),
}

/// Proof that the IR engine was armed by the FS S / ESC # handshake on the visible handle
/// right after a complete raster. Consumed by the IR pass; only `extended_handshake` mints it:
///
/// ```compile_fail
/// let forged = silvercraft_epson::handle::IrArmed { _private: () };
/// ```
#[derive(Debug)]
pub struct IrArmed {
    _private: (),
}

/// Inputs of one scan-handle pass.
pub(crate) struct HandleJob<'a> {
    pub pass: Pass,
    pub params: [u8; 64],
    pub luts: &'a [[u8; 257]],
    pub poll_attempts: u32,
    pub poll_delay: Duration,
    pub enforce_pre_start_gate: bool,
    pub tile: usize,
    pub tiles: usize,
}

/// What a scan handle produced.
#[derive(Debug)]
pub struct HandleOutcome {
    /// FS F reply (16 bytes, recorded).
    pub fs_f: Vec<u8>,
    /// ESC f header + body (recorded).
    pub esc_f: Vec<u8>,
    /// Pre-start ESC F reply.
    pub pre_start: Vec<u8>,
    /// Final (ready) FS G reply.
    pub start: Option<StartResponse>,
    /// FS G polls made.
    pub poll_attempts: u32,
    /// Image bytes (trailers stripped).
    pub raster: Vec<u8>,
    /// Trailer byte of each chunk read.
    pub trailers: Vec<u8>,
    /// Chunks planned.
    pub planned_chunks: u64,
    /// Present iff the raster is complete.
    pub complete: Option<CompleteRaster>,
    /// Why the raster is incomplete.
    pub raster_error: Option<EpsonError>,
    /// CAN reply when the clean abort ran.
    pub cancel_response: Option<Vec<u8>>,
}

fn emit(progress: &mut dyn FnMut(Phase), p: Phase) {
    progress(p);
}

/// Run the §3.3–§3.7 scan-handle sequence. `Err` means the handshake/start failed (the guard
/// has already aborted cleanly); `Ok` with `complete == None` means the raster stopped early
/// (cancel or read error) and the clean abort ran.
pub(crate) fn run_scan_handle<T: Transport>(
    io: &mut Io<T>,
    job: &HandleJob<'_>,
    progress: &mut dyn FnMut(Phase),
) -> Result<HandleOutcome, EpsonError> {
    // Belt and braces: the session never builds such a block, but a width the firmware NAKs
    // or an out-of-range area must never reach the device.
    if let Some(p) = parse_param_block(&job.params)
        && (p.area.width > MAX_SINGLE_READ_WIDTH_PX || p.area.width == 0 || p.area.height == 0)
    {
        return Err(EpsonError::InvalidRequest(format!("single-read width {} px outside 1..={MAX_SINGLE_READ_WIDTH_PX}", p.area.width)));
    }
    let mut g = AbortGuard::new(io);
    let res = scan_handle_body(g.io(), job, progress);
    match res {
        Err(e) => {
            g.abort();
            Err(e)
        }
        Ok(mut out) if out.complete.is_some() => {
            g.defuse();
            out.cancel_response = None;
            Ok(out)
        }
        Ok(mut out) => {
            // Incomplete raster (cancel / read error): drain + CAN (§3.7 step 3, libusb path).
            out.cancel_response = Some(g.abort());
            Ok(out)
        }
    }
}

fn scan_handle_body<T: Transport>(io: &mut Io<T>, job: &HandleJob<'_>, progress: &mut dyn FnMut(Phase)) -> Result<HandleOutcome, EpsonError> {
    let pass = job.pass;
    emit(progress, Phase::Setup { pass, tile: job.tile, tiles: job.tiles });
    for d in ESC_D_PROBES {
        io.command_ack(&ESC_D, "ESC D command")?;
        io.write(&[d], "ESC D parameter")?;
        io.ack("ESC D parameter")?;
    }
    io.write(&FS_F, "FS F")?;
    let fs_f = io.read_exact(16, "FS F status")?;
    io.write(&ESC_LOWER_F, "ESC f")?;
    let mut esc_f = io.read_exact(4, "ESC f header")?;
    // Production uses only byte 2 as the body length.
    let body_len = usize::from(esc_f[2]);
    if body_len > 0 {
        let body = io.read_exact(body_len, "ESC f body")?;
        esc_f.extend_from_slice(&body);
    }
    io.command_ack(&ESC_AT, "ESC @")?;
    io.write(&ESC_F, "ESC F")?;
    let pre_start = io.read_exact(4, "ESC F pre-start state")?;
    // Vendor gate, enforced *before* FS G — never after.
    if job.enforce_pre_start_gate && !(Status { raw: pre_start.clone() }).pre_start_ready() {
        return Err(EpsonError::Protocol(format!("scanner not ready before start (ESC F {})", hex(&pre_start))));
    }
    for lut in job.luts {
        io.command_ack(&ESC_LOWER_Z, "ESC z command")?;
        io.write(lut, "ESC z LUT")?;
        io.ack("ESC z LUT")?;
    }
    io.command_ack(&FS_W, "FS W command")?;
    io.write(&job.params, "FS W parameter")?;
    io.ack("FS W parameter")?;
    io.command_ack(&ESC_Z, "ESC Z command")?;
    io.write(&[ESC_Z_VALUE], "ESC Z value")?;
    io.ack("ESC Z value")?;

    // FS G busy-poll (§3.4). Extend (never shorten) the timeout for the start polls only.
    let saved = io.timeout();
    io.set_timeout(saved.max(FS_G_TIMEOUT_FLOOR));
    let poll = poll_start(io, job, progress);
    io.set_timeout(saved);
    let (start, poll_attempts) = poll?;
    let plan = start.plan()?;
    let expected = parse_param_block(&job.params).map(|p| p.area.raster_bytes()).unwrap_or(0);
    let total = plan.total_bytes();
    // Hostile-descriptor guard: refuse (clean abort) a transfer wildly larger than the area.
    if total > expected.saturating_mul(2).saturating_add(1 << 20) {
        return Err(EpsonError::Protocol(format!("start descriptor announces {total} raster bytes for a {expected}-byte area: {}", hex(&start.raw))));
    }
    let mut out = HandleOutcome {
        fs_f,
        esc_f,
        pre_start,
        start: Some(start),
        poll_attempts,
        raster: Vec::with_capacity(usize::try_from(total).unwrap_or(0)),
        trailers: Vec::new(),
        planned_chunks: plan.chunk_count(),
        complete: None,
        raster_error: None,
        cancel_response: None,
    };
    read_raster(io, &plan, &mut out, job, progress);
    if out.raster_error.is_none() {
        out.complete = Some(CompleteRaster { _private: () });
    }
    Ok(out)
}

fn poll_start<T: Transport>(io: &mut Io<T>, job: &HandleJob<'_>, progress: &mut dyn FnMut(Phase)) -> Result<(StartResponse, u32), EpsonError> {
    let mut last = Vec::new();
    for attempt in 1..=job.poll_attempts.max(1) {
        if io.cancel_requested() {
            return Err(EpsonError::Cancelled);
        }
        io.write(&FS_G, "FS G")?;
        // The first FS G after power-on can be silent for ~30 s; abandoning that read wedges
        // the device, so cancellation is only honoured between polls.
        let prev = io.set_honor_cancel(false);
        let r = io.read_exact(14, "FS G start");
        io.set_honor_cancel(prev);
        let r = r?;
        let s = StartResponse::parse(&r)?;
        if !s.busy() {
            return Ok((s, attempt));
        }
        last = r;
        emit(progress, Phase::WarmingUp { pass: job.pass, poll: attempt, tile: job.tile, tiles: job.tiles });
        io.clock().sleep(job.poll_delay);
    }
    Err(EpsonError::Protocol(format!("FS G did not become ready within {} polls; last status {}", job.poll_attempts, hex(&last))))
}

fn read_raster<T: Transport>(io: &mut Io<T>, plan: &TransferPlan, out: &mut HandleOutcome, job: &HandleJob<'_>, progress: &mut dyn FnMut(Phase)) {
    let count = plan.chunk_count();
    let total = plan.total_bytes();
    for i in 0..count {
        if io.cancel_requested() {
            out.raster_error = Some(EpsonError::Cancelled);
            return;
        }
        let n = plan.chunk_len(i) as usize;
        let before = out.raster.len();
        // +1 trailing status byte on every chunk, including the remainder (§3.6).
        if let Err(e) = io.read_exact_into(&mut out.raster, n + 1, "raster chunk") {
            out.raster.truncate(before);
            out.raster_error = Some(e);
            return;
        }
        let trailer = out.raster.pop().unwrap_or(0);
        out.trailers.push(trailer);
        if i + 1 != count {
            // ACK requests the next chunk; the final chunk is not ACKed.
            if let Err(e) = io.write(&[ACK], "chunk ACK") {
                out.raster_error = Some(e);
                return;
            }
        }
        emit(progress, Phase::Reading { pass: job.pass, bytes_done: out.raster.len() as u64, bytes_total: total, tile: job.tile, tiles: job.tiles });
    }
}

/// FS S + ESC # on the visible handle right after a complete raster (§4.2). Consumes the proof;
/// returns the token the IR pass requires. A failure is cleaned up with the safe abort.
pub(crate) fn extended_handshake<T: Transport>(io: &mut Io<T>, _proof: CompleteRaster) -> Result<IrArmed, EpsonError> {
    let mut g = AbortGuard::new(io);
    let io = g.io();
    // The raster is complete: the handshake must go out even if Cancel arrived just now
    // (the pipe is idle either way; the session decides whether IR starts).
    io.set_honor_cancel(false);
    io.write(&FS_S, "FS S")?;
    let src = io.read_exact(64, "FS S source")?;
    let payload = handshake_payload_from_source(&src)?;
    io.command_ack(&ESC_HASH, "ESC # command")?;
    io.write(&payload, "ESC # payload")?;
    io.ack("ESC # payload")?;
    g.defuse();
    Ok(IrArmed { _private: () })
}

/// The IR scan handle: identical sequence, IR parameter block and the single master LUT.
/// Requires (and consumes) the arming token.
pub(crate) fn run_ir_handle<T: Transport>(
    io: &mut Io<T>,
    _armed: IrArmed,
    job: &HandleJob<'_>,
    progress: &mut dyn FnMut(Phase),
) -> Result<HandleOutcome, EpsonError> {
    run_scan_handle(io, job, progress)
}

/// `W 1B 46 ; R 4` on the same handle (§3.7). Ignores cancellation.
pub(crate) fn postflight<T: Transport>(io: &mut Io<T>) -> Result<Status, EpsonError> {
    let prev = io.set_honor_cancel(false);
    let r = io.write(&ESC_F, "ESC F postflight").and_then(|()| io.read_exact(4, "ESC F postflight"));
    io.set_honor_cancel(prev);
    r.map(|raw| Status { raw })
}
