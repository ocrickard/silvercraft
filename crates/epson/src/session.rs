//! The scanner session: preflight/postflight gates, calibration tracking, recovery, and the
//! production visible, dual-pass infrared and bed-prescan jobs (with wide-scan tiling).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use silvercraft_usb::Transport;

use crate::codec::{
    DeviceArea, Dpi, LutChannel, MAX_SINGLE_READ_WIDTH_PX, PREVIEW_DPI, PRIMING_CYCLES, PassKind, Region, Status, TILE_OVERLAP_PX, area_pixels, hex,
    identity_lut, param_block,
};
use crate::handle::{HandleJob, HandleOutcome, extended_handshake, postflight, priming_cycle, run_ir_handle, run_scan_handle};
use crate::io::{FS_G_TIMEOUT_FLOOR, Io};
use crate::opener::{CloneOpener, FoundScanner, NusbOpener, discover_scanners, platform_supported};
use crate::raster::{
    GrayImage, RgbImage, Seam, assemble_bil, collapse_ir, concat_columns, level_match_columns, stitch_overlapped, transmission_ir_offset,
};
use crate::recovery::{PostflightGate, RecoveryClass, RecoveryOptions, RecoveryReport, postflight_gate, recover_on_handle, status_probe};
use crate::{CancelToken, Clock, EpsonError, HandleOpener, SystemClock};

/// Which optical pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Visible RGB.
    Visible,
    /// Infrared.
    Infrared,
}

/// Progress events for a GUI. Callbacks never touch the transport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Status probe before a scan.
    Preflight,
    /// Software recovery is running.
    Recovering,
    /// Preview priming cycle `cycle` of `of`.
    Priming {
        /// 1-based cycle.
        cycle: usize,
        /// Total cycles.
        of: usize,
    },
    /// Sending the scan-handle command sequence.
    Setup {
        /// Pass.
        pass: Pass,
        /// 0-based tile.
        tile: usize,
        /// Tile count.
        tiles: usize,
    },
    /// FS G answered busy: lamp warm-up / calibration.
    WarmingUp {
        /// Pass.
        pass: Pass,
        /// Busy polls so far.
        poll: u32,
        /// 0-based tile.
        tile: usize,
        /// Tile count.
        tiles: usize,
    },
    /// Raster transfer progress.
    Reading {
        /// Pass.
        pass: Pass,
        /// Image bytes read.
        bytes_done: u64,
        /// Image bytes planned.
        bytes_total: u64,
        /// 0-based tile.
        tile: usize,
        /// Tile count.
        tiles: usize,
    },
    /// FS S / ESC # IR-arming handshake.
    Handshake {
        /// 0-based tile.
        tile: usize,
    },
    /// Postflight status.
    Postflight,
    /// Building the image.
    Assembling,
    /// Stitching tiles.
    Stitching,
    /// Measuring the IR↔visible residual offset.
    Registering,
}

/// Session settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionConfig {
    /// Per-operation USB timeout. Production entry points use 30 s; FS G start polls never use
    /// less than 30 s regardless.
    pub timeout: Duration,
    /// FS G polls for scans (240).
    pub poll_attempts: u32,
    /// Delay between FS G polls (500 ms).
    pub poll_delay: Duration,
    /// Delay between FS G polls in a dual pass (200 ms).
    pub dual_poll_delay: Duration,
    /// FS G polls for the prescan (60).
    pub prescan_poll_attempts: u32,
    /// Widest single read before tiling (≤ 4096; lower only for tests).
    pub max_single_read_width_px: u32,
    /// Refuse to start (before FS G) when the pre-start ESC F fails the vendor gate.
    pub enforce_pre_start_gate: bool,
    /// Recovery attempts made by [`Session::recover`].
    pub recovery_attempts: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            poll_attempts: 240,
            poll_delay: Duration::from_millis(500),
            dual_poll_delay: Duration::from_millis(200),
            prescan_poll_attempts: 60,
            max_single_read_width_px: MAX_SINGLE_READ_WIDTH_PX,
            enforce_pre_start_gate: true,
            recovery_attempts: 3,
        }
    }
}

/// A targeted scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanRequest {
    /// Region (inches, validated against the 8×10 in film area).
    pub region: Region,
    /// Resolution.
    pub dpi: Dpi,
    /// When the carriage is not known to be calibrated this power cycle: run the origin
    /// prescan first (`true`) or refuse with [`EpsonError::NotCalibrated`] (`false`).
    pub auto_calibrate: bool,
}

