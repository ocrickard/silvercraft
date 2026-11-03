//! The flatbed workflow: a worker thread that owns the scanner session, takes [`ScannerCommand`]s
//! and reports [`ScannerEvent`]s, so a UI never blocks on USB.
//!
//! Prescan → (frame detection and editing in the UI) → scan each frame (visible, or dual-pass
//! with infrared) → captures and `roll.json` in the roll folder → [`crate::roll`] develops them.
//!
//! Cancelling goes through the driver's [`CancelToken`], which stops at a safe point; dropping
//! the worker cancels and waits for the scanner to be idle again. A session is never torn down
//! mid-transfer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use silvercraft_epson::session::{Phase, PrescanOptions, Preview, ScanRequest, Session};
use silvercraft_epson::{CancelToken, Dpi, EpsonError, FrameBox, FramePlanOptions, HandleOpener, plan_frames_px};

use crate::manifest::{CaptureSource, ManifestFrame, RollManifest, now_unix};
use crate::tiffio::{write_gray16, write_rgb16};

/// What to scan after the prescan.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanJob {
    /// Frames in preview pixels (from detection, edited by the user).
    pub frames: Vec<FrameBox>,
    /// Size and resolution of the preview the boxes refer to.
    pub preview_size: (u32, u32),
    pub preview_dpi: u32,
    /// Scan resolution.
    pub dpi: u32,
    /// Dual pass with infrared (for dust repair).
    pub infrared: bool,
    /// Margin scanned around each frame, so the film base in the gaps can be measured.
    pub padding_mm: f64,
    /// Roll folder (captures and `roll.json` go here; an existing manifest is extended).
    pub dir: PathBuf,
    pub roll: String,
    pub stock: String,
}

