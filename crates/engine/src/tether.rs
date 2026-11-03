//! The camera-scanning workflow: a worker thread that owns a tethered camera, streams live view,
//! measures where the film frame sits, captures on request or automatically when a new frame has
//! been advanced and has settled, and records each capture in the roll's `roll.json`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use silvercraft_ptp::autocapture::{AutoCapture, AutoCaptureParams, AutoPhase, Rect, frame_signature};

use crate::manifest::{CaptureSource, ManifestFrame, RollManifest, now_unix};
use crate::rotation::Rotation;

/// A decoded live-view frame (8-bit RGB, interleaved).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

/// A finished capture's bytes went to the sink; this says what they were.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    /// File extension for the bytes (`arw`, `jpg`, …).
    pub extension: String,
    pub bytes: u64,
    /// A user-facing warning (e.g. the camera sent a JPEG, not a raw).
    pub warning: Option<String>,
}

/// What the tether workflow needs from a camera.
pub trait TetherCamera {
    fn model(&self) -> String;
    /// One live-view frame, JPEG-encoded.
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String>;
    /// Take a picture and write the file to `sink`.
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String>;
    /// End the session cleanly.
    fn close(self: Box<Self>) -> Result<(), String>;
    /// The exposure settings as the camera has them (empty: it doesn't say).
    fn exposure(&mut self) -> Result<Vec<ExposureSetting>, String> {
        Ok(Vec::new())
    }
    /// Set one exposure control to `value` (one of its [`ExposureSetting::choices`]).
    fn set_exposure(&mut self, control: ExposureControl, value: i64) -> Result<(), String> {
        let _ = value;
        Err(format!("{} can't be set from here; set it on the camera", control.name()))
    }
}

/// An exposure control of a tethered camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExposureControl {
    /// Exposure program (manual, aperture priority, …); read only.
    Mode,
    Shutter,
    Aperture,
    Iso,
    Compensation,
}

impl ExposureControl {
    pub fn name(self) -> &'static str {
        match self {
            Self::Mode => "Mode",
            Self::Shutter => "Shutter",
            Self::Aperture => "Aperture",
            Self::Iso => "ISO",
            Self::Compensation => "Exposure comp.",
        }
    }
}

/// One exposure control's state.
#[derive(Clone, Debug, PartialEq)]
pub struct ExposureSetting {
    pub control: ExposureControl,
    /// The camera's raw value.
    pub value: i64,
    /// The value for people ("1/125", "f/5.6", "+0.3 EV").
    pub label: String,
    /// Values the camera accepts now, in its order (empty: none listed).
    pub choices: Vec<i64>,
    /// The camera takes changes to it now (e.g. compensation is off in manual).
    pub settable: bool,
}

impl ExposureSetting {
    /// The neighbouring choice: `+1` brighter-or-next in the camera's list, `-1` the other way.
    pub fn step(&self, by: i32) -> Option<i64> {
        let i = self.choices.iter().position(|v| *v == self.value)?;
        let j = i as i64 + i64::from(by);
        (0..self.choices.len() as i64).contains(&j).then(|| self.choices[j as usize])
    }
}

/// A Sony property value for people.
pub fn exposure_label(control: ExposureControl, v: i64) -> String {
    match control {
        ExposureControl::Mode => match v {
            1 => "Manual".into(),
            2 => "Program".into(),
            3 => "Aperture priority".into(),
            4 => "Shutter priority".into(),
            _ => format!("Mode 0x{v:04x}"),
        },
        ExposureControl::Shutter => {
            let (num, den) = ((v >> 16) & 0xffff, v & 0xffff);
            match (num, den) {
                (0, _) | (_, 0) => "Bulb".into(),
                (n, 1) => format!("{n}\""),
                (1, d) => format!("1/{d}"),
                (n, d) if n % d == 0 => format!("{}\"", n / d),
                (n, d) => format!("{:.1}\"", n as f64 / d as f64),
            }
        }
        ExposureControl::Aperture => format!("f/{:.1}", v as f64 / 100.0).replace(".0", ""),
        ExposureControl::Iso => {
            let iso = v & 0x00ff_ffff;
            if iso == 0x00ff_ffff { "Auto".into() } else { iso.to_string() }
        }
        ExposureControl::Compensation => {
            if v == 0 {
                "0 EV".into()
            } else {
                format!("{:+.1} EV", v as f64 / 1000.0)
            }
        }
    }
}

