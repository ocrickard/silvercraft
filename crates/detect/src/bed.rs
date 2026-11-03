//! Flatbed prescan frame detection.
//!
//! Input: a low-resolution prescan of the whole bed (linear transmittance RGB, bare light ≈ the
//! brightest thing on it) and its dpi. Steps:
//!
//! 1. **Openings.** Light passes only through the holder's openings; the holder is opaque.
//!    Pixels above `open_threshold` × the light level are open. A small morphological open and
//!    close, then hole filling, make each opening one component even where the film is dense.
//!    Components narrower than `min_across_mm` are holder features (registration marks,
//!    calibration windows) and are skipped. Collinear pieces of one strip that a dense band of
//!    film split apart are merged again.
//! 2. **Geometry.** Each opening's long axis is fitted from its two long edges (robust line fits),
//!    which gives the strip's skew angle and width. The opening is resampled *along that axis*
//!    (bilinear), so skewed strips are measured straight: per line across the strip, the median
//!    transmittance, the p10–p90 spread and the chroma.
//! 3. **Film runs.** Lines that are bright and neutral are bare light (no film, e.g. beyond a
//!    short strip). The remaining runs are film. Each run is a strip of the output.
//! 4. **Gaps.** For a negative the inter-frame gap is unexposed base: the highest transmittance
//!    on the film, uniform across the strip. For a positive it is black. Each line gets a gap
//!    score = closeness to that level × uniformity.
//! 5. **Pitch comb.** Frames repeat at the format pitch, so a comb of teeth `φ + k·P` is fitted
//!    to the gap score (pitch searched within a tolerance of the format pitch; or, with no prior,
//!    over the whole plausible range, preferring the smallest pitch that scores nearly as well as
//!    the best, since 2·P matches every other gap equally well). The comb fills gaps it cannot
//!    see (content as bright as base at a gap), and rejects isolated bright bands inside a frame
//!    (a bright sky, a shadow as thin as base), because they are not periodic. Each tooth then
//!    snaps to its local gap and the frame spans gap end to next gap start.
//! 6. **Frames.** Frames clipped by the run end are kept only if most of a frame is visible
//!    (flagged `partial`). Blank frames (all base) are flagged, and dropped at the strip ends
//!    (leader). Slides and sheets are one frame per opening.
//!
//! The scale is the dpi (not the strip width), the strip may be skewed or run
//! either way, and the gap polarity follows the film type.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use silvercraft_imgproc::{Mask, Plane, label, morph, stats};

use crate::config::{BedConfig, FilmType, Format35, Format120, Holder};
use crate::geom::{MmRect, PxRect, RotRect};
use crate::util::{clean, luma, pcts_in_place, robust_line};
use crate::{DetectError, Result, Rgb};

/// Direction a strip runs on the bed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Axis {
    /// Along the bed's y axis (prescan rows).
    Vertical,
    /// Along the bed's x axis.
    Horizontal,
}

/// Frame orientation in the prescan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Orientation {
    /// Wider than tall.
    Landscape,
    /// Taller than wide.
    Portrait,
}

/// What an opening holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StripKind {
    Strip35,
    Medium120,
    Sheet,
    Slide,
}

/// One detected frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BedFrame {
    /// Strip index (0-based, ordered across the bed: left to right for vertical strips).
    pub strip: usize,
    /// Frame index within the strip (0-based, increasing along the strip: down or right).
    pub index: usize,
    /// Axis-aligned bounds in prescan pixels (covers the rotated frame).
    pub bounds: PxRect,
    /// `bounds` in millimetres from the prescan origin.
    pub mm: MmRect,
    /// The frame as a rotated rectangle (strip skew) in prescan pixels.
    pub rotated: RotRect,
    pub axis: Axis,
    pub orientation: Orientation,
    /// 0..1: gap evidence on both sides, lowered for partial, blank and fallback frames.
    pub confidence: f32,
    /// The frame is all base (negative) or all black (positive): probably unexposed.
    pub blank: bool,
    /// The strip ends inside this frame (a cut or the holder edge hides part of it).
    pub partial: bool,
    /// `"s{strip+1}-f{index+1:02}"`.
    pub label: String,
}

/// One detected strip (a film run inside an opening).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StripInfo {
    pub index: usize,
    pub kind: StripKind,
    pub bounds: PxRect,
    pub rotated: RotRect,
    pub axis: Axis,
    /// Skew of the strip axis from the bed axis (degrees, clockwise on screen positive).
    pub angle_deg: f64,
    pub across_mm: f64,
    pub along_mm: f64,
    /// Fitted frame pitch, for multi-frame strips.
    pub pitch_mm: Option<f64>,
    /// Mean gap score of the fitted comb (0..1), for multi-frame strips.
    pub comb_score: Option<f32>,
    /// Frames were placed at the nominal pitch because no gaps were visible.
    pub fallback: bool,
}

/// Bed detection result.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BedDetection {
    pub frames: Vec<BedFrame>,
    pub strips: Vec<StripInfo>,
    /// Bare-light luminance the prescan was normalised by.
    pub light_level: f32,
    pub warnings: Vec<String>,
}

/// An opening: a component of the open mask with a fitted axis.
#[derive(Debug, Clone)]
struct Opening {
    axis: Axis,
    /// Point on the centre line at t = 0.
    origin: (f64, f64),
    /// Unit vector along the strip.
    a: (f64, f64),
    /// Unit vector across the strip (+x-ish for vertical, +y-ish for horizontal strips).
    n: (f64, f64),
    width: f64,
    t0: f64,
    t1: f64,
}

