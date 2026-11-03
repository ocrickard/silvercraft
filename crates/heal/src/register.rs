//! Sub-pixel IR → RGB registration by correlating defects.
//!
//! Dust and hairs sit on the film, so they are dark in the infrared pass AND in the visible pass
//! (on the negative, before inversion). A local "relative darkness" map of each channel
//! (`1 - v / local mean`) is mostly defects plus a little image edge structure; the Pearson
//! correlation of the two maps over integer offsets peaks at the misregistration, and a
//! three-point Gaussian (log-parabolic) fit around the peak gives the sub-pixel part. Large
//! scans are searched on a block-averaged level first and refined at full resolution, on a
//! central window of bounded size, so the cost does not grow with the frame.

use rayon::prelude::*;
use silvercraft_imgproc::{Mask, Plane, filter, morph, resample};

use crate::util;

/// An estimated IR offset: the IR feature of RGB pixel (x, y) sits at (x + dx, y + dy).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shift {
    pub dx: f32,
    pub dy: f32,
    /// 0 (no evidence) ..= 1 (sharp, unambiguous peak).
    pub confidence: f32,
}

/// Largest window (pixels) the correlation runs on.
const MAX_WINDOW_PIXELS: usize = 2_000_000;

/// Relative-darkness map: `clamp(1 - v / box_mean(v), 0, 0.5)`, zero on the opaque surround.
fn speck_map(v: &Plane, radius: usize) -> Plane {
    let finite: Vec<f32> = v.data().iter().copied().filter(|x| x.is_finite()).collect();
    let level = util::median(silvercraft_imgproc::stats::subsample(&finite, 200_000)).unwrap_or(0.0);
    let clean = v.map(|x| if x.is_finite() { x } else { level });
    let low = Mask::from_plane(&clean, |x| x.is_nan() || x <= 0.4 * level);
    let low = morph::dilate(&util::edge_connected(&low, 64), radius + 2);
    let mean = filter::box_mean(&clean, radius);
    let mut out = clean.zip_map(&mean, |x, m| if m > 1e-6 { (1.0 - x / m).clamp(0.0, 0.5) } else { 0.0 }).unwrap_or(clean);
    for (o, &l) in out.data_mut().iter_mut().zip(low.data()) {
        if l {
            *o = 0.0;
        }
    }
    out
}

