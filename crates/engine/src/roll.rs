//! Develop a roll: captures on disk → scene-linear DNGs → LightCraft.
//!
//! Two passes keep one full frame in memory at a time: pass 1 decodes each capture, measures its
//! film base in the gaps around the frame and keeps a subsample; the roll calibration is fitted
//! from those; pass 2 decodes each capture again, develops it and writes its DNG.
//!
//! On a camera roll, pass 1 also finds each capture's film frame alone. The confident frames give
//! the roll's frame size (a copy stand keeps it fixed); the captures that were not confident, or
//! disagree with that size, are decoded again, framed with it, and their film base and samples
//! measured again before the roll model is fitted. Pass 2 uses the boxes pass 1 settled on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::json;
use silvercraft_config::{PipelineConfig, Resolved};
use silvercraft_decode::{Capture, CaptureKind};
use silvercraft_detect::{CameraFrame, fit_roll_prior, is_confident, needs_prior};
use silvercraft_film::base::{FilmBaseSampleOptions, sample_film_base};
use silvercraft_film::diag::Diagnostic;
use silvercraft_film::pipeline::{RollModel, RollSamples, develop_frame, fit_roll};
use silvercraft_film::profiles::{Capture as ProfileCapture, FilmProfile};
use silvercraft_film::{RgbImage, SceneFrame};
use silvercraft_handoff::config::{TemplateVars, expand};
use silvercraft_handoff::write::{XmpPlacement, write_frame_with};
use silvercraft_handoff::{Delivered, DeliveryOptions, DngFrameInfo, LinearFrame, XmpPacket, deliver};

use crate::context::{ConfigSources, is_camera_scan};
use crate::{EngineError, Result};

/// One frame to develop: where its capture is and where the frame sits in it.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSource {
    pub path: PathBuf,
    pub label: String,
    /// `x0, y0, x1, y1` of the exposed frame in the capture; `None` = the whole capture.
    pub frame_box: Option<[usize; 4]>,
    /// Capture size the box was measured on, when known (boxes scale if the decode differs).
    pub box_space: Option<(usize, usize)>,
    /// A separate infrared capture registered to this one (Epson dual-pass scans).
    pub ir_path: Option<PathBuf>,
    /// Scan resolution, when known (dust repair sizes scale with it).
    pub dpi: Option<u32>,
    /// The scanner's own infrared → visible offset, the fallback when dust repair can't
    /// measure one.
    pub ir_offset: Option<(i32, i32)>,
    /// How the frame is turned to stand upright (the DNG's orientation).
    pub rotation: crate::rotation::Rotation,
    /// The picture cut out of the capture, set by hand: `x0, y0, x1, y1` as fractions of the
    /// capture (not turned). Wins over the frame box, which still places the film base.
    pub crop: Option<[f64; 4]>,
}

impl FrameSource {
    /// A whole capture, labelled by its file name.
    pub fn whole(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let label = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Self { path, label, frame_box: None, box_space: None, ir_path: None, dpi: None, ir_offset: None, rotation: Default::default(), crop: None }
    }
}

/// A roll to develop.
#[derive(Clone, Debug)]
pub struct RollRequest {
    pub roll: String,
    /// Stock profile id.
    pub stock: String,
    pub frames: Vec<FrameSource>,
    pub sources: ConfigSources,
}

/// Progress of a roll, in order (more kinds may be added).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// Config and profile findings (warnings, notes, and errors before an `InvalidConfig`).
    Diagnostic(Diagnostic),
    /// The config layers in effect, lowest first.
    Config { layers: Vec<String> },
    /// Pass 1 measured a frame.
    Measured { index: usize, total: usize, label: String, capture: (usize, usize), frame: (usize, usize), gap_base: Option<[f64; 3]> },
    /// Pass 1 framed a camera capture again with the roll's frame size (it was not found
    /// confidently alone, or its size disagreed with the roll's) and measured it again; this
    /// replaces its `Measured`. Sent after every frame's `Measured`.
    Reframed { index: usize, total: usize, label: String, frame: (usize, usize), gap_base: Option<[f64; 3]>, confident: bool },
    /// The roll model is fitted.
    Fitted { summary: String },
    /// Pass 2 wrote a frame.
    Developed {
        index: usize,
        total: usize,
        label: String,
        exposure_ev: f64,
        neutral: [f64; 3],
        clipped_fraction: f64,
        path: PathBuf,
        /// A small display rendering of the frame (for thumbnails; LightCraft does the real one).
        preview: Option<std::sync::Arc<Thumbnail>>,
    },
    /// All frames written.
    Finished { frames: usize, seconds: f64 },
    /// The handoff to LightCraft finished.
    Delivered { message: String },
    /// Scanner line/banding repair and the scan's health, for a scanner capture.
    ScanChecked { label: String, lines_repaired: usize, banding_repaired: bool, warnings: Vec<String> },
    /// Dust and scratch repair ran on a frame.
    Healed { label: String, coverage_pct: f64, defects: usize, line_scratches: usize, ir_shift: Option<(f32, f32)>, warnings: Vec<String> },
}

