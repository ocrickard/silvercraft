//! Line defects: narrow streaks along the scan direction.
//!
//! A flatbed CCD images one column per sensor element for the whole pass, so a weak element or a
//! speck of dust on the sensor, mirror or lens leaves a column that is consistently brighter or
//! darker than its neighbours (full length), or that starts or stops when the dust moves
//! (partial). Everything here works in a canonical orientation where streaks are columns; the
//! public functions transpose for [`StreakAxis::Rows`].
//!
//! **Per-pixel evidence.** Each pixel is compared with the `baseline_radius` pixels on its left and
//! on its right. Each side gives a median (a streak narrower than the radius cannot move it) and a
//! slope (the median first difference, which one step cannot move), and is clean when its near and
//! far halves differ by no more than that slope explains. Between two clean sides the baseline is
//! interpolated (exact on a gradient). Beside an edge the side holding the edge is not clean and
//! the baseline is extrapolated from the other side, but only when the unclean side's far half is
//! step-free and back on that trend (a streak spilling into it, not an edge). A pixel on a step
//! between two clean sides (the jump exceeds their slopes by more than `step_ratio` times its own
//! deviation) and a pixel with no trustworthy baseline carry no evidence (NaN). The relative
//! residual over the noise (MAD of second differences, floored at `min_abs_noise` and
//! `min_rel_noise`) is a z-score clipped to `±z_clip`.
//!
//! **Candidates.** The z-scores are averaged along the line over `±smooth_radius` and high-passed
//! across columns (minus their median over `±baseline_radius` columns), which cancels smooth scene
//! curvature that biases many adjacent baselines alike. Runs above `z_threshold` with one sign are
//! bridged across short gaps; their extent is the maximum-sum interval of the z-scores (both change
//! points at once, undecidable rows weightless, no bridging into separate content). A faint
//! full-length streak (a weak element: a fraction of the noise per pixel, but on every row) is
//! found by the robust column profile instead: a column whose mean high-passed z over the whole
//! scan is `full_mad_k` MADs from the median of all columns, in every quarter of the scan.
//!
//! **Content rejection.** A candidate is kept only when it is long (full length, or a partial
//! streak of at least `min_partial_fraction` reaching the start or end of the scan), narrow (its
//! measured width, against a baseline excluding `max_width` around it, is at most `max_width`;
//! a band wider than `max_dead_width` that does not follow the scene is a frame gap seen edge-on),
//! not the halo of a much stronger column-aligned edge within `2·baseline_radius` (`halo_ratio`:
//! glow beside a film edge, halation), stationary (split into `consistency_segments`, a constant
//! gain, offset or stuck value with a robust coefficient of variation of at most `max_variation`;
//! a pole's contrast fades) and visible (at least `min_deviation`). Walls, poles, frame borders and
//! film edges each fail at least one of these.
//!
//! **Repair.** Each defective column is fitted, over the defect's extent, against the
//! interpolation of the clean columns just outside the defect: a gain (the prior for a
//! transmittance defect), an offset, or a free gain and offset, each later model taken only when
//! it fits at least 20 % better. A column whose gain is within `max_gain_deviation` of 1 and whose
//! corrected values sit about as close to that reference as clean columns do is corrected by
//! inverting the fit (its own texture is kept). Dead columns (gain below `dead_gain`, unfit or too
//! strong) are replaced by linear interpolation between the clean columns on either side.
//! Non-finite pixels are never written.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use silvercraft_imgproc::Plane;

use crate::config::{LineConfig, StreakAxis};
use crate::util::{MAD_SIGMA, median_in_place, median_mad, transpose};
use crate::{Channel, Result};

/// Values at or above this are treated as saturated and kept out of the fits.
const SATURATED: f32 = 0.998;

/// Whether a streak is brighter or darker than its surroundings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Polarity {
    Bright,
    Dark,
}

/// How one defective line is (or would be) repaired.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum LineRepair {
    /// The line follows the scene with a gain and offset: `clean = (value - offset) / gain`.
    Gain { gain: f32, offset: f32 },
    /// The line is dead or not correctable: interpolate from the clean lines on either side.
    Interpolate,
}

/// One detected streak (a group of adjacent defective lines).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineDefect {
    pub channel: Channel,
    pub axis: StreakAxis,
    /// First defective line (a column index for [`StreakAxis::Columns`]).
    pub position: usize,
    /// Number of adjacent defective lines.
    pub width: usize,
    /// First pixel along the line covered by the defect (a row for columns).
    pub start: usize,
    /// Last pixel along the line covered by the defect (inclusive).
    pub end: usize,
    /// Length of the scan along the line, for context.
    pub line_length: usize,
    /// The defect spans (nearly) the whole scan.
    pub full_length: bool,
    pub polarity: Polarity,
    /// Median relative deviation from the baseline on the strongest line (-0.02 = 2 % darker).
    pub deviation: f32,
    /// Mean clipped z-score over the extent on the strongest line.
    pub score: f32,
    /// Planned repair for each line, `position..position + width`.
    pub repairs: Vec<LineRepair>,
    /// Set once the repair has been applied.
    pub repaired: bool,
}