/// Pearson correlation of `a(x)` with `b(x + d)` over the interior `margin` window, for every
/// integer d in `[cx-r, cx+r] × [cy-r, cy+r]`. Row-major `(2r+1)²` scores.
fn correlate(a: &Plane, b: &Plane, cx: isize, cy: isize, r: isize, margin: usize) -> Vec<f64> {
    let (w, h) = (a.width(), a.height());
    let offsets: Vec<(isize, isize)> = (-r..=r).flat_map(|dy| (-r..=r).map(move |dx| (cx + dx, cy + dy))).collect();
    if w <= 2 * margin || h <= 2 * margin {
        return vec![0.0; offsets.len()];
    }
    offsets
        .par_iter()
        .map(|&(dx, dy)| {
            let (mut sa, mut sb, mut sab, mut saa, mut sbb, mut n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for y in margin..h - margin {
                let yb = y as isize + dy;
                if yb < 0 || yb >= h as isize {
                    continue;
                }
                let ra = a.row(y);
                let rb = b.row(yb as usize);
                for x in margin..w - margin {
                    let xb = x as isize + dx;
                    if xb < 0 || xb >= w as isize {
                        continue;
                    }
                    let (va, vb) = (f64::from(ra[x]), f64::from(rb[xb as usize]));
                    sa += va;
                    sb += vb;
                    sab += va * vb;
                    saa += va * va;
                    sbb += vb * vb;
                    n += 1.0;
                }
            }
            if n < 16.0 {
                return 0.0;
            }
            let cov = sab - sa * sb / n;
            let va = saa - sa * sa / n;
            let vb = sbb - sb * sb / n;
            if va <= 1e-18 || vb <= 1e-18 { 0.0 } else { cov / (va * vb).sqrt() }
        })
        .collect()
}

/// Sub-pixel vertex of three samples around a peak (Gaussian fit when all positive).
fn vertex(m: f64, c: f64, p: f64) -> f64 {
    let d = if m > 0.0 && c > 0.0 && p > 0.0 {
        let (lm, lc, lp) = (m.ln(), c.ln(), p.ln());
        let den = lm - 2.0 * lc + lp;
        if den < -1e-12 { 0.5 * (lm - lp) / den } else { 0.0 }
    } else {
        let den = m - 2.0 * c + p;
        if den < -1e-12 { 0.5 * (m - p) / den } else { 0.0 }
    };
    if d.is_finite() { d.clamp(-0.5, 0.5) } else { 0.0 }
}

struct Peak {
    ix: isize,
    iy: isize,
    fx: f64,
    fy: f64,
    r: f64,
    confidence: f64,
}

fn peak(scores: &[f64], cx: isize, cy: isize, r: isize) -> Option<Peak> {
    let side = (2 * r + 1) as usize;
    let (best_i, &best) = scores.iter().enumerate().filter(|(_, v)| v.is_finite()).max_by(|a, b| a.1.total_cmp(b.1))?;
    let (bx, by) = ((best_i % side) as isize, (best_i / side) as isize);
    let at = |x: isize, y: isize| -> f64 {
        if x < 0 || y < 0 || x >= side as isize || y >= side as isize { f64::NAN } else { scores[y as usize * side + x as usize] }
    };
    let second = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| ((*i % side) as isize - bx).abs() > 1 || ((*i / side) as isize - by).abs() > 1)
        .map(|(_, v)| *v)
        .fold(0.0f64, f64::max);
    let on_edge = bx == 0 || by == 0 || bx == 2 * r || by == 2 * r;
    let fx = if bx > 0 && bx < 2 * r { vertex(at(bx - 1, by), best, at(bx + 1, by)) } else { 0.0 };
    let fy = if by > 0 && by < 2 * r { vertex(at(bx, by - 1), best, at(bx, by + 1)) } else { 0.0 };
    let distinct = if best > 0.0 { (2.0 * (best - second) / best).clamp(0.0, 1.0) } else { 0.0 };
    let confidence = if on_edge && r > 0 { 0.0 } else { (best.max(0.0) * distinct).clamp(0.0, 1.0) };
    Some(Peak { ix: cx + bx - r, iy: cy + by - r, fx, fy, r: best, confidence })
}

/// Estimate the IR offset of `ir` relative to `luma` (both linear transmittance, same size),
/// searching `|d| <= max_shift` pixels. `scale` is the pixel scale relative to 600 dpi.
pub fn estimate(luma: &Plane, ir: &Plane, max_shift: f32, scale: f32) -> Shift {
    let none = Shift { dx: 0.0, dy: 0.0, confidence: 0.0 };
    if luma.same_size(ir).is_err() || !(max_shift.is_finite() && max_shift >= 0.0) {
        return none;
    }
    let (w, h) = (luma.width(), luma.height());
    // central window of bounded area
    let side = (MAX_WINDOW_PIXELS as f64).sqrt() as usize;
    let (cw, ch) = if w * h <= MAX_WINDOW_PIXELS {
        (w, h)
    } else if w < side {
        (w, (MAX_WINDOW_PIXELS / w).min(h))
    } else if h < side {
        ((MAX_WINDOW_PIXELS / h).min(w), h)
    } else {
        (side, side)
    };
    let (x0, y0) = ((w - cw) / 2, (h - ch) / 2);
    let (Ok(l), Ok(i)) = (luma.crop(x0, y0, cw, ch), ir.crop(x0, y0, cw, ch)) else {
        return none;
    };
    let radius = ((2.0 * scale).round() as usize).max(2);
    let max_r = max_shift.ceil() as isize + 1;
    let level = (scale.round() as usize).max(1);

    let (cx, cy) = if level > 1 {
        let (Ok(lc), Ok(ic)) = (resample::downsample(&l, level), resample::downsample(&i, level)) else {
            return none;
        };
        let (a, b) = (speck_map(&lc, 2), speck_map(&ic, 2));
        let rc = (max_shift / level as f32).ceil() as isize;
        let s = correlate(&a, &b, 0, 0, rc, rc as usize + 3);
        match peak(&s, 0, 0, rc) {
            Some(p) => (p.ix * level as isize, p.iy * level as isize),
            None => return none,
        }
    } else {
        (0, 0)
    };
    let a = speck_map(&l, radius);
    let b = speck_map(&i, radius);
    let r = if level > 1 { (level as isize / 2 + 1).min(max_r.max(1)) } else { max_r };
    let margin = (cx.unsigned_abs().max(cy.unsigned_abs()) + r as usize + radius + 2).min(cw.min(ch) / 2);
    let scores = correlate(&a, &b, cx, cy, r, margin);
    let Some(p) = peak(&scores, cx, cy, r) else {
        return none;
    };
    let (dx, dy) = (p.ix as f64 + p.fx, p.iy as f64 + p.fy);
    if dx.abs() > f64::from(max_shift) + 0.5 || dy.abs() > f64::from(max_shift) + 0.5 || !p.r.is_finite() {
        return none;
    }
    Shift { dx: dx as f32, dy: dy as f32, confidence: p.confidence as f32 }
}