/// An 8-bit sRGB display thumbnail.
#[derive(Clone, Debug, PartialEq)]
pub struct Thumbnail {
    pub width: usize,
    pub height: usize,
    /// Interleaved RGB.
    pub rgb: Vec<u8>,
}

impl Thumbnail {
    /// Turned to stand upright.
    pub fn rotated(self, r: crate::rotation::Rotation) -> Self {
        let (width, height, rgb) = r.pixels(self.width, self.height, 3, &self.rgb);
        Thumbnail { width, height, rgb }
    }
}

/// Longest side of a [`Thumbnail`].
pub const THUMBNAIL_SIDE: usize = 384;

/// A quick display rendering of a scene-linear frame: its exposure and white balance applied, a
/// soft highlight roll-off, sRGB encoding.
pub fn thumbnail(scene: &SceneFrame) -> Option<Thumbnail> {
    let img = &scene.image;
    display_rendering(img.width(), img.height(), img.data(), scene.meta.baseline_exposure_ev, scene.meta.as_shot_neutral, THUMBNAIL_SIDE)
}

/// Display rendering of scene-linear RGB (`width * height * 3`) at `exposure_ev` and white
/// balance `as_shot_neutral`, box-downsampled so the longer side is at most `max_side`.
pub fn display_rendering(
    width: usize,
    height: usize,
    data: &[f32],
    exposure_ev: f64,
    as_shot_neutral: [f64; 3],
    max_side: usize,
) -> Option<Thumbnail> {
    let (w, h) = (width, height);
    if data.len() < w * h * 3 {
        return None;
    }
    let step = w.max(h).div_ceil(max_side.max(1)).max(1);
    let (tw, th) = (w / step, h / step);
    if tw == 0 || th == 0 {
        return None;
    }
    let gain = 2f64.powf(exposure_ev);
    let wb: Vec<f64> = as_shot_neutral.iter().map(|n| if *n > 1e-6 { gain / n } else { gain }).collect();
    let mut rgb = Vec::with_capacity(tw * th * 3);
    for y in 0..th {
        for x in 0..tw {
            for (c, k) in wb.iter().enumerate() {
                let mut s = 0.0f64;
                for yy in 0..step {
                    for xx in 0..step {
                        let v = f64::from(data[((y * step + yy) * w + x * step + xx) * 3 + c]);
                        s += if v.is_finite() { v.max(0.0) } else { 0.0 };
                    }
                }
                rgb.push(display_encode(s / (step * step) as f64 * k));
            }
        }
    }
    Some(Thumbnail { width: tw, height: th, rgb })
}

/// A scene-linear value for the screen: Reinhard-style shoulder above mid-tones, then sRGB.
fn display_encode(v: f64) -> u8 {
    let t = v * (1.0 + v / 4.0) / (1.0 + v);
    let e = if t <= 0.003_130_8 { t * 12.92 } else { 1.055 * t.powf(1.0 / 2.4) - 0.055 };
    (e.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// What a finished roll produced.
#[derive(Clone, Debug, PartialEq)]
pub struct RollOutcome {
    pub dir: PathBuf,
    pub written: Vec<PathBuf>,
    pub delivered: Option<String>,
}

fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(EngineError::Cancelled) } else { Ok(()) }
}

