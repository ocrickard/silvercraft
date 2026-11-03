//! Camera-scan frame box: one film frame per capture, photographed over a light table.
//!
//! The exposed frame is bounded by film base: the rebate (with sprocket holes on 35 mm) and the
//! inter-frame gaps. Outside the film there is either a dark gate (holder) or the light source.
//!
//! 1. Area-downsample to `work_size` and normalise by the 99th percentile.
//! 2. **Deskew**: the frame, rebate and gate edges are long and straight; content edges point
//!    every which way. Strong edge pixels projected onto the edge normal pile up sharply only at
//!    the frame rotation (projection sharpness). The working image is resampled upright.
//! 3. **Aperture**: when a dark gate surrounds the film, the largest bright component bounds the
//!    search (the original film-vs-gate step). Without a gate, the whole view is searched.
//! 4. **Four edges**: column
//!    and row profiles of a 4-px step response (40th percentile over the middle rows/columns,
//!    so only edges spanning most of a side count) give up to `edge_candidates` (8) candidates
//!    per side. A
//!    rectangle is kept when every side is supported along most of its length, all four outside
//!    strips have the same colour (they all abut the same rebate; mixing a gate edge with a
//!    photo edge fails this) and are flat along their length (a strip through a thin, pale photo
//!    border varies), and the inside is not smooth on all four sides (a smooth inside is
//!    a gate or rebate). The largest such rectangle wins, so a horizon or a smooth sky cannot
//!    trim the photograph.
//! 5. With no four-edge rectangle, the aperture is returned with low confidence (for manual
//!    framing); automatic capture must not use it.

use rayon::prelude::*;
use silvercraft_imgproc::{Mask, Plane, filter, label, morph, stats};

use crate::config::CameraConfig;
use crate::geom::{PxRect, RotRect};
use crate::util::{clean, downsample_rgb, luma};
use crate::{Result, Rgb};

/// A detected camera-scan frame.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CameraFrame {
    /// Axis-aligned bounds of the (rotated) frame in capture pixels.
    pub rect: PxRect,
    /// The frame as a rotated rectangle in capture pixels.
    pub rotated: RotRect,
    /// Frame rotation in the capture (degrees, clockwise on screen positive); deskew by its
    /// negative.
    pub angle_deg: f64,
    /// 0.9: four well-supported edges and a textured interior; 0.55: four edges but weak;
    /// 0.35: aperture only (manual framing, never autocapture).
    pub confidence: f32,
    /// Four frame edges were found (otherwise the rectangle is the film aperture).
    pub edges_found: bool,
    /// Weakest side's edge support (0..1).
    pub support: f32,
}

/// Detect the frame in a camera capture (interleaved linear RGB).
pub fn detect_camera_frame(rgb: &Rgb<'_, f32>, cfg: &CameraConfig) -> Result<Option<CameraFrame>> {
    let longest = rgb.width().max(rgb.height());
    let factor = longest.div_ceil(cfg.work_size.clamp(64, 8192)).max(1);
    let planes = downsample_rgb(rgb, factor, clean)?;
    Ok(detect_planes(&planes, factor as f64, rgb.width(), rgb.height(), cfg, cfg.deskew))
}

/// Detection on working-size planes; `scale` maps working to output pixels.
pub(crate) fn detect_planes(planes: &[Plane; 3], scale: f64, out_w: usize, out_h: usize, cfg: &CameraConfig, deskew: bool) -> Option<CameraFrame> {
    let work = Work::new(planes, cfg)?;
    let angle = if deskew { work.skew(cfg)? } else { 0.0 };
    let search = |(gray, data): (&Plane, &[Plane; 3])| -> Option<((usize, usize, usize, usize), Option<Boundary>)> {
        let aperture = work.aperture(gray)?;
        let data = work.film_normalised(data, aperture);
        Some((aperture, boundary_frame(&data, aperture, cfg, work.scale_px(6.0))))
    };
    let (mut angle, (mut aperture, mut found)) = if angle.abs() > 0.05 {
        let (g, d) = work.rotated(angle);
        (angle, search((&g, &d))?)
    } else {
        (0.0, search((&work.gray, &work.data))?)
    };
    // A wrong small skew estimate (content edges, a film slightly rotated in its holder) would
    // shear the frame edges off their 1-px support: retry upright.
    // With no frame either way the skew is unconfirmed: report the upright aperture.
    if found.is_none() && angle != 0.0 {
        (aperture, found) = search((&work.gray, &work.data))?;
        angle = 0.0;
    }
    let (x, y, bw, bh, confidence, support, edges) = match found {
        // Four edges with weaker support (0.55 confidence) still beat the aperture for framing.
        Some(b) => (b.x, b.y, b.w, b.h, b.confidence, b.support, true),
        _ if work.gate => (aperture.0, aperture.1, aperture.2, aperture.3, 0.35, found.map_or(0.0, |b| b.support), false),
        _ => return None,
    };
    work.output((x, y, bw, bh), angle, scale, (out_w, out_h), confidence, edges, support)
}