#[derive(Clone, Debug)]
pub enum ScannerCommand {
    Status,
    Recover,
    Prescan(PrescanOptions),
    Scan(ScanJob),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // a handful per frame; boxing buys nothing
pub enum ScannerEvent {
    /// The session is open.
    Connected,
    /// The scanner is busy with a command (`what` names it).
    Busy {
        what: String,
    },
    /// The command finished; ready for another.
    Idle,
    Status {
        clean_idle: bool,
        text: String,
    },
    Recovered {
        clean_idle: bool,
        text: String,
    },
    Phase(Phase),
    Preview {
        preview: Arc<Preview>,
        path: PathBuf,
    },
    /// One frame captured and recorded in the manifest.
    FrameScanned {
        index: usize,
        total: usize,
        frame: ManifestFrame,
        manifest: PathBuf,
    },
    ScanFinished {
        manifest: PathBuf,
        frames: usize,
    },
    Cancelled,
    /// A failure; `power_cycle` means the scanner must be switched off and on before anything
    /// else (the session refuses further commands until then).
    Failed {
        message: String,
        power_cycle: bool,
    },
}

/// Handle to the worker thread.
///
/// Cancellation is generation-scoped: every command is stamped with the current
/// generation when queued, and [`cancel`](Self::cancel) both triggers the running
/// command's token and advances the generation. Queued commands from an older
/// generation are dropped at dequeue (reported [`ScannerEvent::Cancelled`]) instead
/// of starting — so disconnecting or dropping the worker can never leave a queued
/// prescan/scan to run on afterwards, while commands sent *after* a cancel run
/// normally.
pub struct ScannerWorker {
    tx: Option<Sender<(ScannerCommand, u64)>>,
    rx: Receiver<ScannerEvent>,
    cancel: CancelToken,
    generation: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl ScannerWorker {
    /// Start a worker. `open` runs on the worker thread and opens the session (real hardware:
    /// `V800::discover` + `V800::open`); `wake` is called after every event (a UI repaint).
    pub fn spawn<O, F>(open: F, work_dir: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> Self
    where
        O: HandleOpener + 'static,
        F: FnOnce() -> Result<Session<O>, EpsonError> + Send + 'static,
    {
        let (tx, cmd_rx) = channel::<(ScannerCommand, u64)>();
        let (ev_tx, rx) = channel::<ScannerEvent>();
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = generation.clone();
        let thread = std::thread::Builder::new()
            .name("silvercraft-scanner".into())
            .spawn(move || {
                let emit = |e: ScannerEvent| {
                    let _ = ev_tx.send(e);
                    wake();
                };
                let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open));
                let mut session = match opened {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        emit(failed(&e));
                        return;
                    }
                    Err(_) => {
                        emit(ScannerEvent::Failed { message: "opening the scanner crashed".into(), power_cycle: false });
                        return;
                    }
                };
                emit(ScannerEvent::Connected);
                while let Ok((cmd, stamp)) = cmd_rx.recv() {
                    if stamp != worker_generation.load(Ordering::SeqCst) {
                        // Queued before a cancel/disconnect: never start it.
                        emit(ScannerEvent::Cancelled);
                        emit(ScannerEvent::Idle);
                        continue;
                    }
                    // Safe: only commands stamped at the current generation reach
                    // here, so clearing the token cannot resurrect cancelled work.
                    token.reset();
                    emit(ScannerEvent::Busy { what: command_name(&cmd).into() });
                    let what = command_name(&cmd);
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(&mut session, cmd, &token, &work_dir, &emit);
                    }));
                    if ran.is_err() {
                        // The driver's abort guard has already cleaned up on the
                        // device (RAII, including panics); session state beyond
                        // that is unknown, so stop serving instead of pretending.
                        emit(ScannerEvent::Failed {
                            message: format!("the scanner worker crashed during {what}; disconnect and reconnect"),
                            power_cycle: false,
                        });
                        emit(ScannerEvent::Idle);
                        break;
                    }
                    emit(ScannerEvent::Idle);
                }
            })
            .ok();
        Self { tx: Some(tx), rx, cancel, generation, thread }
    }

    /// Queue a command (commands run one at a time, in order).
    pub fn send(&self, cmd: ScannerCommand) -> bool {
        let stamp = self.generation.load(Ordering::SeqCst);
        self.tx.as_ref().is_some_and(|t| t.send((cmd, stamp)).is_ok())
    }

    /// Stop the running command at the next safe point, and drop everything queued
    /// behind it. Commands sent after this call are unaffected.
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Events so far (non-blocking).
    pub fn poll(&self) -> Vec<ScannerEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }

    /// Block for the next event (tests and the CLI).
    pub fn recv(&self) -> Option<ScannerEvent> {
        self.rx.recv().ok()
    }

    /// Whether the worker thread is still running.
    pub fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for ScannerWorker {
    /// Cancel whatever is running, drop everything queued behind it, and wait for
    /// the session to finish cleanly.
    fn drop(&mut self) {
        self.cancel();
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn command_name(cmd: &ScannerCommand) -> &'static str {
    match cmd {
        ScannerCommand::Status => "status",
        ScannerCommand::Recover => "recovery",
        ScannerCommand::Prescan(_) => "prescan",
        ScannerCommand::Scan(_) => "scan",
    }
}

fn failed(e: &EpsonError) -> ScannerEvent {
    match e {
        EpsonError::Cancelled => ScannerEvent::Cancelled,
        e => ScannerEvent::Failed { message: e.to_string(), power_cycle: e.requires_power_cycle() },
    }
}

fn run<O: HandleOpener>(session: &mut Session<O>, cmd: ScannerCommand, cancel: &CancelToken, work_dir: &Path, emit: &dyn Fn(ScannerEvent)) {
    let mut progress = |p: Phase| emit(ScannerEvent::Phase(p));
    match cmd {
        ScannerCommand::Status => match session.status() {
            Ok(r) => emit(ScannerEvent::Status { clean_idle: r.clean_idle, text: format!("{:?}", r.status) }),
            Err(e) => emit(failed(&e)),
        },
        ScannerCommand::Recover => match session.recover() {
            Ok(r) => emit(ScannerEvent::Recovered { clean_idle: r.clean_idle, text: format!("{:?}", r.class) }),
            Err(e) => emit(failed(&e)),
        },
        ScannerCommand::Prescan(opts) => match session.prescan(&opts, cancel, &mut progress) {
            Ok(p) => {
                let path = work_dir.join("prescan.tif");
                if let Err(e) = std::fs::create_dir_all(work_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|()| write_rgb16(&path, p.image.width(), p.image.height(), p.image.data()).map_err(|e| e.to_string()))
                {
                    log::warn!("prescan not saved: {e}");
                }
                emit(ScannerEvent::Preview { preview: Arc::new(p), path });
            }
            Err(e) => emit(failed(&e)),
        },
        ScannerCommand::Scan(job) => scan(session, &job, cancel, emit),
    }
}

fn scan<O: HandleOpener>(session: &mut Session<O>, job: &ScanJob, cancel: &CancelToken, emit: &dyn Fn(ScannerEvent)) {
    let fail = |m: String| emit(ScannerEvent::Failed { message: m, power_cycle: false });
    let Ok(preview_dpi) = Dpi::new(job.preview_dpi) else { return fail(format!("bad preview dpi {}", job.preview_dpi)) };
    let opts = FramePlanOptions { padding_mm: job.padding_mm, target_dpi: Some(job.dpi), ..FramePlanOptions::default() };
    let plans = match plan_frames_px(job.preview_size.0, job.preview_size.1, preview_dpi, &job.frames, &opts) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return fail("no scannable frames".into()),
        Err(e) => return emit(failed(&e)),
    };
    if let Err(e) = std::fs::create_dir_all(&job.dir) {
        return fail(format!("{}: {e}", job.dir.display()));
    }
    // Absent manifest: a new roll. Unreadable manifest: stop before capturing —
    // starting empty here would overwrite the roll on the first frame's save.
    let mut manifest = match RollManifest::load_existing(&job.dir) {
        Ok(Some(m)) => m,
        Ok(None) => RollManifest::new(&job.roll, &job.stock, CaptureSource::Scanner),
        Err(e) => return fail(e.to_string()),
    };
    let total = plans.len();
    for (i, plan) in plans.iter().enumerate() {
        if cancel.is_cancelled() {
            return emit(ScannerEvent::Cancelled);
        }
        let req = ScanRequest { region: plan.region, dpi: plan.dpi, auto_calibrate: true };
        let mut progress = |p: Phase| emit(ScannerEvent::Phase(p));
        let rgb_path = job.dir.join(format!("{}.tif", plan.file_stem));
        let ir_path = job.dir.join(format!("{}.ir.tif", plan.file_stem));
        let (size, ir_offset, wrote) = if job.infrared {
            match session.scan_dual_ir(&req, cancel, &mut progress) {
                Ok(s) => {
                    let w = write_rgb16(&rgb_path, s.rgb.width(), s.rgb.height(), s.rgb.data())
                        .and_then(|()| write_gray16(&ir_path, s.ir.width(), s.ir.height(), s.ir.data()));
                    ((s.rgb.width(), s.rgb.height()), Some(s.registration.ir_offset), w)
                }
                Err(e) => return emit(failed(&e)),
            }
        } else {
            match session.scan_visible(&req, cancel, &mut progress) {
                Ok(s) => ((s.image.width(), s.image.height()), None, write_rgb16(&rgb_path, s.image.width(), s.image.height(), s.image.data())),
                Err(e) => return emit(failed(&e)),
            }
        };
        if let Err(e) = wrote {
            return fail(format!("could not save frame {}: {e}", plan.label));
        }
        // the detected frame (unpadded) in capture pixels
        let original = job.frames.get(plan.index - 1).map(|f| f.bounds);
        let frame_box = original.map(|b| {
            let k = f64::from(plan.dpi.get()) / f64::from(job.preview_dpi);
            let px = |v: u32, origin: u32, max: usize| ((f64::from(v.saturating_sub(origin)) * k).round() as usize).min(max);
            [
                px(b.x, plan.padded.x, size.0),
                px(b.y, plan.padded.y, size.1),
                px(b.x + b.width, plan.padded.x, size.0),
                px(b.y + b.height, plan.padded.y, size.1),
            ]
        });
        let rel = |p: &Path| p.file_name().map(PathBuf::from).unwrap_or_default();
        let frame = ManifestFrame {
            label: plan.label.clone(),
            capture: rel(&rgb_path),
            infrared: job.infrared.then(|| rel(&ir_path)),
            frame_box: frame_box.filter(|b| b[2] > b[0] && b[3] > b[1]),
            capture_size: Some(size),
            dpi: Some(plan.dpi.get()),
            region_in: Some([plan.region.left(), plan.region.top(), plan.region.width(), plan.region.height()]),
            ir_offset,
            captured_at: Some(now_unix()),
            rotation: Default::default(),
            crop: None,
        };
        // The manifest may have changed under us (a crop set in the frame viewer
        // while later frames scan): merge disk state in instead of overwriting it.
        // If it became unreadable, stop — the captures so far stay on disk.
        match RollManifest::load_existing(&job.dir) {
            Ok(Some(disk)) => manifest.merge_disk(disk),
            Ok(None) => {}
            Err(e) => return fail(e.to_string()),
        }
        manifest.frames.retain(|f| f.label != frame.label);
        manifest.frames.push(frame.clone());
        match manifest.save(&job.dir) {
            Ok(path) => emit(ScannerEvent::FrameScanned { index: i, total, frame, manifest: path }),
            Err(e) => return fail(e.to_string()),
        }
    }
    emit(ScannerEvent::ScanFinished { manifest: job.dir.join(crate::manifest::FILE_NAME), frames: total });
}