impl LineDefect {
    /// Human-readable one-liner, e.g. "dark full-length streak at column 312 in green (2.3 %)".
    pub fn describe(&self) -> String {
        let (line, along) = match self.axis {
            StreakAxis::Columns => ("column", "rows"),
            StreakAxis::Rows => ("row", "columns"),
        };
        let pos = if self.width > 1 {
            format!("{line}s {}–{}", self.position, self.position + self.width - 1)
        } else {
            format!("{line} {}", self.position)
        };
        let kind = if self.full_length { "full-length".to_owned() } else { format!("partial ({along} {}–{})", self.start, self.end) };
        let pol = match self.polarity {
            Polarity::Bright => "bright",
            Polarity::Dark => "dark",
        };
        let dead = self.repairs.iter().any(|r| matches!(r, LineRepair::Interpolate));
        format!(
            "{pol} {kind} streak at {pos} in {}: {:.1}% {}",
            self.channel.name(),
            100.0 * self.deviation.abs(),
            if dead { "(dead, interpolated)" } else { "(gain-corrected)" }
        )
    }
}

/// Detect line defects on one plane.
pub fn detect_lines(plane: &Plane, axis: StreakAxis, channel: Channel, cfg: &LineConfig) -> Result<Vec<LineDefect>> {
    if !cfg.detect {
        return Ok(Vec::new());
    }
    let canon = match axis {
        StreakAxis::Columns => None,
        StreakAxis::Rows => Some(transpose(plane)?),
    };
    let p = canon.as_ref().unwrap_or(plane);
    let mut out = detect_canonical(p, cfg)?;
    for d in &mut out {
        d.channel = channel;
        d.axis = axis;
    }
    Ok(out)
}

/// Apply the planned repairs of `defects` (as returned by [`detect_lines`] for this plane) and mark
/// them repaired. Returns the number of defects changed. Repairs disabled in `cfg` are skipped.
pub fn repair_lines(plane: &mut Plane, axis: StreakAxis, defects: &mut [LineDefect], cfg: &LineConfig) -> Result<usize> {
    let wanted = |r: &LineRepair| match r {
        LineRepair::Gain { .. } => cfg.correct_gain,
        LineRepair::Interpolate => cfg.interpolate_dead,
    };
    if !defects.iter().any(|d| d.axis == axis && d.repairs.iter().any(wanted)) {
        return Ok(0);
    }
    let mut canon = match axis {
        StreakAxis::Columns => plane.clone(),
        StreakAxis::Rows => transpose(plane)?,
    };
    let mut n = 0;
    for d in defects.iter_mut().filter(|d| d.axis == axis) {
        if apply_canonical(&mut canon, d, cfg) {
            d.repaired = true;
            n += 1;
        }
    }
    *plane = match axis {
        StreakAxis::Columns => canon,
        StreakAxis::Rows => transpose(&canon)?,
    };
    Ok(n)
}

/// A side needs this many finite pixels to count.
const MIN_SIDE: usize = 3;

/// One side of a core on one row: its median and centre, its own slope, its far half (away from
/// the core), and whether it is clean (its near and far halves differ by no more than the slope
/// explains: no step or streak inside it).
#[derive(Debug, Clone, Copy)]
struct Side {
    median: f32,
    centre: f32,
    slope: f32,
    far_median: f32,
    far_centre: f32,
    /// How far the far half's two quarters depart from the slope (a step inside it).
    far_step: f32,
    clean: bool,
}

impl Side {
    /// The side's linear trend evaluated at `x`.
    fn at(&self, x: f32) -> f32 {
        self.median + self.slope * (x - self.centre)
    }
}

/// The pixels `[a, b)` of `row` as one side; `far_low` when the far half is the low-x half.
fn side(row: &[f32], a: usize, b: usize, far_low: bool, sigma: f32, buf: &mut Vec<f32>) -> Option<Side> {
    let px = &row[a..b];
    if px.iter().filter(|v| v.is_finite()).count() < MIN_SIDE {
        return None;
    }
    let med = |x: &[f32], buf: &mut Vec<f32>| {
        buf.clear();
        buf.extend_from_slice(x);
        median_in_place(buf)
    };
    let median = med(px, buf)?;
    let half = px.len() / 2;
    let (lo, hi) = px.split_at(half);
    let (lo_m, hi_m) = (med(lo, buf)?, med(hi, buf)?);
    buf.clear();
    buf.extend(px.windows(2).map(|p| p[1] - p[0]));
    // The median first difference: a step or a streak edge is one or two outliers among them.
    let slope = median_in_place(buf).unwrap_or(0.0);
    let consistent = |a: f32, b: f32, n: usize| ((b - a) - slope * n as f32).abs() <= 4.0 * sigma;
    let clean = consistent(lo_m, hi_m, px.len() / 2);
    let centre = |s: usize, e: usize| 0.5 * (s + e - 1) as f32;
    let far = if far_low { lo } else { hi };
    let q = far.len() / 2;
    let far_step =
        if q == 0 { 0.0 } else { med(&far[..q], buf).zip(med(&far[q..], buf)).map_or(f32::INFINITY, |(a, b)| ((b - a) - slope * q as f32).abs()) };
    let (far_median, far_centre) = if far_low { (lo_m, centre(a, a + half)) } else { (hi_m, centre(a + half, b)) };
    Some(Side { median, centre: centre(a, b), slope, far_median, far_centre, far_step, clean })
}

/// The two sides of a core `[x0, x1]` on one row (`r` pixels each), the pixel position measured,
/// and the noise sigma from the MAD of the sides' second differences (immune to gradients).
#[derive(Debug, Clone, Copy)]
struct Sides {
    x: f32,
    left: Option<Side>,
    right: Option<Side>,
    sigma: f32,
}