/// Working-size planes prepared for detection: luminance normalised by its 99th percentile,
/// RGB by the 99th percentile over all channels, and whether a dark gate surrounds the film.
pub(crate) struct Work {
    pub(crate) gray: Plane,
    pub(crate) data: [Plane; 3],
    pub(crate) w: usize,
    pub(crate) h: usize,
    film_thr: f32,
    pub(crate) gate: bool,
}

impl Work {
    pub(crate) fn new(planes: &[Plane; 3], cfg: &CameraConfig) -> Option<Self> {
        let (w, h) = (planes[0].width(), planes[0].height());
        if w < 16 || h < 16 || planes.iter().any(|p| p.width() != w || p.height() != h) {
            return None;
        }
        let gray = luma(planes).ok()?;
        let g99 = stats::percentile(&stats::subsample(gray.data(), 200_000), 99.0)?;
        if g99.is_nan() || g99 <= 1e-9 {
            return None;
        }
        let mut all: Vec<f32> = Vec::with_capacity(3 * 70_000);
        for p in planes {
            all.extend(stats::subsample(p.data(), 70_000));
        }
        let d99 = stats::percentile(&all, 99.0).unwrap_or(g99).max(1.0e-9);
        let gray = gray.map(|v| v / g99);
        let data: [Plane; 3] = [planes[0].map(|v| v / d99), planes[1].map(|v| v / d99), planes[2].map(|v| v / d99)];

        // Film vs gate: halfway (geometrically) between the gate level (5th percentile) and the
        // top (1.0 after normalising), at most `film_threshold`. A fixed fraction of the top
        // fails when sprocket holes or the light source make the top bare light rather than
        // film base.
        let p5 = f64::from(stats::percentile(&stats::subsample(gray.data(), 200_000), 5.0).unwrap_or(0.0)).max(1e-6);
        let film_thr = p5.sqrt().clamp(0.05, cfg.film_threshold.max(0.05)) as f32;
        let film_frac = gray.data().iter().filter(|&&v| v > film_thr).count() as f64 / gray.len() as f64;
        if film_frac < 0.02 {
            return None;
        }
        // A gate (holder) shows as a mostly dark image border; a light table without one shows
        // light.
        let band = ((w.min(h) as f64 * 0.02).round() as usize).max(1);
        let (mut dark, mut total) = (0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if x < band || y < band || x + band >= w || y + band >= h {
                    total += 1;
                    dark += usize::from(gray.get(x, y) <= film_thr);
                }
            }
        }
        let gate = film_frac <= 0.95 && dark * 2 >= total;
        Some(Self { gray, data, w, h, film_thr, gate })
    }

    /// `v` working pixels at a 900-px working size, scaled to this one.
    pub(crate) fn scale_px(&self, v: f64) -> usize {
        ((v * self.w.max(self.h) as f64 / 900.0).round() as usize).max(1)
    }

    /// The film aperture `(x, y, w, h)` in `gray`: the largest bright component inside a gate,
    /// the whole view without one.
    pub(crate) fn aperture(&self, gray: &Plane) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (self.w, self.h);
        if !self.gate {
            return Some((0, 0, w, h));
        }
        let thr = self.film_thr;
        let m = Mask::from_plane(gray, |v| v > thr);
        let r = self.scale_px(4.0);
        let m = morph::close(&morph::open(&m, r), r);
        let (_, comps) = label(&m);
        let c = comps.iter().max_by_key(|c| c.area)?;
        let [x0, y0, x1, y1] = c.bbox;
        if ((x1 - x0) as f64) < w as f64 * 0.15 || ((y1 - y0) as f64) < h as f64 * 0.15 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// The frame rotation. Skew is measured inside the aperture, away from the gate edges: a
    /// holder can sit a little rotated against the film it holds, and its long edges would win.
    pub(crate) fn skew(&self, cfg: &CameraConfig) -> Option<f64> {
        let (ax, ay, aw, ah) = self.aperture(&self.gray)?;
        let (ix, iy) = (aw / 25, ah / 25);
        let inner =
            if self.gate { self.gray.crop(ax + ix, ay + iy, aw.saturating_sub(2 * ix).max(1), ah.saturating_sub(2 * iy).max(1)).ok() } else { None };
        Some(estimate_skew(inner.as_ref().unwrap_or(&self.gray), cfg.max_skew_deg.clamp(0.0, 20.0)))
    }

    /// Luminance and RGB resampled with a frame rotated by `angle` upright.
    pub(crate) fn rotated(&self, angle: f64) -> (Plane, [Plane; 3]) {
        (rotate(&self.gray, angle), self.data.clone().map(|p| rotate(&p, angle)))
    }

    /// RGB normalised by the film base, not by the brightest pixels: visible sprocket holes or
    /// a light source around the film are bare light, which would scale every edge and texture
    /// threshold down by the base density.
    pub(crate) fn film_normalised(&self, data: &[Plane; 3], aperture: (usize, usize, usize, usize)) -> [Plane; 3] {
        let level = film_level(data, aperture);
        let k = if level > 1e-6 && level.is_finite() { 1.0 / level } else { 1.0 };
        [data[0].map(|v| v * k), data[1].map(|v| v * k), data[2].map(|v| v * k)]
    }

    /// A box `(x, y, w, h)` found in the working image rotated upright by `angle`, back in
    /// output pixels: undo the rotation about the working centre, then scale.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn output(
        &self,
        (x, y, bw, bh): (usize, usize, usize, usize),
        angle: f64,
        scale: f64,
        (out_w, out_h): (usize, usize),
        confidence: f32,
        edges_found: bool,
        support: f32,
    ) -> Option<CameraFrame> {
        let (cx, cy) = ((self.w as f64) / 2.0, (self.h as f64) / 2.0);
        let (px, py) = (x as f64 + bw as f64 / 2.0 - cx, y as f64 + bh as f64 / 2.0 - cy);
        let (s, c) = angle.to_radians().sin_cos();
        let (ox, oy) = (cx + px * c - py * s, cy + px * s + py * c);
        let rotated = RotRect { cx: ox * scale, cy: oy * scale, width: bw as f64 * scale, height: bh as f64 * scale, angle_deg: angle };
        let rect = rotated.bounding_box(out_w, out_h)?;
        if (rect.width as f64) < out_w as f64 * 0.08 || (rect.height as f64) < out_h as f64 * 0.08 {
            return None;
        }
        Some(CameraFrame { rect, rotated, angle_deg: angle, confidence, edges_found, support })
    }
}

