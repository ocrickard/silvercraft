//! Defect masks: IR hysteresis thresholding, component statistics, and the conservative no-IR
//! speck detector.

use std::collections::VecDeque;

use rayon::prelude::*;
use silvercraft_imgproc::{Mask, Plane, filter, morph, resample};

use crate::config::HealConfig;
use crate::util;

/// Which fill a defect gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Small speck: weighted average of the surrounding ring.
    Small,
    /// Medium: multi-scale membrane (harmonic) fill.
    Medium,
    /// Large blob or long scratch/hair: membrane tone plus best-match patch texture.
    Large,
    /// Partial occluder corrected by dividing out its IR attenuation (Digital-ICE style), or a
    /// faint long scratch whose smoothed attenuation was divided out.
    Corrected,
    /// Long scratch interpolated across its width from both sides, with matched grain.
    Interpolated,
}

/// Shape class of a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Compact dust.
    Speck,
    /// Long and thin: hair, fibre, scratch.
    Scratch,
}

/// One connected defect (after the halo dilation).
#[derive(Debug, Clone, PartialEq)]
pub struct Defect {
    /// Bounding box `[x0, y0, x1, y1)`.
    pub bbox: [usize; 4],
    /// Pixels filled (with halo).
    pub area: usize,
    /// Pixels actually detected (before the halo).
    pub core_area: usize,
    /// Principal-axis length, pixels (orientation independent).
    pub length: f32,
    /// Length over principal-axis width (≥ 1).
    pub elongation: f32,
    pub kind: Kind,
    /// Deepest IR attenuation in the defect (1 = clear, 0 = opaque); 0 when unknown (no IR).
    pub min_attenuation: f32,
    /// Fully opaque somewhere (`min_attenuation < deep_attenuation`).
    pub deep: bool,
    pub tier: Tier,
}

/// Seeds above `t_seed`, grown through 8-connected pixels above `t_grow` at most `steps` pixel
/// steps from a seed (bounded hysteresis: growth cannot percolate through grain).
pub fn hysteresis(s: &Plane, valid: &Mask, t_seed: f32, t_grow: f32, steps: usize) -> Mask {
    hysteresis_by(s, valid, |_, v| v > t_seed, |_, v| v > t_grow, steps)
}

/// [`hysteresis`] with per-pixel seed and grow predicates `(index, value) -> bool`.
pub fn hysteresis_by(s: &Plane, valid: &Mask, seed: impl Fn(usize, f32) -> bool, grow: impl Fn(usize, f32) -> bool, steps: usize) -> Mask {
    let (w, h) = (s.width(), s.height());
    let mut dist = vec![u16::MAX; w * h];
    let mut queue = VecDeque::new();
    for (i, (&v, &ok)) in s.data().iter().zip(valid.data()).enumerate() {
        if ok && seed(i, v) {
            dist[i] = 0;
            queue.push_back(i);
        }
    }
    let steps = steps.min(u16::MAX as usize - 1) as u16;
    while let Some(i) = queue.pop_front() {
        let d = dist[i];
        if d >= steps {
            continue;
        }
        let (x, y) = (i % w, i / w);
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                let j = ny * w + nx;
                if dist[j] == u16::MAX && valid.data()[j] && grow(j, s.data()[j]) {
                    dist[j] = d + 1;
                    queue.push_back(j);
                }
            }
        }
    }
    let mut out = valid.clone();
    for (o, &d) in out.data_mut().iter_mut().zip(&dist) {
        *o = d != u16::MAX;
    }
    out
}

/// Drop components smaller than `min_area` pixels.
pub fn remove_small(m: &Mask, min_area: usize) -> Mask {
    if min_area <= 1 {
        return m.clone();
    }
    let (labels, comps) = silvercraft_imgproc::label(m);
    let keep: Vec<bool> = std::iter::once(false).chain(comps.iter().map(|c| c.area >= min_area)).collect();
    let mut out = m.clone();
    for (o, &l) in out.data_mut().iter_mut().zip(&labels) {
        *o = keep.get(l as usize).copied().unwrap_or(false);
    }
    out
}

