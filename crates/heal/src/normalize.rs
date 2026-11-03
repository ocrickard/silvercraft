//! IR normalisation: from the raw infrared transmittance to a defect attenuation map.
//!
//! The raw IR plane is `lamp(x, y) · T_ir(film) · occlusion`. Dust and hairs are the occlusion
//! term. The lamp and vignetting falloff is large-scale; it is removed by a masked Gaussian
//! background that excludes defect candidates. Chromogenic dyes (C-41, Ilford XP2) pass most IR
//! but not all, so a faint copy of the image leaks into it: in log space
//! `ln IR ≈ background + Σ k_c · ln C_c`, and the slopes `k_c` are fitted by trimmed least
//! squares on the high-passed log planes of clean pixels. The prediction is removed, and the
//! visible channels are first filled across defect candidates so that the dust's own shadow in
//! the visible pass is not "explained" away. The result is `s = expected − ln IR` (0 on clean
//! film, > 0 where something blocks IR); attenuation is `exp(−s)`.
//!
//! The fitted slopes double as the silver detector: silver-image B&W film (HP5, Tri-X) is as
//! opaque in IR as in the visible, so its slopes are near 1, against ~0.2 for XP2 and less for
//! colour negative.

use rayon::prelude::*;
use silvercraft_imgproc::{Mask, Plane, filter, morph, resample};

use crate::config::HealConfig;
use crate::util;

/// The normalised IR.
pub struct Normalized {
    /// Log attenuation `s = expected − ln IR` (> 0 where IR is blocked).
    pub s: Plane,
    /// Pixels where detection is meaningful (film, finite, away from the holder).
    pub valid: Mask,
    /// Everything around the film: the opaque holder (edge-connected low IR, with its lip) and
    /// unobstructed lamp beyond the strip (see [`surround`]).
    pub holder: Mask,
    /// Fitted log slopes of IR on the visible R, G, B (image leakage).
    pub leakage: [f32; 3],
    /// Correlation of the high-passed IR with its leakage prediction.
    pub leakage_correlation: f32,
    /// Robust (MAD) noise sigma of `s` on clean film.
    pub noise: f32,
    /// `s` minus its neighbourhood median (`local_radius`): sharp defects only.
    pub detail: Plane,
    /// Local RMS of `detail` on clean film (≥ `noise`): grain plus residual low-contrast structure
    /// such as Newton rings, which raise the detection threshold where they are.
    pub local_noise: Plane,
}

/// Why the IR plane cannot be used.
#[derive(Debug, Clone, PartialEq)]
pub enum Unusable {
    /// No finite positive IR (all zero, all NaN).
    Empty,
    /// Not enough film area to model.
    NoFilm,
}

fn ln_floor(v: f32, floor: f32) -> f32 {
    if v.is_finite() { v.max(floor).ln() } else { f32::NAN }
}

