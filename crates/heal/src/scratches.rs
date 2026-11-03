//! Visible-light repair of long, thin scratches along the film strip ("rain").
//!
//! Winding and processing leave fine straight scratches that run along the strip, often through
//! whole frames. They are on the film, so the infrared pass does not see them (it sees only
//! occluders on the surface), and they are usually far fainter per pixel than the grain: a typical
//! rain line on a V850 scan of XP2 is 2-3 px wide and changes density by 0.3-1 %, against a grain
//! of about 1 % per pixel. What gives them away is their length.
//!
//! Unlike scanner streaks (`silvercraft-scanfix`, column-locked, constant contrast) they move with
//! the film: they are registered to the image, may be slanted by a degree or two or gently
//! curved, have gaps, and vary in contrast. Everything here works on the luma **density**
//! (`-ln` transmittance) in a canonical orientation where the strip runs top to bottom; a
//! horizontal strip is handled by swapping the axes.
//!
//! # Detection
//!
//! 1. **Ridge.** Each pixel's density is compared with the mean of its neighbours a few pixels
//!    to either side across the strip: the centre (a small triangular window) minus the mean of
//!    the two side windows. A narrow line gives its full contrast; a step edge at most half its
//!    height, beside it (the boundary guard below rejects those). The response is clipped at
//!    three robust sigmas so dust and strong image edges cannot dominate.
//! 2. **Band projections.** The frame is cut into bands along the strip (64 px at 600 dpi). In
//!    each band the ridge is averaged along every slant within `scratch_max_angle` (a per-band
//!    Hough projection: one sample per pixel of shift across the band). Grain averages away; a
//!    line keeps its contrast. Each band's profile is normalised by the robust spread of its
//!    neighbourhood across the strip, so busy image texture weighs less than flat image.
//! 3. **Linking.** A dynamic programme links the bands into paths: a path continues from one band
//!    into the next at the position its slant predicts, may change slant by one step at a cost,
//!    and pays a fixed toll per band, so grain drifts down while a line that persists climbs.
//!    Paths can start and stop anywhere (gaps cost a little and are bridged). The best paths are
//!    extracted greedily, each claiming its neighbourhood, in a few rounds.
//!
//! # Content rejection
//!
//! Rows alternate in short blocks between finding and measuring: the paths are linked on one set
//! and measured on the other, so a path that grain happened to line up on the finding rows has no
//! reason to look like a line on the measuring rows. On those, per band, the median cross-profile
//! along the path (row means removed) gives the line's contrast, its side step and the activity
//! beside it. A path is kept only when it is
//! - **long**: at least `scratch_length`;
//! - **consistent**: its mean contrast over the bands is significant (Student's t at about the
//!   one-sided 1e-4 point for its number of bands), so its polarity holds along its length.
//!   `scratch_sensitivity` divides this threshold and the linking score: above 1 it finds fainter
//!   scratches and lets some grain alignments through (harmless, sub-percent corrections);
//! - **narrow**: the half-maximum width of the mean profile is at most `scratch_max_width`;
//! - **not a boundary**: the two sides match. Each band's step between its sides (out to a dozen
//!   pixels) is fitted together with the background slope (a gradient is not a step); a pole or
//!   wire between two different backgrounds, the edge of a building, a tone boundary, the frame
//!   edge and a fringe beside it step consistently;
//! - **not image-borne**: it also crosses flat image (side activity near the grain's) for a
//!   quarter of its length; railings, slats and water ripples are lines only inside busy
//!   structure;
//! - **long and steady if strong**: a line visible pixel by pixel (contrast above the pixel noise)
//!   must span three quarters of the strip with a steady contrast. Winding scratches that strong
//!   run through the frame; thin image lines that strong (a pole, the lip of a pool) rarely do,
//!   and their contrast follows their changing backgrounds.
//!
//! # Repair
//!
//! Only the scratch and a one-pixel halo are written. A faint scratch is **corrected**: its
//! density offset against the straight line between its two sides is averaged along its length
//! and divided out of the transmittance, so the grain underneath stays. A strong scratch (or
//! with `scratch_repair = "interpolate"`) is **interpolated** across, from both sides along the
//! perpendicular, with multiplicative grain matched to the sides. Both use one factor per pixel
//! for all channels, so no colour is introduced.

use rayon::prelude::*;
use silvercraft_imgproc::Mask;

use crate::config::{HealConfig, ScratchAxis, ScratchRepair};
use crate::util;

/// Per-band score cost of continuing a path (grain has a negative drift).
const TOLL: f32 = 0.3;
/// Cost of changing slant by one step between bands.
const BEND: f32 = 3.0;
/// Band z-scores are clipped to this (one band cannot carry a path).
const Z_CLIP: f32 = 4.0;
/// Path score needed at sensitivity 1. Chance alignments of grain and image texture reach it
/// often; the content guards below are what keep them out.
const SCORE: f32 = 8.0;
/// Largest slant search either way, in one-pixel steps across a band.
const MAX_SLOPES: usize = 32;
/// A path's end bands carry at least this z on the finding rows.
const END_Z: f32 = 1.0;
/// Extraction rounds (each round claims the paths it found and relinks the rest).
const ROUNDS: usize = 6;
/// At most this many candidate paths are examined.
const MAX_CANDIDATES: usize = 600;
/// The sides of a scratch may differ by at most this fraction of its contrast.
const STEP_RATIO: f32 = 0.5;
/// ...unless the step is not significant: within this many standard errors (from its band-to-band
/// spread) of zero.
const STEP_T: f32 = 3.0;
/// A scratch's mean contrast (on the measuring rows) is at least `CONSISTENCY + CONSISTENCY_SMALL
/// / (bands - 1)` standard errors from zero: about the one-sided 1e-4 point of Student's t, so a
/// line picked out of grain rarely passes however many candidates there are.
const CONSISTENCY: f32 = 4.0;
const CONSISTENCY_SMALL: f32 = 16.0;
/// A band is calm (flat image) when its activity is at most this times the frame's typical one
/// (or that of grain alone, whichever is lower).
const CALM: f32 = 1.5;
/// A line whose contrast is at least this many pixel sigmas is strong...
const STRONG: f32 = 1.0;
/// ...and must span at least this fraction of the strip, with a band-to-band contrast spread of
/// at most this fraction of its contrast (an image line's contrast follows its backgrounds).
const STRONG_LENGTH: f32 = 0.75;
const STRONG_VARIATION: f32 = 0.5;
/// `scratch_axis = "auto"` takes the shorter side only with at least this many scratches...
const AUTO_LINES: usize = 3;
/// ...scoring at least this many times the longer side's.
const AUTO_RATIO: f32 = 2.0;
/// A line in auto mode is interpolated when its contrast is at least this many pixel sigmas.
const INTERPOLATE_SIGMA: f32 = 3.0;