/// IR defect core: bounded hysteresis. A pixel seeds when its `detail` (contrast over the
/// neighbourhood median) clears both the local-noise and the absolute floor, or when `s` itself is
/// deeper than `broad_seed_depth` (large or wide defects, whose neighbourhood median is the defect
/// itself); it grows through pixels whose detail clears the grow thresholds.
pub fn ir_core(s: &Plane, detail: &Plane, valid: &Mask, local_noise: &Plane, cfg: &HealConfig, scale: f32) -> Mask {
    let depth = |d: f64| -((1.0 - d).max(1e-3) as f32).ln();
    let (seed_floor, grow_floor, broad) = (depth(cfg.seed_depth), depth(cfg.grow_depth), depth(cfg.broad_seed_depth));
    let (ks, kg) = (cfg.seed_sigma as f32, cfg.grow_sigma as f32);
    let noise = local_noise.data();
    let det = detail.data();
    let steps = (cfg.grow_steps as f32 * scale).round().max(0.0) as usize;
    let core = hysteresis_by(
        s,
        valid,
        |i, v| v > broad || det[i] > (ks * noise[i]).max(seed_floor),
        |i, v| v > broad || det[i] > (kg * noise[i]).max(grow_floor).min((ks * noise[i]).max(seed_floor)),
        steps,
    );
    remove_small(&core, (cfg.min_area as f32 * scale * scale).round() as usize)
}

/// Relative darkness of the visible luma against its neighbourhood median, `1 - L / median(L)`
/// over a `(2r+1)²` window (sparsely sampled), evaluated only inside `region` (0 elsewhere).
pub fn visible_darkness(luma: &Plane, region: &Mask, radius: usize) -> Plane {
    let (w, h) = (luma.width(), luma.height());
    let r = radius.max(1) as isize;
    let step = (radius / 4).max(1);
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let mut buf = Vec::new();
        for (x, o) in row.iter_mut().enumerate() {
            if !region.data()[y * w + x] {
                continue;
            }
            buf.clear();
            for yy in ((y as isize - r).max(0)..=(y as isize + r).min(h as isize - 1)).step_by(step) {
                for xx in ((x as isize - r).max(0)..=(x as isize + r).min(w as isize - 1)).step_by(step) {
                    let v = luma.get(xx as usize, yy as usize);
                    if v.is_finite() {
                        buf.push(v);
                    }
                }
            }
            let v = luma.get(x, y);
            if buf.is_empty() || !v.is_finite() {
                continue;
            }
            let m = buf.len() / 2;
            buf.select_nth_unstable_by(m, f32::total_cmp);
            if buf[m] > 1e-6 {
                *o = 1.0 - v / buf[m];
            }
        }
    });
    Plane::new(w, h, out).unwrap_or_else(|_| luma.map(|_| 0.0))
}

/// Robust relative pixel-to-pixel noise of the luma (grain), from horizontal neighbours.
pub fn relative_grain(luma: &Plane) -> f32 {
    let w = luma.width();
    let d: Vec<f32> = luma
        .data()
        .windows(2)
        .enumerate()
        .filter(|(i, _)| (i + 1) % w != 0)
        .step_by(7)
        .map(|(_, p)| if p[0] > 1e-6 && p[1].is_finite() { (p[1] - p[0]) / p[0] } else { f32::NAN })
        .collect();
    util::median_sigma(d).map_or(0.0, |(_, s)| s / std::f32::consts::SQRT_2)
}

/// Grow an IR core at most `steps` pixels through pixels that are visibly dark (`darkness >
/// threshold`): occluders that block visible light more than IR (tinted flakes, fibres) are
/// covered to their visible extent instead of leaving a ghost around the fill.
pub fn grow_visible(core: &Mask, darkness: &Plane, valid: &Mask, threshold: f32, steps: usize) -> Mask {
    if steps == 0 {
        return core.clone();
    }
    let seed = core.to_plane();
    let d = darkness.data();
    hysteresis_by(&seed, valid, |_, v| v > 0.5, |i, _| d[i] > threshold, steps).or(core).unwrap_or_else(|_| core.clone())
}