impl Opening {
    fn point(&self, t: f64, u: f64) -> (f64, f64) {
        (self.origin.0 + t * self.a.0 + u * self.n.0, self.origin.1 + t * self.a.1 + u * self.n.1)
    }
    fn angle_deg(&self) -> f64 {
        let side = if self.axis == Axis::Vertical { self.n } else { self.a };
        side.1.atan2(side.0).to_degrees()
    }
    /// Rotated rect for `[ta, tb)` along the strip, full width.
    fn rect(&self, ta: f64, tb: f64) -> RotRect {
        let (cx, cy) = self.point((ta + tb) / 2.0, 0.0);
        let len = (tb - ta).abs();
        let (width, height) = if self.axis == Axis::Vertical { (self.width, len) } else { (len, self.width) };
        RotRect { cx, cy, width, height, angle_deg: self.angle_deg() }
    }
}

/// Per-line statistics across a strip.
#[derive(Debug, Clone, Copy, Default)]
struct Line {
    median: f32,
    spread: f32,
    bare: bool,
}

/// How frames are laid out on a film run.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Layout {
    /// One frame per opening (slides, sheets, a single cut frame).
    Single,
    /// Known pitch and frame length (mm), with a search tolerance.
    Prior { pitch: f64, frame: f64, tol: f64 },
    /// Pitch searched over `[lo, hi]` mm.
    Free { lo: f64, hi: f64 },
}