/// Orientation of a scratch in the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAxis {
    /// Runs top to bottom.
    Vertical,
    /// Runs left to right.
    Horizontal,
}

/// How a scratch was repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRepair {
    /// Its smoothed attenuation was divided out (grain kept).
    Corrected,
    /// Interpolated across from both sides, with matched grain.
    Interpolated,
}

/// One long scratch.
#[derive(Debug, Clone, PartialEq)]
pub struct LineScratch {
    pub axis: LineAxis,
    /// First pixel along the strip (a row for [`LineAxis::Vertical`]).
    pub start: usize,
    /// One past the last pixel along the strip.
    pub end: usize,
    /// Centre line as `[along, across]` knots (pixels), one per band.
    pub knots: Vec<[f32; 2]>,
    /// Half-maximum width, pixels.
    pub width: f32,
    /// Median density contrast against its sides: positive = denser than its surroundings on the
    /// negative (lighter in the positive), negative = more transparent (darker in the positive).
    pub contrast: f32,
    /// Path score (sum of band z-scores less the per-band toll).
    pub score: f32,
    pub repair: LineRepair,
    /// Bounding box `[x0, y0, x1, y1)` of the written pixels.
    pub bbox: [usize; 4],
    /// Pixels written.
    pub pixels: usize,
    /// Pixels written inside the scratch itself (without the halo).
    pub core_pixels: usize,
}

/// The outcome of [`repair`].
#[derive(Debug, Clone)]
pub struct Outcome {
    pub lines: Vec<LineScratch>,
    /// Every pixel written.
    pub mask: Mask,
    pub warnings: Vec<String>,
}

/// Canonical view: `u` across the strip (`0..w`), `v` along it (`0..h`).
#[derive(Debug, Clone, Copy)]
struct Canon {
    axis: LineAxis,
    w: usize,
    h: usize,
    img_w: usize,
}

impl Canon {
    fn new(axis: LineAxis, img_w: usize, img_h: usize) -> Self {
        match axis {
            LineAxis::Vertical => Self { axis, w: img_w, h: img_h, img_w },
            LineAxis::Horizontal => Self { axis, w: img_h, h: img_w, img_w },
        }
    }

    fn index(&self, u: usize, v: usize) -> usize {
        match self.axis {
            LineAxis::Vertical => v * self.img_w + u,
            LineAxis::Horizontal => u * self.img_w + v,
        }
    }
}

/// Filter geometry, pixels.
#[derive(Debug, Clone, Copy)]
struct Geo {
    /// Centre half-width (triangular window `-hc..=hc`).
    hc: usize,
    /// Distance of the side windows from the centre.
    gap: usize,
    /// Side window width.
    side: usize,
    band: usize,
    /// Slant steps either way (one pixel of shift across a band each).
    slopes: usize,
    max_width: f32,
    /// Neighbourhood a found path claims, either side.
    claim: usize,
    /// Half-width of the measured cross-profile.
    reach: usize,
    /// Row block of the find/measure split.
    block: usize,
}

impl Geo {
    fn new(cfg: &HealConfig, scale: f32) -> Self {
        let max_width = (cfg.scratch_max_width as f32 * scale).max(1.0);
        let hc = ((max_width * 0.25).round() as usize).clamp(1, 4);
        let gap = hc + 2;
        let side = ((max_width * 0.8).round() as usize).clamp(3, 12);
        let angle = (cfg.scratch_max_angle as f32).clamp(0.0, 10.0).to_radians();
        // bands short enough that the slant search stays at most 2·MAX_SLOPES + 1 projections
        let mut band = ((64.0 * scale).round() as usize).clamp(16, 512);
        if angle > 0.0 {
            band = band.min(((MAX_SLOPES as f32 / angle.tan()).floor() as usize).max(16));
        }
        let slopes = ((band as f32 * angle.tan()).ceil() as usize).min(MAX_SLOPES);
        // narrower than the side lobes (which sit a gap away), so a lobe cannot claim its line
        let claim = hc + 1;
        let block = ((4.0 * scale).round() as usize).clamp(2, 64);
        Self { hc, gap, side, band, slopes, max_width, claim, reach: gap + side + 4, block }
    }

    /// Rows alternate in blocks between finding paths and measuring them, so the measurements
    /// (and every guard on them) are independent of the selection: a path picked out of grain
    /// does not look consistent on the rows it was not picked from.
    fn finds(&self, v: usize) -> bool {
        (v / self.block).is_multiple_of(2)
    }

    /// Pixels either side the ridge needs.
    fn span(&self) -> usize {
        self.gap + self.side - 1
    }
}

/// Luma density in canonical order; NaN where the pixel is unusable.
fn density(data: &[f32], never: &Mask, c: &Canon) -> Vec<f32> {
    let mut out = vec![f32::NAN; c.w * c.h];
    out.par_chunks_mut(c.w.max(1)).enumerate().for_each(|(v, row)| {
        for (u, o) in row.iter_mut().enumerate() {
            let i = c.index(u, v);
            if never.data().get(i).copied().unwrap_or(true) {
                continue;
            }
            let Some(p) = data.get(3 * i..3 * i + 3) else {
                continue;
            };
            let l = (p[0] + p[1] + p[2]) / 3.0;
            if l.is_finite() && l > 1e-6 {
                *o = -l.ln();
            }
        }
    });
    out
}