impl Sides {
    /// `x` is the pixel measured (inside the core); `sigma_floor` keeps a noiseless region from
    /// calling every wiggle a step.
    fn new(row: &[f32], x: usize, x0: usize, x1: usize, r: usize, sigma_floor: f32, buf: &mut Vec<f32>) -> Self {
        let n = row.len();
        let (l0, l1) = (x0.saturating_sub(r), x0.min(n));
        let (r0, r1) = ((x1 + 1).min(n), (x1 + r + 1).min(n));
        buf.clear();
        let dd = |s: &[f32], buf: &mut Vec<f32>| buf.extend(s.windows(3).map(|t| (t[0] - 2.0 * t[1] + t[2]).abs()));
        dd(&row[l0..l1], buf);
        dd(&row[r0..r1], buf);
        // sigma of a second difference is sqrt(6) times the pixel sigma.
        let sigma = median_in_place(buf).map_or(0.0, |mad| MAD_SIGMA * mad / 6.0f32.sqrt()).max(sigma_floor);
        let left = side(row, l0, l1, true, sigma, buf);
        let right = side(row, r0, r1, false, sigma, buf);
        Self { x: x as f32, left, right, sigma }
    }

    /// The clean baseline for value `v`: interpolated between two clean sides (exact on a
    /// gradient), or extrapolated from the one clean side when the other side's far half returns
    /// to that trend (a streak spilling into it, not an edge). `None` when undecidable.
    fn baseline(&self, v: f32) -> Option<f32> {
        let clean = |s: Option<Side>| s.filter(|s| s.clean);
        let one_sided = |good: Side, other: Option<Side>| {
            let base = good.at(self.x);
            // Trust one side only when the other side's far half has no step of the pixel's own
            // size inside it and is back on the good side's trend: a streak spilling into it, not
            // an edge inside it.
            let dev = (v - base).abs();
            match other {
                Some(o) if o.far_step > (4.0 * self.sigma).max(0.5 * dev) || (o.far_median - good.at(o.far_centre)).abs() >= 0.5 * dev => None,
                _ => Some(base),
            }
        };
        match (clean(self.left), clean(self.right)) {
            (Some(l), Some(r)) => {
                let t = ((self.x - l.centre) / (r.centre - l.centre).max(1e-6)).clamp(0.0, 1.0);
                Some(l.median * (1.0 - t) + r.median * t)
            }
            (Some(l), None) => one_sided(l, self.right),
            (None, Some(r)) => one_sided(r, self.left),
            (None, None) => None,
        }
    }

    /// Whether `v` sits on a step between two clean sides: their difference exceeds what their
    /// slopes predict by more than 4 sigma and by more than `ratio` times the pixel's deviation.
    fn is_step(&self, v: f32, ratio: f32) -> bool {
        match (self.left.filter(|s| s.clean), self.right.filter(|s| s.clean)) {
            (Some(l), Some(r)) => {
                let predicted = 0.5 * (l.slope + r.slope) * (r.centre - l.centre);
                let excess = ((r.median - l.median) - predicted).abs();
                let base = self.baseline(v).unwrap_or(v);
                excess > 4.0 * self.sigma && excess > ratio * (v - base).abs()
            }
            _ => false,
        }
    }
}

/// Clipped per-pixel z-scores, stored column-major (one contiguous slice per column); NaN where
/// undecidable.
fn zscores(p: &Plane, cfg: &LineConfig) -> Vec<f32> {
    let (w, h) = (p.width(), p.height());
    let r = cfg.baseline_radius;
    let mut rows = vec![0.0f32; w * h];
    rows.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let row = p.row(y);
        let mut buf = Vec::with_capacity(2 * r + 1);
        for (x, o) in out.iter_mut().enumerate() {
            let v = row[x];
            let sides = Sides::new(row, x, x, x, r, 0.25 * cfg.min_rel_noise * row[x].abs().max(cfg.signal_floor), &mut buf);
            // Both sides must exist to tell a streak from an edge: the outermost columns are not
            // measured.
            *o = match sides.baseline(v) {
                Some(m) if v.is_finite() && sides.left.is_some() && sides.right.is_some() && !sides.is_step(v, cfg.step_ratio) => {
                    let base = m.max(cfg.signal_floor);
                    let noise = (sides.sigma.max(cfg.min_abs_noise) / base).max(cfg.min_rel_noise);
                    ((v - m) / base / noise).clamp(-cfg.z_clip, cfg.z_clip)
                }
                // Undecidable (edge, missing value, outermost column): no evidence either way.
                _ => f32::NAN,
            };
        }
    });
    // Column-major for the along-line pass.
    let mut cols = vec![0.0f32; w * h];
    cols.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        for (y, v) in col.iter_mut().enumerate() {
            *v = rows[y * w + x];
        }
    });
    cols
}

/// Along-line running mean of the z-scores, minus its median over the neighbouring columns
/// `max_width < |dx| <= baseline_radius`:
/// smooth scene curvature biases the baseline of many adjacent columns alike and cancels; a narrow
/// streak does not. Column-major in and out.
fn smoothed_high_pass(z: &[f32], w: usize, h: usize, cfg: &LineConfig) -> Vec<f32> {
    let along = cfg.smooth_radius.min(h / 4);
    let smooth: Vec<f32> = z.par_chunks(h).flat_map_iter(|col| running_mean(col, along)).collect();
    let r = cfg.baseline_radius;
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        let lo = x.saturating_sub(r);
        let hi = (x + r).min(w - 1);
        let mut buf = Vec::with_capacity(2 * r + 1);
        for (y, o) in col.iter_mut().enumerate() {
            buf.clear();
            // Columns within `max_width` may belong to the streak itself: leave them out.
            // An undecidable neighbour carries no bias: count it as zero, so that a wide streak
            // whose edges are undecidable cannot become the majority of its own neighbourhood.
            buf.extend((lo..=hi).map(|i| smooth[i * h + y]).map(|v| if v.is_finite() { v } else { 0.0 }));
            *o = smooth[x * h + y] - median_in_place(&mut buf).unwrap_or(0.0);
        }
    });
    out
}