/// The frame box in decoded-capture pixels (scaled when the box was measured on a different
/// size), clamped to the capture; the whole capture when there is no usable box.
pub fn frame_box(src: &FrameSource, w: usize, h: usize) -> [usize; 4] {
    let Some([x0, y0, x1, y1]) = src.frame_box else { return [0, 0, w, h] };
    let (sx, sy) = match src.box_space {
        Some((bw, bh)) if bw > 0 && bh > 0 && (bw, bh) != (w, h) => (w as f64 / bw as f64, h as f64 / bh as f64),
        _ => (1.0, 1.0),
    };
    let s = |v: usize, k: f64, max: usize| ((v as f64 * k).round() as usize).min(max);
    let b = [s(x0, sx, w), s(y0, sy, h), s(x1, sx, w), s(y1, sy, h)];
    if b[2] > b[0] && b[3] > b[1] { b } else { [0, 0, w, h] }
}

/// A crop (fractions of the capture) in pixels of a `w × h` capture; `None` when it holds no
/// pixels.
pub fn crop_box(c: [f64; 4], w: usize, h: usize) -> Option<[usize; 4]> {
    let px = |v: f64, max: usize| ((v.clamp(0.0, 1.0) * max as f64).round() as usize).min(max);
    let b = [px(c[0], w), px(c[1], h), px(c[2], w), px(c[3], h)];
    (b[2] > b[0] && b[3] > b[1]).then_some(b)
}

/// A decoded capture with its frame cut out.
pub struct Decoded {
    pub capture: Capture,
    pub frame: RgbImage,
    pub gap_base: Option<[f64; 3]>,
    /// The frame box in the capture.
    pub frame_box: [usize; 4],
    /// Scanner fixes and health (scanner captures).
    pub scan_report: Option<silvercraft_scanfix::ScanReport>,
}

/// Where the film frame of a camera capture is: its box in the capture and whether all four film
/// edges were seen (otherwise the box is the holder's gate opening, and what lies outside it is
/// not film base).
pub type FoundFrame = ([usize; 4], bool);

/// How [`decode_framed`] places the frame of a camera capture that has no box of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Framing {
    /// Detect it in this capture alone.
    Detect,
    /// Already found by the roll pass (`None`: no frame; the whole capture is used).
    Found(Option<FoundFrame>),
}

/// Decode a capture (and its separate infrared file) and cut out the frame.
pub fn decode(src: &FrameSource, config: &PipelineConfig) -> Result<Decoded> {
    decode_framed(src, config, Framing::Detect)
}

/// [`decode`], with the frame of a camera capture placed as `framing` says.
pub fn decode_framed(src: &FrameSource, config: &PipelineConfig, framing: Framing) -> Result<Decoded> {
    let (capture, scan_report) = decode_capture(src, config)?;
    let found = match framing {
        Framing::Found(f) => f,
        Framing::Detect if detects_frame(src, &capture) => crate::frames::camera_frame_box(&capture.rgb, &config.detect),
        Framing::Detect => None,
    };
    cut(src, config, capture, scan_report, found)
}

/// A camera capture without a box: its film frame is found by detection (a scanner capture is
/// already the frame).
fn detects_frame(src: &FrameSource, capture: &Capture) -> bool {
    src.frame_box.is_none() && matches!(capture.kind, CaptureKind::Camera { .. })
}

/// Decode a capture and its separate infrared file, and repair scanner defects.
fn decode_capture(src: &FrameSource, config: &PipelineConfig) -> Result<(Capture, Option<silvercraft_scanfix::ScanReport>)> {
    let mut capture = silvercraft_decode::decode_file(&src.path, &config.decode).map_err(|e| format!("{}: {e}", src.path.display()))?;
    let (w, h) = (capture.rgb.width(), capture.rgb.height());
    if let Some(ir) = &src.ir_path
        && config.decode.infrared == silvercraft_decode::Infrared::Use
    {
        let bytes = silvercraft_decode::read_capped(ir).map_err(|e| format!("{}: {e}", ir.display()))?;
        capture.ir = Some(silvercraft_decode::tiff_scan::decode_ir(&bytes, w, h).map_err(|e| format!("{}: {e}", ir.display()))?);
    }
    // scanner line defects and banding, on the whole capture (the infrared plane too)
    let scan_report = match capture.kind {
        CaptureKind::Scanner if config.scanfix.enabled => {
            let ir = capture.ir.as_deref_mut();
            Some(
                silvercraft_scanfix::repair_interleaved(w, h, capture.rgb.data_mut(), ir, &config.scanfix)
                    .map_err(|e| format!("{}: scanner fixes: {e}", src.label))?,
            )
        }
        _ => None,
    };
    Ok((capture, scan_report))
}