/// Ridge response across the strip: triangular centre mean minus the mean of the two side
/// windows. NaN where any tap is unusable.
fn ridge(d: &[f32], w: usize, g: &Geo) -> Vec<f32> {
    let span = g.span();
    let mut out = vec![f32::NAN; d.len()];
    if w <= 2 * span {
        return out;
    }
    let hc = g.hc as isize;
    let cw: f32 = (-hc..=hc).map(|o| (hc + 1 - o.abs()) as f32).sum();
    out.par_chunks_mut(w).zip(d.par_chunks(w)).for_each(|(o, row)| {
        for u in span..w - span {
            let mut centre = 0.0f32;
            for k in -hc..=hc {
                centre += (hc + 1 - k.abs()) as f32 * row[(u as isize + k) as usize];
            }
            let mut sides = 0.0f32;
            for k in g.gap..g.gap + g.side {
                sides += row[u - k] + row[u + k];
            }
            let r = centre / cw - sides / (2 * g.side) as f32;
            if r.is_finite() {
                o[u] = r;
            }
        }
    });
    out
}

/// Per-band slant projections of the clipped ridge, as z-scores `[band][slope][u]`.
fn project(r: &[f32], w: usize, h: usize, g: &Geo, sigma: f32) -> (Vec<f32>, usize, usize) {
    let nb = h / g.band;
    let ns = 2 * g.slopes + 1;
    let clip = 3.0 * sigma;
    let mut z = vec![0.0f32; nb * ns * w];
    if nb == 0 || w == 0 {
        return (z, 0, ns);
    }
    z.par_chunks_mut(ns * w).enumerate().for_each(|(b, zb)| {
        let mut acc = vec![0.0f32; w];
        let mut cnt = vec![0.0f32; w];
        let mid = (g.band as f32 - 1.0) / 2.0;
        for si in 0..ns {
            let s = si as f32 - g.slopes as f32;
            acc.iter_mut().for_each(|a| *a = 0.0);
            cnt.iter_mut().for_each(|a| *a = 0.0);
            for rr in 0..g.band {
                let v = b * g.band + rr;
                if !g.finds(v) {
                    continue;
                }
                let row = &r[v * w..(v + 1) * w];
                let off = s * (rr as f32 - mid) / g.band as f32;
                let fl = off.floor();
                let fr = off - fl;
                let fl = fl as isize;
                let lo = (-fl).max(0) as usize;
                let hi = (w as isize - 1 - fl).clamp(0, w as isize) as usize;
                for x in lo..hi {
                    let x0 = (x as isize + fl) as usize;
                    let (a, bb) = (row[x0], row[x0 + 1]);
                    if a.is_finite() {
                        acc[x] += (1.0 - fr) * a.clamp(-clip, clip);
                        cnt[x] += 1.0 - fr;
                    }
                    if bb.is_finite() {
                        acc[x] += fr * bb.clamp(-clip, clip);
                        cnt[x] += fr;
                    }
                }
            }
            let zr = &mut zb[si * w..(si + 1) * w];
            for x in 0..w {
                zr[x] = if cnt[x] >= g.band as f32 * 0.25 { acc[x] / cnt[x] } else { f32::NAN };
            }
        }
        // local spread: robust sigma per tile across the strip (all slants), interpolated
        let tile = g.band.min(w).max(1);
        let tiles = w.div_ceil(tile);
        let global = util::robust_stats(zb, |_| true, 20_000).map_or(0.0, |(_, s)| s);
        let spread: Vec<f32> = (0..tiles)
            .map(|t| {
                let (a, e) = (t * tile, ((t + 1) * tile).min(w));
                let vals: Vec<f32> = (0..ns).flat_map(|si| zb[si * w + a..si * w + e].iter().copied()).collect();
                match util::median_sigma(vals) {
                    Some((_, s)) if s.is_finite() && s > 0.0 => s.max(0.5 * global),
                    _ => global,
                }
            })
            .collect();
        let at = |x: usize| -> f32 {
            let f = (x as f32 + 0.5) / tile as f32 - 0.5;
            let i = (f.floor().max(0.0) as usize).min(tiles - 1);
            let j = (i + 1).min(tiles - 1);
            let t = (f - i as f32).clamp(0.0, 1.0);
            spread[i] * (1.0 - t) + spread[j] * t
        };
        for si in 0..ns {
            for x in 0..w {
                let v = &mut zb[si * w + x];
                let sp = at(x);
                *v = if v.is_finite() && sp > 0.0 { (*v / sp).clamp(-Z_CLIP, Z_CLIP) } else { 0.0 };
            }
        }
    });
    (z, nb, ns)
}

/// Best path score ending at every cell, and the previous cell (`u32::MAX` = path start).
fn link(z: &[f32], nb: usize, ns: usize, w: usize, slopes: usize, pol: f32) -> (Vec<f32>, Vec<u32>) {
    let n = ns * w;
    let mut score = vec![0.0f32; nb * n];
    let mut back = vec![u32::MAX; nb * n];
    for b in 0..nb {
        let (done, rest) = score.split_at_mut(b * n);
        let prev = if b > 0 { Some(&done[(b - 1) * n..]) } else { None };
        let cur = &mut rest[..n];
        let bk = &mut back[b * n..(b + 1) * n];
        let zb = &z[b * n..(b + 1) * n];
        cur.par_chunks_mut(w).zip(bk.par_chunks_mut(w)).enumerate().for_each(|(si, (row, brow))| {
            for x in 0..w {
                let own = pol * zb[si * w + x] - TOLL;
                let mut best = 0.0f32;
                let mut arg = u32::MAX;
                if let Some(prev) = prev {
                    for ds in -1isize..=1 {
                        let sp = si as isize + ds;
                        if sp < 0 || sp >= ns as isize {
                            continue;
                        }
                        let px = x as isize - (sp - slopes as isize);
                        if px < 0 || px >= w as isize {
                            continue;
                        }
                        let j = sp as usize * w + px as usize;
                        let cand = prev[j] - BEND * ds.unsigned_abs() as f32;
                        if cand > best {
                            best = cand;
                            arg = j as u32;
                        }
                    }
                }
                row[x] = own + best;
                brow[x] = arg;
            }
        });
    }
    (score, back)
}

/// A linked path: `(band, slope index, u)` per band, ascending.
#[derive(Debug, Clone)]
struct Candidate {
    pol: f32,
    score: f32,
    path: Vec<(usize, usize, usize)>,
}