/// Detect strips and frames on a bed prescan (`rgb`: interleaved linear RGB, `dpi` > 0).
pub fn detect_bed(rgb: &Rgb<'_, f32>, dpi: f64, cfg: &BedConfig) -> Result<BedDetection> {
    if !(dpi.is_finite() && (10.0..=20_000.0).contains(&dpi)) {
        return Err(DetectError::Param(format!("prescan dpi {dpi} must be in 10..=20000")));
    }
    let ppm = dpi / 25.4;
    let (w, h) = (rgb.width(), rgb.height());
    let planes = crate::util::downsample_rgb(rgb, 1, clean)?;
    let lum = luma(&planes)?;
    let mut out = BedDetection::default();
    let light = stats::percentile(&stats::subsample(lum.data(), 500_000), 99.9).unwrap_or(0.0);
    if light.is_nan() || light <= 1e-6 {
        out.warnings.push("prescan is black: no light reaches the sensor".into());
        return Ok(out);
    }
    out.light_level = light;
    let inv = 1.0 / light;
    let lum = lum.map(|v| v * inv);
    let planes = planes.map(|p| p.map(|v| v * inv));

    // 1. Openings.
    let open_thr = cfg.open_threshold as f32;
    let mut open = Mask::from_plane(&lum, |v| v > open_thr);
    let r_small = (0.3 * ppm).round().max(1.0) as usize;
    let r_close = (1.0 * ppm).round().max(1.0) as usize;
    open = morph::close(&morph::open(&open, r_small), r_close);
    let open = fill_holes(&open)?;
    let (labels, comps) = label(&open);
    let min_across = cfg.min_across_mm.max(0.0) * ppm;
    // At most the 64 largest components (a bed holds a handful; noise must not cost quadratically).
    let mut comps = comps;
    comps.sort_by_key(|c| std::cmp::Reverse(c.area));
    comps.truncate(64);
    let mut openings: Vec<Opening> = comps
        .iter()
        .filter(|c| (c.width().min(c.height()) as f64) >= min_across * 0.9 && (c.width().max(c.height()) as f64) >= cfg.min_along_mm * ppm)
        .filter_map(|c| fit_opening(&labels, w, c.label, c.bbox))
        .filter(|o| o.width >= min_across)
        .collect();
    if !matches!(cfg.holder, Holder::Slides35 | Holder::Sheet4x5) {
        openings = merge_collinear(openings, cfg.merge_gap_mm.max(0.0) * ppm);
    }
    // Strips in reading order across the bed: left to right for vertical strips, top to bottom
    // for horizontal ones; mixed axes (slides in both orientations) by rows of 10 mm, then x.
    let uniform_axis = openings.windows(2).all(|p| p[0].axis == p[1].axis);
    let row = 10.0 * ppm;
    openings.sort_by(|p, q| {
        let key = |o: &Opening| {
            let c = o.point((o.t0 + o.t1) / 2.0, 0.0);
            match (uniform_axis, o.axis) {
                (true, Axis::Vertical) => (c.0, c.1),
                (true, Axis::Horizontal) => (c.1, c.0),
                (false, _) => ((c.1 / row).round(), c.0),
            }
        };
        let (a, b) = (key(p), key(q));
        a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
    });

    // 2–4. Per opening: lines and film runs, classified.
    struct Pending {
        op: usize,
        lines: Vec<Line>,
        run: Run,
        kind: StripKind,
        layout: Layout,
        across_mm: f64,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for (oi, op) in openings.iter().enumerate() {
        let lines = sample_lines(op, &lum, &planes, cfg);
        let across_mm = op.width / ppm;
        let opening_mm = (op.t1 - op.t0) / ppm;
        for run in film_runs(&lines, cfg.min_along_mm * ppm, 0.5 * ppm) {
            let run_mm = (run.end - run.start) as f64 / ppm;
            if let Some((kind, layout)) = classify(cfg.holder, across_mm, opening_mm, run_mm, cfg) {
                pending.push(Pending { op: oi, lines: lines[run.start..run.end].to_vec(), run, kind, layout, across_mm });
            }
        }
    }

    // 5a. One roll, one pitch: free-pitch strips of a kind share a consensus pitch (a strip
    // whose content mimics the gap, e.g. skies as thin as base, is outvoted by the others).
    if cfg.consensus_pitch {
        for kind in [StripKind::Strip35, StripKind::Medium120] {
            let group: Vec<usize> =
                (0..pending.len()).filter(|&i| pending[i].kind == kind && matches!(pending[i].layout, Layout::Free { .. })).collect();
            if group.len() < 2 {
                continue;
            }
            let Layout::Free { lo, hi } = pending[group[0]].layout else { continue };
            let ws: Vec<Vec<f32>> =
                group.iter().map(|&i| window_max(&gap_scores(&pending[i].lines, cfg), ((0.015 * lo * ppm).round() as usize).max(2))).collect();
            if let Some(p) = consensus_pitch(&ws, lo * ppm, hi * ppm, cfg.smaller_pitch_preference as f32) {
                let pm = p / ppm;
                for &i in &group {
                    pending[i].layout = Layout::Prior { pitch: pm, frame: 0.95 * pm, tol: 0.015 };
                }
            }
        }
    }

    // 5b–6. Frames.
    for pd in &pending {
        let (op, run, kind, across_mm) = (&openings[pd.op], pd.run, pd.kind, pd.across_mm);
        let run_mm = (run.end - run.start) as f64 / ppm;
        {
            let layout = pd.layout;
            let strip = out.strips.len();
            let t_base = op.t0 + run.start as f64;
            let placed = place_frames(&pd.lines, &run, layout, ppm, cfg);
            if let (false, Some(score), Some(p)) = (placed.fallback, placed.score, placed.pitch)
                && f64::from(score) < cfg.weak_comb_score
            {
                out.warnings.push(format!(
                    "strip {}: the frame gaps fit a {:.1} mm pitch poorly (score {score:.2}); the film may be another format (try the \"free\" format)",
                    strip + 1,
                    p / ppm
                ));
            }
            if placed.fallback {
                out.warnings.push(format!("strip {}: no frame gaps visible; frames placed at the nominal pitch", strip + 1));
            }
            let srect = op.rect(t_base, t_base + (run.end - run.start) as f64);
            let mut index = 0;
            for f in &placed.frames {
                let rotated = op.rect(t_base + f.start, t_base + f.end);
                let Some(bounds) = rotated.bounding_box(w, h) else { continue };
                out.frames.push(BedFrame {
                    strip,
                    index,
                    bounds,
                    mm: bounds.to_mm(dpi),
                    rotated,
                    axis: op.axis,
                    orientation: if bounds.width >= bounds.height { Orientation::Landscape } else { Orientation::Portrait },
                    confidence: f.confidence,
                    blank: f.blank,
                    partial: f.partial,
                    label: format!("s{}-f{:02}", strip + 1, index + 1),
                });
                index += 1;
            }
            out.strips.push(StripInfo {
                index: strip,
                kind,
                bounds: srect.bounding_box(w, h).unwrap_or_default(),
                rotated: srect,
                axis: op.axis,
                angle_deg: op.angle_deg(),
                across_mm,
                along_mm: run_mm,
                pitch_mm: placed.pitch.map(|p| p / ppm),
                comb_score: placed.score,
                fallback: placed.fallback,
            });
        }
    }
    if out.frames.is_empty() {
        out.warnings.push("no film frames found on the bed".into());
    }
    Ok(out)
}

/// Fill regions of `false` not connected to the image border.
fn fill_holes(m: &Mask) -> Result<Mask> {
    let (labels, comps) = label(&m.not());
    let (w, h) = (m.width(), m.height());
    let mut border = vec![false; comps.len() + 1];
    for c in &comps {
        let [x0, y0, x1, y1] = c.bbox;
        if (x0 == 0 || y0 == 0 || x1 >= w || y1 >= h)
            && let Some(b) = border.get_mut(c.label as usize)
        {
            *b = true;
        }
    }
    let data = m.data().iter().zip(&labels).map(|(&v, &l)| v || (l != 0 && !border.get(l as usize).copied().unwrap_or(true))).collect();
    Ok(Mask::new(w, h, data)?)
}

/// Fit the axis of the component `id` (bbox `[x0, y0, x1, y1)`) from its two long edges.
fn fit_opening(labels: &[u32], w: usize, id: u32, bbox: [usize; 4]) -> Option<Opening> {
    let [x0, y0, x1, y1] = bbox;
    let vertical = y1 - y0 >= x1 - x0;
    let (along0, along1, across0, across1) = if vertical { (y0, y1, x0, x1) } else { (x0, x1, y0, y1) };
    let at = |i: usize, j: usize| -> bool {
        let (x, y) = if vertical { (j, i) } else { (i, j) };
        labels.get(y * w + x).is_some_and(|&l| l == id)
    };
    let len = along1 - along0;
    let span = across1 - across0;
    let (mut is, mut los, mut his, mut widths) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for i in (along0 + len / 10)..(along1 - len / 10) {
        let lo = (across0..across1).find(|&j| at(i, j));
        let hi = (across0..across1).rev().find(|&j| at(i, j));
        if let (Some(lo), Some(hi)) = (lo, hi)
            && (hi + 1 - lo) * 2 >= span
        {
            is.push(i as f64 + 0.5);
            los.push(lo as f64);
            his.push(hi as f64 + 1.0);
            widths.push((hi + 1 - lo) as f32);
        }
    }
    let (la, lb) = robust_line(&is, &los)?;
    let (ha, hb) = robust_line(&is, &his)?;
    let (c0, s) = ((la + ha) / 2.0, (lb + hb) / 2.0);
    let norm = (1.0 + s * s).sqrt();
    let width = f64::from(stats::percentile(&widths, 50.0)?) / norm;
    let (origin, a, n, axis) = if vertical {
        ((c0, 0.0), (s / norm, 1.0 / norm), (1.0 / norm, -s / norm), Axis::Vertical)
    } else {
        ((0.0, c0), (1.0 / norm, s / norm), (-s / norm, 1.0 / norm), Axis::Horizontal)
    };
    // Along extent: pixels near the centre line, projected on the axis.
    let (mut t0, mut t1) = (f64::INFINITY, f64::NEG_INFINITY);
    for y in y0..y1 {
        for x in x0..x1 {
            if labels.get(y * w + x).is_some_and(|&l| l == id) {
                let (px, py) = (x as f64 + 0.5 - origin.0, y as f64 + 0.5 - origin.1);
                let u = px * n.0 + py * n.1;
                if u.abs() <= 0.4 * width {
                    let t = px * a.0 + py * a.1;
                    t0 = t0.min(t - 0.5);
                    t1 = t1.max(t + 0.5);
                }
            }
        }
    }
    (t1 > t0 && width > 2.0).then_some(Opening { axis, origin, a, n, width, t0, t1 })
}

/// Merge pieces of one strip split by a dense (dark) band: same axis, overlapping across, and
/// separated along by at most `max_gap` px.
fn merge_collinear(mut ops: Vec<Opening>, max_gap: f64) -> Vec<Opening> {
    'again: loop {
        for i in 0..ops.len() {
            for j in 0..ops.len() {
                if i == j {
                    continue;
                }
                let (p, q) = (&ops[i], &ops[j]);
                if p.axis != q.axis || (p.angle_deg() - q.angle_deg()).abs() > 1.5 || (p.width - q.width).abs() > 0.2 * p.width {
                    continue;
                }
                // q's end points in p's frame.
                let proj = |t: f64| {
                    let (x, y) = q.point(t, 0.0);
                    let (dx, dy) = (x - p.origin.0, y - p.origin.1);
                    (dx * p.a.0 + dy * p.a.1, dx * p.n.0 + dy * p.n.1)
                };
                let (ta, ua) = proj(q.t0);
                let (tb, ub) = proj(q.t1);
                let (qt0, qt1) = (ta.min(tb), ta.max(tb));
                if ua.abs().max(ub.abs()) > 0.3 * p.width {
                    continue;
                }
                let gap = (qt0 - p.t1).max(p.t0 - qt1);
                if gap > max_gap {
                    continue;
                }
                let mut merged = p.clone();
                merged.t0 = p.t0.min(qt0);
                merged.t1 = p.t1.max(qt1);
                merged.width = (p.width + q.width) / 2.0;
                let (lo, hi) = (i.min(j), i.max(j));
                ops.remove(hi);
                ops.remove(lo);
                ops.push(merged);
                continue 'again;
            }
        }
        return ops;
    }
}