/// Cut the frame out of a decoded capture: at `found` (a detected camera frame) or the source's
/// box, and sample the film base around it when the box is bounded by film edges.
fn cut(
    src: &FrameSource,
    config: &PipelineConfig,
    capture: Capture,
    scan_report: Option<silvercraft_scanfix::ScanReport>,
    found: Option<FoundFrame>,
) -> Result<Decoded> {
    let (w, h) = (capture.rgb.width(), capture.rgb.height());
    let [x0, y0, x1, y1] = found.map(|f| f.0).unwrap_or_else(|| frame_box(src, w, h));
    let edges = found.filter(|f| f.1).map(|f| f.0);
    let gap_base = match (src.frame_box.or(edges), FilmBaseSampleOptions::from_config(&config.film.base, y0, y1)) {
        (Some(_), Some(o)) => sample_film_base(&capture.rgb, &o),
        _ => None,
    };
    let [x0, y0, x1, y1] = src.crop.and_then(|c| crop_box(c, w, h)).unwrap_or([x0, y0, x1, y1]);
    let frame = capture.rgb.crop(x0, y0, x1, y1).map_err(|e| e.to_string())?;
    Ok(Decoded { capture, frame, gap_base, frame_box: [x0, y0, x1, y1], scan_report })
}

/// The fallback infrared shift for dust repair: a configured one wins; otherwise the scanner's
/// measured offset, negated (the driver reports where the visible lies relative to the IR, dust
/// repair where the IR lies relative to the visible).
pub fn ir_shift_hint(configured: [f64; 2], scanner: Option<(i32, i32)>) -> [f64; 2] {
    match scanner {
        Some((dx, dy)) if configured == [0.0, 0.0] => [-f64::from(dx), -f64::from(dy)],
        _ => configured,
    }
}

/// Repair dust and scratches on the whole capture (the negative, before inversion, so the
/// margins around the frame can feed the fill) and cut the frame out again. `None` when repair
/// is off or has nothing to work from (no infrared, no visible-only speck detection and no
/// long-scratch repair).
pub fn heal(src: &FrameSource, d: &mut Decoded, config: &PipelineConfig) -> Result<Option<Event>> {
    let h = &config.heal;
    if !h.enabled || (d.capture.ir.is_none() && !h.no_ir_detect && !h.scratches) {
        return Ok(None);
    }
    let mut cfg = h.clone();
    if cfg.dpi.is_none() {
        cfg.dpi = src.dpi.map(f64::from);
    }
    cfg.ir_shift = ir_shift_hint(cfg.ir_shift, src.ir_offset);
    let r = silvercraft_heal::heal_capture(&d.capture.rgb, d.capture.ir.as_deref(), &cfg).map_err(|e| format!("{}: dust repair: {e}", src.label))?;
    let [x0, y0, x1, y1] = d.frame_box;
    d.frame = r.image.crop(x0, y0, x1, y1).map_err(|e| e.to_string())?;
    d.capture.rgb = r.image;
    Ok(Some(Event::Healed {
        label: src.label.clone(),
        coverage_pct: r.stats.coverage_pct,
        defects: r.stats.defects,
        line_scratches: r.stats.line_scratches,
        ir_shift: r.stats.registration.filter(|g| g.estimated).map(|g| (g.dx, g.dy)),
        warnings: r.warnings,
    }))
}

/// Template values for one frame (`frame` empty for roll-level templates).
pub fn tvars<'a>(roll: &'a str, frame: &'a str, profile: &'a FilmProfile) -> TemplateVars<'a> {
    TemplateVars { roll, frame, stock: &profile.id, stock_name: &profile.name }
}

/// Develop `req` and hand it to LightCraft as configured. `cancel` is checked between frames.
pub fn develop_roll(req: &RollRequest, cancel: &AtomicBool, on: &mut dyn FnMut(Event)) -> Result<RollOutcome> {
    develop_roll_frames(req, None, cancel, on)
}