fn candidates(z: &mut [f32], nb: usize, ns: usize, w: usize, g: &Geo, threshold: f32) -> Vec<Candidate> {
    let n = ns * w;
    if nb.saturating_mul(n) >= u32::MAX as usize {
        return Vec::new(); // cells are addressed with u32
    }
    let mut claimed = vec![false; nb * w];
    let mut out: Vec<Candidate> = Vec::new();
    for _ in 0..ROUNDS {
        let ((sp, bp), (sn, bn)) = rayon::join(|| link(z, nb, ns, w, g.slopes, 1.0), || link(z, nb, ns, w, g.slopes, -1.0));
        let mut cells: Vec<(f32, u8, u32)> = Vec::new();
        for (k, (s, _)) in [(&sp, &bp), (&sn, &bn)].iter().enumerate() {
            cells.extend(s.iter().enumerate().filter(|(_, v)| **v >= threshold).map(|(i, v)| (*v, k as u8, i as u32)));
        }
        cells.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let mut found = 0;
        for (sc, k, cell) in cells {
            if out.len() >= MAX_CANDIDATES {
                break;
            }
            let (b, rem) = (cell as usize / n, cell as usize % n);
            if claimed[b * w + rem % w] {
                continue;
            }
            let back = if k == 0 { &bp } else { &bn };
            let mut path = vec![(b, rem / w, rem % w)];
            let mut at = cell as usize;
            let mut clash = false;
            while let Some(&p) = back.get(at) {
                if p == u32::MAX || path.len() > nb {
                    break;
                }
                let bb = path.last().map_or(0, |l| l.0).saturating_sub(1);
                let r = p as usize % n;
                if claimed[bb * w + r % w] {
                    clash = true;
                    break;
                }
                path.push((bb, r / w, r % w));
                at = bb * n + r;
            }
            path.reverse();
            let pol = if k == 0 { 1.0 } else { -1.0 };
            // a path that ran into a claimed line keeps only its own part, rescored
            let sc = if clash {
                let mut t = 0.0f32;
                for (j, &(bb, si, u)) in path.iter().enumerate() {
                    t += pol * z[bb * n + si * w + u] - TOLL;
                    if j > 0 && path[j - 1].1 != si {
                        t -= BEND;
                    }
                }
                t
            } else {
                sc
            };
            if sc < threshold {
                continue;
            }
            for &(bb, _, u) in &path {
                for x in u.saturating_sub(g.claim)..(u + g.claim + 1).min(w) {
                    claimed[bb * w + x] = true;
                }
            }
            out.push(Candidate { pol, score: sc, path });
            found += 1;
        }
        if found == 0 || out.len() >= MAX_CANDIDATES {
            break;
        }
        // relink the rest without the claimed neighbourhoods
        for b in 0..nb {
            for si in 0..ns {
                for x in 0..w {
                    if claimed[b * w + x] {
                        z[b * n + si * w + x] = 0.0;
                    }
                }
            }
        }
    }
    out
}

/// A candidate measured on the density and accepted.
#[derive(Debug, Clone)]
struct Line {
    score: f32,
    /// Canonical rows `v0..v1`.
    v0: usize,
    v1: usize,
    /// Centre `u` for every row of `v0..v1`.
    centre: Vec<f32>,
    knots: Vec<[f32; 2]>,
    width: f32,
    contrast: f32,
    /// Peak contrast over the pixel noise.
    snr: f32,
}

/// Centre positions for rows `v0..v1` through band-centre knots `(v, u)`, extrapolated with the
/// end slopes.
fn centres(knots: &[(f32, f32)], s_first: f32, s_last: f32, v0: usize, v1: usize) -> Vec<f32> {
    let (Some(&first), Some(&last)) = (knots.first(), knots.last()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(v1.saturating_sub(v0));
    let mut k = 0;
    for v in v0..v1 {
        let vf = v as f32;
        let u = if vf <= first.0 {
            first.1 + s_first * (vf - first.0)
        } else if vf >= last.0 {
            last.1 + s_last * (vf - last.0)
        } else {
            while k + 1 < knots.len() && knots[k + 1].0 < vf {
                k += 1;
            }
            let (a, b) = (knots[k], knots[(k + 1).min(knots.len() - 1)]);
            if b.0 > a.0 { a.1 + (b.1 - a.1) * (vf - a.0) / (b.0 - a.0) } else { a.1 }
        };
        out.push(u);
    }
    out
}

/// Median cross-profile (row means removed) of rows `rows` along `centre` (indexed from `v0`),
/// with the mean side activity. `None` when too few rows are usable.
fn band_profile(d: &[f32], w: usize, rows: std::ops::Range<usize>, v0: usize, centre: &[f32], g: &Geo) -> Option<(Vec<f32>, f32)> {
    let reach = g.reach;
    let len = 2 * reach + 1;
    let mut cols: Vec<Vec<f32>> = vec![Vec::new(); len];
    let mut activity = 0.0f32;
    let mut used = 0usize;
    let total = rows.len() / 2;
    for v in rows.filter(|&v| !g.finds(v)) {
        let Some(&cu) = centre.get(v.checked_sub(v0)?) else {
            continue;
        };
        let ur = cu.round();
        if !(ur >= reach as f32 && ur + (reach as f32) < w as f32) {
            continue;
        }
        let ur = ur as usize;
        let win = &d[v * w + ur - reach..v * w + ur + reach + 1];
        if !win.iter().all(|x| x.is_finite()) {
            continue;
        }
        let mean = win.iter().sum::<f32>() / len as f32;
        for (c, &x) in cols.iter_mut().zip(win) {
            c.push(x - mean);
        }
        activity += side_activity(win, g.gap);
        used += 1;
    }
    if used * 4 < total || used < 4 {
        return None;
    }
    let prof: Option<Vec<f32>> = cols.into_iter().map(util::median).collect();
    Some((prof?, activity / used as f32))
}

/// Mean of `p[reach + o]` for `o` in `range`.
fn mean_at(p: &[f32], reach: usize, range: std::ops::RangeInclusive<isize>) -> f32 {
    let (mut s, mut n) = (0.0f32, 0.0f32);
    for o in range {
        if let Some(&v) = p.get((reach as isize + o) as usize) {
            s += v;
            n += 1.0;
        }
    }
    if n > 0.0 { s / n } else { f32::NAN }
}

/// Step between the two sides of a cross-profile `p` (indexed `reach + offset`): the jump `s` of
/// the least-squares fit `a + b·o + s·[o > 0]` over the offsets at least `gap` from the centre, so
/// a background gradient is not a step.
fn side_step(p: &[f32], reach: usize, gap: usize) -> f32 {
    let mut m = [[0.0f64; 3]; 3];
    let mut r = [0.0f64; 3];
    for (i, &v) in p.iter().enumerate() {
        let o = i as f64 - reach as f64;
        if o.abs() < gap as f64 || !v.is_finite() {
            continue;
        }
        let x = [1.0, o, if o > 0.0 { 1.0 } else { 0.0 }];
        for a in 0..3 {
            for b in 0..3 {
                m[a][b] += x[a] * x[b];
            }
            r[a] += x[a] * f64::from(v);
        }
    }
    util::solve3(m, r).map_or(f32::INFINITY, |s| s[2] as f32)
}

/// Student's t of the mean of `v` (outliers beyond four robust sigmas clipped first); 0 for fewer
/// than three values.
fn consistency(v: &[f32]) -> f32 {
    let Some((m, s)) = util::median_sigma(v.to_vec()) else {
        return 0.0;
    };
    let lim = 4.0 * s.max(1e-9);
    let x: Vec<f64> = v.iter().filter(|x| x.is_finite()).map(|&x| f64::from(x.clamp(m - lim, m + lim))).collect();
    let n = x.len() as f64;
    if x.len() < 3 {
        return 0.0;
    }
    let mean = x.iter().sum::<f64>() / n;
    let var = x.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1.0);
    (mean / (var / n).sqrt().max(1e-12)) as f32
}

