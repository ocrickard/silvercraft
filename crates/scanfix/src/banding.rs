//! Row banding: row-to-row gain flicker from the lamp or the carriage stepper.
//!
//! Banding multiplies a whole scan line by a gain. In the canonical orientation (streaks are
//! columns, so scan lines are rows) every pixel's clean baseline is the median of the `±radius`
//! pixels above and below it (a median keeps horizontal step edges exactly), and its log ratio to
//! that baseline estimates the row gain (the pixel itself is left out of its window, or ratios
//! would pile up at exactly zero and snap small gains away). The row's gain is the median of those ratios over the row
//! (content occupying less than half the row cannot move it); its standard error comes from the
//! MAD of second differences of ln v along the scan, which measure pixel noise only. Smooth scene curvature biases a median baseline coherently across a row, so the per-row
//! gain profile (and each segment's) is high-passed by its own running median over
//! `±profile_radius` rows: banding is what changes faster than that. When the robust RMS of that
//! profile (the banding score) reaches `pervasive_rms`, the scan flickers on every line and every
//! measured line is corrected by its estimate. Otherwise a row is corrected when it departs in its
//! own direction from the line through the nearest unflagged rows on both sides, which must not
//! differ by much more than its gain (the shoulder of a horizontal edge sits beside a step), and when its gain is significant (`z_threshold` standard errors and
//! at least `min_gain_deviation`), is matched within 0.5–1.5× by every one of the row's `segments`
//! (a gain scales the whole row alike; a feature in one part of the frame does not), and is no stronger than `max_gain_deviation`. Stronger rows
//! are reported as strong row defects (the original "transient rows") when they come in runs of at
//! most `max_strong_run` rows (a frame gap is a longer run), and are only interpolated from their
//! neighbours on request, since a thin full-width content line looks the same.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use silvercraft_imgproc::Plane;

use crate::config::{BandingConfig, StreakAxis};
use crate::util::{MAD_SIGMA, median_in_place, median_mad, transpose};
use crate::{Channel, Result};

/// One scan line with a significant gain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LineGain {
    /// Scan-line index (a row for [`StreakAxis::Columns`]).
    pub index: usize,
    /// Measured gain relative to its neighbours (1.004 = 0.4 % too bright).
    pub gain: f32,
}

/// Banding measurement (and repair record) of one plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Banding {
    pub channel: Channel,
    /// The streak axis; banding varies along the other one.
    pub axis: StreakAxis,
    /// Scan lines with enough usable pixels to measure.
    pub lines_measured: usize,
    /// Robust RMS of the per-line relative gain (0.003 = 0.3 %): the banding score.
    pub rms: f32,
    /// Largest |gain - 1| among measured lines.
    pub max_deviation: f32,
    /// Lines whose gain is significant and mild enough to correct.
    pub banded: Vec<LineGain>,
    /// Lines that deviate more than `max_gain_deviation` (transient rows or content).
    pub strong: Vec<LineGain>,
    /// Set once the repair has been applied.
    pub repaired: bool,
}

impl Banding {
    fn empty(channel: Channel, axis: StreakAxis) -> Self {
        Self { channel, axis, lines_measured: 0, rms: 0.0, max_deviation: 0.0, banded: Vec::new(), strong: Vec::new(), repaired: false }
    }
}

/// Measure banding on one plane.
pub fn detect_banding(plane: &Plane, axis: StreakAxis, channel: Channel, cfg: &BandingConfig) -> Result<Banding> {
    if !cfg.detect {
        return Ok(Banding::empty(channel, axis));
    }
    let canon = match axis {
        StreakAxis::Columns => None,
        StreakAxis::Rows => Some(transpose(plane)?),
    };
    let p = canon.as_ref().unwrap_or(plane);
    let mut b = detect_canonical(p, cfg)?;
    b.channel = channel;
    b.axis = axis;
    Ok(b)
}