/// [`develop_roll`], writing only the frames labelled `only` (all when `None`). The whole roll
/// is still measured, so they develop as they would with the rest; nothing is sent to
/// LightCraft when only some are written.
pub fn develop_roll_frames(req: &RollRequest, only: Option<&[String]>, cancel: &AtomicBool, on: &mut dyn FnMut(Event)) -> Result<RollOutcome> {
    let mut diags = Vec::new();
    let reg = req.sources.registry();
    diags.extend(reg.diagnostics().iter().cloned());
    let camera_scan = req.frames.first().is_some_and(|f| is_camera_scan(&f.path));
    let capture = if camera_scan { ProfileCapture::Camera } else { ProfileCapture::Scanner };
    let profile =
        reg.get_for_capture(&req.stock, Some(capture)).ok_or_else(|| format!("unknown stock `{}` (see `silvercraft-cli profiles`)", req.stock));
    let profile = match profile {
        Ok(p) => p,
        Err(e) => {
            diags.into_iter().for_each(|d| on(Event::Diagnostic(d)));
            return Err(e.into());
        }
    };
    let resolved = req.sources.resolve_or_err(Some(profile), camera_scan, &mut diags);
    diags.into_iter().for_each(|d| on(Event::Diagnostic(d)));
    let resolved = resolved?;
    if req.frames.is_empty() {
        return Err("the roll has no frames".to_string().into());
    }
    let config = &resolved.config;
    let roll = req.roll.as_str();
    let out = config.output.dir_for(&tvars(roll, "", profile)).map_err(|e| format!("output.dir: {e}"))?;
    // Preflight: two frames whose labels slug to the same file stem ("1/2" and "1-2")
    // would overwrite each other's DNG while both report success. Refuse up front,
    // before anything is measured or written.
    {
        let mut stems: Vec<(String, &str)> = Vec::new();
        for src in &req.frames {
            if only.is_some_and(|o| !o.contains(&src.label)) {
                continue;
            }
            let stem = config.output.stem_for(&tvars(roll, &src.label, profile)).map_err(|e| format!("output.file_name: {e}"))?;
            if let Some((_, other)) = stems.iter().find(|(s, _)| *s == stem) {
                return Err(format!(
                    "frames `{other}` and `{}` both develop to {stem}.dng (output.file_name slugging); rename one frame or change output.file_name",
                    src.label
                )
                .into());
            }
            stems.push((stem, &src.label));
        }
    }
    on(Event::Config { layers: resolved.layers.clone() });
    let t0 = Instant::now();
    let total = req.frames.len();

    // pass 1 (film base and calibration), or the roll's model from its last develop when only
    // some frames are written again
    let saved = only.and_then(|_| saved_model(&out, profile, &config.film));
    let fresh = saved.is_none();
    let (model, framing) = match saved {
        Some(model) => {
            on(Event::Fitted { summary: format!("roll {roll}: as fitted when it was last developed ({})", out.join(ROLL_MODEL).display()) });
            (model, vec![Framing::Detect; total])
        }
        None => measure_roll(req, config, profile, cancel, on)?,
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    if config.output.roll_model && fresh {
        let doc = json!({"roll": roll, "stock": profile.id, "layers": resolved.layers, "config": config, "model": model});
        let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        silvercraft_handoff::write::write_atomic(&out.join(ROLL_MODEL), text.as_bytes()).map_err(|e| e.to_string())?;
    }

    // pass 2: develop and write each frame
    let mut written = Vec::new();
    for (i, src) in req.frames.iter().enumerate() {
        if only.is_some_and(|o| !o.contains(&src.label)) {
            continue;
        }
        check(cancel)?;
        let mut d = decode_framed(src, config, framing.get(i).copied().unwrap_or(Framing::Detect))?;
        if let Some(r) = &d.scan_report {
            let banding_repaired = r.banding.iter().any(|b| b.repaired);
            on(Event::ScanChecked {
                label: src.label.clone(),
                lines_repaired: r.line_defects.iter().filter(|l| l.repaired).count(),
                banding_repaired,
                warnings: r.warnings.iter().map(|w| w.message.clone()).collect(),
            });
        }
        if let Some(e) = heal(src, &mut d, config)? {
            on(e);
        }
        let scene = develop_frame(&model, &d.frame, Some(&src.label)).map_err(|e| format!("{}: {e}", src.label))?;
        let stem = config.output.stem_for(&tvars(roll, &src.label, profile)).map_err(|e| format!("output.file_name: {e}"))?;
        let path = write_dng(&out, &stem, roll, src, &d.capture, &scene, &model, profile, &resolved)?;
        on(Event::Developed {
            index: i,
            total,
            label: src.label.clone(),
            exposure_ev: scene.meta.baseline_exposure_ev,
            neutral: scene.meta.as_shot_neutral,
            clipped_fraction: scene.meta.clipped_fraction,
            path: path.clone(),
            preview: thumbnail(&scene).map(|t| std::sync::Arc::new(t.rotated(src.rotation))),
        });
        written.push(path);
    }
    on(Event::Finished { frames: written.len(), seconds: t0.elapsed().as_secs_f64() });

    let delivered = if config.handoff.send && only.is_none() {
        let look = if config.handoff.stock_look && config.handoff.preset.is_empty() { crate::looks::write_look(profile, &out)? } else { None };
        let message = send(&written, config, &tvars(roll, "", profile), look)?;
        on(Event::Delivered { message: message.clone() });
        Some(message)
    } else {
        None
    };
    Ok(RollOutcome { dir: out, written, delivered })
}

/// The roll model's file in a roll's output folder.
const ROLL_MODEL: &str = "roll-model.json";

/// The model a roll was last developed with (in `out`), if it was fitted for this stock and film
/// config.
fn saved_model(out: &Path, profile: &FilmProfile, film: &silvercraft_film::FilmConfig) -> Option<RollModel> {
    // Ours is a small JSON document; anything large is not it (don't slurp it whole).
    if std::fs::metadata(out.join(ROLL_MODEL)).map(|m| m.len()).unwrap_or(u64::MAX) > 16 << 20 {
        return None;
    }
    let text = std::fs::read_to_string(out.join(ROLL_MODEL)).ok()?;
    let mut doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let model: RollModel = serde_json::from_value(doc.get_mut("model")?.take()).ok()?;
    (model.profile == profile.id && model.config == *film).then_some(model)
}

/// Pass 1: measure every frame of the roll (film base and calibration samples) and fit the roll
/// model. Also returns how pass 2 frames each capture: camera frames found here are not detected
/// again.
fn measure_roll(
    req: &RollRequest,
    config: &PipelineConfig,
    profile: &FilmProfile,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Event),
) -> Result<(RollModel, Vec<Framing>)> {
    let roll = req.roll.as_str();
    let total = req.frames.len();
    let mut samples = RollSamples::for_config(&config.film);
    // How pass 2 frames each capture: camera frames found here are not detected again.
    let mut framing = vec![Framing::Detect; total];
    // Single-frame detections of camera captures (and the capture size), for the roll prior.
    let mut singles: Vec<Option<(Option<CameraFrame>, (usize, usize))>> = vec![None; total];
    for (i, src) in req.frames.iter().enumerate() {
        check(cancel)?;
        let (capture, scan_report) = decode_capture(src, config)?;
        let mut found = None;
        if detects_frame(src, &capture) {
            let (w, h) = (capture.rgb.width(), capture.rgb.height());
            let single = crate::frames::camera_frame(&capture.rgb, &config.detect);
            found = single.as_ref().and_then(|f| crate::frames::frame_box_of(f, w, h));
            framing[i] = Framing::Found(found);
            singles[i] = Some((single, (w, h)));
        }
        let d = cut(src, config, capture, scan_report, found)?;
        on(Event::Measured {
            index: i,
            total,
            label: src.label.clone(),
            capture: (d.capture.rgb.width(), d.capture.rgb.height()),
            frame: (d.frame.width(), d.frame.height()),
            gap_base: d.gap_base,
        });
        samples.push_image(&d.frame, d.gap_base, Some(&src.label));
    }
    // A camera roll on a copy stand has one frame size: the frames that were not found
    // confidently alone (or disagree with the roll's size) are found again with it, and their
    // film base and samples measured again before the roll model is fitted.
    let camera = &config.detect.camera;
    let prior =
        if camera.roll_prior { fit_roll_prior(singles.iter().flatten().filter_map(|(f, size)| Some((f.as_ref()?, *size))), camera) } else { None };
    for (i, src) in req.frames.iter().enumerate() {
        let (Some(prior), Some((single, (w, h)))) = (&prior, &singles[i]) else { continue };
        if !needs_prior(single.as_ref(), *w, *h, prior, camera) {
            continue;
        }
        check(cancel)?;
        let (capture, scan_report) = decode_capture(src, config)?;
        let (w, h) = (capture.rgb.width(), capture.rgb.height());
        let Some(r) = crate::frames::camera_frame_with_prior(&capture.rgb, &config.detect, prior) else { continue };
        let found = r.frame.as_ref().and_then(|f| crate::frames::frame_box_of(f, w, h));
        if framing[i] == Framing::Found(found) {
            continue;
        }
        framing[i] = Framing::Found(found);
        let d = cut(src, config, capture, scan_report, found)?;
        on(Event::Reframed {
            index: i,
            total,
            label: src.label.clone(),
            frame: (d.frame.width(), d.frame.height()),
            gap_base: d.gap_base,
            confident: r.frame.as_ref().is_some_and(is_confident),
        });
        let mut again = RollSamples::for_config(&config.film);
        again.push_image(&d.frame, d.gap_base, Some(&src.label));
        if let (Some(slot), Some(f)) = (samples.frames.get_mut(i), again.frames.pop()) {
            *slot = f;
        }
    }
    let model = fit_roll(&samples, profile, &config.film).map_err(|e| e.to_string())?;
    drop(samples);
    on(Event::Fitted {
        summary: format!(
            "roll {roll}: {} ({}) · base {:.4} {:.4} {:.4} · G_ref {:.2} · k {:.4} · calibration {}",
            profile.name,
            profile.kind.as_str(),
            model.film_base[0],
            model.film_base[1],
            model.film_base[2],
            model.g_ref,
            model.k,
            if model.calibration.is_some() { "fitted" } else { "none" }
        ),
    });
    Ok((model, framing))
}