impl<T: silvercraft_usb::Transport, C: silvercraft_ptp::Clock> TetherCamera for silvercraft_ptp::sony::SonyCamera<T, C> {
    fn model(&self) -> String {
        self.ext_info().map(|i| format!("Sony (ext v{})", i.version)).unwrap_or_else(|| "Sony camera".into())
    }
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String> {
        self.live_view_frame().map(|f| f.jpeg).map_err(|e| e.to_string())
    }
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String> {
        use silvercraft_ptp::sony::ObjectKind;
        let r = silvercraft_ptp::sony::SonyCamera::capture(self, sink).map_err(|e| e.to_string())?;
        // The camera-supplied file name is hostile input: only a short alphanumeric
        // extension from it may reach the filesystem.
        let from_name = r
            .info
            .as_ref()
            .and_then(|i| Path::new(&i.filename).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()))
            .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
        let extension = from_name.unwrap_or_else(|| {
            match r.kind {
                ObjectKind::Raw => "arw",
                ObjectKind::Jpeg => "jpg",
                ObjectKind::Other => "bin",
            }
            .into()
        });
        Ok(Shot { extension, bytes: r.bytes, warning: r.warning })
    }
    fn close(self: Box<Self>) -> Result<(), String> {
        silvercraft_ptp::sony::SonyCamera::close(*self).map_err(|e| e.to_string())
    }
    fn exposure(&mut self) -> Result<Vec<ExposureSetting>, String> {
        use silvercraft_ptp::sony::props::PropForm;
        let snap = self.all_props().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (control, code) in SONY_EXPOSURE {
            let Some(p) = snap.get(code) else { continue };
            let Some(value) = prop_i64(&p.current) else { continue };
            let choices: Vec<i64> = match &p.form {
                PropForm::Enum { values, get_values } => {
                    let list = if values.is_empty() { get_values.as_deref().unwrap_or_default() } else { values.as_slice() };
                    list.iter().filter_map(prop_i64).collect()
                }
                PropForm::Range { min, max, step } => match (prop_i64(min), prop_i64(max), prop_i64(step)) {
                    (Some(a), Some(b), Some(s)) if s > 0 && (b - a) / s < 512 => (a..=b).step_by(s as usize).collect(),
                    _ => Vec::new(),
                },
                PropForm::None => Vec::new(),
            };
            let settable = control != ExposureControl::Mode && p.get_set != 0 && p.is_enabled == 1 && choices.len() > 1;
            out.push(ExposureSetting { control, value, label: exposure_label(control, value), choices, settable });
        }
        Ok(out)
    }
    fn set_exposure(&mut self, control: ExposureControl, value: i64) -> Result<(), String> {
        use silvercraft_ptp::sony::props::{PropValue, datatype};
        let code = SONY_EXPOSURE.iter().find(|(c, _)| *c == control).map(|(_, code)| *code).ok_or("not a Sony control")?;
        let snap = self.all_props().map_err(|e| e.to_string())?;
        let dt = snap.get(code).map(|p| p.datatype).ok_or_else(|| format!("the camera doesn't list {}", control.name()))?;
        let signed = matches!(dt, datatype::INT8 | datatype::INT16 | datatype::INT32 | datatype::INT64);
        let v = if signed { PropValue::Int(value) } else { PropValue::UInt(value.max(0) as u64) };
        self.set_ext_prop(code, dt, &v).map_err(|e| format!("{}: {e}", control.name()))
    }
}

/// Sony property codes of the exposure controls, in display order.
const SONY_EXPOSURE: [(ExposureControl, u16); 5] = {
    use silvercraft_ptp::sony::props::code;
    [
        (ExposureControl::Mode, code::EXPOSURE_PROGRAM),
        (ExposureControl::Shutter, code::SHUTTER_SPEED),
        (ExposureControl::Aperture, code::F_NUMBER),
        (ExposureControl::Iso, code::ISO),
        (ExposureControl::Compensation, code::EXPOSURE_COMPENSATION),
    ]
};

fn prop_i64(v: &silvercraft_ptp::sony::props::PropValue) -> Option<i64> {
    use silvercraft_ptp::sony::props::PropValue;
    match *v {
        PropValue::Int(i) => Some(i),
        PropValue::UInt(u) => i64::try_from(u).ok(),
        _ => None,
    }
}