/// Resample `ir` onto the RGB grid for an offset (bilinear, edge-clamped).
pub fn apply(ir: &Plane, dx: f32, dy: f32) -> Plane {
    if dx == 0.0 && dy == 0.0 {
        return ir.clone();
    }
    resample::shift(ir, -dx, -dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anti-aliased dark discs on a smooth field; `off` moves every disc.
    fn specks(w: usize, h: usize, off: (f32, f32), depth: f32) -> Plane {
        let centres: Vec<(f32, f32, f32)> = (0..90u64)
            .map(|k| {
                let a = util::splitmix(k);
                let b = util::splitmix(a);
                let c = util::splitmix(b);
                (
                    8.0 + (a % 1000) as f32 / 1000.0 * (w as f32 - 16.0),
                    8.0 + (b % 1000) as f32 / 1000.0 * (h as f32 - 16.0),
                    0.8 + (c % 100) as f32 / 50.0,
                )
            })
            .collect();
        Plane::from_fn(w, h, |x, y| {
            let mut t = 0.7 + 0.1 * (x as f32 / w as f32);
            for &(cx, cy, r) in &centres {
                let mut cover = 0.0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f32 + (sx as f32 + 0.5) / 4.0 - 0.5 - (cx + off.0);
                        let py = y as f32 + (sy as f32 + 0.5) / 4.0 - 0.5 - (cy + off.1);
                        if px * px + py * py <= r * r {
                            cover += 1.0 / 16.0;
                        }
                    }
                }
                t *= 1.0 - depth * cover;
            }
            t
        })
        .unwrap()
    }

    #[test]
    fn recovers_known_subpixel_offset() {
        for &(dx, dy) in &[(1.0f32, 0.0f32), (1.3, -0.4), (-2.6, 0.7), (0.0, 0.0)] {
            let rgb = specks(240, 200, (0.0, 0.0), 0.8);
            let ir = specks(240, 200, (dx, dy), 0.9);
            let s = estimate(&rgb, &ir, 3.0, 1.0);
            assert!((s.dx - dx).abs() < 0.15 && (s.dy - dy).abs() < 0.15, "want ({dx},{dy}) got {s:?}");
            assert!(s.confidence > 0.3, "{s:?}");
            // applying the shift registers the planes
            let reg = apply(&ir, s.dx, s.dy);
            let s2 = estimate(&rgb, &reg, 3.0, 1.0);
            assert!(s2.dx.abs() < 0.2 && s2.dy.abs() < 0.2, "{s2:?}");
        }
    }

    #[test]
    fn coarse_to_fine_at_high_resolution() {
        let rgb = specks(400, 300, (0.0, 0.0), 0.8);
        let ir = specks(400, 300, (5.4, -3.2), 0.9);
        let s = estimate(&rgb, &ir, 8.0, 3.0);
        assert!((s.dx - 5.4).abs() < 0.25 && (s.dy + 3.2).abs() < 0.25, "{s:?}");
    }

    #[test]
    fn featureless_planes_have_no_confidence() {
        let flat = Plane::filled(64, 64, 0.5).unwrap();
        assert_eq!(estimate(&flat, &flat, 3.0, 1.0).confidence, 0.0);
        let tiny = Plane::filled(1, 1, 0.5).unwrap();
        assert_eq!(estimate(&tiny, &tiny, 3.0, 1.0).confidence, 0.0);
        let nan = Plane::filled(32, 32, f32::NAN).unwrap();
        assert_eq!(estimate(&nan, &nan, 3.0, 1.0).confidence, 0.0);
    }
}