/// The bed prescan; always at origin (0,0), which is what calibrates the carriage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrescanOptions {
    /// Resolution (200).
    pub dpi: Dpi,
    /// Width in inches (5.9).
    pub width_in: f64,
    /// Height in inches (9.7).
    pub height_in: f64,
}

impl Default for PrescanOptions {
    fn default() -> Self {
        Self { dpi: PREVIEW_DPI, width_in: 5.9, height_in: 9.7 }
    }
}

/// Per-pass record (what a scan manifest stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassMeta {
    /// 0-based tile.
    pub tile: usize,
    /// Pass.
    pub pass: Pass,
    /// Device geometry.
    pub area: DeviceArea,
    /// Ready FS G reply.
    pub start_response: Vec<u8>,
    /// FS G polls.
    pub poll_attempts: u32,
    /// Chunks read.
    pub chunks: usize,
    /// Chunks whose trailer was not 0x00 (recorded, never validated in production).
    pub nonzero_trailers: usize,
    /// Image bytes received.
    pub raster_bytes: u64,
    /// `w*h*6`.
    pub expected_bytes: u64,
    /// Pre-start ESC F reply.
    pub pre_start: Vec<u8>,
    /// FS F reply.
    pub fs_f: Vec<u8>,
    /// ESC f reply.
    pub esc_f: Vec<u8>,
}

/// A visible scan.
#[derive(Debug, Clone, PartialEq)]
pub struct Scan {
    /// 16-bit RGB, raw linear scanner values (identity LUTs; "scanner-rgb").
    pub image: RgbImage,
    /// Resolution.
    pub dpi: Dpi,
    /// Requested region.
    pub region: Region,
    /// Tiles captured.
    pub tiles: usize,
    /// Per-pass records.
    pub passes: Vec<PassMeta>,
    /// Postflight of the last handle.
    pub postflight: PostflightGate,
}

/// The bed preview from the origin prescan.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// 16-bit RGB.
    pub image: RgbImage,
    /// Preview resolution.
    pub dpi: Dpi,
    /// Bed region covered (origin 0,0).
    pub region: Region,
    /// Per-pass records.
    pub passes: Vec<PassMeta>,
}

/// IR ↔ visible registration.
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    /// Both passes used the identical FS W geometry in one carriage session (layer 1).
    pub same_geometry: bool,
    /// Measured residual IR→visible translation `(dx, dy)` in pixels (layer 2); apply it to the
    /// IR defect signal only — the planes are never shifted here.
    pub ir_offset: (i32, i32),
    /// Wide-scan seams; each seam's RGB-derived shift was applied to the IR plane too (layer 3).
    pub seams: Vec<Seam>,
}

/// A dual-pass (visible + infrared) scan.
#[derive(Debug, Clone, PartialEq)]
pub struct DualScan {
    /// 16-bit RGB.
    pub rgb: RgbImage,
    /// 16-bit IR plane on the same grid.
    pub ir: GrayImage,
    /// Resolution.
    pub dpi: Dpi,
    /// Requested region.
    pub region: Region,
    /// Registration metadata.
    pub registration: Registration,
    /// Tiles captured.
    pub tiles: usize,
    /// Per-pass records.
    pub passes: Vec<PassMeta>,
    /// Postflight of the last IR handle.
    pub postflight: PostflightGate,
}

/// Result of [`Session::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// The ESC F reply.
    pub status: Status,
    /// `02 12 00 00`.
    pub clean_idle: bool,
}

/// What the session knows about carriage calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calibration {
    /// An origin scan completed on this enumeration of the device.
    Calibrated,
    /// Unknown: a targeted scan needs the origin prescan first.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Unknown,
    Clean,
    NeedsRecovery,
}

/// Process-wide record of device enumerations whose carriage an origin scan calibrated.
/// Keyed by [`HandleOpener::enumeration_id`], which changes when the device re-enumerates
/// (power cycle / replug), so a stale entry can never match a freshly powered scanner.
static CALIBRATED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn registry() -> std::sync::MutexGuard<'static, Vec<String>> {
    match CALIBRATED.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Entry points for real hardware.
#[derive(Debug, Clone, Copy)]
pub struct V800;

impl V800 {
    /// Enumerate attached V800/V850s from cached descriptors (no I/O to the device).
    pub fn discover() -> Result<Vec<FoundScanner>, EpsonError> {
        discover_scanners()
    }