/// Running mean of the finite values over `±r` (window truncated at the ends); NaN where the
/// window has none.
fn running_mean(z: &[f32], r: usize) -> Vec<f32> {
    let n = z.len();
    let mut sum = vec![0.0f64; n + 1];
    let mut cnt = vec![0usize; n + 1];
    for (i, &v) in z.iter().enumerate() {
        let ok = v.is_finite();
        sum[i + 1] = sum[i] + if ok { f64::from(v) } else { 0.0 };
        cnt[i + 1] = cnt[i] + usize::from(ok);
    }
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(r);
            let hi = (i + r + 1).min(n);
            let c = cnt[hi] - cnt[lo];
            if c == 0 { f32::NAN } else { ((sum[hi] - sum[lo]) / c as f64) as f32 }
        })
        .collect()
}

/// Mean of `sign·z` over the finite values of `z`.
fn mean_level(z: &[f32], sign: f32) -> f32 {
    let (s, n) = z.iter().filter(|v| v.is_finite()).fold((0.0f32, 0usize), |(s, n), v| (s + sign * v, n + 1));
    if n == 0 { 0.0 } else { s / n as f32 }
}

/// One qualifying run on one column.
#[derive(Debug, Clone, Copy)]
struct Run {
    x: usize,
    sign: f32,
    start: usize,
    end: usize,
    score: f32,
    /// Found by the whole-column profile: full length by construction.
    whole: bool,
}

fn column_runs(x: usize, smooth: &[f32], z: &[f32], cfg: &LineConfig) -> Vec<Run> {
    let h = z.len();
    let max_gap = ((cfg.max_gap_fraction * h as f32) as usize).max(1);
    let tol = (cfg.edge_tolerance_fraction * h as f32) as usize + cfg.smooth_radius;
    let mut out = Vec::new();
    for sign in [1.0f32, -1.0] {
        let flagged: Vec<usize> = (0..h).filter(|&y| sign * smooth[y] >= cfg.z_threshold).collect();
        let mut i = 0;
        while i < flagged.len() {
            let mut j = i;
            while j + 1 < flagged.len() && flagged[j + 1] - flagged[j] <= max_gap {
                j += 1;
            }
            let (start, end) = (flagged[i], flagged[j]);
            let len = end - start + 1;
            let decided = z[start..=end].iter().filter(|v| v.is_finite()).count().max(1);
            let coverage = (j - i + 1) as f32 / decided as f32;
            let full = len as f32 >= cfg.full_length_fraction * h as f32;
            let touches = start <= tol || end + tol >= h - 1;
            let partial = cfg.detect_partial && len as f32 >= cfg.min_partial_fraction * h as f32 && (touches || !cfg.partial_must_touch_edge);
            if coverage >= cfg.min_coverage && (full || partial) {
                let score = mean_level(&z[start..=end], sign);
                out.push(Run { x, sign, start, end, score, whole: false });
            }
            i = j + 1;
        }
    }
    out
}

/// Columns whose mean high-passed z-score over the whole scan is a robust outlier among all
/// columns (at least `full_z_threshold` and `full_mad_k` MADs from the median), with the excess
/// present in every quarter of the column.
fn whole_column_runs(hp: &[f32], w: usize, h: usize, cfg: &LineConfig) -> Vec<Run> {
    let mean = |s: &[f32]| {
        let (sum, n) = s.iter().filter(|v| v.is_finite()).fold((0.0f64, 0usize), |(a, n), v| (a + f64::from(*v), n + 1));
        if 2 * n < s.len() { f32::NAN } else { (sum / n as f64) as f32 }
    };
    let means: Vec<f32> = hp.chunks(h).map(mean).collect();
    let Some((med, mad)) = median_mad(&means) else { return Vec::new() };
    let lim = cfg.full_z_threshold.max(cfg.full_mad_k * MAD_SIGMA * mad);
    (0..w)
        .filter(|&x| (means[x] - med).abs() >= lim)
        .filter_map(|x| {
            let d = means[x] - med;
            // Full length means the evidence is spread along the column: every quarter carries at
            // least half of it (a partial streak or a pole is left to the local test).
            let col = &hp[x * h..(x + 1) * h];
            let spread = col.chunks(h.div_ceil(4)).all(|q| (mean(q) - med) * d.signum() >= 0.5 * d.abs());
            spread.then_some(Run { x, sign: d.signum(), start: 0, end: h - 1, score: d.abs(), whole: true })
        })
        .collect()
}