/// Sample the opening line by line across its width (1 px steps along the axis).
fn sample_lines(op: &Opening, lum: &Plane, rgb: &[Plane; 3], cfg: &BedConfig) -> Vec<Line> {
    let n = (op.t1 - op.t0).ceil().max(0.0) as usize;
    let inset = (cfg.edge_inset.clamp(0.0, 0.45) * op.width).max(1.5);
    let usable = (op.width - 2.0 * inset).max(1.0);
    let nu = (usable.floor() as usize).max(3);
    let (bare_level, bare_chroma) = (cfg.bare_level as f32, cfg.bare_max_chroma as f32);
    (0..n)
        .into_par_iter()
        .map_init(
            || (vec![0f32; nu], vec![0f32; nu]),
            |(vals, chroma), i| {
                let t = op.t0 + i as f64 + 0.5;
                for k in 0..nu {
                    let u = -usable / 2.0 + usable * (k as f64 + 0.5) / nu as f64;
                    let (x, y) = op.point(t, u);
                    let (x, y) = (x as f32 - 0.5, y as f32 - 0.5);
                    vals[k] = lum.sample(x, y);
                    let c = [rgb[0].sample(x, y), rgb[1].sample(x, y), rgb[2].sample(x, y)];
                    let mx = c[0].max(c[1]).max(c[2]);
                    let mn = c[0].min(c[1]).min(c[2]);
                    chroma[k] = (mx - mn) / ((c[0] + c[1] + c[2]) / 3.0).max(1e-6);
                }
                let mut p = [0f32; 3];
                let mut cm = [0f32; 1];
                if !pcts_in_place(vals, &[10.0, 50.0, 90.0], &mut p) || !pcts_in_place(chroma, &[50.0], &mut cm) {
                    return Line::default();
                }
                Line { median: p[1], spread: (p[2] - p[0]).max(0.0), bare: p[1] > bare_level && cm[0] < bare_chroma }
            },
        )
        .collect()
}

/// A run of film lines `[start, end)`, and whether bare light (a cut film end) bounds each side.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    start: usize,
    end: usize,
    bare_before: bool,
    bare_after: bool,
}