/// Send a roll that was already developed (the DNGs in its output folder) to LightCraft again,
/// e.g. after starting LightCraft. Returns what happened.
pub fn send_developed(req: &RollRequest) -> Result<String> {
    let mut diags = Vec::new();
    let reg = req.sources.registry();
    let camera_scan = req.frames.first().is_some_and(|f| is_camera_scan(&f.path));
    let capture = if camera_scan { ProfileCapture::Camera } else { ProfileCapture::Scanner };
    let profile = reg.get_for_capture(&req.stock, Some(capture)).ok_or_else(|| format!("unknown stock `{}`", req.stock))?;
    let resolved = req.sources.resolve_or_err(Some(profile), camera_scan, &mut diags)?;
    let config = &resolved.config;
    let vars = tvars(&req.roll, "", profile);
    let out = config.output.dir_for(&vars).map_err(|e| format!("output.dir: {e}"))?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&out)
        .map_err(|e| format!("{}: {e} (develop the roll first)", out.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dng")))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no developed frames in {} (develop the roll first)", out.display()).into());
    }
    let look = if config.handoff.stock_look && config.handoff.preset.is_empty() { crate::looks::write_look(profile, &out)? } else { None };
    send(&files, config, &vars, look)
}

/// Where each frame of `req` is (or will be) developed to, as `(label, DNG path)` in roll order.
/// The files need not exist yet.
pub fn developed_paths(req: &RollRequest) -> Result<Vec<(String, PathBuf)>> {
    let mut diags = Vec::new();
    let reg = req.sources.registry();
    let camera_scan = req.frames.first().is_some_and(|f| is_camera_scan(&f.path));
    let capture = if camera_scan { ProfileCapture::Camera } else { ProfileCapture::Scanner };
    let profile = reg.get_for_capture(&req.stock, Some(capture)).ok_or_else(|| format!("unknown stock `{}`", req.stock))?;
    let resolved = req.sources.resolve_or_err(Some(profile), camera_scan, &mut diags)?;
    let config = &resolved.config;
    let out = config.output.dir_for(&tvars(&req.roll, "", profile)).map_err(|e| format!("output.dir: {e}"))?;
    req.frames
        .iter()
        .map(|f| {
            let stem = config.output.stem_for(&tvars(&req.roll, &f.label, profile)).map_err(|e| format!("output.file_name: {e}"))?;
            Ok((f.label.clone(), out.join(format!("{stem}.dng"))))
        })
        .collect()
}