/// Where the film frame is in a live image, and whether the measurement is trustworthy.
pub type FrameMeasure = Box<dyn FnMut(&LiveImage) -> (Option<Rect>, bool) + Send>;

/// The whole image, always confident: autocapture by image stability alone.
pub fn whole_frame_measure() -> FrameMeasure {
    Box::new(|img: &LiveImage| (Some(Rect::new(0, 0, img.width as i32, img.height as i32)), true))
}

/// Decode a JPEG to 8-bit RGB (pure Rust).
pub fn decode_jpeg(bytes: &[u8]) -> Result<LiveImage, String> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let opts =
        DecoderOptions::default().set_max_width(1 << 14).set_max_height(1 << 14).set_strict_mode(false).jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    let rgb = d.decode().map_err(|e| e.to_string())?;
    let info = d.info().ok_or("JPEG without a header")?;
    let (width, height) = (info.width as usize, info.height as usize);
    if width == 0 || height == 0 || rgb.len() < width * height * 3 {
        return Err("JPEG decoded short".into());
    }
    Ok(LiveImage { width, height, rgb })
}

#[derive(Clone, Debug)]
pub enum TetherCommand {
    LiveView(bool),
    /// Capture now.
    Capture,
    AutoCapture(bool),
    /// "This is a new frame": clear autocapture's duplicate guard.
    NewFrame,
    /// Where captures go.
    SetRoll {
        dir: PathBuf,
        roll: String,
        stock: String,
    },
    /// The film frame set by hand, in live-view pixels (it replaces detection until `None`).
    SetFrame(Option<Rect>),
    /// How captures are turned to stand upright (recorded with each one).
    SetRotation(Rotation),
    /// Set one exposure control (the new settings follow as [`TetherEvent::Exposure`]).
    SetExposure(ExposureControl, i64),
}

/// Autocapture state for display.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoStatus {
    pub phase: AutoPhase,
    pub message: String,
    /// Waiting for an advance for a long time: offer manual capture.
    pub stuck: bool,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // a handful per frame; boxing buys nothing
pub enum TetherEvent {
    Connected {
        model: String,
    },
    Live {
        image: Arc<LiveImage>,
        rect: Option<Rect>,
        auto: Option<AutoStatus>,
    },
    /// The camera's exposure settings (after connecting, after a change, and every few seconds).
    Exposure(Vec<ExposureSetting>),
    Captured {
        frame: ManifestFrame,
        path: PathBuf,
        manifest: PathBuf,
    },
    Warning(String),
    Failed(String),
    Disconnected,
}

struct Roll {
    dir: PathBuf,
    manifest: RollManifest,
}

/// Handle to the tether worker.
pub struct TetherWorker {
    tx: Option<Sender<TetherCommand>>,
    rx: Receiver<TetherEvent>,
    thread: Option<JoinHandle<()>>,
}

/// Live-view pacing: at most this many frames a second (the camera, USB and decode cap it lower).
const MAX_FPS: f64 = 15.0;
const SIGNATURE_SIZE: usize = 32;
/// How often the exposure settings are read again (they change on the camera's dials too).
const EXPOSURE_EVERY: Duration = Duration::from_secs(3);

