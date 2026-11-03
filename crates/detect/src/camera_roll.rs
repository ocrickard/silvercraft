//! Camera rolls: every frame of a roll detected together.
//!
//! A roll is shot on a copy stand: the camera, the lens and the holder do not move, so the
//! frame's size in pixels (and nearly its position and rotation) is the same in every capture.
//! On a thin (underexposed) negative a frame edge can be almost as clear as the base around it,
//! and single-frame detection ([`detect_camera_frame`]) falls back to the holder's gate opening.
//!
//! 1. Detect every capture on its own.
//! 2. **Roll prior** ([`fit_roll_prior`]): the median size of the confident detections, kept
//!    when enough of them agree within `camera.prior_size_tolerance`, with their median
//!    centre and rotation.
//! 3. **Re-detect** the captures that are not confident, or whose size disagrees with the
//!    prior, with the size fixed: only the position is searched (at the capture's own
//!    rotation and the roll's). The edge evidence is measured in density, per channel: the
//!    base around a colour negative is its clearest part, and even a thin image is a little
//!    denser in some channel (often blue). Each side's step is a robust statistic (the 40th
//!    percentile) along its length, and a placement adds the steps of two opposite sides one
//!    frame size apart, so a faint edge is found together with its strong partner. The
//!    search spans the whole film aperture, so a frame that really moved is found where it is;
//!    the typical position only places an axis with no edge on either side.
//! 4. A prior-assisted frame is **confident** (0.8; 0.9 with all four sides) only when at
//!    least three sides have a significant step (`camera.prior_edge_step`), the four outside
//!    strips are base of one colour and the interior is a photograph. Two sides give a weak
//!    frame (0.55). Fewer leave the single-frame result.

use rayon::prelude::*;
use silvercraft_imgproc::{Plane, stats};

use crate::camera::{CameraFrame, Work, detect_camera_frame, interior_textured, strips_ok};
use crate::config::CameraConfig;
use crate::util::{clean, downsample_rgb};
use crate::{Result, Rgb};

/// Fewest confident frames a roll prior is fitted from.
pub const MIN_PRIOR_FRAMES: usize = 3;

/// What the frames of one roll share: the frame size, and the typical centre and rotation.
/// Sizes and positions are fractions of the capture width and height, so a prior fitted on
/// full captures applies to downsampled copies of them.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RollPrior {
    /// Frame width (the deskewed rectangle's `width`) / capture width.
    pub width: f64,
    /// Frame height / capture height.
    pub height: f64,
    /// Median frame centre / capture width and height.
    pub cx: f64,
    pub cy: f64,
    /// Median frame rotation (degrees).
    pub angle_deg: f64,
    /// Confident frames that agree with the size (the prior was fitted from them).
    pub frames: usize,
}

impl RollPrior {
    fn valid(&self) -> bool {
        [self.width, self.height, self.cx, self.cy, self.angle_deg].iter().all(|v| v.is_finite())
            && (0.05..=1.0).contains(&self.width)
            && (0.05..=1.0).contains(&self.height)
            && (0.0..=1.0).contains(&self.cx)
            && (0.0..=1.0).contains(&self.cy)
            && self.angle_deg.abs() <= 20.0
    }

    /// `frame` (in a `width × height` capture) has the roll's frame size, within
    /// `camera.prior_size_tolerance`.
    pub fn fits(&self, frame: &CameraFrame, width: usize, height: usize, cfg: &CameraConfig) -> bool {
        let Some((fw, fh)) = fractions(frame, width, height) else { return false };
        let tol = cfg.prior_size_tolerance;
        (fw / self.width - 1.0).abs() <= tol && (fh / self.height - 1.0).abs() <= tol
    }
}

/// One capture of a roll.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RollFrame {
    /// The frame (as [`detect_camera_frame`] reports it).
    pub frame: Option<CameraFrame>,
    /// The frame was found with the roll prior.
    pub used_prior: bool,
    /// Sides with a significant edge under the prior (0 when the prior was not used).
    pub sides: u8,
}

/// A whole roll.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RollDetection {
    /// One per capture, in order.
    pub frames: Vec<RollFrame>,
    /// `None`: too few confident frames agreed on a size (every frame is its single result).
    pub prior: Option<RollPrior>,
}

fn fractions(f: &CameraFrame, width: usize, height: usize) -> Option<(f64, f64)> {
    if width == 0 || height == 0 {
        return None;
    }
    let (fw, fh) = (f.rotated.width / width as f64, f.rotated.height / height as f64);
    (fw.is_finite() && fh.is_finite() && fw > 0.0 && fh > 0.0).then_some((fw, fh))
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) })
}