/// The interval of a column holding the streak: the maximum-sum run of `sign·z − τ` (Kadane)
/// within `±reach` of the candidate run, with τ half the run's mean level and undecidable rows
/// weightless (bridged inside, never added at the ends). Both change points are placed globally,
/// and a separate patch of content beyond a clean stretch is not bridged.
fn best_interval(z: &[f32], sign: f32, start: usize, end: usize, reach: usize) -> (usize, usize) {
    let h = z.len();
    let tau = 0.5 * mean_level(&z[start..=end], sign).max(0.0);
    let (lo, hi) = (start.saturating_sub(reach), (end + reach).min(h - 1));
    let mut best = (f32::NEG_INFINITY, start, end);
    let (mut acc, mut from) = (0.0f32, lo);
    for y in lo..=hi {
        if acc <= 0.0 {
            acc = 0.0;
            from = y;
        }
        // Undecidable rows carry no evidence.
        acc += if z[y].is_finite() { sign * z[y] - tau } else { 0.0 };
        if acc > best.0 {
            best = (acc, from, y);
        }
    }
    (best.1, best.2)
}

fn detect_canonical(p: &Plane, cfg: &LineConfig) -> Result<Vec<LineDefect>> {
    let (w, h) = (p.width(), p.height());
    // Too small to tell a streak from content.
    if w < 2 * cfg.max_width + 3 || h < 16 {
        return Ok(Vec::new());
    }
    let z = zscores(p, cfg);
    let hp = smoothed_high_pass(&z, w, h, cfg);
    let mut runs: Vec<Run> = z.par_chunks(h).zip(hp.par_chunks(h)).enumerate().flat_map_iter(|(x, (col, s))| column_runs(x, s, col, cfg)).collect();
    // Faint full-length streaks (a weak CCD element: a fraction of the noise per pixel, but on
    // every row) are invisible to the local test; the whole-column profile finds them as robust
    // outliers among all columns.
    // Such a column's whole-scan evidence supersedes any local run on it.
    let whole = whole_column_runs(&hp, w, h, cfg);
    runs.retain(|q| !whole.iter().any(|r| r.x == q.x && r.sign == q.sign));
    runs.extend(whole);
    runs.sort_by(|a, b| a.sign.total_cmp(&b.sign).then(a.x.cmp(&b.x)).then(a.start.cmp(&b.start)));

    // Group adjacent (or nearly adjacent) columns of one sign whose extents overlap.
    let mut groups: Vec<Vec<Run>> = Vec::new();
    for run in runs {
        let joined = groups.iter_mut().rev().take(4).find(|g| {
            g.last().is_some_and(|last| {
                let overlap = run.end.min(last.end) as i64 - run.start.max(last.start) as i64 + 1;
                let shorter = (run.end - run.start).min(last.end - last.start) as i64 + 1;
                // Up to two undecidable columns inside a wide defect are bridged.
                last.sign == run.sign && run.x > last.x && run.x <= last.x + 3 && 2 * overlap >= shorter
            })
        });
        match joined {
            Some(g) => g.push(run),
            None => groups.push(vec![run]),
        }
    }

    let mut out = Vec::new();
    for g in groups {
        let (Some(first), Some(last)) = (g.first(), g.last()) else { continue };
        let width = last.x - first.x + 1;
        if width > cfg.max_width {
            continue; // too wide: image content
        }
        let Some(strong) = g.iter().copied().max_by(|a, b| a.score.total_cmp(&b.score)) else { continue };
        let col = &z[strong.x * h..(strong.x + 1) * h];
        let (s, e) =
            if strong.whole { (0, h - 1) } else { best_interval(col, strong.sign, strong.start, strong.end, 2 * cfg.smooth_radius.min(h / 4)) };
        let tol = (cfg.edge_tolerance_fraction * h as f32) as usize;
        let (start, end) = (if s <= tol { 0 } else { s }, if e + tol >= h - 1 { h - 1 } else { e });
        let len = end - start + 1;
        let full = len as f32 >= cfg.full_length_fraction * h as f32;
        let touches = start == 0 || end == h - 1;
        if !full && !(cfg.detect_partial && len as f32 >= cfg.min_partial_fraction * h as f32 && (touches || !cfg.partial_must_touch_edge)) {
            continue;
        }
        // A full-length defect is repaired over the whole line: texture only hides it locally.
        let (start, end) = if full { (0, h - 1) } else { (start, end) };
        // The anomaly's real width, measured against a baseline that excludes `max_width` on both
        // sides: undecidable inner columns are filled in, and a wider band is content.
        let (x0, x1) = measured_extent(p, (first.x, last.x), start, end, strong.sign, cfg);
        let width = x1 - x0 + 1;
        if width > cfg.max_width {
            continue;
        }
        let abs_dev = column_abs_deviation(p, strong.x, (x0, x1), start, end);
        if near_strong_edge(p, (x0, x1), start, end, abs_dev, cfg) {
            continue; // the halo of a much stronger column-aligned edge (film edge glow, halation)
        }
        if !is_stationary(p, strong.x, (x0, x1), start, end, cfg) {
            continue; // contrast varies along the line: image content
        }
        let reference = neighbour_residual(p, (x0, x1), start, end, cfg);
        let repairs: Vec<LineRepair> = (x0..=x1).map(|x| plan_column(p, x, (x0, x1), start, end, reference, cfg)).collect();
        if width > cfg.max_dead_width && repairs.iter().any(|r| matches!(r, LineRepair::Interpolate)) {
            continue; // a wide band that does not follow the scene: content (a frame gap), not a dead element
        }
        let deviation = column_deviation(p, strong.x, (x0, x1), start, end, cfg);
        if deviation.abs() < cfg.min_deviation {
            continue; // too faint to see: leave the sensor's pattern alone
        }
        out.push(LineDefect {
            channel: Channel::Red,
            axis: StreakAxis::Columns,
            position: x0,
            width,
            start,
            end,
            line_length: h,
            full_length: end - start + 1 >= (cfg.full_length_fraction * h as f32) as usize,
            polarity: if strong.sign > 0.0 { Polarity::Bright } else { Polarity::Dark },
            deviation,
            score: strong.score * strong.sign,
            repairs,
            repaired: false,
        });
    }
    out.sort_by(|a, b| a.position.cmp(&b.position).then(a.start.cmp(&b.start)));
    Ok(out)
}