/// The film base level inside the aperture: the 99th percentile of the brightest channel among
/// pixels that are not bare light. Bare light (sprocket holes, the light source around the
/// film) is neutral and brighter than any base; when the top 0.5 % is neutral it is excluded.
fn film_level(data: &[Plane; 3], aperture: (usize, usize, usize, usize)) -> f32 {
    let (fx, fy, w, h) = aperture;
    let step = ((w * h) / 200_000).max(1);
    let px: Vec<[f32; 3]> = (0..w * h)
        .step_by(step)
        .map(|i| {
            let (x, y) = (fx + i % w, fy + i / w);
            [data[0].get(x, y), data[1].get(x, y), data[2].get(x, y)]
        })
        .collect();
    let mx: Vec<f32> = px.iter().map(|p| p[0].max(p[1]).max(p[2])).collect();
    let Some(top) = stats::percentile(&mx, 99.5) else { return 1.0 };
    let neutral = |p: &[f32; 3]| {
        let (hi, lo) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
        hi - lo <= 0.1 * hi
    };
    let bare: Vec<bool> = px.iter().zip(&mx).map(|(p, &m)| m >= 0.8 * top && neutral(p)).collect();
    let top_bare = px.iter().zip(&mx).zip(&bare).filter(|((_, m), _)| **m >= top).all(|(_, b)| *b);
    if !top_bare {
        return top;
    }
    let film: Vec<f32> = mx.iter().zip(&bare).filter(|(_, b)| !**b).map(|(m, _)| *m).collect();
    if film.len() * 10 < mx.len() { top } else { stats::percentile(&film, 99.0).unwrap_or(top) }
}

