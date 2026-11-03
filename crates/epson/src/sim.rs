//! A faithful simulator of the V800/V850 wire behaviour, implementing
//! [`silvercraft_usb::Transport`].
//!
//! Modelled (each from hardware history):
//! - the command/parameter stream (parsed as a byte stream, so split writes reassemble and a
//!   resent prefix corrupts the parse exactly as it would on the device);
//! - carriage calibration: an uncalibrated carriage arms only an origin-(0,0) FS G; a targeted
//!   FS G wedges; calibration is lost on power cycle;
//! - per-lamp busy-poll warm-up (visible and IR lamps separately);
//! - the silent cold-start FS G: nothing on the wire for `cold_start_fsg_latency`; a host that
//!   gives up (writes anything) before it ends wedges the device;
//! - out-of-range area (beyond 8×10 in) wedges at FS G; FS W wider than 4096 px is NAKed;
//! - IR arming: an IR FS G wedges unless the FS S / ESC # handshake (with the correctly
//!   transformed payload) ran on a handle right after a *complete* visible raster;
//! - ACK-paced raster chunks with a trailing status byte; OUT is back-pressured (writes time
//!   out) while a served chunk is still unread; any other command mid-raster wedges;
//! - closing a handle mid-command, mid-raster, with unread bytes, or after an abandoned read
//!   that was not followed by CAN wedges;
//! - a wedge makes every transfer time out until [`SimScanner::power_cycle`], which also
//!   re-enumerates (open handles go stale) and clears calibration and lamp warmth.
//!
//! The raster is synthesised from absolute bed coordinates (a smooth negative-like gradient
//! with dust specks visible in both passes), so tiles of the same film agree and IR/visible
//! registration can be checked. `ir_offset` shifts the IR content to model the measured
//! residual mechanical offset.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use silvercraft_usb::{Pipe, Transport, UsbError};

use crate::codec::{
    ACK, CAN, CHUNK_LINES, CLEAN_IDLE, IR_COLOR_BYTE, MAX_SINGLE_READ_WIDTH_PX, NAK, ParsedParams, TPU_MAX_HEIGHT_IN, TPU_MAX_WIDTH_IN,
    handshake_payload_from_source, parse_param_block,
};
use crate::{Clock, EpsonError, HandleOpener, VirtualClock};