/// Trimmed least squares of `y` on three regressors; returns slopes and the correlation of `y`
/// with the fitted prediction.
fn fit_leakage(y: &[f32], x: &[[f32; 3]]) -> ([f64; 3], f64) {
    let mut keep: Vec<bool> = vec![true; y.len()];
    let mut k = [0.0f64; 3];
    for _pass in 0..3 {
        let mut a = [[0.0f64; 3]; 3];
        let mut b = [0.0f64; 3];
        let mut n = 0usize;
        for ((yi, xi), &kp) in y.iter().zip(x).zip(&keep) {
            if !kp {
                continue;
            }
            for r in 0..3 {
                b[r] += f64::from(xi[r]) * f64::from(*yi);
                for c in 0..3 {
                    a[r][c] += f64::from(xi[r]) * f64::from(xi[c]);
                }
            }
            n += 1;
        }
        if n < 64 {
            return ([0.0; 3], 0.0);
        }
        let trace = (a[0][0] + a[1][1] + a[2][2]) / 3.0;
        for (r, row) in a.iter_mut().enumerate() {
            row[r] += 1e-3 * trace + 1e-12;
        }
        let Some(sol) = util::solve3(a, b) else {
            return ([0.0; 3], 0.0);
        };
        k = sol;
        let resid: Vec<f32> = y.iter().zip(x).map(|(yi, xi)| yi - (k[0] as f32 * xi[0] + k[1] as f32 * xi[1] + k[2] as f32 * xi[2])).collect();
        let Some((_, sigma)) = util::median_sigma(resid.clone()) else {
            break;
        };
        let lim = 3.5 * sigma.max(1e-6);
        for (kp, r) in keep.iter_mut().zip(&resid) {
            *kp = r.abs() <= lim;
        }
    }
    // correlation of y with the prediction over the kept samples
    let (mut sp, mut sy, mut spp, mut syy, mut spy, mut n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for ((yi, xi), &kp) in y.iter().zip(x).zip(&keep) {
        if !kp {
            continue;
        }
        let p = k[0] * f64::from(xi[0]) + k[1] * f64::from(xi[1]) + k[2] * f64::from(xi[2]);
        let yv = f64::from(*yi);
        sp += p;
        sy += yv;
        spp += p * p;
        syy += yv * yv;
        spy += p * yv;
        n += 1.0;
    }
    let corr = if n > 2.0 {
        let cov = spy - sp * sy / n;
        let vp = spp - sp * sp / n;
        let vy = syy - sy * sy / n;
        if vp > 1e-18 && vy > 1e-18 { cov / (vp * vy).sqrt() } else { 0.0 }
    } else {
        0.0
    };
    (k, corr)
}

/// Defect candidates of a provisional `s`: above `max(3σ, floor)`, dilated by `grow`.
fn candidates(s: &Plane, valid: &Mask, floor: f32, grow: usize) -> (Mask, f32) {
    let sigma = util::robust_stats(s.data(), |i| valid.data()[i], 150_000).map_or(0.0, |(_, sg)| sg);
    let t = (3.0 * sigma).max(floor);
    let mut c = Mask::from_plane(s, |v| v > t);
    for (o, &v) in c.data_mut().iter_mut().zip(valid.data()) {
        *o &= v;
    }
    (morph::dilate(&c, grow), sigma)
}

fn weight_of(valid: &Mask, exclude: &Mask) -> Plane {
    let data = valid.data().par_iter().zip(exclude.data()).map(|(&v, &e)| if v && !e { 1.0 } else { 0.0 }).collect();
    Plane::new(valid.width(), valid.height(), data).unwrap_or_else(|_| valid.to_plane())
}

/// Replace the `cand` pixels of `raw` by the median of nearby non-candidate pixels (the window
/// grows until it has enough). It keeps image edges, where a blur would smear them.
fn fill_from_surroundings(raw: &Plane, cand: &Mask, scale: f32) -> Plane {
    let (w, h) = (raw.width(), raw.height());
    let r0 = ((3.0 * scale).round() as isize).max(2);
    let mut out = raw.clone();
    out.data_mut().par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let mut buf = Vec::new();
        for (x, o) in row.iter_mut().enumerate() {
            if !cand.data()[y * w + x] {
                continue;
            }
            for r in [r0, 2 * r0, 4 * r0, 8 * r0] {
                buf.clear();
                let step = (r / 6).max(1) as usize;
                for yy in ((y as isize - r).max(0)..=(y as isize + r).min(h as isize - 1)).step_by(step) {
                    for xx in ((x as isize - r).max(0)..=(x as isize + r).min(w as isize - 1)).step_by(step) {
                        let j = yy as usize * w + xx as usize;
                        if !cand.data()[j] {
                            buf.push(raw.data()[j]);
                        }
                    }
                }
                if buf.len() >= 12 {
                    break;
                }
            }
            if !buf.is_empty() {
                let m = buf.len() / 2;
                buf.select_nth_unstable_by(m, f32::total_cmp);
                *o = buf[m];
            }
        }
    });
    out
}