/// Rotation of the dominant straight edges (degrees, within ±`max_deg`; 0 when none stand out).
///
/// Projection sharpness: strong edge pixels (top 5 % gradient) are projected onto the edge
/// normal for each candidate angle (0.05° steps): horizontal-ish edges onto
/// `r = -x·sin θ + y·cos θ`, vertical-ish ones onto `x·cos θ + y·sin θ`. At the frame's rotation the
/// long straight edges pile into single 1-px bins, so the sum of squared bin weights peaks there.
/// Unlike a local gradient-orientation histogram this is not biased toward 0° by the staircase
/// of an aliased edge.
pub(crate) fn estimate_skew(gray: &Plane, max_deg: f64) -> f64 {
    if max_deg.is_nan() || max_deg <= 0.0 {
        return 0.0;
    }
    let max_deg = max_deg.min(20.0);
    let (gx, gy) = filter::sobel(&filter::gaussian(gray, 1.0));
    let mag: Vec<f32> = gx.data().iter().zip(gy.data()).map(|(a, b)| a.hypot(*b)).collect();
    let thr = stats::percentile(&stats::subsample(&mag, 200_000), 95.0).unwrap_or(0.0).max(0.02);
    let w = gray.width();
    let mut pts: Vec<(f32, f32, f32, bool)> = Vec::new();
    for (i, ((&a, &b), &m)) in gx.data().iter().zip(gy.data()).zip(&mag).enumerate() {
        if m >= thr && m.is_finite() {
            pts.push(((i % w) as f32, (i / w) as f32, m, b.abs() > a.abs()));
        }
    }
    if pts.len() < 16 {
        return 0.0;
    }
    let span = (gray.width() + gray.height()) * 2 + 4;
    let offset = (gray.width() + gray.height()) as f32 + 2.0;
    let step = 0.05;
    let n = (2.0 * max_deg / step).round() as usize + 1;
    let scores: Vec<f64> = (0..n)
        .into_par_iter()
        .map(|k| {
            let th = (-max_deg + k as f64 * step).to_radians();
            let (sn, cs) = (th.sin() as f32, th.cos() as f32);
            let mut hb = vec![0f32; span];
            let mut vb = vec![0f32; span];
            for &(x, y, m, horizontal) in &pts {
                let (r, bins) = if horizontal { (-x * sn + y * cs, &mut hb) } else { (x * cs + y * sn, &mut vb) };
                if let Some(v) = bins.get_mut((r + offset).round().max(0.0) as usize) {
                    *v += m;
                }
            }
            hb.iter().chain(&vb).map(|&v| f64::from(v) * f64::from(v)).sum()
        })
        .collect();
    let Some((bi, &bv)) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { return 0.0 };
    let mut sorted = scores.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    // No straight edges (a flat curve), or upright is as sharp as the best angle: no skew.
    let at_zero = scores.get((max_deg / step).round() as usize).copied().unwrap_or(0.0);
    if bv.is_nan() || bv <= 1.15 * median || at_zero >= 0.97 * bv {
        return 0.0;
    }
    let off = if bi > 0 && bi + 1 < n {
        let (a, b, c) = (scores[bi - 1], bv, scores[bi + 1]);
        let d = a - 2.0 * b + c;
        if d.abs() > 1e-12 { (0.5 * (a - c) / d).clamp(-0.5, 0.5) } else { 0.0 }
    } else {
        0.0
    };
    -max_deg + (bi as f64 + off) * step
}

/// `out(p) = src(c + R(angle)(p - c))`: content rotated by `angle` in `src` is upright in `out`.
pub(crate) fn rotate(src: &Plane, angle_deg: f64) -> Plane {
    let (w, h) = (src.width(), src.height());
    let (s, c) = (angle_deg.to_radians().sin_cos().0 as f32, angle_deg.to_radians().cos() as f32);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    Plane::from_fn(w, h, |x, y| {
        let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        src.sample(cx + px * c - py * s - 0.5, cy + px * s + py * c - 0.5)
    })
    .unwrap_or_else(|_| src.clone())
}

#[derive(Debug, Clone, Copy)]
struct Boundary {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    confidence: f32,
    support: f32,
    /// One side was inferred from the aspect prior.
    completed: bool,
}

/// Candidate edge positions in `[start, end)`: local maxima (largest within ±`sep`) above `thr`;
/// the `max` strongest plus the `max / 2` outermost (nearest `outer_end`). Frame edges are the
/// outermost edges inside the rebate, and on a thin negative they can be weaker than many
/// content edges. Non-maximum suppression keeps one candidate per feature.
fn peaks(profile: &[f32], start: usize, end: usize, thr: f32, sep: usize, max: usize, outer_is_start: bool) -> Vec<usize> {
    let end = end.min(profile.len());
    if start >= end {
        return Vec::new();
    }
    let maxima: Vec<usize> = (start..end)
        .filter(|&i| {
            let v = profile[i];
            let lo = i.saturating_sub(sep);
            v >= thr && profile[lo..(i + sep + 1).min(profile.len())].iter().enumerate().all(|(j, &o)| o < v || (o == v && lo + j >= i))
        })
        .collect();
    let mut strongest = maxima.clone();
    strongest.sort_by(|&a, &b| profile[b].total_cmp(&profile[a]));
    strongest.truncate(max);
    let outer: Vec<usize> =
        if outer_is_start { maxima.iter().copied().take(max / 2).collect() } else { maxima.iter().rev().copied().take(max / 2).collect() };
    for o in outer {
        if !strongest.contains(&o) {
            strongest.push(o);
        }
    }
    strongest
}