/// Hand `files` to LightCraft as `config.handoff` says; returns what happened.
pub fn send(files: &[PathBuf], config: &PipelineConfig, vars: &TemplateVars<'_>, look: Option<silvercraft_handoff::LookPreset>) -> Result<String> {
    let h = &config.handoff;
    let ex = |t: &str| expand(t, vars);
    let opt = |t: &str| ex(t).map(|s| (!s.is_empty()).then_some(s));
    let opts = DeliveryOptions {
        route: h.route,
        port: h.port,
        library: if h.library.is_empty() { silvercraft_handoff::lightcraft::default_library() } else { Some(PathBuf::from(&h.library)) },
        album: opt(&h.album)?,
        keywords: h.keywords.iter().map(|k| ex(k)).collect::<std::result::Result<Vec<_>, _>>()?,
        preset: opt(&h.preset)?,
        look,
        copy: h.copy,
        reveal: h.reveal,
    };
    Ok(match deliver(files, &opts).map_err(|e| e.to_string())? {
        Delivered::Imported { ids, album } => {
            format!("LightCraft imported {} photos (album {})", ids.len(), album.map(|a| a.to_string()).unwrap_or_else(|| "—".into()))
        }
        Delivered::WatchedFolder(f) => format!("copied into LightCraft's watched folder {}", f.display()),
        Delivered::LeftInPlace { reason } => format!("not sent: {reason}"),
    })
}