/// Breadth-first growth of `seed` through pixels where `pass` holds, at most `steps` pixel steps
/// (8-connected).
fn grow_bounded(seed: &Mask, pass: impl Fn(usize) -> bool, steps: usize) -> Mask {
    let (w, h) = (seed.width(), seed.height());
    let mut out = seed.clone();
    let mut front: Vec<usize> = (0..w * h).filter(|&i| seed.data()[i]).collect();
    for _ in 0..steps {
        let mut next = Vec::new();
        for &i in &front {
            let (x, y) = (i % w, i / w);
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    let j = ny * w + nx;
                    if !out.data()[j] && pass(j) {
                        out.data_mut()[j] = true;
                        next.push(j);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        front = next;
    }
    out
}

/// Everything around the film that is not film, connected to the image edge:
///
/// - the opaque holder (IR at most 35 % of the film level), grown a few pixels through its
///   semi-opaque lip and the blurred film edge (IR below 75 % of the film level). Without the
///   growth that shelf reads as a long, deep "scratch" along the frame edge;
/// - unobstructed lamp beyond the end of the strip or a holder slot: brighter in IR than the film
///   and far brighter in the visible than the clearest film (the base). Counted as film, it pulls
///   the IR background up and makes the clear rebate next to it look attenuated.
fn surround(rgb: &[Plane; 3], ir: &Plane, level: f32, scale: f32) -> Mask {
    let (w, h) = (ir.width(), ir.height());
    let min_area = ((64.0 * scale * scale) as usize).max(16);
    let low = Mask::from_plane(ir, |v| v.is_nan() || v <= 0.35 * level);
    let seed = util::edge_connected(&low, min_area);
    let steps = ((4.0 * scale).round() as usize).max(2);
    let holder = grow_bounded(&seed, |i| ir.data()[i].is_nan() || ir.data()[i] <= 0.75 * level, steps);
    let luma = |i: usize| (rgb[0].data()[i] + rgb[1].data()[i] + rgb[2].data()[i]) / 3.0;
    // the clearest film: a high percentile of the visible over pixels with film-like IR
    let step = (w * h / 200_000).max(1);
    let mut film: Vec<f32> = (0..w * h)
        .step_by(step)
        .filter(|&i| !holder.data()[i] && ir.data()[i].is_finite() && ir.data()[i] < 1.1 * level)
        .map(luma)
        .filter(|v| v.is_finite())
        .collect();
    if film.len() < 64 {
        return holder;
    }
    let k = (film.len() - 1) * 99 / 100;
    let (_, &mut base, _) = film.select_nth_unstable_by(k, f32::total_cmp);
    if base.is_nan() || base <= 0.0 {
        return holder;
    }
    let bright = Mask::new(w, h, (0..w * h).map(|i| ir.data()[i] >= 1.1 * level && luma(i) >= 1.6 * base).collect());
    let Ok(bright) = bright else {
        return holder;
    };
    let open = util::edge_connected(&bright, min_area);
    holder.or(&open).unwrap_or(holder)
}

/// Normalise a registered IR plane against the visible channels (all linear transmittance, same
/// size).
pub fn normalize(rgb: &[Plane; 3], ir: &Plane, cfg: &HealConfig, scale: f32) -> Result<Normalized, Unusable> {
    let (w, h) = (ir.width(), ir.height());
    let positive: Vec<f32> = ir.data().iter().copied().filter(|v| v.is_finite() && *v > 0.0).collect();
    let level = util::median(silvercraft_imgproc::stats::subsample(&positive, 200_000)).unwrap_or(0.0);
    if level.is_nan() || level <= 1e-6 {
        return Err(Unusable::Empty);
    }
    let holder = surround(rgb, ir, level, scale);
    let margin = (cfg.border_margin as f32 * scale).round() as usize;
    let near_holder = morph::dilate(&holder, margin);
    let mut valid = near_holder.not();
    valid.data_mut().par_iter_mut().enumerate().for_each(|(i, v)| {
        let finite = ir.data()[i].is_finite() && ir.data()[i] > 0.0 && rgb.iter().all(|p| p.data()[i].is_finite());
        let (x, y) = (i % w, i / w);
        let inside = x >= margin && y >= margin && x + margin < w && y + margin < h;
        *v &= finite && inside;
    });
    if valid.count() < 64 {
        return Err(Unusable::NoFilm);
    }

    let floor = -(1.0 - cfg.grow_depth as f32).max(1e-3).ln();
    let sigma_bg = (cfg.background_sigma as f32 * scale).max(2.0);
    let halo = ((cfg.halo as f32 + 1.0) * scale).round().max(1.0) as usize;
    let lir = ir.map(|v| ln_floor(v, 1e-4 * level));
    let lc: Vec<Plane> = rgb.iter().map(|p| p.map(|v| ln_floor(v, 1e-5))).collect();
    let none = Mask::empty(w, h).map_err(|_| Unusable::NoFilm)?;

    // pass 1: background only, to find candidates to exclude
    let b1 = util::masked_blur(&lir, &weight_of(&valid, &none), sigma_bg);
    let s1 = b1.zip_map(&lir, |b, l| b - l).map_err(|_| Unusable::NoFilm)?;
    let (cand, _) = candidates(&s1, &valid, floor, halo);
    let w2 = weight_of(&valid, &cand);

    // leakage slopes on high-passed logs of clean pixels
    let bir = util::masked_blur(&lir, &w2, sigma_bg);
    let bc: Vec<Plane> = lc.iter().map(|p| util::masked_blur(p, &w2, sigma_bg)).collect();
    let n = w * h;
    let step = (n / 150_000).max(1);
    let mut ys = Vec::new();
    let mut xs = Vec::new();
    for i in (0..n).step_by(step) {
        if w2.data()[i] <= 0.0 {
            continue;
        }
        let y = lir.data()[i] - bir.data()[i];
        let x = [lc[0].data()[i] - bc[0].data()[i], lc[1].data()[i] - bc[1].data()[i], lc[2].data()[i] - bc[2].data()[i]];
        if y.is_finite() && x.iter().all(|v| v.is_finite()) {
            ys.push(y);
            xs.push(x);
        }
    }
    let (k, corr) = fit_leakage(&ys, &xs);
    let leakage = [k[0] as f32, k[1] as f32, k[2] as f32];

    // leakage prediction; across defect candidates (found on the leakage-free IR) it is replaced
    // from the surroundings, so the dust's own visible shadow is not "explained" away
    let pred = if cfg.leakage && leakage.iter().any(|v| *v != 0.0) {
        let raw = Plane::from_fn(w, h, |x, y| {
            let i = y * w + x;
            let v = leakage[0] * lc[0].data()[i] + leakage[1] * lc[1].data()[i] + leakage[2] * lc[2].data()[i];
            if v.is_finite() { v } else { 0.0 }
        })
        .map_err(|_| Unusable::NoFilm)?;
        let resid_raw = lir.zip_map(&raw, |l, p| l - p).map_err(|_| Unusable::NoFilm)?;
        let s_raw = util::masked_blur(&resid_raw, &w2, sigma_bg).zip_map(&resid_raw, |b, r| b - r).map_err(|_| Unusable::NoFilm)?;
        let (cand_leak, _) = candidates(&s_raw, &valid, floor, 1);
        fill_from_surroundings(&raw, &cand_leak, scale)
    } else {
        Plane::filled(w, h, 0.0).map_err(|_| Unusable::NoFilm)?
    };
    let resid = lir.zip_map(&pred, |l, p| l - p).map_err(|_| Unusable::NoFilm)?;

    // background of the leakage-free IR, twice (the second pass excludes the refined candidates)
    let mut s = {
        let br = util::masked_blur(&resid, &w2, sigma_bg);
        br.zip_map(&resid, |b, r| b - r).map_err(|_| Unusable::NoFilm)?
    };
    let (cand2, _) = candidates(&s, &valid, floor, halo);
    let w3 = weight_of(&valid, &cand2);
    let br = util::masked_blur(&resid, &w3, sigma_bg);
    s = br.zip_map(&resid, |b, r| if b.is_finite() && r.is_finite() { (b - r).clamp(-2.0, 12.0) } else { 0.0 }).map_err(|_| Unusable::NoFilm)?;
    let noise = util::robust_stats(s.data(), |i| w3.data()[i] > 0.0, 150_000).map_or(0.0, |(_, sg)| sg);

    // detail: s above its neighbourhood median. Specks and hairs keep their full contrast; broad
    // low-contrast structure (Newton ring fringes, leakage residue at image edges) does not.
    let level = (scale.round() as usize).max(1);
    let radius = ((cfg.local_radius as f32 * scale / level as f32).round() as usize).clamp(1, 7);
    let coarse = if level > 1 { resample::downsample(&s, level).unwrap_or_else(|_| s.clone()) } else { s.clone() };
    let med = filter::median(&coarse, radius).unwrap_or(coarse);
    let med = if level > 1 { util::upsample(&med, level, w, h) } else { med };
    let detail = s.zip_map(&med, |v, m| v - m).map_err(|_| Unusable::NoFilm)?;
    let noise_detail = util::robust_stats(detail.data(), |i| w3.data()[i] > 0.0, 150_000).map_or(0.0, |(_, sg)| sg);
    let sq = detail.map(|v| v * v);
    let local_noise = util::masked_mean(&sq, &w3, (8.0 * scale).max(3.0)).map(|v| v.max(0.0).sqrt().max(noise_detail));
    Ok(Normalized { s, detail, valid, holder, leakage, leakage_correlation: corr as f32, noise, local_noise })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planes(w: usize, h: usize, f: impl Fn(usize, usize) -> f32 + Sync) -> [Plane; 3] {
        let p = Plane::from_fn(w, h, f).unwrap();
        [p.clone(), p.map(|v| v * 0.9), p.map(|v| v * 1.1)]
    }

    #[test]
    fn removes_lamp_falloff_and_leakage() {
        let (w, h) = (160, 120);
        let img = |x: usize, y: usize| 0.2 + 0.3 * (((x / 20) + (y / 20)) % 2) as f32 + 0.1 * (x as f32 / 9.0).sin();
        let rgb = planes(w, h, img);
        let lamp = |x: usize, y: usize| 0.8 - 0.1 * ((x as f32 - 80.0).powi(2) + (y as f32 - 60.0).powi(2)) / 10_000.0;
        let ir = Plane::from_fn(w, h, |x, y| {
            let dust = if (x as i32 - 50).pow(2) + (y as i32 - 50).pow(2) <= 9 { 0.2 } else { 1.0 };
            lamp(x, y) * img(x, y).powf(0.25) * dust
        })
        .unwrap();
        let n = normalize(&rgb, &ir, &HealConfig::default(), 1.0).unwrap();
        let ksum: f32 = n.leakage.iter().sum();
        assert!((ksum - 0.25).abs() < 0.05, "{:?}", n.leakage);
        // clean film is ~0 everywhere (the checkerboard leakage is gone), the dust is ~ln(5)
        let mut worst = 0.0f32;
        for y in 10..h - 10 {
            for x in 10..w - 10 {
                if (x as i32 - 50).pow(2) + (y as i32 - 50).pow(2) > 25 {
                    worst = worst.max(n.s.get(x, y).abs());
                }
            }
        }
        let arg = (10..h - 10)
            .flat_map(|y| (10..w - 10).map(move |x| (x, y)))
            .filter(|&(x, y)| (x as i32 - 50).pow(2) + (y as i32 - 50).pow(2) > 25)
            .max_by(|a, b| n.s.get(a.0, a.1).abs().total_cmp(&n.s.get(b.0, b.1).abs()))
            .unwrap();
        assert!(worst < 0.03, "residual {worst} at {arg:?} k {:?}", n.leakage);
        assert!((n.s.get(50, 50) - 5f32.ln()).abs() < 0.1, "{}", n.s.get(50, 50));
        // without the leakage model the checkerboard edges stay in s
        let off = HealConfig { leakage: false, ..HealConfig::default() };
        let n2 = normalize(&rgb, &ir, &off, 1.0).unwrap();
        let worst2 = (10..h - 10).flat_map(|y| (10..w - 10).map(move |x| (x, y))).map(|(x, y)| n2.s.get(x, y).abs()).fold(0.0f32, f32::max);
        assert!(worst2 > 0.1, "{worst2}");
    }

    #[test]
    fn holder_is_excluded() {
        let rgb = planes(80, 60, |_, _| 0.4);
        let ir = Plane::from_fn(80, 60, |x, _| if x < 6 { 0.01 } else { 0.7 }).unwrap();
        let n = normalize(&rgb, &ir, &HealConfig::default(), 1.0).unwrap();
        assert!(n.holder.get(2, 30) && !n.valid.get(7, 30) && n.valid.get(20, 30));
    }

    #[test]
    fn empty_ir_is_unusable() {
        let rgb = planes(32, 32, |_, _| 0.4);
        assert_eq!(normalize(&rgb, &Plane::filled(32, 32, 0.0).unwrap(), &HealConfig::default(), 1.0).err(), Some(Unusable::Empty));
        assert_eq!(normalize(&rgb, &Plane::filled(32, 32, f32::NAN).unwrap(), &HealConfig::default(), 1.0).err(), Some(Unusable::Empty));
    }
}