    /// A session for `found`. Does no I/O: each operation opens and closes its own handles.
    pub fn open(found: &FoundScanner, config: SessionConfig) -> Result<Session<NusbOpener>, EpsonError> {
        platform_supported()?;
        Ok(Session::with_opener(NusbOpener::new(found.clone()), config))
    }
}

/// A scanner session. See the crate docs for the safety rules it enforces.
pub struct Session<O: HandleOpener> {
    opener: O,
    cfg: SessionConfig,
    clock: Arc<dyn Clock>,
    calibrated_here: bool,
    /// The enumeration `calibrated_here` was earned on; calibration is only trusted
    /// while the opener still reports that enumeration.
    calibrated_enum: Option<String>,
    /// The enumeration seen at the last handle open; a change invalidates calibration.
    seen_enum: Option<String>,
    health: Health,
    partial_tail_streak: u32,
    power_cycle: Option<String>,
}

impl<O: HandleOpener> std::fmt::Debug for Session<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("config", &self.cfg).field("health", &self.health).field("power_cycle", &self.power_cycle).finish()
    }
}

impl<T: Transport + Clone> Session<CloneOpener<T>> {
    /// A session whose "handles" are clones of `transport`, on the system clock.
    pub fn with_transport(transport: T) -> Self {
        Self::with_transport_and_clock(transport, Arc::new(SystemClock::new()), SessionConfig::default())
    }
    /// As [`Session::with_transport`] with an explicit clock (tests use a virtual one).
    pub fn with_transport_and_clock(transport: T, clock: Arc<dyn Clock>, config: SessionConfig) -> Self {
        Self::with_opener(CloneOpener::new(transport, clock), config)
    }
}

type Progress<'a> = &'a mut dyn FnMut(Phase);

struct PassJob {
    region: Region,
    dpi: Dpi,
    high_speed: bool,
    poll_attempts: u32,
    poll_delay: Duration,
}

fn is_fs_g_failure(e: &EpsonError) -> bool {
    e.to_string().contains("FS G")
}

/// Split a scan into single-read column tiles (§3.10, §4.5). `overlap` is 0 for visible-only
/// scans and 64 for dual passes. the tile count is raised until every
/// overlapped tile also fits the single-read limit (the original `tile_px + 64` could reach
/// 4160 px, which the firmware NAKs).
pub fn plan_tiles(region: &Region, dpi: Dpi, max_width_px: u32, overlap: u32) -> Result<Vec<Region>, EpsonError> {
    let full = area_pixels(region, dpi).width;
    let max_w = max_width_px.clamp(64, MAX_SINGLE_READ_WIDTH_PX);
    if full <= max_w {
        return Ok(vec![*region]);
    }
    if overlap >= max_w {
        return Err(EpsonError::InvalidRequest("tile overlap exceeds the single-read width".into()));
    }
    let mut n = full.div_ceil(max_w);
    while full.div_ceil(n) + overlap > max_w {
        n += 1;
    }
    let tile = full.div_ceil(n);
    let d = f64::from(dpi.get());
    let mut out = Vec::new();
    for k in 0..n {
        let px0 = k * tile;
        let px1 = ((k + 1) * tile + overlap).min(full);
        if px1 <= px0 {
            break;
        }
        let left = region.left() + f64::from(px0) / d;
        out.push(Region::new(left, region.top(), f64::from(px1 - px0) / d, region.height())?);
    }
    Ok(out)
}

impl<O: HandleOpener> Session<O> {
    /// A session over any handle source (simulator, recording wrapper, ...).
    pub fn with_opener(opener: O, config: SessionConfig) -> Self {
        let clock = opener.clock();
        let mut cfg = config;
        cfg.max_single_read_width_px = cfg.max_single_read_width_px.clamp(64, MAX_SINGLE_READ_WIDTH_PX);
        cfg.poll_attempts = cfg.poll_attempts.max(1);
        cfg.prescan_poll_attempts = cfg.prescan_poll_attempts.max(1);
        Self {
            opener,
            cfg,
            clock,
            calibrated_here: false,
            calibrated_enum: None,
            seen_enum: None,
            health: Health::Unknown,
            partial_tail_streak: 0,
            power_cycle: None,
        }
    }

    /// The handle source.
    pub fn opener(&self) -> &O {
        &self.opener
    }