/// Real FS F reply (SIM:69).
pub const SIM_FS_F: [u8; 16] = [0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
/// Real FS I reply (SIM:70-75), 80 bytes.
pub const SIM_FS_I: [u8; 80] = [
    0x42, 0x38, 0x00, 0x00, 0xc0, 0x12, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x00, 0x32, 0x00, 0x00, 0x60, 0x9f, 0x00, 0x00, 0x60, 0x9f, 0x00, 0x00,
    0x60, 0xdb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa0, 0x6e, 0x00, 0x00, 0xe0, 0xb5, 0x00, 0x00, 0x87, 0x10, 0x47, 0x54,
    0x2d, 0x58, 0x39, 0x38, 0x30, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x31, 0x2e, 0x31, 0x31, 0x10, 0x10, 0x00, 0x96, 0x00, 0x00,
    0x80, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// Live ESC I reply.
pub const SIM_ESC_I: [u8; 110] = [
    0x02, 0x12, 0x6a, 0x00, 0x42, 0x38, 0x52, 0x32, 0x00, 0x52, 0x3c, 0x00, 0x52, 0x48, 0x00, 0x52, 0x4b, 0x00, 0x52, 0x50, 0x00, 0x52, 0x5a, 0x00,
    0x52, 0x64, 0x00, 0x52, 0x78, 0x00, 0x52, 0x85, 0x00, 0x52, 0x90, 0x00, 0x52, 0x96, 0x00, 0x52, 0xa0, 0x00, 0x52, 0xaf, 0x00, 0x52, 0xb4, 0x00,
    0x52, 0xc8, 0x00, 0x52, 0xd8, 0x00, 0x52, 0xf0, 0x00, 0x52, 0x0a, 0x01, 0x52, 0x2c, 0x01, 0x52, 0x40, 0x01, 0x52, 0x5e, 0x01, 0x52, 0x68, 0x01,
    0x52, 0x90, 0x01, 0x52, 0xe0, 0x01, 0x52, 0x58, 0x02, 0x52, 0xd0, 0x02, 0x52, 0x20, 0x03, 0x52, 0x84, 0x03, 0x52, 0xb0, 0x04, 0x52, 0x40, 0x06,
    0x52, 0x08, 0x07, 0x52, 0x60, 0x09, 0x52, 0x80, 0x0c, 0x41, 0x40, 0x6a, 0x40, 0x92,
];
/// Live ESC f reply, header + 42 bytes.
pub const SIM_ESC_LOWER_F: [u8; 46] = [
    0x02, 0x12, 0x2a, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xc0, 0x49, 0x40, 0x79, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x47, 0x54, 0x2d, 0x58, 0x39, 0x38, 0x30, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
];
const BUSY_START: [u8; 14] = [0x02, 0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Simulator knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimConfig {
    /// Busy FS G replies before a cold lamp is ready (per lamp, per handle).
    pub warm_up_polls: u32,
    /// IR FS G needs arming (or `ir_warm`).
    pub infrared_requires_warm: bool,
    /// Silent first-FS-G bring-up time after power-on (0 = none).
    pub cold_start_fsg_latency: Duration,
    /// IR content shift in scan pixels (models the residual mechanical offset).
    pub ir_offset: (i32, i32),
}

impl Default for SimConfig {
    fn default() -> Self {
        Self { warm_up_polls: 2, infrared_requires_warm: true, cold_start_fsg_latency: Duration::ZERO, ir_offset: (0, 0) }
    }
}

/// Injected faults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimFault {
    /// After the host writes exactly these bytes, the next read fails with a USB error and the
    /// host's transfer is abandoned (the device stays mid-command until it gets a CAN on that
    /// handle). Models the ESC Z ack ReadFile timeout of TS:94.
    FailReadAfterWrite(Vec<u8>),
    /// The next write longer than this many bytes is accepted only up to this many bytes
    /// (a timed-out partial transfer); the host must resume with the remainder.
    SplitNextWrite(usize),
}

#[derive(Debug)]
struct State {
    cfg: SimConfig,
    instance: u64,
    calibrated: bool,
    lamp_warm: bool,
    ir_lamp_warm: bool,
    ir_warm: bool,
    ir_armed: bool,
    cold_start_done: bool,
    wedged: bool,
    wedge_reason: String,
    power_cycles: u64,
    started_scans: u64,
    cans: u64,
    fs_g: u64,
    handles_opened: u64,
    handles_open: u64,
    faults: Vec<SimFault>,
    log: Vec<String>,
}

impl State {
    fn wedge(&mut self, reason: &str) {
        if !self.wedged {
            self.wedged = true;
            self.wedge_reason = reason.to_string();
            self.log.push(format!("WEDGED: {reason}"));
        }
    }
}

static INSTANCES: AtomicU64 = AtomicU64::new(1);

/// The simulated scanner (shared device state; cheap to clone).
#[derive(Debug, Clone)]
pub struct SimScanner {
    st: Arc<Mutex<State>>,
    clock: VirtualClock,
}

impl Default for SimScanner {
    fn default() -> Self {
        Self::new(SimConfig::default())
    }
}

macro_rules! getter {
    ($($(#[$m:meta])* $name:ident: $t:ty;)*) => {$(
        $(#[$m])*
        pub fn $name(&self) -> $t { self.lock().$name.clone() }
    )*};
}

impl SimScanner {
    /// A powered-on, cold, uncalibrated scanner.
    pub fn new(cfg: SimConfig) -> Self {
        let st = State {
            cfg,
            instance: INSTANCES.fetch_add(1, Ordering::SeqCst),
            calibrated: false,
            lamp_warm: false,
            ir_lamp_warm: false,
            ir_warm: false,
            ir_armed: false,
            cold_start_done: false,
            wedged: false,
            wedge_reason: String::new(),
            power_cycles: 0,
            started_scans: 0,
            cans: 0,
            fs_g: 0,
            handles_opened: 0,
            handles_open: 0,
            faults: Vec::new(),
            log: Vec::new(),
        };
        Self { st: Arc::new(Mutex::new(st)), clock: VirtualClock::new() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.st.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    getter! {
        /// Carriage calibrated this power cycle.
        calibrated: bool;
        /// Visible lamp warmed.
        lamp_warm: bool;
        /// IR lamp warmed.
        ir_lamp_warm: bool;
        /// IR engine armed by the handshake.
        ir_armed: bool;
        /// Device wedged (needs a power cycle).
        wedged: bool;
        /// Why it wedged.
        wedge_reason: String;
        /// Power cycles so far.
        power_cycles: u64;
        /// Scans that armed and streamed.
        started_scans: u64;
        /// CAN bytes received.
        cans: u64;
        /// FS G commands received.
        fs_g: u64;
        /// Handles opened so far.
        handles_opened: u64;
        /// Handles currently open.
        handles_open: u64;
        /// Every command / event the device saw.
        log: Vec<String>;
    }

    /// Physical power cycle: clears the wedge, calibration and lamp warmth; re-enumerates.
    pub fn power_cycle(&self) {
        let mut s = self.lock();
        s.calibrated = false;
        s.lamp_warm = false;
        s.ir_lamp_warm = false;
        s.ir_warm = false;
        s.ir_armed = false;
        s.cold_start_done = false;
        s.wedged = false;
        s.wedge_reason.clear();
        s.power_cycles += 1;
        s.log.push("POWER CYCLE".into());
    }

    /// Model an already-warm IR engine (criterion-3 runs).
    pub fn set_ir_warm(&self, warm: bool) {
        self.lock().ir_warm = warm;
    }

    /// Queue a fault.
    pub fn inject(&self, fault: SimFault) {
        self.lock().faults.push(fault);
    }

    /// The simulator's virtual clock.
    pub fn clock(&self) -> VirtualClock {
        self.clock.clone()
    }

    /// Open a raw handle (for tests that drive the wire directly).
    pub fn open_handle(&self) -> SimHandle {
        let epoch = {
            let mut s = self.lock();
            s.handles_opened += 1;
            s.handles_open += 1;
            s.power_cycles
        };
        SimHandle::new(self.clone(), epoch)
    }

    /// A [`HandleOpener`] for [`crate::Session::with_opener`].
    pub fn opener(&self) -> SimOpener {
        SimOpener { dev: self.clone() }
    }
}

/// [`HandleOpener`] over a [`SimScanner`].
#[derive(Debug, Clone)]
pub struct SimOpener {
    dev: SimScanner,
}

impl HandleOpener for SimOpener {
    type Handle = SimHandle;
    fn open(&mut self) -> Result<SimHandle, EpsonError> {
        Ok(self.dev.open_handle())
    }
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.dev.clock.clone())
    }
    fn enumeration_id(&self) -> Option<String> {
        // A power cycle re-enumerates the device, so the id changes with it.
        let s = self.dev.lock();
        Some(format!("sim-{}-enum-{}", s.instance, s.power_cycles))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    EscD,
    Lut,
    FsW,
    EscZ,
    Handshake,
}

impl ParamKind {
    fn len(self) -> usize {
        match self {
            ParamKind::EscD | ParamKind::EscZ => 1,
            ParamKind::Lut => 257,
            ParamKind::FsW => 64,
            ParamKind::Handshake => 32,
        }
    }
}

/// One simulated open handle.
#[derive(Debug)]
pub struct SimHandle {
    dev: SimScanner,
    epoch: u64,
    inbuf: Vec<u8>,
    phase: Option<ParamKind>,
    out: VecDeque<u8>,
    raster_out: VecDeque<u8>,
    params: Option<ParsedParams>,
    last_fs_w: Option<Vec<u8>>,
    last_fs_s: Option<Vec<u8>>,
    visible_polls: u32,
    ir_polls: u32,
    planned: VecDeque<u32>,
    raster: Vec<u8>,
    cursor: usize,
    streaming: bool,
    chunk_ready: bool,
    streaming_ir: bool,
    visible_complete: bool,
    stall: Option<(Duration, ParsedParams)>,
    abandoned_io: bool,
    fail_next_read: bool,
}

impl SimHandle {
    fn new(dev: SimScanner, epoch: u64) -> Self {
        Self {
            dev,
            epoch,
            inbuf: Vec::new(),
            phase: None,
            out: VecDeque::new(),
            raster_out: VecDeque::new(),
            params: None,
            last_fs_w: None,
            last_fs_s: None,
            visible_polls: 0,
            ir_polls: 0,
            planned: VecDeque::new(),
            raster: Vec::new(),
            cursor: 0,
            streaming: false,
            chunk_ready: false,
            streaming_ir: false,
            visible_complete: false,
            stall: None,
            abandoned_io: false,
            fail_next_read: false,
        }
    }

    fn parse(&mut self, s: &mut State, now: Duration) {
        loop {
            if s.wedged {
                self.inbuf.clear();
                return;
            }
            if let Some(kind) = self.phase {
                if self.inbuf.len() < kind.len() {
                    return;
                }
                let p: Vec<u8> = self.inbuf.drain(..kind.len()).collect();
                self.phase = None;
                self.param(s, kind, &p);
                continue;
            }
            let Some(&first) = self.inbuf.first() else { return };
            match first {
                ACK => {
                    self.inbuf.remove(0);
                    if self.streaming {
                        self.chunk_ready = true;
                    }
                }
                CAN => {
                    self.inbuf.remove(0);
                    s.cans += 1;
                    s.log.push("CAN".into());
                    self.planned.clear();
                    self.streaming = false;
                    self.chunk_ready = false;
                    self.abandoned_io = false;
                    self.out.push_back(ACK);
                }
                0x1B | 0x1C => {
                    if self.inbuf.len() < 2 {
                        return;
                    }
                    let cmd = [self.inbuf[0], self.inbuf[1]];
                    self.inbuf.drain(..2);
                    self.command(s, cmd, now);
                }
                _ => {
                    self.inbuf.remove(0);
                }
            }
        }
    }

    fn command(&mut self, s: &mut State, cmd: [u8; 2], now: Duration) {
        s.log.push(format!("{:02x} {:02x}", cmd[0], cmd[1]));
        if self.streaming {
            s.wedge("command sent mid-raster without CAN");
            return;
        }
        match cmd {
            [0x1B, 0x40] => self.out.push_back(ACK),
            [0x1C, 0x46] => self.out.extend(SIM_FS_F),
            [0x1C, 0x49] => self.out.extend(SIM_FS_I),
            [0x1B, 0x49] => self.out.extend(SIM_ESC_I),
            [0x1B, 0x66] => self.out.extend(SIM_ESC_LOWER_F),
            [0x1B, 0x46] => self.out.extend(CLEAN_IDLE),
            [0x1B, 0x44] => {
                self.out.push_back(ACK);
                self.phase = Some(ParamKind::EscD);
            }
            [0x1B, 0x7A] => {
                self.out.push_back(ACK);
                self.phase = Some(ParamKind::Lut);
            }
            [0x1C, 0x57] => {
                self.out.push_back(ACK);
                self.phase = Some(ParamKind::FsW);
            }
            [0x1B, 0x5A] => {
                self.out.push_back(ACK);
                self.phase = Some(ParamKind::EscZ);
            }
            [0x1C, 0x53] => {
                // FS S returns the current parameter block.
                let reply = self.last_fs_w.clone().unwrap_or_else(|| vec![0u8; 64]);
                self.last_fs_s = Some(reply.clone());
                self.out.extend(reply);
            }
            [0x1B, 0x23] => {
                self.out.push_back(ACK);
                self.phase = Some(ParamKind::Handshake);
            }
            [0x1C, 0x47] => self.fs_g(s, now),
            _ => self.out.push_back(ACK),
        }
    }

    fn param(&mut self, s: &mut State, kind: ParamKind, p: &[u8]) {
        match kind {
            ParamKind::FsW => match parse_param_block(p) {
                Some(pp) if pp.area.width > MAX_SINGLE_READ_WIDTH_PX => {
                    s.log.push("FS W NAK (width)".into());
                    self.out.push_back(NAK);
                    return;
                }
                Some(pp) => {
                    self.params = Some(pp);
                    self.last_fs_w = Some(p.to_vec());
                }
                None => s.wedge("FS W parameter was not 64 bytes"),
            },
            ParamKind::Handshake => {
                let expected = self.last_fs_s.as_deref().and_then(|src| handshake_payload_from_source(src).ok());
                if self.visible_complete && expected.is_some_and(|e| e[..] == *p) {
                    s.ir_armed = true;
                    s.log.push("IR armed".into());
                } else {
                    s.log.push("ESC # ignored (not after a complete visible raster, or bad payload)".into());
                }
            }
            _ => {}
        }
        self.out.push_back(ACK);
    }

    fn fs_g(&mut self, s: &mut State, now: Duration) {
        s.fs_g += 1;
        let Some(scan) = self.params else {
            s.wedge("FS G without scan parameters");
            return;
        };
        let is_ir = scan.color == IR_COLOR_BYTE;
        let at_origin = scan.area.at_origin();
        let in_range = scan.dpi > 0 && {
            let d = f64::from(scan.dpi);
            (f64::from(scan.area.x) + f64::from(scan.area.width)) / d <= TPU_MAX_WIDTH_IN + 1e-6
                && (f64::from(scan.area.y) + f64::from(scan.area.height)) / d <= TPU_MAX_HEIGHT_IN + 1e-6
        };
        if !in_range {
            s.wedge("scan area exceeds the transparency-unit film area");
            return;
        }
        if !at_origin && !s.calibrated {
            s.wedge("targeted FS G on an uncalibrated carriage");
            return;
        }
        if is_ir && s.cfg.infrared_requires_warm && !(s.ir_warm || s.ir_armed) {
            s.wedge("IR FS G before the IR pass was armed (needs dual-pass handshake or warm scanner)");
            return;
        }
        if !s.cold_start_done && s.cfg.cold_start_fsg_latency > Duration::ZERO && !s.lamp_warm {
            self.stall = Some((now + s.cfg.cold_start_fsg_latency, scan));
            return;
        }
        let warm = if is_ir { s.ir_lamp_warm || s.ir_warm } else { s.lamp_warm };
        let polls = if is_ir { &mut self.ir_polls } else { &mut self.visible_polls };
        if !warm && *polls < s.cfg.warm_up_polls {
            *polls += 1;
            self.out.extend(BUSY_START);
            return;
        }
        self.arm_and_stream(s, scan);
    }

    fn arm_and_stream(&mut self, s: &mut State, scan: ParsedParams) {
        let is_ir = scan.color == IR_COLOR_BYTE;
        if is_ir {
            s.ir_lamp_warm = true;
            s.ir_armed = false; // consumed by this IR pass
        } else {
            s.lamp_warm = true;
            s.ir_armed = false; // a new visible start disarms an earlier arming
        }
        if scan.area.at_origin() {
            s.calibrated = true;
        }
        s.cold_start_done = true;
        s.started_scans += 1;
        s.log.push(format!("armed {} {}x{} @{}", if is_ir { "IR" } else { "visible" }, scan.area.width, scan.area.height, scan.dpi));
        self.visible_complete = false;
        let stride = (scan.area.width as usize * 6).max(1);
        let total = stride * scan.area.height as usize;
        let unit = total.min(CHUNK_LINES as usize * stride).max(stride);
        let full = total / unit;
        let rem = total - full * unit;
        self.planned = std::iter::repeat_n(unit as u32, full).collect();
        if rem > 0 {
            self.planned.push_back(rem as u32);
        }
        self.raster = synth_raster(&scan, s.cfg.ir_offset);
        self.cursor = 0;
        self.streaming = true;
        self.chunk_ready = true;
        self.streaming_ir = is_ir;
        self.out.extend([0x02, 0x12]);
        self.out.extend((unit as u32).to_le_bytes());
        self.out.extend((full as u32).to_le_bytes());
        self.out.extend((rem as u32).to_le_bytes());
    }

    fn serve_chunk(&mut self) {
        let Some(n) = self.planned.pop_front() else {
            self.streaming = false;
            return;
        };
        let n = n as usize;
        let end = (self.cursor + n).min(self.raster.len());
        self.raster_out.extend(&self.raster[self.cursor..end]);
        self.raster_out.push_back(0x00); // trailing status byte
        self.cursor = end;
        self.chunk_ready = false;
        if self.planned.is_empty() {
            self.streaming = false;
            if !self.streaming_ir {
                self.visible_complete = true;
            }
        }
    }

    fn check_epoch(&self, s: &State) -> Result<(), UsbError> {
        if s.power_cycles != self.epoch { Err(UsbError::Disconnected) } else { Ok(()) }
    }
}

impl Transport for SimHandle {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), UsbError> {
        match self.bulk_out_counted(data, timeout)? {
            n if n == data.len() => Ok(()),
            _ => Err(UsbError::Timeout),
        }
    }

    fn bulk_out_counted(&mut self, data: &[u8], _timeout: Duration) -> Result<usize, UsbError> {
        let dev = self.dev.clone();
        let now = dev.clock.now();
        let mut s = dev.lock();
        self.check_epoch(&s)?;
        if s.wedged {
            return Ok(0);
        }
        if self.stall.is_some() {
            s.wedge("cold-start FS G warm-up read abandoned by the host");
            return Ok(0);
        }
        if !self.raster_out.is_empty() {
            // The device back-pressures OUT until the queued raster chunk is read (§6.2 #8).
            return Ok(0);
        }
        let mut accept = data.len();
        if let Some(i) = s.faults.iter().position(|f| matches!(f, SimFault::SplitNextWrite(k) if data.len() > *k))
            && let SimFault::SplitNextWrite(k) = s.faults.remove(i)
        {
            accept = k;
            s.log.push(format!("partial write: accepted {k} of {} bytes", data.len()));
        }
        if let Some(i) = s.faults.iter().position(|f| matches!(f, SimFault::FailReadAfterWrite(b) if b[..] == *data)) {
            s.faults.remove(i);
            self.fail_next_read = true;
        }
        self.inbuf.extend_from_slice(&data[..accept]);
        self.parse(&mut s, now);
        Ok(accept)
    }

    fn bulk_in(&mut self, max_len: usize, _timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let dev = self.dev.clone();
        let now = dev.clock.now();
        let mut s = dev.lock();
        self.check_epoch(&s)?;
        if s.wedged {
            return Err(UsbError::Timeout);
        }
        if self.fail_next_read {
            self.fail_next_read = false;
            self.abandoned_io = true;
            return Err(UsbError::Io("simulated read failure (transfer abandoned)".into()));
        }
        if let Some((until, scan)) = self.stall {
            if now < until {
                return Err(UsbError::Timeout);
            }
            self.stall = None;
            self.arm_and_stream(&mut s, scan);
        }
        let max_len = max_len.max(1);
        if !self.out.is_empty() {
            let n = max_len.min(self.out.len());
            return Ok(self.out.drain(..n).collect());
        }
        if self.raster_out.is_empty() && self.streaming && self.chunk_ready {
            self.serve_chunk();
        }
        if !self.raster_out.is_empty() {
            let n = max_len.min(self.raster_out.len());
            return Ok(self.raster_out.drain(..n).collect());
        }
        Err(UsbError::Timeout)
    }

    fn interrupt_in(&mut self, _max_len: usize, _timeout: Duration) -> Result<Vec<u8>, UsbError> {
        Err(UsbError::NotFound("no interrupt endpoint".into()))
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
    fn clear_halt(&mut self, _pipe: Pipe) -> Result<(), UsbError> {
        Ok(())
    }
    fn reset(&mut self) -> Result<(), UsbError> {
        Err(UsbError::Unsupported("the Epson driver never resets the device".into()))
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        let mut s = self.dev.lock();
        s.handles_open = s.handles_open.saturating_sub(1);
        if s.power_cycles != self.epoch || s.wedged {
            return;
        }
        let reason = if self.stall.is_some() {
            Some("handle closed during the silent cold-start FS G")
        } else if self.abandoned_io {
            Some("handle closed after an abandoned transfer without CAN")
        } else if !self.raster_out.is_empty() || self.streaming {
            Some("handle closed mid-raster (undrained chunk / trailer)")
        } else if self.phase.is_some() || !self.inbuf.is_empty() {
            Some("handle closed mid-command")
        } else if !self.out.is_empty() {
            Some("handle closed with unread reply bytes")
        } else {
            None
        };
        if let Some(r) = reason {
            s.wedge(r);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic film.

fn hash2(x: i64, y: i64) -> u64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^ (h >> 29)
}

/// Dust density at bed position (inches): 0 clean … 1 opaque speck.
fn dust(u: f64, v: f64) -> f64 {
    const CELL: f64 = 0.05;
    let (cx, cy) = ((u / CELL).floor() as i64, (v / CELL).floor() as i64);
    let mut d: f64 = 0.0;
    for gy in cy - 1..=cy + 1 {
        for gx in cx - 1..=cx + 1 {
            let h = hash2(gx, gy);
            if h % 100 >= 30 {
                continue;
            }
            let fx = ((h >> 8) & 0xFFFF) as f64 / 65535.0;
            let fy = ((h >> 24) & 0xFFFF) as f64 / 65535.0;
            let fr = ((h >> 40) & 0xFFFF) as f64 / 65535.0;
            let (sx, sy) = ((gx as f64 + 0.1 + 0.8 * fx) * CELL, (gy as f64 + 0.1 + 0.8 * fy) * CELL);
            let r = 0.004 + 0.004 * fr;
            let q = ((u - sx).powi(2) + (v - sy).powi(2)) / (r * r);
            d = d.max((1.0 - q).clamp(0.0, 1.0));
        }
    }
    d
}

/// Visible sample (negative-like: orange base, R > G > B) at bed inches.
pub fn film_visible(u: f64, v: f64, c: usize) -> u16 {
    let t = 0.5 + 0.3 * (2.1 * u + 0.7).sin() * (1.7 * v).cos() + 0.15 * (7.3 * u + 3.1 * v).sin();
    let base = match c {
        0 => 14000.0 + 36000.0 * t,
        1 => 9000.0 + 28000.0 * t,
        _ => 5000.0 + 20000.0 * t,
    };
    (base * (1.0 - 0.75 * dust(u, v))).clamp(0.0, 65535.0) as u16
}

/// IR sample (dyes transparent, dust opaque) at bed inches.
pub fn film_infrared(u: f64, v: f64) -> u16 {
    (50000.0 * (1.0 - 0.85 * dust(u, v)) + 500.0 * (0.9 * u).sin()).clamp(1.0, 65534.0) as u16
}

fn synth_raster(scan: &ParsedParams, ir_offset: (i32, i32)) -> Vec<u8> {
    let (w, h) = (scan.area.width as usize, scan.area.height as usize);
    let d = f64::from(scan.dpi.max(1));
    let mut out = Vec::with_capacity(w * h * 6);
    let is_ir = scan.color == IR_COLOR_BYTE;
    for y in 0..h {
        let gy = f64::from(scan.area.y) + y as f64;
        if is_ir {
            // Pixel-interleaved: three IR sub-samples per pixel (mean = the IR value).
            for x in 0..w {
                let gx = f64::from(scan.area.x) + x as f64;
                let v = film_infrared((gx + f64::from(ir_offset.0)) / d, (gy + f64::from(ir_offset.1)) / d);
                for s in [v, v + 1, v - 1] {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        } else {
            // Band-interleaved by line: R[w] G[w] B[w].
            for c in 0..3 {
                for x in 0..w {
                    let gx = f64::from(scan.area.x) + x as f64;
                    out.extend_from_slice(&film_visible(gx / d, gy / d, c).to_le_bytes());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Dpi, FS_G, FS_W, PassKind, area_pixels, param_block};

    const T: Duration = Duration::from_millis(200);

    fn read_all(h: &mut SimHandle, n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        while v.len() < n {
            match h.bulk_in(n - v.len(), T) {
                Ok(d) => v.extend(d),
                Err(_) => break,
            }
        }
        v
    }

    #[test]
    fn device_model_wedges_on_oversized_area() {
        // Port of test_device_model_wedges_on_oversized_area: origin 0, 120x160 in @ 100 dpi.
        let sim = SimScanner::default();
        {
            let mut h = sim.open_handle();
            let mut p = [0u8; 64];
            p[0..4].copy_from_slice(&100u32.to_le_bytes());
            p[16..20].copy_from_slice(&4000u32.to_le_bytes());
            p[20..24].copy_from_slice(&16000u32.to_le_bytes());
            p[24] = 0x12;
            p[25] = 0x10;
            h.bulk_out(&FS_W, T).unwrap();
            assert_eq!(read_all(&mut h, 1), vec![ACK]);
            h.bulk_out(&p, T).unwrap();
            assert_eq!(read_all(&mut h, 1), vec![ACK]);
            h.bulk_out(&FS_G, T).unwrap();
            assert_eq!(h.bulk_in(14, T), Err(UsbError::Timeout));
        }
        assert!(sim.wedged());
        assert!(sim.wedge_reason().contains("film area"));
        let mut h = sim.open_handle();
        assert_eq!(h.bulk_out_counted(&[0x1B, 0x40], T), Ok(0)); // WriteFile times out while wedged
        drop(h);
        sim.power_cycle();
        assert!(!sim.wedged() && !sim.calibrated());
    }

    #[test]
    fn wide_fs_w_is_naked_not_wedged() {
        let sim = SimScanner::default();
        let mut h = sim.open_handle();
        let a = area_pixels(&crate::Region::new(0.0, 0.0, 1.0, 0.1).unwrap(), Dpi::new(4800).unwrap());
        h.bulk_out(&FS_W, T).unwrap();
        read_all(&mut h, 1);
        h.bulk_out(&param_block(&a, Dpi::new(4800).unwrap(), PassKind::Visible { high_speed: false }), T).unwrap();
        assert_eq!(read_all(&mut h, 1), vec![NAK]);
        drop(h);
        assert!(!sim.wedged());
    }

    #[test]
    fn cold_start_read_abandoned_wedges_and_ridden_out_arms() {
        for (wait, wedges) in [(Duration::from_secs(5), true), (Duration::from_secs(13), false)] {
            let sim = SimScanner::new(SimConfig { cold_start_fsg_latency: Duration::from_secs(12), ..SimConfig::default() });
            let mut h = sim.open_handle();
            let a = area_pixels(&crate::Region::new(0.0, 0.0, 0.6, 0.6).unwrap(), Dpi::new(200).unwrap());
            h.bulk_out(&FS_W, T).unwrap();
            read_all(&mut h, 1);
            h.bulk_out(&param_block(&a, Dpi::new(200).unwrap(), PassKind::Visible { high_speed: false }), T).unwrap();
            read_all(&mut h, 1);
            h.bulk_out(&FS_G, T).unwrap();
            assert_eq!(h.bulk_in(14, T), Err(UsbError::Timeout)); // silent
            sim.clock().advance(wait);
            if wedges {
                // Host gave up after 5 s and sends CAN: the device wedges.
                assert_eq!(h.bulk_out_counted(&[CAN], T), Ok(0));
                assert!(sim.wedged());
                assert!(!sim.calibrated());
            } else {
                let d = read_all(&mut h, 14);
                assert_eq!(&d[..2], &[0x02, 0x12]);
                assert!(sim.calibrated());
                // Read the whole raster then close cleanly.
                let start = crate::codec::StartResponse::parse(&d).unwrap().plan().unwrap();
                for i in 0..start.chunk_count() {
                    read_all(&mut h, start.chunk_len(i) as usize + 1);
                    if i + 1 != start.chunk_count() {
                        h.bulk_out(&[ACK], T).unwrap();
                    }
                }
                drop(h);
                assert!(!sim.wedged(), "{}", sim.wedge_reason());
            }
        }
    }

    #[test]
    fn closing_mid_raster_wedges() {
        let sim = SimScanner::default();
        let mut h = sim.open_handle();
        let a = area_pixels(&crate::Region::new(0.0, 0.0, 0.5, 0.5).unwrap(), Dpi::new(200).unwrap());
        h.bulk_out(&FS_W, T).unwrap();
        read_all(&mut h, 1);
        h.bulk_out(&param_block(&a, Dpi::new(200).unwrap(), PassKind::Visible { high_speed: false }), T).unwrap();
        read_all(&mut h, 1);
        sim.lock().lamp_warm = true;
        h.bulk_out(&FS_G, T).unwrap();
        read_all(&mut h, 14);
        read_all(&mut h, 100); // part of chunk 0
        assert_eq!(h.bulk_out_counted(&[0x1B, 0x46], T), Ok(0)); // back-pressured
        drop(h);
        assert!(sim.wedged());
        assert!(sim.wedge_reason().contains("mid-raster"));
    }

    #[test]
    fn synthetic_film_is_negative_like_and_dusty() {
        let px: Vec<u16> = (0..3).map(|c| film_visible(1.0, 1.0, c)).collect();
        assert!(px[0] > px[1] && px[1] > px[2]);
        let mut specks = 0;
        for i in 0..2000 {
            if film_infrared(i as f64 * 0.001, 0.37) < 40000 {
                specks += 1;
            }
        }
        assert!(specks > 0);
    }
}