fn std_dev(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let n = v.len() as f32;
    let mean = v.iter().sum::<f32>() / n;
    (v.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n).sqrt()
}

/// The box `(x, y, w, h)` holds a photograph, not a uniform patch: some channel varies enough
/// (standard deviation and 5–95 % spread) inside it.
pub(crate) fn interior_textured(data: &[Plane; 3], (bx, by, bw, bh): (usize, usize, usize, usize)) -> bool {
    let (mut spread, mut texture) = (0f32, 0f32);
    let (mut p95, mut p5) = (0f32, f32::MAX);
    for p in data {
        let (x1, y1) = ((bx + bw).min(p.width()), (by + bh).min(p.height()));
        let interior: Vec<f32> = (by..y1).flat_map(|y| (bx..x1).map(move |x| (x, y))).step_by(3).map(|(x, y)| p.get(x, y)).collect();
        if let Some(v) = stats::percentiles(&interior, &[5.0, 95.0]) {
            p5 = p5.min(v[0]);
            p95 = p95.max(v[1]);
        }
        texture = texture.max(std_dev(&interior));
    }
    if p95 > p5 {
        spread = p95 - p5;
    }
    texture > 0.012 && spread > 0.04
}

/// Colour and texture checks of a rectangle `(x0, x1, y0, y1)` (relative to `aperture`) in
/// film-normalised planes, on strips `gap` pixels outside and inside each side. All four outside strips abut the same base (rebate or inter-frame
/// gap): equal colour, and flat along their length; a strip running through image content,
/// even a thin, pale border of a photograph, varies. Smooth inner sides describe a gate or
/// rebate, not a photograph; `real` marks the sides whose inside texture counts (an inferred
/// side has no edge of its own), `None` skips the inside check.
pub(crate) fn strips_ok(
    data: &[Plane; 3],
    aperture: (usize, usize, usize, usize),
    (x0, x1, y0, y1): (usize, usize, usize, usize),
    real: Option<[bool; 4]>,
    gap: usize,
    cfg: &CameraConfig,
) -> bool {
    let (fx, fy, w, h) = aperture;
    if x0 < gap || y0 < gap || x1 + gap + 1 > w || y1 + gap + 1 > h || x1 <= x0 || y1 <= y0 || x1 - x0 <= 2 * gap || y1 - y0 <= 2 * gap {
        return false;
    }
    let px = |c: usize, x: usize, y: usize| data[c].get(fx + x, fy + y);
    let column = |c: usize, x: usize, ya: usize, yb: usize| -> Vec<f32> { (ya..yb).map(|y| px(c, x, y)).collect() };
    let row = |c: usize, y: usize, xa: usize, xb: usize| -> Vec<f32> { (xa..xb).map(|x| px(c, x, y)).collect() };
    let (padx, pady) = (((x1 - x0) / 10).max(2), ((y1 - y0) / 10).max(2));
    let (ya, yb, xa, xb) = (y0 + pady, y1.saturating_sub(pady), x0 + padx, x1.saturating_sub(padx));
    let (mut spread, mut rough) = (0f32, 0f32);
    for c in 0..3 {
        let mut strips = [column(c, x0 - gap, ya, yb), column(c, x1 + gap, ya, yb), row(c, y0 - gap, xa, xb), row(c, y1 + gap, xa, xb)];
        let mut meds = [0f32; 4];
        for (m, st) in meds.iter_mut().zip(strips.iter_mut()) {
            // Interquartile: sprocket holes along a rebate strip are a minority.
            let mut q = [0f32; 3];
            if crate::util::pcts_in_place(st, &[25.0, 50.0, 75.0], &mut q) {
                *m = q[1];
                rough = rough.max((q[2] - q[0]) / q[1].max(0.05));
            }
        }
        let (lo, hi) = meds.iter().fold((f32::MAX, f32::MIN), |a, &v| (a.0.min(v), a.1.max(v)));
        spread = spread.max(hi - lo);
    }
    if f64::from(spread) > cfg.max_outside_spread || f64::from(rough) > cfg.max_outside_roughness {
        return false;
    }
    let Some(real) = real else { return true };
    let mut inside = 0f32;
    for c in 0..3 {
        let sides = [
            (real[0], std_dev(&column(c, x0 + gap, ya, yb))),
            (real[1], std_dev(&column(c, x1 - gap, ya, yb))),
            (real[2], std_dev(&row(c, y0 + gap, xa, xb))),
            (real[3], std_dev(&row(c, y1 - gap, xa, xb))),
        ];
        inside = sides.iter().filter(|s| s.0).fold(inside, |m, s| m.max(s.1));
    }
    f64::from(inside) >= cfg.min_inside_std
}