fn write_dng(
    out: &Path,
    stem: &str,
    roll: &str,
    src: &FrameSource,
    capture: &Capture,
    scene: &SceneFrame,
    model: &RollModel,
    profile: &FilmProfile,
    resolved: &Resolved,
) -> Result<PathBuf> {
    let config = &resolved.config;
    let vars = tvars(roll, &src.label, profile);
    let keywords = config.handoff.keywords.iter().map(|k| expand(k, &vars)).collect::<std::result::Result<Vec<_>, _>>()?;
    let provenance = json!({
        "source": src.path.to_string_lossy(),
        "infrared": src.ir_path.as_ref().map(|p| p.to_string_lossy()),
        "capture": match &capture.kind {
            CaptureKind::Camera { make, model } => json!({"kind": "camera", "make": make, "model": model}),
            CaptureKind::Scanner => json!({"kind": "scanner"}),
        },
        "srgbPrimaries": capture.srgb_primaries,
        "frame": scene.meta,
        "roll": {"stock": model.profile, "filmBase": model.film_base, "gRef": model.g_ref, "k": model.k},
        "configLayers": resolved.layers,
        "config": config,
    });
    let xmp = XmpPacket {
        title: Some(format!("{roll} · {}", src.label)),
        keywords,
        sc: vec![
            ("roll".into(), roll.into()),
            ("frame".into(), src.label.clone()),
            ("stock".into(), profile.id.clone()),
            ("provenance".into(), provenance.to_string()),
        ],
        creator_tool: Some(concat!("SilverCraft ", env!("CARGO_PKG_VERSION")).into()),
        ..Default::default()
    };
    let info = DngFrameInfo {
        as_shot_neutral: Some(scene.meta.as_shot_neutral),
        exposure_ev: scene.meta.baseline_exposure_ev,
        samples: config.output.samples,
        orientation: src.rotation.orientation(),
        ..Default::default()
    };
    let frame = LinearFrame { width: scene.image.width(), height: scene.image.height(), rgb: scene.image.data() };
    let placement = XmpPlacement { sidecar: config.output.sidecar, embed: config.output.embed_xmp };
    Ok(write_frame_with(out, stem, &frame, &info, &xmp, placement).map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_is_small_and_exposed() {
        let image = RgbImage::filled(1000, 500, [0.18, 0.18, 0.18]).unwrap();
        let p = silvercraft_film::profiles::builtin_registry().get("generic-c41").unwrap();
        let mut samples = RollSamples::for_config(&silvercraft_film::FilmConfig::default());
        let neg = RgbImage::filled(64, 32, [0.5, 0.4, 0.3]).unwrap();
        samples.push_image(&neg, None, None);
        let model = fit_roll(&samples, p, &silvercraft_film::FilmConfig::default()).unwrap();
        let mut scene = develop_frame(&model, &neg, None).unwrap();
        scene.image = image;
        scene.meta.baseline_exposure_ev = 0.0;
        scene.meta.as_shot_neutral = [1.0, 1.0, 1.0];
        let t = thumbnail(&scene).unwrap();
        assert_eq!((t.width, t.height), (333, 166));
        assert_eq!(t.rgb.len(), t.width * t.height * 3);
        // mid grey lands in the middle of the display range
        assert!((90..150).contains(&t.rgb[0]), "{}", t.rgb[0]);
    }

    #[test]
    fn scanner_offset_is_the_negated_fallback_unless_configured() {
        assert_eq!(ir_shift_hint([0.0, 0.0], Some((-1, 0))), [1.0, 0.0]);
        assert_eq!(ir_shift_hint([0.0, 0.0], None), [0.0, 0.0]);
        assert_eq!(ir_shift_hint([0.5, 0.0], Some((-1, 0))), [0.5, 0.0], "an explicit ir_shift wins");
    }
}