/// Apply the banding correction measured by [`detect_banding`] on this plane. Returns the number
/// of scan lines changed.
pub fn repair_banding(plane: &mut Plane, banding: &mut Banding, cfg: &BandingConfig) -> Result<usize> {
    let do_gain = cfg.repair && !banding.banded.is_empty();
    let do_strong = cfg.interpolate_strong_rows && !banding.strong.is_empty();
    if !do_gain && !do_strong {
        return Ok(0);
    }
    let mut canon = match banding.axis {
        StreakAxis::Columns => plane.clone(),
        StreakAxis::Rows => transpose(plane)?,
    };
    let (w, h) = (canon.width(), canon.height());
    let mut n = 0;
    let data = canon.data_mut();
    if do_gain {
        for lg in &banding.banded {
            if lg.index >= h || !(lg.gain.is_finite() && lg.gain > 0.0) {
                continue;
            }
            for v in &mut data[lg.index * w..(lg.index + 1) * w] {
                if v.is_finite() {
                    *v /= lg.gain;
                }
            }
            n += 1;
        }
    }
    if do_strong {
        let bad: Vec<bool> = {
            let mut b = vec![false; h];
            for s in &banding.strong {
                if s.index < h {
                    b[s.index] = true;
                }
            }
            b
        };
        for y in (0..h).filter(|&y| bad[y]) {
            let above = (0..y).rev().find(|&i| !bad[i]);
            let below = (y + 1..h).find(|&i| !bad[i]);
            for x in 0..w {
                if !data[y * w + x].is_finite() {
                    continue; // missing stays missing
                }
                let a = above.map(|i| data[i * w + x]).filter(|v| v.is_finite());
                let b = below.map(|i| data[i * w + x]).filter(|v| v.is_finite());
                let value = match (above, a, below, b) {
                    (Some(ia), Some(va), Some(ib), Some(vb)) => {
                        let t = (y - ia) as f32 / (ib - ia) as f32;
                        va * (1.0 - t) + vb * t
                    }
                    (_, Some(va), _, None) => va,
                    (_, None, _, Some(vb)) => vb,
                    _ => continue,
                };
                data[y * w + x] = value;
            }
            n += 1;
        }
    }
    *plane = match banding.axis {
        StreakAxis::Columns => canon,
        StreakAxis::Rows => transpose(&canon)?,
    };
    banding.repaired = n > 0;
    Ok(n)
}

/// Median ln(transmittance) of each row over its pixels above `signal_floor` (NaN if none).
fn row_levels(p: &Plane, cfg: &BandingConfig) -> Vec<f32> {
    (0..p.height())
        .into_par_iter()
        .map(|y| {
            let mut v: Vec<f32> = p.row(y).iter().filter(|v| v.is_finite() && **v >= cfg.signal_floor).map(|v| v.ln()).collect();
            median_in_place(&mut v).unwrap_or(f32::NAN)
        })
        .collect()
}

/// Per-pixel log ratio to the along-scan median, row-major; NaN where unusable.
fn log_ratios(p: &Plane, cfg: &BandingConfig) -> Result<Plane> {
    let t = transpose(p)?; // one contiguous slice per column
    let (h, w) = (t.width(), t.height());
    let r = cfg.radius;
    let usable = |v: f32| v.is_finite() && v >= cfg.signal_floor && v < cfg.clip_level;
    let mut q = vec![f32::NAN; w * h];
    q.par_chunks_mut(h).enumerate().for_each(|(x, out)| {
        let col = t.row(x);
        let mut buf = Vec::with_capacity(2 * r + 1);
        for (y, o) in out.iter_mut().enumerate() {
            let v = col[y];
            if !usable(v) {
                continue;
            }
            buf.clear();
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(h);
            // The pixel itself is left out: otherwise it is often its own median and the ratios
            // pile up at exactly zero, snapping small row gains to nothing.
            buf.extend((lo..hi).filter(|&i| i != y && usable(col[i])).map(|i| col[i]));
            if let Some(m) = median_in_place(&mut buf)
                && usable(m)
            {
                *o = (v / m).ln();
            }
        }
    });
    transpose(&Plane::new(h, w, q)?)
}