/// Signed median of finite values.
fn med(v: impl Iterator<Item = f32>) -> f32 {
    util::median(v.collect()).unwrap_or(f32::NAN)
}

/// Measure a candidate on the density and apply the content guards.
fn accept(
    cand: &Candidate,
    d: &[f32],
    c: &Canon,
    g: &Geo,
    nb: usize,
    z: &[f32],
    ns: usize,
    min_len: f32,
    pixel_sigma: f32,
    activity: f32,
    sensitivity: f32,
) -> Option<Line> {
    let (w, h) = (c.w, c.h);
    let pol = cand.pol;
    let bandf = g.band as f32;
    // trim the ends where the finding rows do not clearly carry the line (the toll lets a path
    // run on through a few grain bands)
    let carried = |&(b, si, u): &(usize, usize, usize)| pol * z.get(b * ns * w + si * w + u).copied().unwrap_or(0.0) >= END_Z;
    let lo = cand.path.iter().position(carried)?;
    let hi = cand.path.iter().rposition(carried)?;
    let path = &cand.path[lo..=hi];
    let knots: Vec<(f32, f32)> = path
        .iter()
        .map(|&(b, si, u)| {
            // parabolic sub-pixel refinement on the band profile
            let at = |x: isize| -> f32 {
                if x < 0 || x >= w as isize { 0.0 } else { pol * z.get(b * ns * w + si * w + x as usize).copied().unwrap_or(0.0) }
            };
            let (l, m, r) = (at(u as isize - 1), at(u as isize), at(u as isize + 1));
            let den = l - 2.0 * m + r;
            let du = if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
            ((b * g.band) as f32 + (bandf - 1.0) / 2.0, u as f32 + du)
        })
        .collect();
    let slope = |si: usize| (si as f32 - g.slopes as f32) / bandf;
    let (&(b0, s0, _), &(b1, s1, _)) = (path.first()?, path.last()?);
    let v0 = b0 * g.band;
    let v1 = if b1 + 1 >= nb { h } else { (b1 + 1) * g.band };
    let centre = centres(&knots, slope(s0), slope(s1), v0, v1);
    // per-band profiles
    let reach = g.reach;
    let (hc, gap, side) = (g.hc as isize, g.gap as isize, g.side as isize);
    let bands: Vec<(usize, Option<(Vec<f32>, f32)>)> = (b0..=b1)
        .map(|b| {
            let rows = b * g.band..if b + 1 >= nb { h } else { (b + 1) * g.band };
            (b, band_profile(d, w, rows, v0, &centre, g))
        })
        .collect();
    let stats: Vec<(usize, f32, f32, f32)> = bands
        .iter()
        .filter_map(|(b, p)| {
            let (p, act) = p.as_ref()?;
            let core = mean_at(p, reach, -hc..=hc);
            let left = mean_at(p, reach, -(gap + side - 1)..=-gap);
            let right = mean_at(p, reach, gap..=gap + side - 1);
            Some((*b, core - (left + right) / 2.0, side_step(p, reach, g.gap), *act))
        })
        .collect();
    // the extent is the path's (found on the other rows), so the measurements stay unbiased
    let inside: Vec<&(usize, f32, f32, f32)> = stats.iter().collect();
    let span_bands = b1 - b0 + 1;
    let (sv0, sv1) = (v0, v1);
    if ((sv1 - sv0) as f32) < min_len || inside.len() * 2 < span_bands {
        return None;
    }
    let contrast = med(inside.iter().map(|s| s.1));
    if contrast.is_nan() || pol * contrast <= 0.0 {
        return None;
    }
    // narrow: half-maximum width of the mean profile against the line between its sides
    let len = 2 * reach + 1;
    let agg: Vec<f32> = (0..len).map(|i| med(bands.iter().filter_map(|(_, p)| p.as_ref().map(|p| p.0[i])))).collect();
    if agg.iter().any(|v| !v.is_finite()) {
        return None;
    }
    // a boundary: the two sides differ, consistently, by more than the background's slope
    // explains
    let (step, step_spread) = util::median_sigma(inside.iter().map(|s| s.2).collect()).unwrap_or((f32::INFINITY, 0.0));
    let step_t = step.abs() * (inside.len() as f32).sqrt() / step_spread.max(1e-6);
    if !(step.abs() <= STEP_RATIO * contrast.abs() || step_t < STEP_T) {
        return None;
    }
    let lc = -(gap as f32 + (side as f32 - 1.0) / 2.0);
    let (lm, rm) = (mean_at(&agg, reach, -(gap + side - 1)..=-gap), mean_at(&agg, reach, gap..=gap + side - 1));
    let base = |o: f32| lm + (rm - lm) * (o - lc) / (-2.0 * lc);
    let dev: Vec<f32> = (0..len).map(|i| pol * (agg[i] - base(i as f32 - reach as f32))).collect();
    let (pk, peak) = (reach - g.hc - 1..=reach + g.hc + 1).map(|i| (i, dev[i])).max_by(|a, b| a.1.total_cmp(&b.1))?;
    if peak.is_nan() || peak <= 0.0 {
        return None;
    }
    let half = peak / 2.0;
    let mut lo = pk as f32;
    let mut i = pk;
    while i > 0 && dev[i - 1] >= half {
        i -= 1;
    }
    if i > 0 {
        lo = i as f32 - (dev[i] - half) / (dev[i] - dev[i - 1]).max(1e-12);
    }
    let mut hi = pk as f32;
    let mut j = pk;
    while j + 1 < len && dev[j + 1] >= half {
        j += 1;
    }
    if j + 1 < len {
        hi = j as f32 + (dev[j] - half) / (dev[j] - dev[j + 1]).max(1e-12);
    }
    let width = (hi - lo).max(1.0);
    if width > g.max_width + 1.0 {
        return None;
    }
    // consistent along its length: the contrast stands out from its own band-to-band spread
    let need = (CONSISTENCY + CONSISTENCY_SMALL / (inside.len() as f32 - 1.0)) / sensitivity;
    let t = consistency(&inside.iter().map(|s| pol * s.1).collect::<Vec<_>>());
    if t.is_nan() || t < need {
        return None;
    }
    // image-borne: a scratch also crosses flat image, not only busy structure
    let calm = inside.iter().filter(|s| s.3 <= CALM * activity).count();
    if calm < 2 || calm * 4 < inside.len() {
        return None;
    }
    let centre = centre[sv0 - v0..sv1 - v0].to_vec();
    let knots: Vec<[f32; 2]> = knots.into_iter().filter(|k| k.0 >= sv0 as f32 - bandf && k.0 < sv1 as f32 + bandf).map(|k| [k.0, k.1]).collect();
    let snr = if pixel_sigma > 0.0 { peak / pixel_sigma } else { 0.0 };
    // a line visible pixel by pixel is held to more: winding scratches that strong run through
    // the frame, while thin image lines that strong (a pole, the lip of a pool) rarely do
    if snr >= STRONG {
        let (_, spread) = util::median_sigma(inside.iter().map(|s| s.1).collect()).unwrap_or((0.0, f32::INFINITY));
        if ((sv1 - sv0) as f32) < STRONG_LENGTH * h as f32 || spread.is_nan() || spread > STRONG_VARIATION * contrast.abs() {
            return None;
        }
    }
    Some(Line { score: cand.score, v0: sv0, v1: sv1, centre, knots, width, contrast, snr })
}