/// Maximal runs of non-bare lines, bridging bare interruptions shorter than `bridge` px.
fn film_runs(lines: &[Line], min_len: f64, bridge: f64) -> Vec<Run> {
    let mut film: Vec<bool> = lines.iter().map(|l| !l.bare).collect();
    // Bridge short bare interruptions (dust-free clear spots, specular glints).
    let mut i = 0;
    while i < film.len() {
        if !film[i] {
            let s = i;
            while i < film.len() && !film[i] {
                i += 1;
            }
            if s > 0 && i < film.len() && ((i - s) as f64) < bridge {
                film[s..i].iter_mut().for_each(|v| *v = true);
            }
        } else {
            i += 1;
        }
    }
    let mut runs = Vec::new();
    let mut i = 0;
    while i < film.len() {
        if film[i] {
            let s = i;
            while i < film.len() && film[i] {
                i += 1;
            }
            if (i - s) as f64 >= min_len {
                runs.push(Run { start: s, end: i, bare_before: s > 0, bare_after: i < film.len() });
            }
        } else {
            i += 1;
        }
    }
    runs
}

/// What an opening holds, from the hint and its size (mm).
fn classify(holder: Holder, across: f64, opening_len: f64, run_len: f64, cfg: &BedConfig) -> Option<(StripKind, Layout)> {
    let lo35 = cfg.min_across_mm;
    let (tol, wide) = (cfg.pitch_tolerance, cfg.pitch_tolerance_wide);
    let prior = |pitch: f64, frame: f64, tol: f64| Layout::Prior { pitch, frame, tol };
    let in35 = (lo35..=42.0).contains(&across);
    let in120 = (42.0..=75.0).contains(&across);
    let in_sheet = (75.0..=150.0).contains(&across);
    let k = match holder {
        Holder::Auto if in35 && opening_len < 50.0 => (StripKind::Slide, Layout::Single),
        Holder::Auto if in35 => (StripKind::Strip35, Layout::Free { lo: 16.0, hi: 75.0 }),
        Holder::Auto if in120 => (StripKind::Medium120, Layout::Free { lo: 40.0, hi: 92.0 }),
        Holder::Auto if in_sheet => (StripKind::Sheet, Layout::Single),
        Holder::Strip35 { format } if in35 => (
            StripKind::Strip35,
            match format {
                Format35::Full => prior(38.0, 36.0, tol),
                Format35::Half => prior(19.0, 18.0, tol),
                Format35::Panoramic => prior(67.0, 65.0, wide),
                Format35::Free => Layout::Free { lo: 16.0, hi: 75.0 },
            },
        ),
        Holder::Medium120 { format } if in120 => (
            StripKind::Medium120,
            match format {
                Format120::SixByFourFive => prior(44.0, 41.5, wide),
                Format120::SixBySix => prior(58.5, 56.0, wide),
                Format120::SixBySeven => prior(72.5, 70.0, wide),
                Format120::SixByNine => prior(86.5, 84.0, wide),
                Format120::Free => Layout::Free { lo: 40.0, hi: 92.0 },
            },
        ),
        Holder::Sheet4x5 if in_sheet => (StripKind::Sheet, Layout::Single),
        Holder::Slides35 if (lo35..=45.0).contains(&across) => (StripKind::Slide, Layout::Single),
        _ => return None,
    };
    // A run shorter than ~1.3 pitches is a single cut frame.
    let k = match k.1 {
        Layout::Prior { pitch, .. } if run_len < 1.3 * pitch => (k.0, Layout::Single),
        Layout::Free { lo, .. } if run_len < 1.3 * lo => (k.0, Layout::Single),
        _ => k,
    };
    Some(k)
}

/// A frame placed on a run, in run-relative lines.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placed {
    start: f64,
    end: f64,
    confidence: f32,
    blank: bool,
    partial: bool,
}

#[derive(Debug, Clone, Default)]
struct Placement {
    frames: Vec<Placed>,
    pitch: Option<f64>,
    score: Option<f32>,
    fallback: bool,
}

/// Gap score per line: closeness to the gap level × uniformity across the strip.
fn gap_scores(lines: &[Line], cfg: &BedConfig) -> Vec<f32> {
    gap_signals(lines, cfg).0
}

/// Per line: (gap score, level closeness). The level closeness (how near the line's median is to
/// the base, or black, level) is crisp at gap edges; the score adds uniformity, which separates
/// gaps from flat content but varies from line to line.
fn gap_signals(lines: &[Line], cfg: &BedConfig) -> (Vec<f32>, Vec<f32>) {
    let med: Vec<f32> = lines.iter().map(|l| l.median).collect();
    let pick = |q: f64| stats::percentile(&med, q).unwrap_or(0.0).max(1e-6);
    let uni = cfg.gap_uniformity.max(1e-6) as f32;
    let (base, black, white) = (pick(99.0), pick(1.0), pick(99.0));
    let tol_n = cfg.gap_level_tolerance.max(1e-6) as f32;
    let tol_p = cfg.gap_level_tolerance_positive.max(1e-6) as f32;
    lines
        .iter()
        .map(|l| {
            let m = l.median.max(1e-6);
            let (c, u) = match cfg.film {
                FilmType::Negative => {
                    (if m >= base { 1.0 } else { (1.0 - (base / m).ln() / tol_n).clamp(0.0, 1.0) }, (1.0 - l.spread / m / uni).clamp(0.0, 1.0))
                }
                FilmType::Positive => {
                    (if m <= black { 1.0 } else { (1.0 - (m / black).ln() / tol_p).clamp(0.0, 1.0) }, (1.0 - l.spread / white / uni).clamp(0.0, 1.0))
                }
            };
            (c * u, c)
        })
        .unzip()
}