    /// The effective configuration.
    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }

    /// Carriage calibration as far as the driver can know.
    pub fn calibration(&self) -> Calibration {
        if self.is_calibrated() { Calibration::Calibrated } else { Calibration::Unknown }
    }

    /// `Some(reason)` once the scanner needs a physical power cycle (scans are refused until a
    /// later [`Session::recover`] finds it clean idle).
    pub fn power_cycle_required(&self) -> Option<&str> {
        self.power_cycle.as_deref()
    }

    fn is_calibrated(&self) -> bool {
        let current = self.opener.enumeration_id();
        (self.calibrated_here && self.calibrated_enum == current) || current.is_some_and(|id| registry().contains(&id))
    }

    fn mark_calibrated(&mut self) {
        self.calibrated_here = true;
        self.calibrated_enum = self.opener.enumeration_id();
        self.seen_enum = self.calibrated_enum.clone();
        if let Some(id) = &self.calibrated_enum {
            let mut r = registry();
            if !r.contains(id) {
                r.push(id.clone());
            }
        }
    }

    fn invalidate_calibration(&mut self) {
        self.calibrated_here = false;
        self.calibrated_enum = None;
        if let Some(id) = self.opener.enumeration_id() {
            registry().retain(|k| *k != id);
        }
    }

    fn note_error(&mut self, e: &EpsonError) {
        self.health = Health::Unknown;
        match e {
            EpsonError::Disconnected(_) => self.invalidate_calibration(),
            EpsonError::PowerCycleRequired { reason } => {
                self.invalidate_calibration();
                self.power_cycle = Some(reason.clone());
            }
            other if is_fs_g_failure(other) => self.invalidate_calibration(),
            _ => {}
        }
    }

    fn open_io(&mut self, cancel: Option<&CancelToken>) -> Result<Io<O::Handle>, EpsonError> {
        // A different enumeration than last time (power cycle / replug) means the
        // carriage calibration earned earlier belongs to a previous power cycle.
        let current = self.opener.enumeration_id();
        if self.seen_enum.is_some() && self.seen_enum != current {
            self.invalidate_calibration();
        }
        self.seen_enum = current;
        match self.opener.open() {
            Ok(t) => Ok(Io::new(t, self.cfg.timeout, self.clock.clone(), cancel.cloned())),
            Err(e) => {
                // A failed open is exactly how a power cycle first shows up; never
                // carry calibration across it.
                self.invalidate_calibration();
                Err(e)
            }
        }
    }

    fn refuse_if_wedged(&self) -> Result<(), EpsonError> {
        match &self.power_cycle {
            Some(reason) => Err(EpsonError::PowerCycleRequired { reason: reason.clone() }),
            None => Ok(()),
        }
    }

    /// One status probe on a fresh handle: `W 1B 46`, `R 4` (timeout ≤ 1.5 s).
    pub fn status(&mut self) -> Result<StatusReport, EpsonError> {
        let r = (|| {
            let mut io = self.open_io(None)?;
            status_probe(&mut io)
        })();
        match r {
            Ok(status) => {
                let clean_idle = status.is_clean_idle();
                if clean_idle && self.health != Health::NeedsRecovery {
                    self.health = Health::Clean;
                }
                Ok(StatusReport { status, clean_idle })
            }
            Err(e) => {
                self.note_error(&e);
                Err(e)
            }
        }
    }

    /// Bounded software recovery (§6.3): up to `recovery_attempts` runs, each on a fresh
    /// handle (`fast_if_idle` on the first), stopping at clean idle. Two consecutive
    /// partial-native-tail runs, or no clean idle at all, → [`EpsonError::PowerCycleRequired`].
    pub fn recover(&mut self) -> Result<RecoveryReport, EpsonError> {
        let mut last: Option<RecoveryReport> = None;
        let mut streak = 0;
        for attempt in 0..self.cfg.recovery_attempts.max(1) {
            let opts = RecoveryOptions { fast_if_idle: attempt == 0, ..RecoveryOptions::default() };
            let report = match self.open_io(None) {
                Ok(mut io) => recover_on_handle(&mut io, &opts),
                Err(e) => {
                    self.note_error(&e);
                    return Err(e);
                }
            };
            if report.clean_idle {
                self.health = Health::Clean;
                self.power_cycle = None;
                self.partial_tail_streak = 0;
                return Ok(report);
            }
            if report.class == RecoveryClass::PartialNativeTail {
                streak += 1;
                if streak >= 2 {
                    let e = EpsonError::PowerCycleRequired { reason: "recovery saw repeated partial native tails".into() };
                    self.note_error(&e);
                    return Err(e);
                }
            } else {
                streak = 0;
            }
            last = Some(report);
        }
        let reason = match last {
            Some(r) => format!("software recovery could not reach clean idle (last status {})", hex(&r.final_status)),
            None => "software recovery did not run".into(),
        };
        let e = EpsonError::PowerCycleRequired { reason };
        self.note_error(&e);
        Err(e)
    }

    /// Ready gate (§6.4 `ensure_epson_direct_b8_ready`): status probe; if not clean idle,
    /// one recovery; if still not clean, power-cycle required.
    fn ensure_ready(&mut self, progress: Progress<'_>) -> Result<(), EpsonError> {
        progress(Phase::Preflight);
        let probe = self.open_io(None).and_then(|mut io| status_probe(&mut io));
        if let Ok(s) = &probe
            && s.is_clean_idle()
        {
            self.health = Health::Clean;
            return Ok(());
        }
        if let Err(EpsonError::Disconnected(m)) = &probe {
            return Err(EpsonError::Disconnected(m.clone()));
        }
        progress(Phase::Recovering);
        let mut io = self.open_io(None)?;
        let report = recover_on_handle(&mut io, &RecoveryOptions::default());
        drop(io);
        if report.clean_idle {
            self.health = Health::Clean;
            return Ok(());
        }
        Err(EpsonError::PowerCycleRequired {
            reason: format!("scanner pipe is wedged and software recovery could not clear it (status {})", hex(&report.final_status)),
        })
    }

    fn apply_gate(&mut self, gate: &PostflightGate) {
        if gate.safe_to_continue {
            self.health = Health::Clean;
            self.partial_tail_streak = 0;
        } else {
            self.health = Health::NeedsRecovery;
            if gate.requires_power_cycle_if_repeated {
                self.partial_tail_streak += 1;
                if self.partial_tail_streak >= 2 {
                    self.invalidate_calibration();
                    self.power_cycle = Some(format!("postflight status repeatedly failed ({})", gate.reason));
                }
            }
        }
    }

    fn prime(&mut self, cancel: &CancelToken, progress: Progress<'_>) -> Result<(), EpsonError> {
        for i in 0..PRIMING_CYCLES {
            if cancel.is_cancelled() {
                return Err(EpsonError::Cancelled);
            }
            progress(Phase::Priming { cycle: i + 1, of: PRIMING_CYCLES });
            let mut io = self.open_io(Some(cancel))?;
            priming_cycle(&mut io)?;
        }
        Ok(())
    }

    /// Calibration preflight for a scan whose first tile has `first` geometry.
    fn require_calibration(&mut self, first: &DeviceArea, auto: bool, cancel: &CancelToken, progress: Progress<'_>) -> Result<(), EpsonError> {
        if first.at_origin() || self.is_calibrated() {
            return Ok(());
        }
        if !auto {
            return Err(EpsonError::NotCalibrated);
        }
        self.prescan_inner(&PrescanOptions::default(), cancel, progress).map(|_| ())
    }

    fn job(
        &self,
        pass: Pass,
        params: [u8; 64],
        luts: &[[u8; 257]],
        poll_attempts: u32,
        poll_delay: Duration,
        tile: usize,
        tiles: usize,
    ) -> HandleJobOwned {
        HandleJobOwned { pass, params, luts: luts.to_vec(), poll_attempts, poll_delay, enforce: self.cfg.enforce_pre_start_gate, tile, tiles }
    }

    /// The origin bed prescan (§5.1): 5.9 × 9.7 in at 200 dpi, high speed, origin (0,0). It
    /// homes and calibrates the carriage; afterwards targeted scans are allowed until the
    /// scanner re-enumerates.
    pub fn prescan(&mut self, opts: &PrescanOptions, cancel: &CancelToken, progress: Progress<'_>) -> Result<Preview, EpsonError> {
        let r = self.prescan_inner(opts, cancel, progress);
        if let Err(e) = &r {
            self.note_error(e);
        }
        r
    }

    fn prescan_inner(&mut self, opts: &PrescanOptions, cancel: &CancelToken, progress: Progress<'_>) -> Result<Preview, EpsonError> {
        self.refuse_if_wedged()?;
        let region = Region::new(0.0, 0.0, opts.width_in, opts.height_in)?;
        let job = PassJob { region, dpi: opts.dpi, high_speed: true, poll_attempts: self.cfg.prescan_poll_attempts, poll_delay: self.cfg.poll_delay };
        let (image, passes, _gate) = self.visible_pipeline(&job, cancel, progress)?;
        self.mark_calibrated();
        Ok(Preview { image, dpi: opts.dpi, region, passes })
    }

    /// Production visible scan (§3), auto-tiled above the single-read width.
    pub fn scan_visible(&mut self, req: &ScanRequest, cancel: &CancelToken, progress: Progress<'_>) -> Result<Scan, EpsonError> {
        let r = self.scan_visible_inner(req, cancel, progress);
        if let Err(e) = &r {
            self.note_error(e);
        }
        r
    }

    fn scan_visible_inner(&mut self, req: &ScanRequest, cancel: &CancelToken, progress: Progress<'_>) -> Result<Scan, EpsonError> {
        self.refuse_if_wedged()?;
        let tiles = plan_tiles(&req.region, req.dpi, self.cfg.max_single_read_width_px, 0)?;
        let first = tiles.first().map(|t| area_pixels(t, req.dpi)).ok_or_else(|| EpsonError::InvalidRequest("empty scan".into()))?;
        self.require_calibration(&first, req.auto_calibrate, cancel, progress)?;
        let job =
            PassJob { region: req.region, dpi: req.dpi, high_speed: false, poll_attempts: self.cfg.poll_attempts, poll_delay: self.cfg.poll_delay };
        let (image, passes, postflight) = self.visible_pipeline(&job, cancel, progress)?;
        Ok(Scan { image, dpi: req.dpi, region: req.region, tiles: tiles.len(), passes, postflight })
    }

    fn visible_pipeline(
        &mut self,
        job: &PassJob,
        cancel: &CancelToken,
        progress: Progress<'_>,
    ) -> Result<(RgbImage, Vec<PassMeta>, PostflightGate), EpsonError> {
        let tiles = plan_tiles(&job.region, job.dpi, self.cfg.max_single_read_width_px, 0)?;
        self.ensure_ready(progress)?;
        self.prime(cancel, progress)?;
        let luts = [identity_lut(LutChannel::Red), identity_lut(LutChannel::Green), identity_lut(LutChannel::Blue)];
        let n = tiles.len();
        let mut cols = Vec::with_capacity(n);
        let mut passes = Vec::with_capacity(n);
        let mut last_gate = None;
        for (k, tile) in tiles.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(EpsonError::Cancelled);
            }
            let area = area_pixels(tile, job.dpi);
            if !area.at_origin() && !self.is_calibrated() {
                return Err(EpsonError::NotCalibrated);
            }
            let params = param_block(&area, job.dpi, PassKind::Visible { high_speed: job.high_speed });
            let hj = self.job(Pass::Visible, params, &luts, job.poll_attempts, job.poll_delay, k, n);
            let mut io = self.open_io(Some(cancel))?;
            let outcome = run_scan_handle(&mut io, &hj.borrow(), progress);
            progress(Phase::Postflight);
            let pf = postflight(&mut io);
            drop(io);
            let gate = postflight_gate(&pf);
            self.apply_gate(&gate);
            let outcome = outcome?;
            let meta = pass_meta(k, Pass::Visible, area, &outcome);
            let raster = finish_pass(outcome)?;
            if area.at_origin() {
                self.mark_calibrated();
            }
            progress(Phase::Assembling);
            cols.push(assemble_bil(&raster, area.width as usize, area.height as usize)?);
            passes.push(meta);
            if k + 1 < n && !gate.safe_to_continue {
                return Err(EpsonError::Protocol(format!("scanner not clean idle after tile {k} (status {}); stopping", gate.status_hex)));
            }
            last_gate = Some(gate);
        }
        let image = if cols.len() == 1 {
            cols.pop().unwrap_or_else(RgbImage::empty)
        } else {
            progress(Phase::Stitching);
            concat_columns(&level_match_columns(cols))
        };
        let gate = last_gate.ok_or_else(|| EpsonError::Protocol("no tiles captured".into()))?;
        Ok((image, passes, gate))
    }

    /// Registered dual-pass visible + infrared scan (§4), auto-tiled with a 64 px overlap.
    pub fn scan_dual_ir(&mut self, req: &ScanRequest, cancel: &CancelToken, progress: Progress<'_>) -> Result<DualScan, EpsonError> {
        let r = self.scan_dual_inner(req, cancel, progress);
        if let Err(e) = &r {
            self.note_error(e);
        }
        r
    }

    fn scan_dual_inner(&mut self, req: &ScanRequest, cancel: &CancelToken, progress: Progress<'_>) -> Result<DualScan, EpsonError> {
        self.refuse_if_wedged()?;
        let tiles = plan_tiles(&req.region, req.dpi, self.cfg.max_single_read_width_px, TILE_OVERLAP_PX)?;
        let first = tiles.first().map(|t| area_pixels(t, req.dpi)).ok_or_else(|| EpsonError::InvalidRequest("empty scan".into()))?;
        self.require_calibration(&first, req.auto_calibrate, cancel, progress)?;
        self.ensure_ready(progress)?;
        self.prime(cancel, progress)?;
        let vis_luts = [identity_lut(LutChannel::Red), identity_lut(LutChannel::Green), identity_lut(LutChannel::Blue)];
        let ir_luts = [identity_lut(LutChannel::Master)];
        let n = tiles.len();
        let (mut rgb_cols, mut ir_cols, mut passes) = (Vec::new(), Vec::new(), Vec::new());
        let mut last_gate = None;
        for (k, tile) in tiles.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(EpsonError::Cancelled);
            }
            // Both passes use this one geometry: registered planes by construction.
            let area = area_pixels(tile, req.dpi);
            if !area.at_origin() && !self.is_calibrated() {
                return Err(EpsonError::NotCalibrated);
            }
            // --- handle 1: visible + IR-arming handshake (no postflight on this handle) ---
            let vj = self.job(
                Pass::Visible,
                param_block(&area, req.dpi, PassKind::Visible { high_speed: false }),
                &vis_luts,
                self.cfg.poll_attempts,
                self.cfg.dual_poll_delay,
                k,
                n,
            );
            let mut io = self.open_io(Some(cancel))?;
            let mut vis = run_scan_handle(&mut io, &vj.borrow(), progress)?;
            let Some(proof) = vis.complete.take() else {
                drop(io);
                // Starting IR now would ignore Cancel and can stall FS G indefinitely.
                return Err(match vis.raster_error.take() {
                    Some(EpsonError::Cancelled) => EpsonError::Cancelled,
                    Some(e) => EpsonError::VisibleIncomplete(e.to_string()),
                    None => EpsonError::VisibleIncomplete("raster incomplete".into()),
                });
            };
            if area.at_origin() {
                self.mark_calibrated();
            }
            progress(Phase::Handshake { tile: k });
            let armed = extended_handshake(&mut io, proof);
            drop(io);
            let armed = armed?;
            if cancel.is_cancelled() {
                // The visible pass finished; do not start the IR pass after a Cancel.
                return Err(EpsonError::Cancelled);
            }
            // --- handle 2: IR pass, consuming the arming token ---
            let ij = self.job(
                Pass::Infrared,
                param_block(&area, req.dpi, PassKind::Infrared),
                &ir_luts,
                self.cfg.poll_attempts,
                self.cfg.dual_poll_delay,
                k,
                n,
            );
            let mut io = self.open_io(Some(cancel))?;
            let ir = run_ir_handle(&mut io, armed, &ij.borrow(), progress);
            progress(Phase::Postflight);
            let pf = postflight(&mut io);
            drop(io);
            let gate = postflight_gate(&pf);
            self.apply_gate(&gate);
            let ir = ir?;
            passes.push(pass_meta(k, Pass::Visible, area, &vis));
            passes.push(pass_meta(k, Pass::Infrared, area, &ir));
            let vis_raster = vis.raster;
            let ir_raster = finish_pass(ir)?;
            progress(Phase::Assembling);
            let (w, h) = (area.width as usize, area.height as usize);
            rgb_cols.push(assemble_bil(&vis_raster, w, h)?);
            ir_cols.push(collapse_ir(&ir_raster, w, h)?);
            if k + 1 < n && !gate.safe_to_continue {
                return Err(EpsonError::Protocol(format!("scanner not clean idle after tile {k} (status {}); stopping", gate.status_hex)));
            }
            last_gate = Some(gate);
        }
        let (rgb, ir, seams) = if rgb_cols.len() == 1 {
            let rgb = rgb_cols.pop().unwrap_or_else(RgbImage::empty);
            let ir = ir_cols.pop().unwrap_or_else(GrayImage::empty);
            (rgb, ir, vec![])
        } else {
            progress(Phase::Stitching);
            stitch_overlapped(rgb_cols, ir_cols, TILE_OVERLAP_PX as usize)
        };
        progress(Phase::Registering);
        let ir_offset = transmission_ir_offset(&rgb, &ir, 8);
        let postflight = last_gate.ok_or_else(|| EpsonError::Protocol("no tiles captured".into()))?;
        Ok(DualScan {
            rgb,
            ir,
            dpi: req.dpi,
            region: req.region,
            registration: Registration { same_geometry: true, ir_offset, seams },
            tiles: n,
            passes,
            postflight,
        })
    }
}