impl TetherWorker {
    /// Start a worker. `open` runs on the worker thread; `measure` finds the film frame in live
    /// view; `wake` is called after every event.
    pub fn spawn<F>(open: F, mut measure: FrameMeasure, params: AutoCaptureParams, wake: Arc<dyn Fn() + Send + Sync>) -> Self
    where
        F: FnOnce() -> Result<Box<dyn TetherCamera>, String> + Send + 'static,
    {
        let (tx, cmd_rx) = channel::<TetherCommand>();
        let (ev_tx, rx) = channel::<TetherEvent>();
        let thread = std::thread::Builder::new()
            .name("silvercraft-tether".into())
            .spawn(move || {
                let emit = |e: TetherEvent| {
                    let _ = ev_tx.send(e);
                    wake();
                };
                let mut cam = match open() {
                    Ok(c) => c,
                    Err(e) => return emit(TetherEvent::Failed(e)),
                };
                emit(TetherEvent::Connected { model: cam.model() });
                let t0 = Instant::now();
                let mut live = false;
                let mut auto: Option<AutoCapture> = None;
                let mut roll: Option<Roll> = None;
                let mut capture_now = false;
                let mut last_rect = None;
                let mut last_live: Option<Arc<LiveImage>> = None;
                let mut manual: Option<Rect> = None;
                let mut rotation = Rotation::NONE;
                let mut exposure_at: Option<Instant> = Some(Instant::now());
                let mut exposure_failed = false;
                'run: loop {
                    // commands: block briefly when idle, drain when streaming
                    let first = if live || capture_now {
                        match cmd_rx.try_recv() {
                            Ok(c) => Some(c),
                            Err(TryRecvError::Empty) => None,
                            Err(TryRecvError::Disconnected) => break 'run,
                        }
                    } else {
                        match cmd_rx.recv_timeout(Duration::from_millis(200)) {
                            Ok(c) => Some(c),
                            Err(RecvTimeoutError::Timeout) => None,
                            Err(RecvTimeoutError::Disconnected) => break 'run,
                        }
                    };
                    let mut pending: Vec<TetherCommand> = first.into_iter().collect();
                    while let Ok(c) = cmd_rx.try_recv() {
                        pending.push(c);
                    }
                    for c in pending {
                        match c {
                            TetherCommand::LiveView(on) => live = on,
                            TetherCommand::Capture => capture_now = true,
                            TetherCommand::AutoCapture(on) => auto = on.then(|| AutoCapture::new(params)),
                            TetherCommand::NewFrame => {
                                if let Some(a) = auto.as_mut() {
                                    a.release_hold();
                                }
                            }
                            TetherCommand::SetRoll { dir, roll: name, stock } => {
                                // Absent: a new roll. Unreadable: refuse the folder —
                                // starting empty would overwrite the roll on the
                                // first capture's save.
                                match RollManifest::load_existing(&dir) {
                                    Ok(Some(manifest)) => roll = Some(Roll { dir, manifest }),
                                    Ok(None) => roll = Some(Roll { dir, manifest: RollManifest::new(name, stock, CaptureSource::Camera) }),
                                    Err(e) => {
                                        roll = None;
                                        emit(TetherEvent::Failed(e.to_string()));
                                    }
                                }
                            }
                            TetherCommand::SetFrame(r) => manual = r.filter(|r| r.width > 0 && r.height > 0),
                            TetherCommand::SetRotation(r) => rotation = r,
                            TetherCommand::SetExposure(control, value) => {
                                if let Err(e) = cam.set_exposure(control, value) {
                                    emit(TetherEvent::Warning(e));
                                }
                                // read back what the camera made of it, now and once it has settled
                                exposure_at = Some(Instant::now());
                                exposure_failed = false;
                            }
                        }
                    }

                    if !exposure_failed && exposure_at.is_some_and(|t| t <= Instant::now()) {
                        match cam.exposure() {
                            Ok(s) if s.is_empty() => exposure_at = None,
                            Ok(s) => {
                                exposure_at = Some(Instant::now() + EXPOSURE_EVERY);
                                emit(TetherEvent::Exposure(s));
                            }
                            Err(e) => {
                                // not again until asked: a camera that can't say shouldn't slow live view
                                exposure_failed = true;
                                emit(TetherEvent::Warning(format!("reading the exposure: {e}")));
                            }
                        }
                    }

                    if live {
                        let started = Instant::now();
                        match cam.live_view_jpeg().and_then(|j| decode_jpeg(&j)) {
                            Ok(img) => {
                                let (rect, confident) = match manual {
                                    Some(r) => (Some(clamp_rect(r, &img)), true),
                                    None => measure(&img),
                                };
                                last_rect = rect;
                                let status = auto.as_mut().map(|a| {
                                    let sig = frame_signature(&img.rgb, img.width, img.height, rect, SIGNATURE_SIZE);
                                    let d = a.update(rect, sig.as_ref(), t0.elapsed().as_secs_f64(), confident);
                                    if d.should_fire {
                                        capture_now = true;
                                        a.notify_captured(sig.as_ref(), rect);
                                    }
                                    AutoStatus { phase: a.phase(), message: d.message, stuck: d.stuck }
                                });
                                let img = Arc::new(img);
                                last_live = Some(img.clone());
                                emit(TetherEvent::Live { image: img, rect, auto: status });
                            }
                            Err(e) => emit(TetherEvent::Warning(format!("live view: {e}"))),
                        }
                        let min = Duration::from_secs_f64(1.0 / MAX_FPS);
                        if let Some(rest) = min.checked_sub(started.elapsed()) {
                            std::thread::sleep(rest);
                        }
                    }

                    if capture_now {
                        capture_now = false;
                        match roll.as_mut() {
                            None => emit(TetherEvent::Warning("choose a roll folder before capturing".into())),
                            Some(r) => match capture(cam.as_mut(), r, last_rect, last_live.as_deref(), rotation) {
                                Ok((frame, path, manifest, warning)) => {
                                    if let Some(w) = warning {
                                        emit(TetherEvent::Warning(w));
                                    }
                                    emit(TetherEvent::Captured { frame, path, manifest });
                                }
                                Err(e) => emit(TetherEvent::Failed(e)),
                            },
                        }
                    }
                }
                if let Err(e) = cam.close() {
                    emit(TetherEvent::Warning(format!("closing the camera: {e}")));
                }
                emit(TetherEvent::Disconnected);
            })
            .ok();
        Self { tx: Some(tx), rx, thread }
    }

    pub fn send(&self, cmd: TetherCommand) -> bool {
        self.tx.as_ref().is_some_and(|t| t.send(cmd).is_ok())
    }

    pub fn poll(&self) -> Vec<TetherEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            out.push(e);
        }
        out
    }

    pub fn recv(&self) -> Option<TetherEvent> {
        self.rx.recv().ok()
    }

    pub fn recv_timeout(&self, d: Duration) -> Option<TetherEvent> {
        self.rx.recv_timeout(d).ok()
    }

    /// Whether the worker thread is still running.
    pub fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for TetherWorker {
    /// Stop streaming and close the camera session before returning.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `r` kept within `img`.
fn clamp_rect(r: Rect, img: &LiveImage) -> Rect {
    let (w, h) = (img.width as i32, img.height as i32);
    let x0 = r.x.clamp(0, w - 1);
    let y0 = r.y.clamp(0, h - 1);
    let x1 = (r.x + r.width).clamp(x0 + 1, w);
    let y1 = (r.y + r.height).clamp(y0 + 1, h);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

fn capture(
    cam: &mut dyn TetherCamera,
    roll: &mut Roll,
    rect: Option<Rect>,
    live: Option<&LiveImage>,
    rotation: Rotation,
) -> Result<(ManifestFrame, PathBuf, PathBuf, Option<String>), String> {
    std::fs::create_dir_all(&roll.dir).map_err(|e| format!("{}: {e}", roll.dir.display()))?;
    // Frames may have been changed since (a crop set in the frame viewer): keep what's
    // on disk. If the manifest became unreadable, stop before shooting — saving our
    // stale copy over it would destroy the roll.
    if let Some(m) = RollManifest::load_existing(&roll.dir).map_err(|e| e.to_string())? {
        roll.manifest = m;
    }
    let n = roll.manifest.frames.len() + 1;
    let label = (n..).map(|i| format!("{i:03}")).find(|l| roll.manifest.frames.iter().all(|f| &f.label != l)).unwrap_or_default();
    let tmp = roll.dir.join(format!(".{label}.capture.tmp"));
    let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
    let shot = cam.capture(&mut file);
    let file = file.into_inner().map_err(|e| e.to_string())?;
    let shot = match shot {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    let name = format!("{label}.{}", shot.extension);
    let path = roll.dir.join(&name);
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    // the frame box in live-view pixels; the develop step scales it to the decoded raw
    let frame_box = match (rect, live) {
        (Some(r), Some(img)) if r.width > 0 && r.height > 0 => {
            let c = |v: i32, max: usize| (v.max(0) as usize).min(max);
            Some([c(r.x, img.width), c(r.y, img.height), c(r.x + r.width, img.width), c(r.y + r.height, img.height)])
        }
        _ => None,
    };
    let frame = ManifestFrame {
        label,
        capture: PathBuf::from(name),
        infrared: None,
        frame_box: frame_box.filter(|b| b[2] > b[0] && b[3] > b[1]),
        capture_size: live.map(|i| (i.width, i.height)),
        dpi: None,
        region_in: None,
        ir_offset: None,
        captured_at: Some(now_unix()),
        rotation,
        crop: None,
    };
    roll.manifest.frames.push(frame.clone());
    let manifest = roll.manifest.save(&roll.dir).map_err(|e| e.to_string())?;
    Ok((frame, path, manifest, shot.warning))
}