fn window_max(s: &[f32], r: usize) -> Vec<f32> {
    (0..s.len()).map(|i| s[i.saturating_sub(r)..(i + r + 1).min(s.len())].iter().copied().fold(0.0, f32::max)).collect()
}

/// Best phase for one pitch: mean window-max gap score of the teeth inside the run (≥ 2 teeth).
/// Ties (a gap wider than the tooth window scores a plateau of phases) take the plateau middle.
fn comb_at(ws: &[f32], pitch: f64) -> Option<(f32, f64)> {
    if !(pitch.is_finite() && pitch >= 1.0) {
        return None;
    }
    let n = ws.len() as f64;
    let mut scores: Vec<(f64, f32)> = Vec::new();
    let mut phase = 0.0;
    while phase < pitch {
        let (mut sum, mut cnt) = (0.0f32, 0usize);
        let mut t = phase;
        while t < n {
            sum += ws.get(t.round() as usize).copied().unwrap_or(0.0);
            cnt += 1;
            t += pitch;
        }
        if cnt >= 2 {
            scores.push((phase, sum / cnt as f32));
        }
        phase += 0.5;
    }
    let top = scores.iter().map(|s| s.1).fold(f32::MIN, f32::max);
    let first = scores.iter().position(|s| s.1 >= top - 1e-6)?;
    let mut last = first;
    while scores.get(last + 1).is_some_and(|s| s.1 >= top - 1e-6) {
        last += 1;
    }
    Some((top, (scores[first].0 + scores[last].0) / 2.0))
}

/// Fit the comb over a pitch range (px, step `step`). Returns per-pitch best `(pitch, score, phase)`.
fn comb_scan(ws: &[f32], lo: f64, hi: f64, step: f64) -> Vec<(f64, f32, f64)> {
    let lo = lo.max(4.0);
    let steps = if hi > lo { ((hi - lo) / step).floor() as usize + 1 } else { 1 };
    (0..steps.min(20_000))
        .into_par_iter()
        .filter_map(|k| {
            let p = lo + k as f64 * step;
            comb_at(ws, p).map(|(v, ph)| (p, v, ph))
        })
        .collect()
}

/// Highest-scoring pitch; a plateau of equal scores takes its middle entry.
/// Pitch (px) maximising the mean comb score over several strips (each strip counts where the
/// pitch fits it at least twice), preferring the smallest pitch close to the best.
fn consensus_pitch(ws: &[Vec<f32>], lo: f64, hi: f64, pref: f32) -> Option<f64> {
    let longest = ws.iter().map(Vec::len).max()? as f64;
    let curve = |lo: f64, hi: f64, step: f64| -> Vec<(f64, f32, f64)> {
        let steps = if hi > lo { ((hi - lo) / step).floor() as usize + 1 } else { 1 };
        (0..steps.min(20_000))
            .into_par_iter()
            .filter_map(|k| {
                let p = lo + k as f64 * step;
                let v: Vec<f32> = ws.iter().filter_map(|w| comb_at(w, p).map(|c| c.0)).collect();
                (!v.is_empty()).then(|| (p, v.iter().sum::<f32>() / v.len() as f32, 0.0))
            })
            .collect()
    };
    let coarse = curve(lo.max(4.0), hi.min(longest), 1.0);
    let top = best_of(&coarse)?.1;
    let first = coarse.iter().find(|c| c.1 >= top * pref)?;
    best_of(&curve(first.0 - 2.0, first.0 + 2.0, 0.25)).map(|c| c.0)
}

fn best_of(scan: &[(f64, f32, f64)]) -> Option<(f64, f32, f64)> {
    let top = scan.iter().map(|s| s.1).fold(f32::MIN, f32::max);
    let first = scan.iter().position(|s| s.1 >= top - 1e-4)?;
    let mut last = first;
    while scan.get(last + 1).is_some_and(|s| s.1 >= top - 1e-4) {
        last += 1;
    }
    scan.get((first + last) / 2).copied()
}