struct HandleJobOwned {
    pass: Pass,
    params: [u8; 64],
    luts: Vec<[u8; 257]>,
    poll_attempts: u32,
    poll_delay: Duration,
    enforce: bool,
    tile: usize,
    tiles: usize,
}

impl HandleJobOwned {
    fn borrow(&self) -> HandleJob<'_> {
        HandleJob {
            pass: self.pass,
            params: self.params,
            luts: &self.luts,
            poll_attempts: self.poll_attempts,
            poll_delay: self.poll_delay,
            enforce_pre_start_gate: self.enforce,
            tile: self.tile,
            tiles: self.tiles,
        }
    }
}

fn pass_meta(tile: usize, pass: Pass, area: DeviceArea, o: &HandleOutcome) -> PassMeta {
    PassMeta {
        tile,
        pass,
        area,
        start_response: o.start.map(|s| s.raw.to_vec()).unwrap_or_default(),
        poll_attempts: o.poll_attempts,
        chunks: o.trailers.len(),
        nonzero_trailers: o.trailers.iter().filter(|&&t| t != 0).count(),
        raster_bytes: o.raster.len() as u64,
        expected_bytes: area.raster_bytes(),
        pre_start: o.pre_start.clone(),
        fs_f: o.fs_f.clone(),
        esc_f: o.esc_f.clone(),
    }
}