/// Four enclosing image/rebate edges inside `aperture` (x, y, w, h) of the normalised planes.
fn boundary_frame(data: &[Plane; 3], aperture: (usize, usize, usize, usize), cfg: &CameraConfig, sep: usize) -> Option<Boundary> {
    let (fx, fy, w, h) = aperture;
    if w.min(h) < 32 {
        return None;
    }
    let px = |c: usize, x: usize, y: usize| data[c].get(fx + x, fy + y);
    // Centred 4-px step responses, max over channels.
    let mut dx = vec![0f32; w * h];
    let mut dy = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            if x >= 2 && x + 2 < w {
                dx[y * w + x] = (0..3).map(|c| (px(c, x + 2, y) - px(c, x - 2, y)).abs()).fold(0.0, f32::max);
            }
            if y >= 2 && y + 2 < h {
                dy[y * w + x] = (0..3).map(|c| (px(c, x, y + 2) - px(c, x, y - 2)).abs()).fold(0.0, f32::max);
            }
        }
    }
    let q40 = |v: &mut Vec<f32>| -> f32 {
        if v.is_empty() {
            return 0.0;
        }
        let k = (0.4 * (v.len() - 1) as f64).round() as usize;
        *v.select_nth_unstable_by(k, f32::total_cmp).1
    };
    let mut buf = Vec::new();
    let cols: Vec<f32> = (0..w)
        .map(|x| {
            buf.clear();
            buf.extend((h / 5..4 * h / 5).map(|y| dx[y * w + x]));
            q40(&mut buf)
        })
        .collect();
    let rows: Vec<f32> = (0..h)
        .map(|y| {
            buf.clear();
            buf.extend((w / 5..4 * w / 5).map(|x| dy[y * w + x]));
            q40(&mut buf)
        })
        .collect();
    let thr = cfg.edge_threshold as f32;
    let k = cfg.edge_candidates.clamp(1, 16);
    let lefts = peaks(&cols, 3, w / 2, thr, sep, k, true);
    let rights = peaks(&cols, w / 2, w.saturating_sub(5), thr, sep, k, false);
    let tops = peaks(&rows, 3, h / 2, thr, sep, k, true);
    let bottoms = peaks(&rows, h / 2, h.saturating_sub(5), thr, sep, k, false);

    // Edge hits (±1 px) along each candidate line, as prefix sums: support of any span is O(1).
    let vprefix = |x: usize| -> Vec<u32> {
        let mut p = vec![0u32; h + 1];
        for y in 0..h {
            let hit = (x.saturating_sub(1)..=(x + 1).min(w - 1)).any(|xx| dx[y * w + xx] > thr);
            p[y + 1] = p[y] + u32::from(hit);
        }
        p
    };
    let hprefix = |y: usize| -> Vec<u32> {
        let mut p = vec![0u32; w + 1];
        for x in 0..w {
            let hit = (y.saturating_sub(1)..=(y + 1).min(h - 1)).any(|yy| dy[yy * w + x] > thr);
            p[x + 1] = p[x] + u32::from(hit);
        }
        p
    };
    let span = |p: &[u32], a: usize, b: usize| -> f32 {
        let (a, b) = (a.min(p.len() - 1), b.min(p.len() - 1));
        if b <= a { 0.0 } else { (p[b] - p[a]) as f32 / (b - a) as f32 }
    };
    let lp: Vec<Vec<u32>> = lefts.iter().map(|&x| vprefix(x)).collect();
    let rp: Vec<Vec<u32>> = rights.iter().map(|&x| vprefix(x)).collect();
    let tp: Vec<Vec<u32>> = tops.iter().map(|&y| hprefix(y)).collect();
    let bp: Vec<Vec<u32>> = bottoms.iter().map(|&y| hprefix(y)).collect();

    // Frame shape prior: long/short side ratio near `aspect` (either orientation).
    let aspect_ok = |bw: usize, bh: usize| -> bool {
        if cfg.aspect.is_nan() || cfg.aspect <= 0.0 || bw == 0 || bh == 0 {
            return true;
        }
        let r = bw as f64 / bh as f64;
        let want = cfg.aspect.max(1.0 / cfg.aspect);
        (r.max(1.0 / r) / want - 1.0).abs() <= cfg.aspect_tolerance
    };
    let checks = |x0: usize, x1: usize, y0: usize, y1: usize, real: [bool; 4]| strips_ok(data, aperture, (x0, x1, y0, y1), Some(real), 3, cfg);
    let pads = |x0: usize, x1: usize, y0: usize, y1: usize| {
        let (padx, pady) = (((x1 - x0) / 10).max(2), ((y1 - y0) / 10).max(2));
        (y0 + pady, y1.saturating_sub(pady), x0 + padx, x1.saturating_sub(padx))
    };

    // Every four-edge combination, largest area first; the first one passing all checks wins.
    let mut combos: Vec<(usize, [usize; 4])> = Vec::new();
    for (li, &x0) in lefts.iter().enumerate() {
        for (ri, &x1) in rights.iter().enumerate() {
            if x1 <= x0 || ((x1 - x0) as f64) < w as f64 * 0.4 {
                continue;
            }
            for (ti, &y0) in tops.iter().enumerate() {
                for (bi, &y1) in bottoms.iter().enumerate() {
                    if y1 > y0 && ((y1 - y0) as f64) >= h as f64 * 0.4 && aspect_ok(x1 - x0, y1 - y0) {
                        combos.push(((x1 - x0) * (y1 - y0), [li, ri, ti, bi]));
                    }
                }
            }
        }
    }
    combos.sort_by_key(|c| std::cmp::Reverse(c.0));
    let mut best: Option<(usize, Boundary)> = None;
    for (area, [li, ri, ti, bi]) in combos {
        let (x0, x1, y0, y1) = (lefts[li], rights[ri], tops[ti], bottoms[bi]);
        let (ya, yb, xa, xb) = pads(x0, x1, y0, y1);
        let support = span(&lp[li], ya, yb).min(span(&rp[ri], ya, yb)).min(span(&tp[ti], xa, xb)).min(span(&bp[bi], xa, xb));
        if f64::from(support) < cfg.min_support || !checks(x0, x1, y0, y1, [true; 4]) {
            continue;
        }
        best = Some((area, Boundary { x: fx + x0, y: fy + y0, w: x1 - x0, h: y1 - y0, confidence: 0.0, support, completed: false }));
        break;
    }

    // No four-edge rectangle: complete one from three strongly supported sides and the aspect
    // prior (a thin negative can leave one frame edge invisible against the base).
    if best.is_none() && cfg.aspect > 0.0 {
        let strong = cfg.confident_support as f32;
        let ratios = [cfg.aspect, 1.0 / cfg.aspect];
        let mut found: Vec<(usize, Boundary)> = Vec::new();
        let mut consider = |x0: usize, x1: usize, y0: usize, y1: usize, real: [bool; 4], support: f32| {
            if support >= strong && ((x1 - x0) as f64) >= w as f64 * 0.4 && ((y1 - y0) as f64) >= h as f64 * 0.4 && checks(x0, x1, y0, y1, real) {
                found.push((
                    (x1 - x0) * (y1 - y0),
                    Boundary { x: fx + x0, y: fy + y0, w: x1 - x0, h: y1 - y0, confidence: 0.0, support, completed: true },
                ));
            }
        };
        for (ti, &y0) in tops.iter().enumerate() {
            for (bi, &y1) in bottoms.iter().enumerate() {
                if y1 <= y0 {
                    continue;
                }
                for r in ratios {
                    let bw = (r * (y1 - y0) as f64).round();
                    if !(bw.is_finite() && bw >= 1.0 && bw < w as f64) {
                        continue;
                    }
                    let bw = bw as usize;
                    for (ri, &x1) in rights.iter().enumerate() {
                        if let Some(x0) = x1.checked_sub(bw) {
                            let (ya, yb, xa, xb) = pads(x0, x1, y0, y1);
                            consider(
                                x0,
                                x1,
                                y0,
                                y1,
                                [false, true, true, true],
                                span(&rp[ri], ya, yb).min(span(&tp[ti], xa, xb)).min(span(&bp[bi], xa, xb)),
                            );
                        }
                    }
                    for (li, &x0) in lefts.iter().enumerate() {
                        let x1 = x0 + bw;
                        let (ya, yb, xa, xb) = pads(x0, x1, y0, y1);
                        consider(
                            x0,
                            x1,
                            y0,
                            y1,
                            [true, false, true, true],
                            span(&lp[li], ya, yb).min(span(&tp[ti], xa, xb)).min(span(&bp[bi], xa, xb)),
                        );
                    }
                }
            }
        }
        for (li, &x0) in lefts.iter().enumerate() {
            for (ri, &x1) in rights.iter().enumerate() {
                if x1 <= x0 {
                    continue;
                }
                for r in ratios {
                    let bh = ((x1 - x0) as f64 / r).round();
                    if !(bh.is_finite() && bh >= 1.0 && bh < h as f64) {
                        continue;
                    }
                    let bh = bh as usize;
                    for (bi, &y1) in bottoms.iter().enumerate() {
                        if let Some(y0) = y1.checked_sub(bh) {
                            let (ya, yb, xa, xb) = pads(x0, x1, y0, y1);
                            consider(
                                x0,
                                x1,
                                y0,
                                y1,
                                [true, true, false, true],
                                span(&lp[li], ya, yb).min(span(&rp[ri], ya, yb)).min(span(&bp[bi], xa, xb)),
                            );
                        }
                    }
                    for (ti, &y0) in tops.iter().enumerate() {
                        let y1 = y0 + bh;
                        let (ya, yb, xa, xb) = pads(x0, x1, y0, y1);
                        consider(
                            x0,
                            x1,
                            y0,
                            y1,
                            [true, true, true, false],
                            span(&lp[li], ya, yb).min(span(&rp[ri], ya, yb)).min(span(&tp[ti], xa, xb)),
                        );
                    }
                }
            }
        }
        best = found.into_iter().max_by_key(|f| f.0);
    }
    let (_, mut b) = best?;
    // A uniform rectangle is not a photograph; low-contrast photos stay manual.
    let textured = interior_textured(data, (b.x, b.y, b.w, b.h));
    b.confidence = if !b.completed && f64::from(b.support) >= cfg.confident_support && textured { 0.9 } else { 0.55 };
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotated_rect(angle: f64) -> Plane {
        let (s, c) = angle.to_radians().sin_cos();
        Plane::from_fn(400, 300, |x, y| {
            let (dx, dy) = (x as f64 + 0.5 - 200.0, y as f64 + 0.5 - 150.0);
            let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
            if u.abs() < 120.0 && v.abs() < 80.0 { 0.9 } else { 0.1 }
        })
        .unwrap()
    }

    #[test]
    fn skew_of_a_rotated_rectangle() {
        for a in [-3.0, -1.0, 0.0, 0.5, 2.5] {
            let got = estimate_skew(&rotated_rect(a), 5.0);
            assert!((got - a).abs() < 0.2, "{a}: {got}");
        }
        assert_eq!(estimate_skew(&rotated_rect(2.0), 0.0), 0.0);
    }

    /// A crisp textured rectangle in a field of fine noise with the same median everywhere: the
    /// four outside strips agree in colour but are rough, so they are not rebate.
    #[test]
    fn rough_outside_strips_are_not_rebate() {
        let (w, h) = (200, 150);
        let hash = |x: usize, y: usize| ((x * 7919 + y * 104_729) % 97) as f32 / 96.0;
        let p = Plane::from_fn(w, h, |x, y| {
            if (40..160).contains(&x) && (35..115).contains(&y) { 0.15 + 0.2 * hash(x / 3, y / 3) } else { 0.5 + 0.4 * (hash(x, y) - 0.5) }
        })
        .unwrap();
        let data = [p.clone(), p.clone(), p];
        let strict = CameraConfig { min_inside_std: 0.0, ..Default::default() };
        assert!(boundary_frame(&data, (0, 0, w, h), &strict, 2).is_none());
        let lax = CameraConfig { max_outside_roughness: 10.0, ..strict };
        let b = boundary_frame(&data, (0, 0, w, h), &lax, 2).unwrap();
        assert!(b.x.abs_diff(40) <= 2 && b.y.abs_diff(35) <= 2 && b.w.abs_diff(120) <= 3 && b.h.abs_diff(80) <= 3, "{b:?}");
    }

    #[test]
    fn rotate_undoes_the_skew() {
        let a = 2.0;
        let up = rotate(&rotated_rect(a), a);
        // Upright: the rectangle's top edge row is uniform across its width.
        let row: Vec<f32> = (100..300).map(|x| up.get(x, 150 - 78)).collect();
        assert!(row.iter().all(|&v| v > 0.8), "{row:?}");
    }
}