/// Samples for column `x` of the defect `skip = [x0, x1]` over rows `start..=end`:
/// `(value, reference)`, the reference interpolated between the clean columns just outside the
/// defect (the tightest local baseline, nearly free of texture bias).
fn column_samples(p: &Plane, x: usize, skip: (usize, usize), start: usize, end: usize) -> Vec<(f32, f32)> {
    (start..=end)
        .filter_map(|y| {
            let row = p.row(y);
            let n = near_reference(row, x, skip.0, skip.1)?;
            row[x].is_finite().then_some((row[x], n))
        })
        .collect()
}

/// Whether the defect on column `x` keeps a constant relative deviation (gain), a constant
/// absolute deviation (offset) or a constant value (stuck element) along its length.
fn is_stationary(p: &Plane, x: usize, skip: (usize, usize), start: usize, end: usize, cfg: &LineConfig) -> bool {
    let samples = column_samples(p, x, skip, start, end);
    let k = cfg.consistency_segments.max(1);
    if samples.len() < 4 * k {
        return false;
    }
    let seg_len = samples.len().div_ceil(k);
    let mut rel = Vec::with_capacity(k);
    let mut abs = Vec::with_capacity(k);
    let mut val = Vec::with_capacity(k);
    for seg in samples.chunks(seg_len) {
        let mut r: Vec<f32> = seg.iter().map(|&(v, m)| (v - m) / m.max(cfg.signal_floor)).collect();
        let mut a: Vec<f32> = seg.iter().map(|&(v, m)| v - m).collect();
        let mut v: Vec<f32> = seg.iter().map(|&(v, _)| v).collect();
        let (Some(r), Some(a), Some(v)) = (median_in_place(&mut r), median_in_place(&mut a), median_in_place(&mut v)) else {
            return false;
        };
        rel.push(r);
        abs.push(a);
        val.push(v);
    }
    // Content can mask a streak in a segment or two; what it cannot do is keep its contrast
    // constant along the line. Robust coefficient of variation across the segments.
    let constant = |s: &[f32]| match median_mad(s) {
        Some((m, mad)) => m != 0.0 && MAD_SIGMA * mad <= cfg.max_variation * m.abs(),
        None => false,
    };
    let stuck = match (median_mad(&val), median_mad(&abs)) {
        (Some((_, mad_v)), Some((dev, _))) => MAD_SIGMA * mad_v <= cfg.max_variation * dev.abs(),
        _ => false,
    };
    constant(&rel) || constant(&abs) || stuck
}

/// Rows of `start..=end`, thinned to at most 256 for the width and reference measurements.
fn sample_rows(start: usize, end: usize) -> impl Iterator<Item = usize> {
    (start..=end).step_by(((end - start + 1) / 256).max(1))
}

/// Grow the group `[g0, g1]` over adjacent columns that deviate the same way (by at least half the
/// group's own deviation) against a baseline excluding `max_width` columns around it.
fn measured_extent(p: &Plane, (g0, g1): (usize, usize), start: usize, end: usize, sign: f32, cfg: &LineConfig) -> (usize, usize) {
    let w = p.width();
    let (c0, c1) = (g0.saturating_sub(cfg.max_width), (g1 + cfg.max_width).min(w - 1));
    let mut buf = Vec::new();
    let rel: Vec<f32> = (c0..=c1)
        .map(|c| {
            let mut r: Vec<f32> = sample_rows(start, end)
                .filter_map(|y| {
                    let row = p.row(y);
                    let base = Sides::new(row, c, c0, c1, cfg.baseline_radius, 0.0, &mut buf).baseline(row[c])?;
                    Some(sign * (row[c] - base) / base.max(cfg.signal_floor))
                })
                .collect();
            median_in_place(&mut r).unwrap_or(0.0)
        })
        .collect();
    let level = rel[g0 - c0..=g1 - c0].iter().copied().fold(0.0f32, f32::max);
    let deviates = |c: usize| rel[c - c0] >= 0.5 * level && level > 0.0;
    let (mut lo, mut hi) = (g0, g1);
    while lo > c0 && deviates(lo - 1) {
        lo -= 1;
    }
    while hi < c1 && deviates(hi + 1) {
        hi += 1;
    }
    // Still deviating at the window's edge: wider than the window, report it as such.
    if (lo == c0 && c0 > 0) || (hi == c1 && c1 < w - 1) {
        return (c0, c1);
    }
    (lo, hi)
}

/// Median absolute deviation (v − baseline) of column `x` over the extent.
fn column_abs_deviation(p: &Plane, x: usize, skip: (usize, usize), start: usize, end: usize) -> f32 {
    let mut d: Vec<f32> = column_samples(p, x, skip, start, end).into_iter().map(|(v, m)| v - m).collect();
    median_in_place(&mut d).unwrap_or(0.0).abs()
}