/// A detection is confident: four edges and confidence at least 0.8.
pub fn is_confident(f: &CameraFrame) -> bool {
    f.edges_found && f.confidence >= 0.8
}

/// The roll prior from single-frame detections (each with its capture's width and height):
/// the median size of the confident ones, when at least [`MIN_PRIOR_FRAMES`] and half of them
/// agree with it within `camera.prior_size_tolerance`.
pub fn fit_roll_prior<'a>(frames: impl IntoIterator<Item = (&'a CameraFrame, (usize, usize))>, cfg: &CameraConfig) -> Option<RollPrior> {
    let tol = cfg.prior_size_tolerance;
    if !(tol.is_finite() && tol > 0.0) {
        return None;
    }
    let obs: Vec<(f64, f64, f64, f64, f64)> = frames
        .into_iter()
        .filter(|(f, _)| is_confident(f))
        .filter_map(|(f, (w, h))| {
            let (fw, fh) = fractions(f, w, h)?;
            let (cx, cy) = (f.rotated.cx / w as f64, f.rotated.cy / h as f64);
            [cx, cy, f.angle_deg].iter().all(|v| v.is_finite()).then_some((fw, fh, cx, cy, f.angle_deg))
        })
        .collect();
    let mw = median(&mut obs.iter().map(|o| o.0).collect::<Vec<_>>())?;
    let mh = median(&mut obs.iter().map(|o| o.1).collect::<Vec<_>>())?;
    let inliers: Vec<_> = obs.iter().filter(|o| (o.0 / mw - 1.0).abs() <= tol && (o.1 / mh - 1.0).abs() <= tol).collect();
    if inliers.len() < MIN_PRIOR_FRAMES || inliers.len() * 2 < obs.len() {
        return None;
    }
    let med = |k: fn(&(f64, f64, f64, f64, f64)) -> f64| median(&mut inliers.iter().map(|o| k(o)).collect::<Vec<_>>());
    let prior = RollPrior {
        width: med(|o| o.0)?,
        height: med(|o| o.1)?,
        cx: med(|o| o.2)?,
        cy: med(|o| o.3)?,
        angle_deg: med(|o| o.4)?,
        frames: inliers.len(),
    };
    prior.valid().then_some(prior)
}

/// A single-frame result should be re-detected with the prior: it is not confident, or its
/// size disagrees with the roll's.
pub fn needs_prior(single: Option<&CameraFrame>, width: usize, height: usize, prior: &RollPrior, cfg: &CameraConfig) -> bool {
    !single.is_some_and(|f| is_confident(f) && prior.fits(f, width, height, cfg))
}