/// Lay frames out on one film run.
fn place_frames(lines: &[Line], run: &Run, layout: Layout, ppm: f64, cfg: &BedConfig) -> Placement {
    let n = lines.len() as f64;
    let (pitch_mm, frame_mm, lo, hi) = match layout {
        Layout::Single => {
            return Placement { frames: vec![Placed { start: 0.0, end: n, confidence: 0.8, blank: false, partial: false }], ..Default::default() };
        }
        Layout::Prior { pitch, frame, tol } => (Some(pitch), Some(frame), pitch * (1.0 - tol), pitch * (1.0 + tol)),
        Layout::Free { lo, hi } => (None, None, lo, hi),
    };
    let (s, level) = gap_signals(lines, cfg);
    let mid_pitch = pitch_mm.unwrap_or(lo) * ppm;
    let r = ((0.015 * mid_pitch).round() as usize).max(2);
    let ws = window_max(&s, r);
    let (plo, phi) = (lo * ppm, (hi * ppm).min(n));
    let fit = if pitch_mm.is_some() {
        best_of(&comb_scan(&ws, plo, phi.max(plo), 0.25))
    } else {
        let coarse = comb_scan(&ws, plo, phi, 1.0);
        best_of(&coarse).and_then(|(_, top, _)| {
            let bar = top * cfg.smaller_pitch_preference as f32;
            let first = coarse.iter().find(|c| c.1 >= bar)?;
            best_of(&comb_scan(&ws, first.0 - 2.0, first.0 + 2.0, 0.25))
        })
    };
    let nominal_frame = |p: f64| frame_mm.zip(pitch_mm).map_or(0.95 * p, |(f, pm)| f / pm * p);
    match fit {
        Some((p, v, phase)) if f64::from(v) >= cfg.min_comb_score => {
            let frames = frames_from_comb(&s, &level, run, p, phase, nominal_frame(p), r, cfg.drop_blank_end_frames);
            Placement { frames, pitch: Some(p), score: Some(v), fallback: false }
        }
        _ => {
            // No visible gaps: nominal pitch, anchored at a cut end (or centred).
            let Some(pm) = pitch_mm else {
                return Placement {
                    frames: vec![Placed { start: 0.0, end: n, confidence: 0.2, blank: false, partial: false }],
                    score: fit.map(|f| f.1),
                    fallback: true,
                    ..Default::default()
                };
            };
            let p = pm * ppm;
            if !(p.is_finite() && p >= 1.0) {
                return Placement { fallback: true, ..Default::default() };
            }
            let f = nominal_frame(p);
            let count = ((n + 0.3 * p) / p).floor().max(1.0);
            let start = if run.bare_before {
                0.0
            } else if run.bare_after {
                n - count * p
            } else {
                (n - count * p) / 2.0
            };
            let frames = (0..count as usize)
                .filter_map(|k| {
                    let c = start + (k as f64 + 0.5) * p;
                    let (a, b) = ((c - f / 2.0).max(0.0), (c + f / 2.0).min(n));
                    (b - a >= 0.6 * f).then_some(Placed { start: a, end: b, confidence: 0.25, blank: false, partial: b - a < 0.97 * f })
                })
                .collect();
            Placement { frames, pitch: Some(p), score: fit.map(|f| f.1), fallback: true }
        }
    }
}