/// Whether a persistent column-aligned step within `2·baseline_radius` of the group is more than
/// `halo_ratio` times the defect's own absolute deviation: then the "defect" is that edge's halo.
fn near_strong_edge(p: &Plane, (x0, x1): (usize, usize), start: usize, end: usize, abs_dev: f32, cfg: &LineConfig) -> bool {
    let w = p.width();
    let reach = 2 * cfg.baseline_radius;
    let (lo, hi) = (x0.saturating_sub(reach), (x1 + reach).min(w - 1));
    let mut jumps = Vec::new();
    let steps: Vec<f32> = (lo..hi)
        .map(|x| {
            jumps.clear();
            jumps.extend(sample_rows(start, end).map(|y| (p.get(x + 1, y) - p.get(x, y)).abs()));
            median_in_place(&mut jumps).unwrap_or(0.0)
        })
        .collect();
    // A step stands out from the window's typical pixel-to-pixel change (texture, gradient).
    let typical = median_in_place(&mut steps.clone()).unwrap_or(0.0);
    let strongest = steps.iter().copied().fold(0.0f32, f32::max) - typical;
    strongest > cfg.halo_ratio * abs_dev
}

fn column_deviation(p: &Plane, x: usize, skip: (usize, usize), start: usize, end: usize, cfg: &LineConfig) -> f32 {
    let mut rel: Vec<f32> = column_samples(p, x, skip, start, end).into_iter().map(|(v, m)| (v - m) / m.max(cfg.signal_floor)).collect();
    median_in_place(&mut rel).unwrap_or(0.0)
}

fn percentile_sorted(v: &[f32], q: f32) -> f32 {
    let i = ((v.len() - 1) as f32 * q).round() as usize;
    v[i.min(v.len() - 1)]
}

/// Choose gain/offset correction or interpolation for one defective column.
/// The value at `x` interpolated between the clean columns just outside `[x0, x1]` (or the one
/// that exists): the tightest local reference for a narrow defect.
fn near_reference(row: &[f32], x: usize, x0: usize, x1: usize) -> Option<f32> {
    let l = x0.checked_sub(1).map(|i| row[i]).filter(|v| v.is_finite());
    let r = row.get(x1 + 1).copied().filter(|v| v.is_finite());
    let t = (x as f32 - x0 as f32 + 1.0) / (x1 as f32 - x0 as f32 + 2.0);
    match (l, r) {
        (Some(a), Some(b)) => Some(a * (1.0 - t) + b * t),
        (a, b) => a.or(b),
    }
}

/// Median relative residual of the clean columns beside a defect of `width` against the same
/// kind of reference (interpolation across `width` columns): the yardstick a corrected line must
/// meet.
fn neighbour_residual(p: &Plane, (x0, x1): (usize, usize), start: usize, end: usize, cfg: &LineConfig) -> f32 {
    let (w, width) = (p.width(), x1 - x0 + 1);
    let pseudo = [x0.checked_sub(width + 2).map(|s| (s, s + width - 1)), Some((x1 + 3, x1 + 2 + width))];
    let mut per: Vec<f32> = pseudo
        .into_iter()
        .flatten()
        .filter(|&(_, e)| e + 1 < w)
        .flat_map(|(s, e)| (s..=e).map(move |c| (c, s, e)))
        .filter_map(|(c, s, e)| {
            let mut r: Vec<f32> = sample_rows(start, end)
                .filter_map(|y| {
                    let row = p.row(y);
                    let n = near_reference(row, c, s, e)?;
                    Some(((row[c] - n) / n.max(cfg.signal_floor)).abs())
                })
                .collect();
            median_in_place(&mut r)
        })
        .collect();
    median_in_place(&mut per).unwrap_or(0.0)
}

/// Choose gain/offset correction or interpolation for one defective column. Models are fitted
/// against the interpolation of the clean columns just outside the defect.
fn plan_column(p: &Plane, x: usize, (x0, x1): (usize, usize), start: usize, end: usize, reference: f32, cfg: &LineConfig) -> LineRepair {
    let samples: Vec<(f32, f32)> = column_samples(p, x, (x0, x1), start, end)
        .into_iter()
        .filter(|&(v, n)| v.is_finite() && n >= cfg.signal_floor && v < SATURATED && n < SATURATED)
        .collect();
    if samples.len() < 16 {
        return LineRepair::Interpolate;
    }
    let mut ns: Vec<f32> = samples.iter().map(|s| s.1).collect();
    ns.sort_unstable_by(f32::total_cmp);
    let spread = percentile_sorted(&ns, 0.9) - percentile_sorted(&ns, 0.1);
    // Median relative residual of a candidate correction.
    let residual = |(a, b): (f32, f32)| {
        let mut r: Vec<f32> = samples.iter().map(|&(v, n)| (((v - b) / a - n) / n).abs()).collect();
        median_in_place(&mut r).unwrap_or(f32::INFINITY)
    };
    let mut ratio: Vec<f32> = samples.iter().map(|&(v, n)| v / n).collect();
    let mut diff: Vec<f32> = samples.iter().map(|&(v, n)| v - n).collect();
    let mut candidates: Vec<(f32, f32)> = Vec::new();
    if let Some(g) = median_in_place(&mut ratio).filter(|g| *g > 0.0) {
        candidates.push((g, 0.0));
    }
    if let Some(b) = median_in_place(&mut diff) {
        candidates.push((1.0, b));
    }
    // Transmittance defects are multiplicative by nature: the gain model is the prior; offset
    // and then the free affine fit replace it only when they fit at least 20 % better.
    let affine = robust_affine(&samples).filter(|a| spread >= cfg.min_fit_spread && a.0 > 0.0);
    let mut best: Option<((f32, f32), f32)> = None;
    for c in candidates.into_iter().chain(affine) {
        let r = residual(c);
        if best.is_none_or(|(_, br)| r < 0.8 * br) {
            best = Some((c, r));
        }
    }
    let Some(((gain, offset), r)) = best else {
        return LineRepair::Interpolate;
    };
    let typical = percentile_sorted(&ns, 0.5);
    if !gain.is_finite()
        || !offset.is_finite()
        || gain < cfg.dead_gain
        || (gain - 1.0).abs() > cfg.max_gain_deviation
        || offset.abs() > cfg.max_gain_deviation * typical
    {
        return LineRepair::Interpolate;
    }
    // The corrected column must sit about as close to its reference as clean columns do.
    if r > cfg.max_fit_residual * reference.max(1e-4) {
        return LineRepair::Interpolate;
    }
    LineRepair::Gain { gain, offset }
}