/// Mean absolute neighbour difference of a profile window outside its central `gap` pixels
/// either side (the line itself is not activity).
fn side_activity(win: &[f32], gap: usize) -> f32 {
    let reach = win.len() / 2;
    let (mut s, mut n) = (0.0f32, 0usize);
    for (i, p) in win.windows(2).enumerate() {
        if i + gap <= reach || i >= reach + gap {
            s += (p[1] - p[0]).abs();
            n += 1;
        }
    }
    if n > 0 { s / n as f32 } else { 0.0 }
}

/// Typical activity (mean absolute neighbour difference across the strip over a profile window)
/// of the frame: grain plus ordinary image detail.
fn reference_activity(d: &[f32], w: usize, reach: usize, gap: usize) -> f32 {
    let len = 2 * reach + 1;
    if w < len {
        return 0.0;
    }
    let mut acts = Vec::new();
    for row in d.chunks_exact(w).step_by(7) {
        for start in (0..w - len).step_by(13) {
            let win = &row[start..start + len];
            if win.iter().all(|x| x.is_finite()) {
                acts.push(side_activity(win, gap));
            }
        }
    }
    util::median(acts).unwrap_or(0.0)
}

/// Detect scratches along one axis. Returns the lines and their summed score.
fn detect_axis(data: &[f32], never: &Mask, axis: LineAxis, cfg: &HealConfig, scale: f32) -> (Vec<Line>, f32) {
    let c = Canon::new(axis, never.width(), never.height());
    let g = Geo::new(cfg, scale);
    let d = density(data, never, &c);
    let r = ridge(&d, c.w, &g);
    let Some((_, sigma)) = util::robust_stats(&r, |_| true, 200_000) else {
        return (Vec::new(), 0.0);
    };
    if sigma.is_nan() || sigma <= 0.0 {
        return (Vec::new(), 0.0);
    }
    // pixel noise of the density, from neighbour differences across the strip
    let diffs: Vec<f32> =
        d.chunks(c.w.max(1)).step_by(3).flat_map(|row| row.windows(2).step_by(5).map(|p| (p[1] - p[0]) / std::f32::consts::SQRT_2)).collect();
    let pixel_sigma = util::median_sigma(diffs).map_or(0.0, |(_, s)| s);
    let (mut z, nb, ns) = project(&r, c.w, c.h, &g, sigma);
    if nb == 0 {
        return (Vec::new(), 0.0);
    }
    let z0 = z.clone();
    let sensitivity = (cfg.scratch_sensitivity as f32).clamp(0.25, 4.0);
    let threshold = SCORE / sensitivity;
    let cands = candidates(&mut z, nb, ns, c.w, &g, threshold);
    let min_len = (cfg.scratch_length as f32 * scale).max(1.0);
    let activity = reference_activity(&d, c.w, g.reach, g.gap).min(2.0 * pixel_sigma / std::f32::consts::PI.sqrt());
    let lines: Vec<Line> = cands.par_iter().filter_map(|k| accept(k, &d, &c, &g, nb, &z0, ns, min_len, pixel_sigma, activity, sensitivity)).collect();
    let total = lines.iter().map(|l| l.score).sum();
    (lines, total)
}