/// A completed pass's raster, or the error that stopped it.
fn finish_pass(mut o: HandleOutcome) -> Result<Vec<u8>, EpsonError> {
    if o.complete.is_none() {
        return Err(o.raster_error.take().unwrap_or_else(|| EpsonError::Raster("raster incomplete".into())));
    }
    Ok(o.raster)
}

/// FS G start polls never time out sooner than this (re-exported for documentation/tests).
pub const FS_G_MIN_TIMEOUT: Duration = FS_G_TIMEOUT_FLOOR;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_tiles_abut_and_cover_the_width() {
        let r = Region::new(0.0, 0.0, 4.0, 0.5).unwrap();
        let d = Dpi::new(200).unwrap();
        let t = plan_tiles(&r, d, 400, 0).unwrap();
        assert_eq!(t.len(), 2);
        let w: Vec<u32> = t.iter().map(|x| area_pixels(x, d).width).collect();
        assert_eq!(w, vec![400, 400]);
        assert_eq!(area_pixels(&t[1], d).x, 400);
        // Narrow scans are not tiled.
        assert_eq!(plan_tiles(&r, d, 4096, 0).unwrap().len(), 1);
    }

    #[test]
    fn overlapped_tiles_never_exceed_the_single_read_limit() {
        // Naive: 8192 px -> 2 tiles of 4096 + 64 = 4160 px (NAK). Here: 3 tiles.
        let d = Dpi::new(1024).unwrap();
        let r = Region::new(0.0, 0.0, 8.0, 0.1).unwrap();
        let t = plan_tiles(&r, d, 4096, 64).unwrap();
        assert!(t.len() >= 3);
        for tile in &t {
            assert!(area_pixels(tile, d).width <= 4096);
        }
        let last = t.last().unwrap();
        let a = area_pixels(last, d);
        assert!(a.x + a.width >= 8192 - 8);
        // 4.0 in @ 200 with cap 400 and overlap 64: 3 tiles of <= 400.
        let t = plan_tiles(&Region::new(0.0, 0.0, 4.0, 0.5).unwrap(), Dpi::new(200).unwrap(), 400, 64).unwrap();
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|x| area_pixels(x, Dpi::new(200).unwrap()).width <= 400));
    }
}