/// Least-squares `v = a·m + b` with one 3-sigma rejection pass.
fn robust_affine(samples: &[(f32, f32)]) -> Option<(f32, f32)> {
    let fit = |keep: &dyn Fn(f64, f64) -> bool| -> Option<(f64, f64)> {
        let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
        for &(v, m) in samples {
            let (x, y) = (f64::from(m), f64::from(v));
            if keep(x, y) {
                n += 1.0;
                sx += x;
                sy += y;
                sxx += x * x;
                sxy += x * y;
            }
        }
        let det = n * sxx - sx * sx;
        if n < 8.0 || det.abs() < 1e-12 {
            return None;
        }
        let a = (n * sxy - sx * sy) / det;
        Some((a, (sy - a * sx) / n))
    };
    let (a, b) = fit(&|_, _| true)?;
    let mut abs_resid: Vec<f32> = samples.iter().map(|&(v, m)| (f64::from(v) - a * f64::from(m) - b).abs() as f32).collect();
    let (a, b) = match median_in_place(&mut abs_resid) {
        Some(mad) if mad > 0.0 => {
            let lim = 3.0 * f64::from(MAD_SIGMA * mad);
            fit(&|x, y| (y - a * x - b).abs() <= lim).unwrap_or((a, b))
        }
        _ => (a, b),
    };
    Some((a as f32, b as f32))
}

/// Apply one defect's repair to the canonical plane. Returns whether anything changed.
fn apply_canonical(p: &mut Plane, d: &LineDefect, cfg: &LineConfig) -> bool {
    let (w, h) = (p.width(), p.height());
    let x0 = d.position;
    let x1 = d.position + d.width.max(1) - 1;
    if x1 >= w || d.end >= h || d.start > d.end || d.repairs.len() != d.width {
        return false;
    }
    let left = x0.checked_sub(1);
    let right = (x1 + 1 < w).then_some(x1 + 1);
    let mut changed = false;
    let data = p.data_mut();
    for (i, repair) in d.repairs.iter().enumerate() {
        let x = x0 + i;
        match *repair {
            LineRepair::Gain { gain, offset } if cfg.correct_gain => {
                if !(gain.is_finite() && offset.is_finite() && gain > 0.0) {
                    continue;
                }
                for y in d.start..=d.end {
                    let v = &mut data[y * w + x];
                    if v.is_finite() {
                        *v = (*v - offset) / gain;
                        changed = true;
                    }
                }
            }
            LineRepair::Interpolate if cfg.interpolate_dead => {
                let t = (x as f32 - x0 as f32 + 1.0) / (x1 as f32 - x0 as f32 + 2.0);
                for y in d.start..=d.end {
                    if !data[y * w + x].is_finite() {
                        continue; // missing stays missing
                    }
                    let l = left.map(|lx| data[y * w + lx]).filter(|v| v.is_finite());
                    let r = right.map(|rx| data[y * w + rx]).filter(|v| v.is_finite());
                    let value = match (l, r) {
                        (Some(a), Some(b)) => a * (1.0 - t) + b * t,
                        (Some(a), None) => a,
                        (None, Some(b)) => b,
                        (None, None) => continue,
                    };
                    data[y * w + x] = value;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_mean_truncates_at_ends() {
        let m = running_mean(&[1.0, 2.0, 3.0, 4.0], 1);
        assert_eq!(m, vec![1.5, 2.0, 3.0, 3.5]);
        let m = running_mean(&[1.0, f32::NAN, 3.0, f32::NAN, f32::NAN, f32::NAN], 1);
        assert_eq!(&m[..4], &[1.0, 2.0, 3.0, 3.0]);
        assert!(m[5].is_nan());
    }

    #[test]
    fn best_interval_finds_change_points_and_does_not_bridge() {
        let mut z = vec![0.0f32; 200];
        for v in &mut z[40..120] {
            *v = -4.0;
        }
        // A short patch of content after a clean stretch.
        for v in &mut z[170..185] {
            *v = -4.0;
        }
        assert_eq!(best_interval(&z, -1.0, 30, 190, 10), (40, 119));
        // Undecidable rows inside are bridged, at the ends they are not claimed.
        for i in (30..40).chain(70..90) {
            z[i] = f32::NAN;
        }
        assert_eq!(best_interval(&z, -1.0, 30, 190, 10), (40, 119));
    }

    #[test]
    fn affine_fit_recovers_gain_offset() {
        let s: Vec<(f32, f32)> = (0..100).map(|i| i as f32 / 100.0).map(|m| (0.9 * m + 0.01, m)).collect();
        let (a, b) = robust_affine(&s).unwrap();
        assert!((a - 0.9).abs() < 1e-4 && (b - 0.01).abs() < 1e-4, "{a} {b}");
    }
}