/// Detect and repair long scratches in `data` (interleaved linear RGB, `width × height`), never
/// reading or writing `never` pixels. Pixels outside the returned mask are left untouched.
pub fn repair(data: &mut [f32], never: &Mask, cfg: &HealConfig, scale: f32, seed: u64) -> Outcome {
    let (iw, ih) = (never.width(), never.height());
    let empty = || Mask::from_plane(&never.to_plane(), |_| false);
    let mut warnings = Vec::new();
    if data.len() != 3 * iw * ih || iw < 8 || ih < 8 {
        return Outcome { lines: Vec::new(), mask: empty(), warnings };
    }
    let (axis, lines) = match cfg.scratch_axis {
        ScratchAxis::Vertical => (LineAxis::Vertical, detect_axis(data, never, LineAxis::Vertical, cfg, scale).0),
        ScratchAxis::Horizontal => (LineAxis::Horizontal, detect_axis(data, never, LineAxis::Horizontal, cfg, scale).0),
        ScratchAxis::Auto => {
            let ((lv, sv), (lh, sh)) = rayon::join(
                || detect_axis(data, never, LineAxis::Vertical, cfg, scale),
                || detect_axis(data, never, LineAxis::Horizontal, cfg, scale),
            );
            // the strip runs along the longer side unless the other axis has decisively more rain
            let decisive = |(l, s): (&Vec<Line>, f32), other: f32| l.len() >= AUTO_LINES && s > AUTO_RATIO * other;
            let horizontal = if iw > ih { !decisive((&lv, sv), sh) } else { decisive((&lh, sh), sv) };
            if horizontal { (LineAxis::Horizontal, lh) } else { (LineAxis::Vertical, lv) }
        }
    };
    let c = Canon::new(axis, iw, ih);
    let g = Geo::new(cfg, scale);
    let mut lines = lines;
    lines.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.v0.cmp(&b.v0)));
    let mut work = data.to_vec();
    let mut mask = empty();
    let mut out = Vec::new();
    // the strongest scratches first, up to the coverage cap
    let cap = (cfg.max_coverage / 100.0 * (iw * ih) as f64) as usize;
    let (mut covered, mut skipped) = (0usize, 0usize);
    for (k, line) in lines.iter().enumerate() {
        let estimate = (line.v1 - line.v0) * (2 * core_half(line, &g) + 3);
        if covered + estimate > cap {
            skipped += 1;
            continue;
        }
        let interpolate = match cfg.scratch_repair {
            ScratchRepair::Interpolate => true,
            ScratchRepair::Correct => false,
            ScratchRepair::Auto => line.snr >= INTERPOLATE_SIGMA,
        };
        let written = if interpolate {
            interpolate_line(&mut work, never, &c, &g, line, util::splitmix(seed ^ (k as u64 + 1)))
        } else {
            correct_line(&mut work, never, &c, &g, line)
        };
        if written.is_empty() {
            continue;
        }
        let mut bbox = [usize::MAX, usize::MAX, 0, 0];
        let mut core_pixels = 0;
        for &(i, core) in &written {
            let (x, y) = (i % iw, i / iw);
            bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x + 1), bbox[3].max(y + 1)];
            core_pixels += usize::from(core);
            if let Some(m) = mask.data_mut().get_mut(i) {
                covered += usize::from(!*m);
                *m = true;
            }
        }
        out.push(LineScratch {
            axis,
            start: line.v0,
            end: line.v1,
            knots: line.knots.clone(),
            width: line.width,
            contrast: line.contrast,
            score: line.score,
            repair: if interpolate { LineRepair::Interpolated } else { LineRepair::Corrected },
            bbox,
            pixels: written.len(),
            core_pixels,
        });
    }
    if skipped > 0 {
        warnings.push(format!(
            "{skipped} weaker scratches left as scanned: repairing them would rewrite more than {:.1}% of the frame",
            cfg.max_coverage
        ));
    }
    for (i, &m) in mask.data().iter().enumerate() {
        if m {
            data[3 * i..3 * i + 3].copy_from_slice(&work[3 * i..3 * i + 3]);
        }
    }
    Outcome { lines: out, mask, warnings }
}

/// Half-width of the repaired core: whole pixels either side of the centre.
fn core_half(line: &Line, g: &Geo) -> usize {
    (((line.width - 1.0) / 2.0).round().max(0.0) as usize).min(g.max_width.ceil() as usize)
}

fn luma_density(work: &[f32], i: usize) -> f32 {
    match work.get(3 * i..3 * i + 3) {
        Some(p) => {
            let l = (p[0] + p[1] + p[2]) / 3.0;
            if l.is_finite() && l > 1e-6 { -l.ln() } else { f32::NAN }
        }
        None => f32::NAN,
    }
}

/// Divide out the scratch's attenuation, smoothed along its length. Returns `(pixel, in core)`.
fn correct_line(work: &mut [f32], never: &Mask, c: &Canon, g: &Geo, line: &Line) -> Vec<(usize, bool)> {
    let hr = core_half(line, g) as isize;
    let r = hr + 1;
    let (gap, side) = (hr + 2, g.side as isize);
    let outer = gap + side - 1;
    let n = (2 * r + 1) as usize;
    let rows = line.v1 - line.v0;
    let usable = |u: isize, v: usize| u >= 0 && (u as usize) < c.w && !never.data().get(c.index(u as usize, v)).copied().unwrap_or(true);
    // per-row offsets against the line between the two side means
    let mut dev = vec![f32::NAN; rows * n];
    let lc = gap as f32 + (side as f32 - 1.0) / 2.0;
    for (k, &cu) in line.centre.iter().enumerate() {
        let v = line.v0 + k;
        let ur = cu.round() as isize;
        if !usable(ur - outer, v) || !usable(ur + outer, v) {
            continue;
        }
        let side_mean = |dir: isize| -> f32 {
            let (mut s, mut m) = (0.0, 0.0);
            for o in gap..=outer {
                let u = ur + dir * o;
                if usable(u, v) {
                    let x = luma_density(work, c.index(u as usize, v));
                    if x.is_finite() {
                        s += x;
                        m += 1.0;
                    }
                }
            }
            if m > 0.0 { s / m } else { f32::NAN }
        };
        let (lm, rm) = (side_mean(-1), side_mean(1));
        for o in -r..=r {
            let u = ur + o;
            if !usable(u, v) {
                continue;
            }
            let x = luma_density(work, c.index(u as usize, v));
            let base = lm + (rm - lm) * (o as f32 + lc) / (2.0 * lc);
            dev[k * n + (o + r) as usize] = x - base;
        }
    }
    // smooth along the line (clipped running mean)
    let ry = (g.band / 2).max(4);
    let clip = (4.0 * line.contrast.abs()).max(0.01);
    let mut smooth = vec![f32::NAN; rows * n];
    for j in 0..n {
        let col: Vec<f32> = (0..rows).map(|k| dev[k * n + j]).collect();
        let (mut s, mut m) = (0.0f64, 0usize);
        let add = |s: &mut f64, m: &mut usize, x: f32, sign: f64| {
            if x.is_finite() {
                *s += sign * f64::from(x.clamp(-clip, clip));
                if sign > 0.0 {
                    *m += 1;
                } else {
                    *m -= 1;
                }
            }
        };
        for x in col.iter().take(ry.min(rows)) {
            add(&mut s, &mut m, *x, 1.0);
        }
        for k in 0..rows {
            if k + ry < rows {
                add(&mut s, &mut m, col[k + ry], 1.0);
            }
            if k > ry {
                add(&mut s, &mut m, col[k - ry - 1], -1.0);
            }
            if m * 4 >= ry {
                smooth[k * n + j] = (s / m as f64) as f32;
            }
        }
    }
    let mut written = Vec::new();
    for (k, &cu) in line.centre.iter().enumerate() {
        let v = line.v0 + k;
        let ur = cu.round() as isize;
        for o in -r..=r {
            let u = ur + o;
            let dv = smooth[k * n + (o + r) as usize];
            if !usable(u, v) || !dv.is_finite() || dev[k * n + (o + r) as usize].is_nan() {
                continue;
            }
            let i = c.index(u as usize, v);
            let f = dv.clamp(-0.7, 0.7).exp();
            if let Some(p) = work.get_mut(3 * i..3 * i + 3) {
                p.iter_mut().for_each(|x| *x *= f);
                written.push((i, o.abs() <= hr));
            }
        }
    }
    written
}