/// One measured scan line: median log gain, its standard error, and per-segment medians.
struct RowStat {
    gain: f32,
    se: f32,
    segments: Vec<f32>,
}

/// `profile[y]` minus the median of its finite neighbours within `±r`.
fn high_pass(profile: &[f32], r: usize) -> Vec<f32> {
    let n = profile.len();
    let mut buf = Vec::with_capacity(2 * r + 1);
    (0..n)
        .map(|y| {
            buf.clear();
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(n);
            buf.extend(profile[lo..hi].iter().copied());
            match median_in_place(&mut buf) {
                Some(m) => profile[y] - m,
                None => f32::NAN,
            }
        })
        .collect()
}

fn detect_canonical(p: &Plane, cfg: &BandingConfig) -> Result<Banding> {
    let (w, h) = (p.width(), p.height());
    let mut out = Banding::empty(Channel::Red, StreakAxis::Columns);
    if w < 8 || h < 8 {
        return Ok(out);
    }
    let level = row_levels(p, cfg);
    let q = log_ratios(p, cfg)?;
    let min_valid = ((cfg.min_valid_fraction * w as f32) as usize).max(8);
    let segs = cfg.segments.min(w / 4).max(1);
    let seg_len = w.div_ceil(segs);
    let stats: Vec<Option<RowStat>> = (0..h)
        .into_par_iter()
        .map(|y| {
            let row = q.row(y);
            let valid = row.iter().filter(|v| v.is_finite()).count();
            if valid < min_valid {
                return None;
            }
            let (gain, _) = median_mad(row)?;
            // Standard error from the pixel noise alone: second differences of ln v along the
            // scan cancel smooth scene texture and any row-constant gain.
            let l2: Vec<f32> = (0..w)
                .filter(|_| y > 0 && y + 1 < h)
                .map(|x| {
                    let (a, b, c) = (p.get(x, y - 1), p.get(x, y), p.get(x, y + 1));
                    if [a, b, c].iter().all(|v| v.is_finite() && *v >= cfg.signal_floor && *v < cfg.clip_level) {
                        b.ln() - 0.5 * (a.ln() + c.ln())
                    } else {
                        f32::NAN
                    }
                })
                .collect();
            let mad = median_mad(&l2).map_or(f32::INFINITY, |(_, m)| m / 1.5f32.sqrt());
            let segments = row
                .chunks(seg_len)
                .map(|chunk| {
                    let mut vals: Vec<f32> = chunk.iter().copied().filter(|v| v.is_finite()).collect();
                    if vals.len() < (chunk.len() / 4).max(8) { f32::NAN } else { median_in_place(&mut vals).unwrap_or(f32::NAN) }
                })
                .collect();
            Some(RowStat { gain, se: 1.2533 * MAD_SIGMA * mad / (valid as f32).sqrt(), segments })
        })
        .collect();

    // Remove what varies smoothly from row to row (scene curvature biases the pixel baseline
    // coherently across a row); banding is what is left.
    let pr = cfg.profile_radius;
    let gain_hp = high_pass(&stats.iter().map(|s| s.as_ref().map_or(f32::NAN, |s| s.gain)).collect::<Vec<_>>(), pr);
    let seg_hp: Vec<Vec<f32>> = (0..segs)
        .map(|k| {
            let prof: Vec<f32> = stats.iter().map(|s| s.as_ref().and_then(|s| s.segments.get(k).copied()).unwrap_or(f32::NAN)).collect();
            high_pass(&prof, pr)
        })
        .collect();

    let lo_dev = cfg.min_gain_deviation.ln_1p();
    let hi_dev = cfg.max_gain_deviation.ln_1p();
    let gains: Vec<f32> = gain_hp.iter().copied().filter(|g| g.is_finite()).collect();
    out.lines_measured = gains.len();
    if let Some((_, mad)) = median_mad(&gains) {
        out.rms = MAD_SIGMA * mad;
    }
    // Pervasive banding (flicker on every line): every measured line is corrected by its estimate.
    // Otherwise only lines that are individually significant and agreed by every segment.
    let pervasive = out.rms >= cfg.pervasive_rms;
    let rows: Vec<Option<f32>> = (0..h)
        .map(|y| {
            let s = stats[y].as_ref()?;
            let g = gain_hp[y];
            if !g.is_finite() {
                return None;
            }
            if pervasive && g.abs() <= hi_dev {
                return (g.abs() >= 2.0 * s.se).then_some(g);
            }
            if g.abs() < lo_dev || g.abs() < cfg.z_threshold * s.se {
                return None;
            }
            let segs_y: Vec<f32> = seg_hp.iter().map(|sp| sp[y]).filter(|v| v.is_finite()).collect();
            // Lamp or stepper gain scales the whole row alike: every segment within 0.5–1.5× of it.
            let agree = segs_y.iter().all(|v| v.signum() == g.signum() && v.abs() >= 0.5 * g.abs() && v.abs() <= 1.5 * g.abs());
            (agree && segs_y.len() >= segs.min(2)).then_some(g)
        })
        .collect();
    out.max_deviation = gains.iter().map(|g| g.exp_m1().abs()).fold(0.0, f32::max);
    // A banded row departs, in its own direction and by at least half its gain, from the line
    // through the nearest rows above and below that are not candidates themselves (a band of
    // several rows is judged against what surrounds it), and those rows do not differ by more than
    // twice the gain. The shoulder of a horizontal edge (a strip end, a horizon) sits beside a
    // step and fails.
    let banded_shape = |y: usize, g: f32| {
        let reach = cfg.profile_radius.max(1);
        let above = (y.saturating_sub(reach)..y).rev().find(|&i| rows[i].is_none());
        let below = (y + 1..(y + reach + 1).min(h)).find(|&i| rows[i].is_none());
        match (above, below) {
            (Some(ya), Some(yb)) if level[ya].is_finite() && level[yb].is_finite() && level[y].is_finite() => {
                let t = (y - ya) as f32 / (yb - ya) as f32;
                let d = level[y] - (level[ya] * (1.0 - t) + level[yb] * t);
                let jump = (level[yb] - level[ya]).abs();
                d * g > 0.0 && d.abs() >= 0.5 * g.abs() && jump <= 2.0 * g.abs()
            }
            _ => false,
        }
    };
    let is_strong = |r: Option<f32>| r.is_some_and(|g| g.abs() > hi_dev);
    let mut y = 0;
    while y < h {
        match rows[y] {
            Some(g) if g.abs() <= hi_dev && (pervasive || banded_shape(y, g)) => {
                out.banded.push(LineGain { index: y, gain: g.exp() });
            }
            Some(_) => {
                // A run of strong rows: transients are isolated, content (a frame gap) is not; and
                // a transient sits between rows of one level, not on a step between two.
                let run_end = (y..h).find(|&i| !is_strong(rows[i])).unwrap_or(h);
                let step = match (y.checked_sub(1).map(|i| level[i]), level.get(run_end).copied()) {
                    (Some(a), Some(b)) if a.is_finite() && b.is_finite() => (b - a).abs(),
                    _ => 0.0,
                };
                let weakest = (y..run_end).filter_map(|i| rows[i]).map(f32::abs).fold(f32::INFINITY, f32::min);
                if run_end - y <= cfg.max_strong_run && step < 0.5 * weakest {
                    out.strong.extend((y..run_end).filter_map(|i| rows[i].map(|g| LineGain { index: i, gain: g.exp() })));
                }
                y = run_end;
                continue;
            }
            None => {}
        }
        y += 1;
    }
    Ok(out)
}