/// [`grow_visible`], keeping a component's growth only when it looks like an occluder around its
/// IR core: a tinted flake around an opaque grain, or a fibre along a hair, ends within the growth
/// range and surrounds its core. Dense image content next to or around a speck is visibly dark on
/// the negative too (and nearly as clear in IR as the film once the leakage is removed), and
/// growing into it would erase part of the picture. It shows either as a dark region that would
/// keep spreading with twice the range, or as growth lopsided to one side of the core (their
/// centroids further apart than half the growth's spread). Such a component keeps only its IR
/// core. `darkness` must cover twice the growth range around the core.
pub fn grow_visible_bounded(core: &Mask, darkness: &Plane, valid: &Mask, threshold: f32, steps: usize) -> Mask {
    const LOPSIDED: f64 = 0.5;
    if steps == 0 {
        return core.clone();
    }
    let grown = grow_visible(core, darkness, valid, threshold, steps);
    let further = grow_visible(core, darkness, valid, threshold, 2 * steps);
    let (w, h) = (core.width(), core.height());
    let (labels, comps) = silvercraft_imgproc::label(&grown);
    // per component: pixels the growth added, and pixels it would still add (credited to the
    // component of the nearest grown pixel within `steps`)
    let mut added = vec![0usize; comps.len() + 1];
    let mut beyond = vec![0usize; comps.len() + 1];
    // centroid sums (n, x, y) of the core and of the added pixels, and the added pixels' second
    // moments about the origin
    let mut cores = vec![[0.0f64; 3]; comps.len() + 1];
    let mut adds = vec![[0.0f64; 4]; comps.len() + 1];
    for (i, &l) in labels.iter().enumerate() {
        let l = l as usize;
        if l == 0 || l > comps.len() {
            continue;
        }
        let (x, y) = ((i % w) as f64, (i / w) as f64);
        if core.data()[i] {
            cores[l] = [cores[l][0] + 1.0, cores[l][1] + x, cores[l][2] + y];
        } else {
            added[l] += 1;
            adds[l] = [adds[l][0] + 1.0, adds[l][1] + x, adds[l][2] + y, adds[l][3] + x * x + y * y];
        }
    }
    for (i, (&f, &g)) in further.data().iter().zip(grown.data()).enumerate() {
        if !f || g {
            continue;
        }
        let (x, y) = (i % w, i / w);
        let owner = (y.saturating_sub(steps)..=(y + steps).min(h - 1))
            .flat_map(|ny| (x.saturating_sub(steps)..=(x + steps).min(w - 1)).map(move |nx| ny * w + nx))
            .map(|j| labels[j])
            .find(|&l| l != 0);
        if let Some(b) = owner.and_then(|l| beyond.get_mut(l as usize)) {
            *b += 1;
        }
    }
    let runaway: Vec<bool> = (0..=comps.len())
        .map(|l| {
            let (a, b) = (added[l], beyond[l]);
            let (c, d) = (cores[l], adds[l]);
            if a == 0 || c[0] <= 0.0 {
                return false;
            }
            let (cx, cy) = (c[1] / c[0], c[2] / c[0]);
            let (ax, ay) = (d[1] / d[0], d[2] / d[0]);
            // spread of the added pixels about their own centroid
            let spread = (d[3] / d[0] - ax * ax - ay * ay).max(0.0).sqrt();
            let offset = ((ax - cx).powi(2) + (ay - cy).powi(2)).sqrt();
            2 * b > a || offset > LOPSIDED * spread
        })
        .collect();
    let mut out = grown;
    for ((o, &l), &c) in out.data_mut().iter_mut().zip(&labels).zip(core.data()) {
        if *o && runaway.get(l as usize).copied().unwrap_or(false) {
            *o = c;
        }
    }
    out
}

/// Per-component pixel lists of a labelled mask (one pass, independent of the defect count).
pub fn pixel_lists(labels: &[u32], count: usize) -> Vec<Vec<usize>> {
    let mut lists: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (i, &l) in labels.iter().enumerate() {
        if l > 0
            && let Some(list) = lists.get_mut(l as usize - 1)
        {
            list.push(i);
        }
    }
    lists
}