/// Interpolate across the scratch from both sides, with matched multiplicative grain.
fn interpolate_line(work: &mut [f32], never: &Mask, c: &Canon, g: &Geo, line: &Line, seed: u64) -> Vec<(usize, bool)> {
    let hr = core_half(line, g) as isize;
    let r = hr + 1;
    let usable = |u: isize, v: usize| u >= 0 && (u as usize) < c.w && !never.data().get(c.index(u as usize, v)).copied().unwrap_or(true);
    // grain: relative pixel noise of the sides along the line
    let mut diffs = Vec::new();
    for (k, &cu) in line.centre.iter().enumerate().skip(1) {
        let v = line.v0 + k;
        let ur = cu.round() as isize;
        for o in [-(r + 2), -(r + 1), r + 1, r + 2] {
            let u = ur + o;
            if usable(u, v) && usable(u, v - 1) {
                let a = luma_density(work, c.index(u as usize, v));
                let b = luma_density(work, c.index(u as usize, v - 1));
                diffs.push((a - b) / std::f32::consts::SQRT_2);
            }
        }
    }
    let grain = util::median_sigma(diffs).map_or(0.0, |(_, s)| s).clamp(0.0, 0.5);
    let mut written = Vec::new();
    for (k, &cu) in line.centre.iter().enumerate() {
        let v = line.v0 + k;
        let ur = cu.round() as isize;
        let mean_of = |us: [isize; 2], work: &[f32]| -> Option<[f32; 3]> {
            let mut acc = [0.0f32; 3];
            for u in us {
                if !usable(u, v) {
                    return None;
                }
                let i = c.index(u as usize, v);
                let p = work.get(3 * i..3 * i + 3)?;
                for ch in 0..3 {
                    if !p[ch].is_finite() {
                        return None;
                    }
                    acc[ch] += p[ch] / 2.0;
                }
            }
            Some(acc)
        };
        let (Some(left), Some(right)) = (mean_of([ur - r - 1, ur - r - 2], work), mean_of([ur + r + 1, ur + r + 2], work)) else {
            continue;
        };
        // the side means sit at ±(r + 1.5)
        let span = 2.0 * (r as f32 + 1.5);
        for o in -r..=r {
            let u = ur + o;
            if !usable(u, v) {
                continue;
            }
            let t = (o as f32 + r as f32 + 1.5) / span;
            let i = c.index(u as usize, v);
            let noise = if grain > 0.0 { (grain * util::gauss(seed ^ (i as u64).wrapping_mul(0x9E37_79B9))).exp() } else { 1.0 };
            if let Some(p) = work.get_mut(3 * i..3 * i + 3) {
                for ch in 0..3 {
                    p[ch] = (left[ch] + (right[ch] - left[ch]) * t) * noise;
                }
                written.push((i, o.abs() <= hr));
            }
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_maps_both_axes() {
        let v = Canon::new(LineAxis::Vertical, 10, 4);
        assert_eq!((v.w, v.h, v.index(3, 2)), (10, 4, 23));
        let h = Canon::new(LineAxis::Horizontal, 10, 4);
        assert_eq!((h.w, h.h, h.index(3, 2)), (4, 10, 32));
    }

    #[test]
    fn ridge_prefers_lines_to_steps() {
        let g = Geo::new(&HealConfig::default(), 1.0);
        let w = 60;
        let line: Vec<f32> = (0..w).map(|u| if (19..=21).contains(&u) { 1.0 } else { 0.0 }).collect();
        let step: Vec<f32> = (0..w).map(|u| if u >= 40 { 1.0 } else { 0.0 }).collect();
        let r = ridge(&line, w, &g);
        assert!((r[20] - 1.0).abs() < 1e-6, "{}", r[20]);
        let r = ridge(&step, w, &g);
        assert!(r.iter().filter(|v| v.is_finite()).all(|v| v.abs() <= 0.5 + 1e-6), "a step gives at most half its height");
        assert!(r[..g.span()].iter().all(|v| v.is_nan()), "no response without both side windows");
    }

    #[test]
    fn centres_interpolate_and_extrapolate() {
        let c = centres(&[(10.0, 5.0), (20.0, 7.0)], 0.1, 0.3, 0, 31);
        assert!((c[0] - 4.0).abs() < 1e-6 && (c[15] - 6.0).abs() < 1e-6 && (c[30] - 10.0).abs() < 1e-6);
        assert!(centres(&[], 0.0, 0.0, 0, 5).is_empty());
    }
}