/// Detect every frame of a roll: each capture alone, then the roll prior, then the captures
/// that need it again with the prior (see the module docs). With `camera.roll_prior` off, or
/// no prior, every frame is its single-frame result.
pub fn detect_camera_roll(frames: &[Rgb<'_, f32>], cfg: &CameraConfig) -> RollDetection {
    let singles: Vec<Option<CameraFrame>> = frames.par_iter().map(|f| detect_camera_frame(f, cfg).ok().flatten()).collect();
    let alone = |singles: Vec<Option<CameraFrame>>| singles.into_iter().map(|frame| RollFrame { frame, used_prior: false, sides: 0 }).collect();
    if !cfg.roll_prior {
        return RollDetection { frames: alone(singles), prior: None };
    }
    let prior = fit_roll_prior(singles.iter().zip(frames).filter_map(|(s, f)| Some((s.as_ref()?, (f.width(), f.height())))), cfg);
    let Some(prior) = prior else { return RollDetection { frames: alone(singles), prior: None } };
    let out = frames.par_iter().zip(singles).map(|(f, s)| refine(f, cfg, &prior, s)).collect();
    RollDetection { frames: out, prior: Some(prior) }
}

/// One capture of a roll whose prior is known: the single-frame detection, re-detected with
/// the prior when it needs it.
pub fn detect_camera_frame_with_prior(rgb: &Rgb<'_, f32>, cfg: &CameraConfig, prior: &RollPrior) -> Result<RollFrame> {
    let single = detect_camera_frame(rgb, cfg)?;
    Ok(refine(rgb, cfg, prior, single))
}

fn refine(rgb: &Rgb<'_, f32>, cfg: &CameraConfig, prior: &RollPrior, single: Option<CameraFrame>) -> RollFrame {
    let (w, h) = (rgb.width(), rgb.height());
    if !prior.valid() || !needs_prior(single.as_ref(), w, h, prior, cfg) {
        return RollFrame { frame: single, used_prior: false, sides: 0 };
    }
    let hit = prior_search(rgb, cfg, prior);
    let fits = single.as_ref().is_some_and(|f| f.edges_found && prior.fits(f, w, h, cfg));
    match (hit, single) {
        (Some((f, sides)), _) if is_confident(&f) => RollFrame { frame: Some(f), used_prior: true, sides },
        // Four weak edges of the roll's size: as good as two sides and the prior.
        (_, Some(s)) if fits => RollFrame { frame: Some(s), used_prior: false, sides: 0 },
        (Some((f, sides)), _) => RollFrame { frame: Some(f), used_prior: true, sides },
        // Four edges of the wrong size, no better placement: not confident.
        (None, Some(mut s)) if s.edges_found => {
            s.confidence = s.confidence.min(0.55);
            RollFrame { frame: Some(s), used_prior: false, sides: 0 }
        }
        (None, s) => RollFrame { frame: s, used_prior: false, sides: 0 },
    }
}

/// Densities `-ln(v / base)` per channel, the base being the channel's 99th percentile inside
/// the aperture, clamped to `[-0.5, 2.5]` (bare light above, a dark holder below).
fn densities(data: &[Plane; 3], (fx, fy, aw, ah): (usize, usize, usize, usize)) -> Option<[Plane; 3]> {
    let mut out = Vec::with_capacity(3);
    for p in data {
        let step = ((aw * ah) / 200_000).max(1);
        let v: Vec<f32> = (0..aw * ah).step_by(step).map(|i| p.get(fx + i % aw, fy + i / aw)).collect();
        let base = stats::percentile(&v, 99.0).filter(|b| b.is_finite() && *b > 1e-6)?;
        let (lo, hi) = ((-2.5f32).exp(), 0.5f32.exp());
        out.push(p.map(|x| -(clean(x) / base).clamp(lo, hi).ln()));
    }
    out.try_into().ok()
}

/// Step profiles across a set of lines: for each position `i` along `n`, the 40th percentile
/// over the lines of the density step `D(i + 2) - D(i - 2)` (rising: the dense side after) and
/// of its negative (falling), the larger over the channels.
fn step_profiles(d: &[Plane; 3], n: usize, lines: std::ops::Range<usize>, at: impl Fn(&Plane, usize, usize) -> f32) -> (Vec<f32>, Vec<f32>) {
    let mut rise = vec![f32::MIN; n];
    let mut fall = vec![f32::MIN; n];
    let mut buf = Vec::with_capacity(lines.len());
    for p in d {
        for i in 2..n.saturating_sub(2) {
            buf.clear();
            buf.extend(lines.clone().map(|l| at(p, i + 2, l) - at(p, i - 2, l)));
            if buf.is_empty() {
                continue;
            }
            buf.sort_unstable_by(f32::total_cmp);
            let q = |f: f64| buf[((f * (buf.len() - 1) as f64).round() as usize).min(buf.len() - 1)];
            rise[i] = rise[i].max(q(0.4));
            fall[i] = fall[i].max(-q(0.6));
        }
    }
    (rise, fall)
}

/// Where a span of fixed length goes along one axis.
#[derive(Debug, Clone, Copy)]
struct Placed {
    start: usize,
    end: usize,
    lo: bool,
    hi: bool,
    score: f32,
}

/// Place a span of `len` (± `slack`) with its low edge on a `lo` step and its high edge on a
/// `hi` step, inside `[margin, n - margin)`. A side counts when its step reaches `thr` and the
/// placement is unambiguous (no placement more than `sep` away scores within `thr / 2`). An
/// axis with no side found goes to `prior` (the typical start).
#[allow(clippy::too_many_arguments)]
fn place(lo: &[f32], hi: &[f32], len: usize, slack: usize, prior: f64, thr: f32, sep: usize, margin: (usize, usize)) -> Option<Placed> {
    let n = lo.len().min(hi.len());
    let (m0, m1) = margin;
    let last = n.checked_sub(m1)?;
    let mut cands: Vec<(usize, usize, f32)> = Vec::new();
    for p in m0..last {
        let mut best: Option<(usize, f32)> = None;
        for q in (p + len).saturating_sub(slack)..=(p + len + slack) {
            if q >= last {
                break;
            }
            let s = lo[p] + hi[q];
            if best.is_none_or(|b| s > b.1) {
                best = Some((q, s));
            }
        }
        if let Some((q, s)) = best {
            cands.push((p, q, s));
        }
    }
    let dist = |p: usize| (p as f64 - prior).abs();
    let &(p, q, s) = cands.iter().max_by(|a, b| a.2.total_cmp(&b.2).then(dist(b.0).total_cmp(&dist(a.0))))?;
    let alt = cands.iter().filter(|c| c.0.abs_diff(p) > sep).map(|c| c.2).fold(f32::MIN, f32::max);
    let clear = s - alt >= thr / 2.0;
    let (lo_ok, hi_ok) = (clear && lo[p] >= thr, clear && hi[q] >= thr);
    let fit = |start: usize| {
        let s = start.clamp(m0, last.saturating_sub(1 + len).max(m0));
        (s, (s + len).min(last.saturating_sub(1)))
    };
    let (start, end) = match (lo_ok, hi_ok) {
        (true, true) => (p, q),
        (true, false) => fit(p),
        (false, true) => fit(q.saturating_sub(len)),
        (false, false) => fit(prior.round().max(0.0) as usize),
    };
    (end > start).then_some(Placed { start, end, lo: lo_ok, hi: hi_ok, score: s })
}

/// The frame placed with the prior's size, and its count of supported sides.
fn prior_search(rgb: &Rgb<'_, f32>, cfg: &CameraConfig, prior: &RollPrior) -> Option<(CameraFrame, u8)> {
    let (out_w, out_h) = (rgb.width(), rgb.height());
    let factor = out_w.max(out_h).div_ceil(cfg.work_size.clamp(64, 8192)).max(1);
    let planes = downsample_rgb(rgb, factor, clean).ok()?;
    let work = Work::new(&planes, cfg)?;
    let scale = factor as f64;
    let max_skew = cfg.max_skew_deg.clamp(0.0, 20.0);
    let own = if cfg.deskew { work.skew(cfg).unwrap_or(0.0) } else { 0.0 };
    let mut angles = vec![prior.angle_deg.clamp(-max_skew, max_skew)];
    if (own - angles[0]).abs() > 0.05 {
        angles.push(own);
    }
    // A side must show a real step: a zero or negative threshold would count noise.
    let thr = cfg.prior_edge_step as f32;
    if !(thr.is_finite() && thr > 0.0) {
        return None;
    }
    let (fw, fh) = (prior.width * out_w as f64 / scale, prior.height * out_h as f64 / scale);
    let (bw, bh) = (fw.round() as usize, fh.round() as usize);
    let mut best: Option<Placement> = None;
    for angle in angles {
        let angle = if angle.abs() > 0.05 { angle } else { 0.0 };
        let (gray, data) = if angle != 0.0 { work.rotated(angle) } else { (work.gray.clone(), work.data.clone()) };
        let Some(aperture) = work.aperture(&gray) else { continue };
        let data = work.film_normalised(&data, aperture);
        let (fx, fy, aw, ah) = aperture;
        if bw < 16 || bh < 16 || bw + 8 > aw || bh + 8 > ah {
            continue;
        }
        let Some(d) = densities(&data, aperture) else { continue };
        // The typical top-left corner in this (upright) working image, relative to the aperture.
        let (cx, cy) = (work.w as f64 / 2.0, work.h as f64 / 2.0);
        let (ox, oy) = (prior.cx * out_w as f64 / scale - cx, prior.cy * out_h as f64 / scale - cy);
        let (s, c) = angle.to_radians().sin_cos();
        let (px, py) = (cx + ox * c + oy * s, cy - ox * s + oy * c);
        let (px0, py0) = (px - fw / 2.0 - fx as f64, py - fh / 2.0 - fy as f64);
        let slack_x = ((fw * 0.004).round() as usize).max(1);
        let slack_y = ((fh * 0.004).round() as usize).max(1);
        let sep = work.scale_px(3.0);
        let at_x = |p: &Plane, x: usize, y: usize| p.get(fx + x, fy + y);
        let at_y = |p: &Plane, y: usize, x: usize| p.get(fx + x, fy + y);
        // The middle 80 % of a span (the corners are rebate or the next frame).
        let inner = |a: usize, b: usize| (a + (b - a) / 10)..(b - (b - a) / 10);
        // Alternate: columns across the typical rows give x; rows across the found columns
        // give y; once more across the found rows and columns.
        let start = |a: f64, len: usize, n: usize| a.round().clamp(0.0, n.saturating_sub(len) as f64) as usize;
        let ry = start(py0, bh, ah);
        let mut rows = inner(ry, ry + bh);
        let mut placed = None;
        for _ in 0..2 {
            let (l, r) = step_profiles(&d, aw, rows.clone(), at_x);
            let Some(x) = place(&l, &r, bw, slack_x, px0, thr, sep, (3, 4)) else { break };
            let (t, b) = step_profiles(&d, ah, inner(x.start, x.end), at_y);
            let Some(y) = place(&t, &b, bh, slack_y, py0, thr, sep, (3, 4)) else { break };
            rows = inner(y.start, y.end);
            placed = Some((x, y));
        }
        let Some((x, y)) = placed else { continue };
        let p = Placement {
            rect: (fx + x.start, fy + y.start, x.end - x.start, y.end - y.start),
            angle,
            sides: [x.lo, x.hi, y.lo, y.hi],
            score: x.score + y.score,
            data,
        };
        if best.as_ref().is_none_or(|b| (p.count(), p.score) > (b.count(), b.score)) {
            best = Some(p);
        }
    }
    let p = best?;
    let n = p.count();
    if n < 2 {
        return None;
    }
    // The outside strips are one flat base, checked a little further out than single-frame
    // detection does: a faint frame edge is soft, and the placement can sit a pixel inside it.
    // The inside of a side need not be textured: the density step across it already shows
    // image, not gate or rebate (on a thin negative it is often a pale, even wash).
    let (x, y, bw, bh) = p.rect;
    if !strips_ok(&p.data, (0, 0, work.w, work.h), (x, x + bw, y, y + bh), None, work.scale_px(5.0), cfg) {
        return None;
    }
    let textured = interior_textured(&p.data, p.rect);
    let confidence = match n {
        4 if textured => 0.9,
        3 if textured => 0.8,
        _ => 0.55,
    };
    let f = work.output(p.rect, p.angle, scale, (out_w, out_h), confidence, true, f32::from(n) / 4.0)?;
    Some((f, n))
}

/// A frame of the prior's size placed in one rotated working image.
struct Placement {
    /// `(x, y, w, h)` in the working image.
    rect: (usize, usize, usize, usize),
    angle: f64,
    /// Left, right, top, bottom found.
    sides: [bool; 4],
    score: f32,
    /// Film-normalised planes at `angle`.
    data: [Plane; 3],
}

impl Placement {
    fn count(&self) -> u8 {
        self.sides.iter().map(|&s| u8::from(s)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A strong low edge and a faint high edge one span apart are placed together; the faint
    /// one alone would not stand out from the noise.
    #[test]
    fn a_faint_edge_is_found_with_its_partner() {
        let n = 200;
        let mut lo = vec![0.0f32; n];
        let mut hi = vec![0.0f32; n];
        for (i, v) in hi.iter_mut().enumerate() {
            *v = 0.02 * ((i * 7919) % 13) as f32 / 13.0; // noise up to 0.02
        }
        lo[40] = 0.3;
        hi[140] = 0.03;
        let p = place(&lo, &hi, 100, 1, 50.0, 0.025, 3, (3, 4)).unwrap();
        assert_eq!((p.start, p.end, p.lo, p.hi), (40, 140, true, true), "{p:?}");
        // Below the step threshold the faint side does not count, but the span still follows
        // the strong side.
        hi[140] = 0.01;
        let p = place(&lo, &hi, 100, 1, 50.0, 0.025, 3, (3, 4)).unwrap();
        assert_eq!((p.start, p.end, p.lo, p.hi), (40, 140, true, false), "{p:?}");
    }

    /// Two equally good placements far apart: neither side counts, and the span goes to the
    /// typical position.
    #[test]
    fn ambiguous_placements_count_no_side() {
        let n = 300;
        let mut lo = vec![0.0f32; n];
        let hi = vec![0.0f32; n];
        lo[40] = 0.2;
        lo[120] = 0.2;
        let p = place(&lo, &hi, 100, 1, 80.0, 0.025, 3, (3, 4)).unwrap();
        assert!(!p.lo && !p.hi, "{p:?}");
        assert_eq!((p.start, p.end), (80, 180));
        // Nothing to place in: no panic, no span.
        assert!(place(&[0.0; 5], &[0.0; 5], 100, 1, 0.0, 0.025, 3, (3, 4)).is_none());
        assert!(place(&lo, &hi, 1000, 1, 0.0, 0.025, 3, (3, 4)).is_none());
    }
}