/// Principal-axis (length, elongation) of a pixel set: a uniform segment of length L has
/// variance L²/12 along its axis.
pub fn shape(pixels: &[usize], width: usize) -> (f32, f32) {
    if pixels.len() < 2 {
        return (1.0, 1.0);
    }
    let n = pixels.len() as f64;
    let (mut sx, mut sy) = (0.0, 0.0);
    for &i in pixels {
        sx += (i % width) as f64;
        sy += (i / width) as f64;
    }
    let (mx, my) = (sx / n, sy / n);
    let (mut cxx, mut cyy, mut cxy) = (0.0, 0.0, 0.0);
    for &i in pixels {
        let (dx, dy) = ((i % width) as f64 - mx, (i / width) as f64 - my);
        cxx += dx * dx;
        cyy += dy * dy;
        cxy += dx * dy;
    }
    let (cxx, cyy, cxy) = (cxx / n + 1.0 / 12.0, cyy / n + 1.0 / 12.0, cxy / n);
    let tr = cxx + cyy;
    let disc = ((cxx - cyy) * (cxx - cyy) / 4.0 + cxy * cxy).sqrt();
    let major = (tr / 2.0 + disc).max(1e-12);
    let minor = (tr / 2.0 - disc).max(0.0);
    let length = 2.0 * (3.0 * major).sqrt();
    let width = (2.0 * (3.0 * minor).sqrt()).max(1.0);
    (length as f32, (length / width).max(1.0) as f32)
}

/// Conservative no-IR detector on the negative's luma (linear transmittance): dust is a small,
/// sharp, neutral speck darker than its local median and far smaller than image features.
/// Returns the core mask.
pub fn no_ir_core(rgb: &[Plane; 3], valid: &Mask, cfg: &HealConfig, scale: f32) -> Mask {
    let (w, h) = (valid.width(), valid.height());
    let Some(l) = rgb[0].zip_map(&rgb[1], |a, b| a + b).ok().and_then(|p| p.zip_map(&rgb[2], |a, b| (a + b) / 3.0).ok()) else {
        return Mask::from_plane(&rgb[0], |_| false);
    };
    // work on a level where specks are a few pixels across, so the median stays small
    let level = (scale.round() as usize).max(1);
    let lc = if level > 1 { resample::downsample(&l, level).unwrap_or_else(|_| l.clone()) } else { l.clone() };
    let lc = lc.map(|v| if v.is_finite() { v } else { 0.0 });
    let Ok(med) = filter::median(&lc, 3) else {
        return Mask::from_plane(&l, |_| false);
    };
    let d = lc.zip_map(&med, |v, m| if m > 1e-6 { 1.0 - v / m } else { 0.0 }).unwrap_or_else(|_| lc.clone());
    let validc = if level > 1 {
        Mask::from_plane(&resample::downsample(&valid.to_plane(), level).unwrap_or_else(|_| valid.to_plane()), |v| v > 0.999)
    } else {
        valid.clone()
    };
    let sigma = util::robust_stats(d.data(), |i| validc.data()[i], 150_000).map_or(0.0, |(_, s)| s);
    let t = (cfg.no_ir_contrast as f32).max(cfg.no_ir_sigma as f32 * sigma);
    // grow each speck to its half-contrast extent
    let cand = hysteresis(&d, &validc, t, t * 0.5, 2);
    let (labels, comps) = silvercraft_imgproc::label(&cand);
    let lists = pixel_lists(&labels, comps.len());
    let (cw, ch) = (d.width(), d.height());
    let max_area = ((cfg.no_ir_max_area as f32) * (scale / level as f32).powi(2)).max(1.0) as usize;
    let mut keep = vec![false; comps.len() + 1];
    let channel_planes: Vec<Plane> =
        if level > 1 { rgb.iter().map(|p| resample::downsample(p, level).unwrap_or_else(|_| p.clone())).collect() } else { rgb.to_vec() };
    for (k, (c, px)) in comps.iter().zip(&lists).enumerate() {
        if c.area > max_area || px.is_empty() {
            continue;
        }
        // sharp and isolated: the ring 2..=4 px outside is flat (no image structure continuing)
        let [x0, y0, x1, y1] = c.bbox;
        let (rx0, ry0, rx1, ry1) = (x0.saturating_sub(4), y0.saturating_sub(4), (x1 + 4).min(cw), (y1 + 4).min(ch));
        let mut ring_d = Vec::new();
        let mut ring_c: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for y in ry0..ry1 {
            for x in rx0..rx1 {
                let inner = x + 2 > x0 && x < x1 + 1 && y + 2 > y0 && y < y1 + 1;
                if inner {
                    continue;
                }
                let i = y * cw + x;
                if !validc.data()[i] {
                    continue;
                }
                ring_d.push(d.data()[i].abs());
                for (rc, p) in ring_c.iter_mut().zip(&channel_planes) {
                    rc.push(p.data()[i]);
                }
            }
        }
        if ring_d.len() < 8 {
            continue;
        }
        let ring_activity = silvercraft_imgproc::stats::percentile(&ring_d, 90.0).unwrap_or(f32::INFINITY);
        let peak = px.iter().map(|&i| d.data()[i]).fold(0.0f32, f32::max);
        if ring_activity > 0.3 * peak {
            continue;
        }
        // neutral: an occluder dims every channel by the same fraction
        let mut depths = [0.0f32; 3];
        let mut ok = true;
        for (cidx, p) in channel_planes.iter().enumerate() {
            let inside = px.iter().map(|&i| p.data()[i]).fold(f32::INFINITY, f32::min);
            let around = silvercraft_imgproc::stats::percentile(&ring_c[cidx], 50.0).unwrap_or(0.0);
            if !(around > 1e-6 && inside.is_finite()) {
                ok = false;
                break;
            }
            depths[cidx] = 1.0 - inside / around;
        }
        let (lo, hi) = (depths.iter().copied().fold(f32::INFINITY, f32::min), depths.iter().copied().fold(0.0f32, f32::max));
        if !ok || lo <= 0.0 || hi > 1.5 * lo + 0.05 {
            continue;
        }
        keep[k + 1] = true;
    }
    let mut core = cand.clone();
    for (o, &lab) in core.data_mut().iter_mut().zip(&labels) {
        *o = keep.get(lab as usize).copied().unwrap_or(false);
    }
    if level > 1 {
        let up = util::upsample(&core.to_plane(), level, w, h);
        Mask::from_plane(&up, |v| v > 0.25)
    } else {
        core
    }
}