/// Frames between consecutive comb teeth, each tooth snapped to its local gap.
fn frames_from_comb(s: &[f32], level: &[f32], run: &Run, pitch: f64, phase: f64, frame: f64, r: usize, drop_blank_ends: bool) -> Vec<Placed> {
    if !(pitch.is_finite() && pitch >= 1.0 && phase.is_finite() && frame.is_finite()) {
        return Vec::new();
    }
    let n = s.len() as f64;
    let gap = (pitch - frame).max(2.0);
    let max_half = (0.6 * gap).max(r as f64);
    // Teeth of equal comb score form a plateau, so a tooth can sit a few px off its gap: snap to
    // the purest line within 3.5 % of the pitch.
    let snap = ((0.035 * pitch).round() as usize).max(r);
    // Gaps: (centre, measured width, evidence). The centre of a visible gap is the middle of its
    // level core (lines within 0.12 level closeness of the snapped line); content as thin as base next to a gap widens the soft flanks but
    // hardly the core.
    let mut gaps: Vec<(f64, Option<f64>, f32)> = Vec::new();
    let mut k = ((-pitch - phase) / pitch).floor();
    loop {
        let t = phase + k * pitch;
        k += 1.0;
        if t > n + pitch {
            break;
        }
        if t < -pitch {
            continue;
        }
        if t >= 0.0 && t < n {
            let ti = t.round() as usize;
            let (lo, hi) = (ti.saturating_sub(snap), (ti + snap + 1).min(s.len()));
            let (c, peak) = (lo..hi).map(|i| (i, s[i])).fold((ti.min(s.len().saturating_sub(1)), -1.0f32), |b, x| if x.1 > b.1 { x } else { b });
            if peak >= 0.15 {
                // Extent: lines whose level is within 0.12 of the snapped line's level.
                let thr = level.get(c).copied().unwrap_or(0.0) - 0.12;
                let mut a = c;
                while a > 0 && level[a - 1] >= thr && (c - a) as f64 + 1.0 <= max_half {
                    a -= 1;
                }
                let mut b = c;
                while b + 1 < level.len() && level[b + 1] >= thr && (b - c) as f64 + 1.0 <= max_half {
                    b += 1;
                }
                let (ca, cb, wa, wb) = (a, b + 1, a, b + 1);
                gaps.push(((ca + cb) as f64 / 2.0, (peak >= 0.4).then_some((wb - wa) as f64), peak));
            } else {
                gaps.push((t, None, peak.max(0.0)));
            }
        } else {
            let cut = if t < 0.0 { run.bare_before } else { run.bare_after };
            gaps.push((t, None, if cut { 0.7 } else { 0.4 }));
        }
    }
    // One camera, one gap width: frame edges sit half the strip's typical gap from each centre.
    let mut widths: Vec<f32> = gaps.iter().filter_map(|g| g.1.map(|w| w as f32)).collect();
    let typical = if widths.is_empty() {
        gap
    } else {
        let m = (widths.len() - 1) / 2;
        f64::from(*widths.select_nth_unstable_by(m, f32::total_cmp).1)
    }
    .clamp(2.0, 2.0 * max_half);
    let mut frames = Vec::new();
    for pair in gaps.windows(2) {
        let (g0, g1) = (pair[0], pair[1]);
        let (mut a, mut b) = (g0.0 + typical / 2.0, g1.0 - typical / 2.0);
        if ((b - a) - frame).abs() > 0.12 * frame {
            let mid = (g0.0 + g1.0) / 2.0;
            a = mid - frame / 2.0;
            b = mid + frame / 2.0;
        }
        let (ca, cb) = (a.max(0.0), b.min(n));
        if cb - ca < 0.6 * frame {
            continue;
        }
        let partial = cb - ca < 0.97 * (b - a);
        let (ia, ib) = (ca.floor() as usize, (cb.ceil() as usize).min(s.len()));
        let gapish = s.get(ia..ib).map_or(0, |w| w.iter().filter(|&&v| v >= 0.5).count());
        let blank = ib > ia && gapish as f64 > 0.8 * (ib - ia) as f64;
        let ev = g0.2.min(g1.2).clamp(0.0, 1.0);
        let mut conf = 0.35 + 0.65 * ev;
        if partial {
            conf *= 0.6;
        }
        if blank {
            conf *= 0.5;
        }
        frames.push(Placed { start: ca, end: cb, confidence: conf, blank, partial });
    }
    if drop_blank_ends {
        while frames.first().is_some_and(|f| f.blank) {
            frames.remove(0);
        }
        while frames.last().is_some_and(|f| f.blank) {
            frames.pop();
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_from(profile: &[(f32, f32)]) -> Vec<Line> {
        profile.iter().map(|&(m, s)| Line { median: m, spread: s, bare: false }).collect()
    }

    /// A pitch comb rejects one aperiodic false gap and fills one invisible gap.
    #[test]
    fn comb_rejects_false_gap_and_fills_missing_gap() {
        let pitch = 100usize;
        let mut prof = vec![(0.3f32, 0.1f32); 6 * pitch];
        for k in 0..6 {
            if k == 3 {
                continue; // invisible gap
            }
            for i in 0..6 {
                prof[(k * pitch + i).min(6 * pitch - 1)] = (0.5, 0.005);
            }
        }
        for i in 140..146 {
            prof[i] = (0.5, 0.005); // false gap mid-frame
        }
        let lines = lines_from(&prof);
        let run = Run { start: 0, end: lines.len(), bare_before: true, bare_after: true };
        let cfg = BedConfig::default();
        let p = place_frames(&lines, &run, Layout::Prior { pitch: 100.0, frame: 94.0, tol: 0.04 }, 1.0, &cfg);
        assert!(!p.fallback);
        assert!((p.pitch.unwrap() - 100.0).abs() < 1.0, "{:?}", p.pitch);
        assert_eq!(p.frames.len(), 6, "{:?}", p.frames);
        for (k, f) in p.frames.iter().enumerate() {
            let c = (f.start + f.end) / 2.0;
            assert!((c - (k as f64 * 100.0 + 53.0)).abs() < 5.0, "frame {k}: {f:?}");
        }
    }

    /// One strip whose content adds a strong band between every pair of gaps fits half the pitch
    /// on its own; the strips of the same roll outvote it.
    #[test]
    fn consensus_pitch_outvotes_a_misleading_strip() {
        let make = |extra: bool| {
            let mut s = vec![0.05f32; 600];
            for k in 0..6 {
                for i in 0..4 {
                    s[k * 100 + i] = 0.9;
                    if extra {
                        s[k * 100 + 50 + i] = 0.9;
                    }
                }
            }
            window_max(&s, 2)
        };
        let alone = consensus_pitch(&[make(true)], 30.0, 150.0, 0.85).unwrap();
        assert!((alone - 50.0).abs() < 1.0, "{alone}");
        let together = consensus_pitch(&[make(false), make(true), make(false)], 30.0, 150.0, 0.85).unwrap();
        assert!((together - 100.0).abs() < 1.0, "{together}");
    }

    #[test]
    fn degenerate_pitches_do_not_hang() {
        let s = vec![0.5f32; 50];
        assert!(comb_at(&s, 0.0).is_none() && comb_at(&s, f64::NAN).is_none());
        let run = Run { start: 0, end: 50, bare_before: false, bare_after: false };
        assert!(frames_from_comb(&s, &s, &run, f64::NAN, 0.0, 10.0, 2, true).is_empty());
        assert!(frames_from_comb(&s, &s, &run, 0.0, 0.0, 10.0, 2, true).is_empty());
    }

    #[test]
    fn free_pitch_prefers_the_fundamental() {
        let pitch = 60usize;
        let mut prof = vec![(0.3f32, 0.1f32); 8 * pitch];
        for k in 0..8 {
            for i in 0..4 {
                prof[k * pitch + i] = (0.5, 0.004);
            }
        }
        let lines = lines_from(&prof);
        let run = Run { start: 0, end: lines.len(), bare_before: false, bare_after: false };
        let p = place_frames(&lines, &run, Layout::Free { lo: 16.0, hi: 200.0 }, 1.0, &BedConfig::default());
        assert!((p.pitch.unwrap() - 60.0).abs() < 1.0, "{:?}", p.pitch);
        assert!(p.frames.len() >= 7);
    }

    #[test]
    fn film_runs_split_on_bare_light_and_bridge_glints() {
        let mut lines = vec![Line { median: 0.4, spread: 0.1, bare: false }; 100];
        for l in &mut lines[0..10] {
            l.bare = true;
        }
        lines[50].bare = true; // glint
        for l in &mut lines[70..80] {
            l.bare = true;
        }
        let runs = film_runs(&lines, 5.0, 3.0);
        assert_eq!(
            runs,
            vec![Run { start: 10, end: 70, bare_before: true, bare_after: true }, Run { start: 80, end: 100, bare_before: true, bare_after: false }]
        );
    }
}