/// Dilate the core by the halo, never into `forbidden` (the holder).
pub fn final_mask(core: &Mask, halo: usize, forbidden: &Mask) -> Mask {
    let mut m = morph::dilate(core, halo);
    for (o, &f) in m.data_mut().iter_mut().zip(forbidden.data()) {
        *o &= !f;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_grows_bounded() {
        // a seed at x=2 and a long weak tail along the row
        let s = Plane::from_fn(40, 3, |x, y| {
            if y != 1 {
                0.0
            } else if x == 2 {
                1.0
            } else {
                0.3
            }
        })
        .unwrap();
        let valid = Mask::from_plane(&s, |_| true);
        let m = hysteresis(&s, &valid, 0.5, 0.2, 5);
        assert!(m.get(2, 1) && m.get(7, 1) && !m.get(8, 1), "grew exactly 5 steps");
        assert!(!hysteresis(&s, &valid, 2.0, 0.2, 5).get(7, 1), "no seed, no growth");
    }

    #[test]
    fn shape_is_orientation_independent() {
        let w = 100;
        let diag: Vec<usize> = (0..40).map(|k| (k + 5) * w + k + 5).collect();
        let (len, el) = shape(&diag, w);
        assert!((len - 40.0 * 2f32.sqrt()).abs() < 4.0 && el > 10.0, "{len} {el}");
        let blob: Vec<usize> = (0..5).flat_map(|y| (0..5).map(move |x| y * w + x)).collect();
        assert!(shape(&blob, w).1 < 1.2);
    }

    #[test]
    fn no_ir_detects_neutral_speck_not_edges() {
        let (w, h) = (80, 60);
        let base = |x: usize, y: usize| {
            let edge = if x > 50 { 0.3 } else { 0.5 };
            let speck = if (x as i32 - 20).pow(2) + (y as i32 - 30).pow(2) <= 2 { 0.4 } else { 1.0 };
            // a tinted dense dot (a highlight on a colour negative) is not neutral
            let tinted = (x as i32 - 35).pow(2) + (y as i32 - 15).pow(2) <= 2;
            (edge * speck, tinted)
        };
        let ch = |k: usize| {
            Plane::from_fn(w, h, move |x, y| {
                let (v, t) = base(x, y);
                if t && k == 0 { v * 0.4 } else { v }
            })
            .unwrap()
        };
        let rgb = [ch(0), ch(1), ch(2)];
        let valid = Mask::from_plane(&rgb[0], |_| true);
        let cfg = HealConfig::default();
        let core = no_ir_core(&rgb, &valid, &cfg, 1.0);
        assert!(core.get(20, 30), "neutral speck found");
        assert!(!core.get(35, 15), "tinted dot left alone");
        assert!((45..56).all(|x| !core.get(x, 30)), "edge left alone");
        assert!(core.count() < 20);
    }
}
