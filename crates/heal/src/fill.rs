//! Tiered inpainting of defect components.
//!
//! Every component is filled independently from a local window: it reads only clean pixels
//! (outside every defect) and its own fills, and writes only its own pixels, so components run
//! in parallel, deterministically, and the cost is linear in the defect area (never in the
//! defect count squared). All three channels share one set of weights / one source offset, so a
//! fill never introduces colour that the surroundings do not have.
//!
//! - **Small** specks: inverse-square-distance average of the ring just outside the speck; on an
//!   edge, the edge-following membrane below instead (a ring average there is a grey bump).
//! - **Medium**: membrane (harmonic) interpolation, solved coarse-to-fine on a 2× pyramid with
//!   over-relaxed Gauss-Seidel at each level. Edges of the surroundings that run through the
//!   defect are continued first (see [`Structure`]): those pixels are filled along the edge and
//!   held fixed, so the membrane keeps the edge and the tone of each side instead of blending
//!   across it (the rebate next to the image, the outline of a head behind a hair).
//! - **Large** blobs and long scratches/hairs: tone from that membrane, texture from best-match
//!   patches (see [`texture_fill`]): each pixel is `membrane · (1 + d)` with `d` the relative
//!   high-pass of a clean patch that matches both the surrounding detail and the local low-pass
//!   tone, blended between overlapping blocks with feathered weights. No block brings its own
//!   brightness, so there is no patchwork of mismatched squares; where no convincing patch exists
//!   the block gets grain matched to the surroundings instead.
//!
//! The smooth tiers optionally get synthetic grain (same multiplicative deviate in every channel)
//! whose amplitude is measured on the clean ring around the defect.

use silvercraft_imgproc::{Mask, Plane};

use crate::config::HealConfig;
use crate::detect::Tier;
use crate::util;

/// A small speck whose surroundings have an edge at least this coherent is filled along the edge
/// instead of by the ring average.
const EDGE_COHERENCE: f32 = 0.3;

/// At most this many edge pixels of the surroundings are continued into a defect (subsampled
/// beyond), which bounds the cost on huge defects.
const MAX_EDGE_SAMPLES: usize = 1500;

/// A defect pixel is filled along its edge when its edge support is at least this.
const TRACE_SUPPORT: f32 = 0.4;
/// Clean pixels with at least this edge weight are edges that may continue into a defect.
const EDGE_CANDIDATE: f32 = 0.7;
/// Smoothing sweeps over a traced fill.
const SEAM_SWEEPS: usize = 2;
/// Seam smoothing only between neighbours whose tones differ by less than this (log ratio).
const SEAM_TONE: f32 = 0.2;
/// Clean pixels along the edge averaged on each side of a traced pixel, in pixels past the first.
const EDGE_AVERAGE: f32 = 2.0;
/// A traced pixel's two ends must agree within this (absolute log ratio of their tones).
const TRACE_AGREE: f32 = 0.25;

const KNOWN: u8 = 0;
const OWN: u8 = 1;
const FOREIGN: u8 = 2;

/// Shared read-only inputs of the fill.
pub struct FillContext<'a> {
    pub width: usize,
    pub height: usize,
    /// Original interleaved RGB.
    pub rgb: &'a [f32],
    /// Pixels that are never a source: every defect, the holder, non-finite pixels.
    pub forbidden: &'a Mask,
    pub luma: &'a Plane,
    pub cfg: &'a HealConfig,
    pub scale: f32,
}

/// A local window around one component.
struct Window {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    /// KNOWN / OWN / FOREIGN per pixel.
    state: Vec<u8>,
    rgb: Vec<[f32; 3]>,
    luma: Vec<f32>,
}

impl Window {
    fn new(ctx: &FillContext, pixels: &[usize], bbox: [usize; 4], pad: usize) -> Self {
        let x0 = bbox[0].saturating_sub(pad);
        let y0 = bbox[1].saturating_sub(pad);
        let x1 = (bbox[2] + pad).min(ctx.width);
        let y1 = (bbox[3] + pad).min(ctx.height);
        let (w, h) = (x1 - x0, y1 - y0);
        let mut state = vec![KNOWN; w * h];
        let mut rgb = vec![[0.0f32; 3]; w * h];
        let mut luma = vec![0.0f32; w * h];
        for ly in 0..h {
            for lx in 0..w {
                let gi = (y0 + ly) * ctx.width + x0 + lx;
                let li = ly * w + lx;
                rgb[li] = [ctx.rgb[3 * gi], ctx.rgb[3 * gi + 1], ctx.rgb[3 * gi + 2]];
                luma[li] = ctx.luma.data()[gi];
                if ctx.forbidden.data()[gi] || !rgb[li].iter().all(|v| v.is_finite()) || !luma[li].is_finite() {
                    state[li] = FOREIGN;
                }
            }
        }
        for &gi in pixels {
            let (gx, gy) = (gi % ctx.width, gi / ctx.width);
            if gx >= x0 && gy >= y0 && gx < x1 && gy < y1 {
                state[(gy - y0) * w + gx - x0] = OWN;
            }
        }
        Self { x0, y0, w, h, state, rgb, luma }
    }

    fn global(&self, li: usize, width: usize) -> usize {
        (self.y0 + li / self.w) * width + self.x0 + li % self.w
    }
}

/// Fill one component; returns `(pixel index, rgb)` writes.
pub fn fill_component(ctx: &FillContext, pixels: &[usize], bbox: [usize; 4], tier: Tier, seed: u64) -> Vec<(usize, [f32; 3])> {
    if pixels.is_empty() {
        return Vec::new();
    }
    let s = ctx.scale.max(0.1);
    let ring_r = ((2.0 * s).round() as usize).max(2);
    match tier {
        Tier::Small | Tier::Corrected | Tier::Interpolated => {
            let mut win = Window::new(ctx, pixels, bbox, ring_r + 4);
            let mut done = vec![false; win.w * win.h];
            // a speck on an edge follows the edge; elsewhere the ring average
            let structure = Structure::new(&win, s);
            if structure.max_own > EDGE_COHERENCE {
                membrane_fill(&mut win, Some(&structure), &mut done);
            } else {
                ring_fill(&mut win, ring_r, &mut done);
            }
            add_grain(ctx, &mut win, &done, seed);
            collect(ctx, &win, &done)
        }
        Tier::Medium => {
            let mut win = Window::new(ctx, pixels, bbox, ring_r + 4);
            let mut done = vec![false; win.w * win.h];
            let structure = Structure::new(&win, s);
            membrane_fill(&mut win, Some(&structure), &mut done);
            add_grain(ctx, &mut win, &done, seed);
            collect(ctx, &win, &done)
        }
        Tier::Large => {
            let patch = ((ctx.cfg.patch_size as f32 * s).round() as usize).max(3);
            let search = ((ctx.cfg.search_radius as f32 * s).round() as usize).max(4);
            let mut win = Window::new(ctx, pixels, bbox, search + 2 * patch);
            let mut done = vec![false; win.w * win.h];
            // tone: the boundary-matched membrane; texture: high frequencies of matched patches
            let structure = Structure::new(&win, s);
            membrane_fill(&mut win, Some(&structure), &mut done);
            texture_fill(ctx, &mut win, &done, patch, search, ((s.round()) as usize).max(1), seed);
            collect(ctx, &win, &done)
        }
    }
}

fn collect(ctx: &FillContext, win: &Window, done: &[bool]) -> Vec<(usize, [f32; 3])> {
    (0..win.w * win.h).filter(|&li| win.state[li] == OWN && done[li]).map(|li| (win.global(li, ctx.width), win.rgb[li])).collect()
}

/// Inverse-square-distance average of the clean ring within `ring_r` of the component.
fn ring_fill(win: &mut Window, ring_r: usize, done: &mut [bool]) {
    let (w, h) = (win.w, win.h);
    let r = ring_r as isize;
    let mut ring: Vec<(f32, f32, [f32; 3])> = Vec::new();
    for li in 0..w * h {
        if win.state[li] != KNOWN {
            continue;
        }
        let (x, y) = ((li % w) as isize, (li / w) as isize);
        let near = (-r..=r).any(|dy| {
            (-r..=r).any(|dx| {
                let (nx, ny) = (x + dx, y + dy);
                nx >= 0 && ny >= 0 && nx < w as isize && ny < h as isize && win.state[ny as usize * w + nx as usize] == OWN
            })
        });
        if near {
            ring.push((x as f32, y as f32, win.rgb[li]));
        }
    }
    if ring.is_empty() {
        return;
    }
    for li in 0..w * h {
        if win.state[li] != OWN {
            continue;
        }
        let (x, y) = ((li % w) as f32, (li / w) as f32);
        let mut acc = [0.0f64; 3];
        let mut wsum = 0.0f64;
        for &(rx, ry, v) in &ring {
            let d2 = f64::from((rx - x) * (rx - x) + (ry - y) * (ry - y));
            let wt = 1.0 / d2.max(1e-6);
            for c in 0..3 {
                acc[c] += wt * f64::from(v[c]);
            }
            wsum += wt;
        }
        if wsum > 0.0 {
            win.rgb[li] = [(acc[0] / wsum) as f32, (acc[1] / wsum) as f32, (acc[2] / wsum) as f32];
            done[li] = true;
        }
    }
}

/// Coarse-to-fine membrane solve of the OWN pixels with KNOWN pixels as boundary values. With a
/// [`Structure`], the pixels on the continuation of an edge are first filled along that edge
/// (see [`trace_edges`]) and then held fixed, so the smooth fill keeps the edge sharp and each
/// side's tone instead of blending across it.
fn membrane_fill(win: &mut Window, structure: Option<&Structure>, done: &mut [bool]) {
    let mut vals = win.rgb.clone();
    if !win.state.contains(&KNOWN) {
        return;
    }
    let mut state = win.state.clone();
    let mut traced = false;
    if let Some(st) = structure {
        for (i, v) in trace_edges(win, st) {
            vals[i] = v;
            state[i] = KNOWN;
            traced = true;
        }
    }
    membrane(&mut vals, &state, win.w, win.h);
    if traced {
        // soften the seams between neighbouring rays and the membrane (a pixel or so), but not
        // the edge itself: neighbours of a clearly different tone do not count
        let (w, h) = (win.w, win.h);
        for _ in 0..SEAM_SWEEPS {
            let prev = vals.clone();
            for i in (0..w * h).filter(|&i| win.state[i] == OWN) {
                let (x, y) = (i % w, i / w);
                let (mut acc, mut n) = ([0.0f32; 3], 0.0f32);
                let nbrs = [(x > 0).then(|| i - 1), (x + 1 < w).then(|| i + 1), (y > 0).then(|| i - w), (y + 1 < h).then(|| i + w)];
                let li = prev[i][0] + prev[i][1] + prev[i][2];
                for j in nbrs.into_iter().flatten() {
                    let lj = prev[j][0] + prev[j][1] + prev[j][2];
                    if win.state[j] != FOREIGN && li > 0.0 && lj > 0.0 && (lj / li).ln().abs() < SEAM_TONE {
                        for c in 0..3 {
                            acc[c] += prev[j][c];
                        }
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    for c in 0..3 {
                        vals[i][c] = 0.5 * prev[i][c] + 0.5 * acc[c] / n;
                    }
                }
            }
        }
    }
    for li in 0..win.w * win.h {
        if win.state[li] == OWN && vals[li].iter().all(|v| v.is_finite()) {
            win.rgb[li] = vals[li];
            done[li] = true;
        }
    }
}

/// Edge-supported OWN pixels filled along their edge: from the pixel, step both ways along the
/// tangent to the first clean pixels, average a few clean pixels further along on each side (the
/// along-edge direction only, so grain is calmed without blurring across the edge), and
/// interpolate by distance. Pixels whose ray leaves the window or meets a foreign pixel are left
/// to the membrane.
fn trace_edges(win: &Window, st: &Structure) -> Vec<(usize, [f32; 3])> {
    let (w, h) = (win.w as isize, win.h as isize);
    let max_steps = 2 * (w + h) as usize;
    let side = |qx: f32, qy: f32, t: [f32; 2]| -> Option<(f32, [f32; 3])> {
        let mut first: Option<f32> = None;
        let (mut acc, mut n) = ([0.0f32; 3], 0.0f32);
        for k in 1..max_steps {
            let d = k as f32 * 0.5;
            let (x, y) = ((qx + d * t[0]).round() as isize, (qy + d * t[1]).round() as isize);
            if x < 0 || y < 0 || x >= w || y >= h {
                break;
            }
            let j = (y * w + x) as usize;
            match win.state[j] {
                KNOWN => {
                    let f = *first.get_or_insert(d);
                    if d > f + EDGE_AVERAGE {
                        break;
                    }
                    for c in 0..3 {
                        acc[c] += win.rgb[j][c];
                    }
                    n += 1.0;
                }
                OWN if first.is_none() => {}
                _ => break,
            }
        }
        let f = first?;
        (n > 0.0).then(|| (f, [acc[0] / n, acc[1] / n, acc[2] / n]))
    };
    let mut out = Vec::new();
    for &(i, c, t) in &st.edges {
        if c < TRACE_SUPPORT {
            continue;
        }
        let (qx, qy) = ((i % win.w) as f32, (i / win.w) as f32);
        if let (Some((da, va)), Some((db, vb))) = (side(qx, qy, t), side(qx, qy, [-t[0], -t[1]])) {
            // both ends on the same side of the edge, or the ray cut across it (its tangent a
            // little off, or the edge curving): then the membrane decides
            let (la, lb) = (va[0] + va[1] + va[2], vb[0] + vb[1] + vb[2]);
            if !(la > 0.0 && lb > 0.0) || (la / lb).ln().abs() > TRACE_AGREE {
                continue;
            }
            let v: [f32; 3] = std::array::from_fn(|k| (db * va[k] + da * vb[k]) / (da + db));
            if v.iter().all(|x| x.is_finite()) {
                out.push((i, v));
            }
        }
    }
    out
}

/// Edge structure around a defect. On the clean surroundings: the smoothed structure tensor of
/// the log luma, giving an edge weight (strong, single-orientation gradients against the grain
/// level) and the edge tangent. Each defect pixel then takes the edge it lies on the straight
/// continuation of, provided that edge also runs in on the other side of it. A speck on the edge
/// of the clear rebate keeps that edge straight, a hair crossing the outline of a head keeps the
/// outline, and a pole that ends inside a defect is not continued out of the other side.
struct Structure {
    /// OWN pixels with edge support: (window index, support 0..1, unit tangent).
    edges: Vec<(usize, f32, [f32; 2])>,
    /// Largest support inside the defect (0..1).
    max_own: f32,
}

impl Structure {
    fn new(win: &Window, scale: f32) -> Self {
        let (w, h) = (win.w, win.h);
        let n = w * h;
        let lg: Vec<f32> =
            (0..n).map(|i| if win.state[i] == KNOWN && win.luma[i].is_finite() && win.luma[i] > 0.0 { win.luma[i].ln() } else { f32::NAN }).collect();
        // Sobel gradients of the known log luma (isotropic enough that a hard, aliased edge
        // still gets its true orientation)
        let mut g = vec![[0.0f32; 3]; n];
        let mut has = vec![false; n];
        for y in 1..h.saturating_sub(1) {
            for x in 1..w.saturating_sub(1) {
                let i = y * w + x;
                let v = |dx: isize, dy: isize| lg[(i as isize + dy * w as isize + dx) as usize];
                let k = [v(-1, -1), v(0, -1), v(1, -1), v(-1, 0), v(1, 0), v(-1, 1), v(0, 1), v(1, 1)];
                if k.iter().all(|a| a.is_finite()) {
                    let gx = (k[2] + 2.0 * k[4] + k[7] - k[0] - 2.0 * k[3] - k[5]) / 8.0;
                    let gy = (k[5] + 2.0 * k[6] + k[7] - k[0] - 2.0 * k[1] - k[2]) / 8.0;
                    g[i] = [gx * gx, gx * gy, gy * gy];
                    has[i] = true;
                }
            }
        }
        // masked box average (summed-area tables)
        // wide enough that a small blob or point light (all orientations) is not an edge
        let rr = ((2.0 * scale).round() as usize).max(2);
        let ww = w + 1;
        let mut sat = vec![[0.0f64; 4]; ww * (h + 1)];
        for y in 0..h {
            let mut run = [0.0f64; 4];
            for x in 0..w {
                let i = y * w + x;
                if has[i] {
                    for c in 0..3 {
                        run[c] += f64::from(g[i][c]);
                    }
                    run[3] += 1.0;
                }
                let o = (y + 1) * ww + x + 1;
                for c in 0..4 {
                    sat[o][c] = sat[y * ww + x + 1][c] + run[c];
                }
            }
        }
        let mut tensor = vec![[0.0f32; 3]; n];
        let mut traces = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if win.state[i] != KNOWN {
                    continue;
                }
                let (x0, y0, x1, y1) = (x.saturating_sub(rr), y.saturating_sub(rr), (x + rr + 1).min(w), (y + rr + 1).min(h));
                let q = |c: usize| sat[y1 * ww + x1][c] + sat[y0 * ww + x0][c] - sat[y0 * ww + x1][c] - sat[y1 * ww + x0][c];
                let cnt = q(3);
                if cnt >= 4.0 {
                    tensor[i] = [(q(0) / cnt) as f32, (q(1) / cnt) as f32, (q(2) / cnt) as f32];
                    traces.push(tensor[i][0] + tensor[i][2]);
                }
            }
        }
        // edge strength relative to the typical (grain) level of the surroundings
        let g0 = util::median(traces).unwrap_or(0.0).max(1e-8);
        for t in &mut tensor {
            let tr = t[0] + t[2];
            let k = tr / (tr + 4.0 * g0) / tr.max(1e-12);
            *t = [t[0] * k, t[1] * k, t[2] * k];
        }
        // edge pixels of the surroundings: position, weight, tangent
        let mut edges: Vec<(f32, f32, f32, [f32; 2])> = Vec::new();
        for (i, t) in tensor.iter().enumerate() {
            if win.state[i] == KNOWN {
                let (c, tan) = coherence(*t);
                if c > EDGE_CANDIDATE {
                    edges.push(((i % w) as f32, (i / w) as f32, c, tan));
                }
            }
        }
        if edges.len() > MAX_EDGE_SAMPLES {
            let step = edges.len().div_ceil(MAX_EDGE_SAMPLES);
            edges = edges.into_iter().step_by(step).collect();
        }
        let reach = (64.0 * scale).max(32.0);
        let lateral = (2.0 * scale).max(2.0);
        let mut own = Vec::new();
        let mut max_own = 0.0f32;
        // evaluated once per `cell`² block of the defect (at its centre) on large scans; the
        // edge geometry does not change within a few pixels
        let cell = ((scale / 2.0).round() as usize).max(1);
        let (cw, ch) = (w.div_ceil(cell), h.div_ceil(cell));
        let mut cells: Vec<Option<(f32, [f32; 2])>> = vec![None; cw * ch];
        if !edges.is_empty() {
            for i in 0..n {
                if win.state[i] != OWN {
                    continue;
                }
                let (bx, by) = ((i % w) / cell, (i / w) / cell);
                let slot = &mut cells[by * cw + bx];
                let (c, tan) = *slot.get_or_insert_with(|| {
                    let half = (cell as f32 - 1.0) / 2.0;
                    let (qx, qy) = ((bx * cell) as f32 + half, (by * cell) as f32 + half);
                    edge_through(&edges, qx, qy, reach, lateral)
                });
                if c > 0.0 {
                    own.push((i, c, tan));
                    max_own = max_own.max(c);
                }
            }
        }
        Self { edges: own, max_own }
    }
}

/// The edge that point `(qx, qy)` lies on the continuation of: (support 0..1, unit direction).
/// Support is each edge pixel's weight, falling off with the distance off its line (`lateral`)
/// and mildly with distance (`reach`); the edge must run in on both sides of the point, so the
/// weaker side counts.
fn edge_through(edges: &[(f32, f32, f32, [f32; 2])], qx: f32, qy: f32, reach: f32, lateral: f32) -> (f32, [f32; 2]) {
    let support = |p: &(f32, f32, f32, [f32; 2])| {
        let (dx, dy) = (qx - p.0, qy - p.1);
        // distance off the edge's line, not an angle: a far edge that merely points near this
        // pixel (a post below a row of lights) does not continue through it
        let perp = (dx * p.3[1] - dy * p.3[0]) / lateral;
        let d2 = (dx * dx + dy * dy) / (reach * reach);
        p.2 * p.2 * (-perp * perp).exp() / (1.0 + d2)
    };
    let mut lead = (0.0f32, [1.0f32, 0.0]);
    for e in edges {
        let wgt = support(e);
        if wgt > lead.0 {
            lead = (wgt, e.3);
        }
    }
    let (mut ahead, mut behind) = ((0.0f32, qx, qy), (0.0f32, qx, qy));
    for e in edges {
        let wgt = support(e);
        let side = if (e.0 - qx) * lead.1[0] + (e.1 - qy) * lead.1[1] >= 0.0 { &mut ahead } else { &mut behind };
        if wgt > side.0 {
            *side = (wgt, e.0, e.1);
        }
    }
    // the direction from where the edge runs in on one side to where it runs in on the other: a
    // chord, which stays on its side of a curved edge (a tangent line of a convex outline leaves
    // it)
    let (cx, cy) = (ahead.1 - behind.1, ahead.2 - behind.2);
    let len = (cx * cx + cy * cy).sqrt();
    (ahead.0.min(behind.0), if len > 1.0 { [cx / len, cy / len] } else { lead.1 })
}

/// (Edge weight 0..1, unit tangent along the edge) of a normalised structure tensor.
fn coherence(t: [f32; 3]) -> (f32, [f32; 2]) {
    let tr = t[0] + t[2];
    let disc = ((t[0] - t[2]) * (t[0] - t[2]) + 4.0 * t[1] * t[1]).sqrt();
    if tr.is_nan() || tr <= 1e-9 || !disc.is_finite() {
        return (0.0, [1.0, 0.0]);
    }
    // anisotropy (0 random, 1 a single orientation) times strength (already in the trace)
    let aniso = (disc / tr).clamp(0.0, 1.0);
    let c = (((aniso - 0.4) / 0.4).clamp(0.0, 1.0) * tr.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let theta = 0.5 * (2.0 * t[1]).atan2(t[0] - t[2]);
    // the gradient points along theta; the edge runs perpendicular to it
    (c * c * (3.0 - 2.0 * c), [-theta.sin(), theta.cos()])
}

fn membrane(vals: &mut [[f32; 3]], state: &[u8], w: usize, h: usize) {
    let unknown = state.iter().filter(|&&s| s == OWN).count();
    if unknown == 0 {
        return;
    }
    if w >= 8 && h >= 8 && unknown > 16 {
        // coarse level: 2×2 blocks; known if any known pixel, unknown if any unknown
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut cvals = vec![[0.0f32; 3]; cw * ch];
        let mut cstate = vec![FOREIGN; cw * ch];
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut acc, mut n, mut any_own) = ([0.0f32; 3], 0.0f32, false);
                for y in 2 * cy..(2 * cy + 2).min(h) {
                    for x in 2 * cx..(2 * cx + 2).min(w) {
                        let i = y * w + x;
                        match state[i] {
                            KNOWN => {
                                for c in 0..3 {
                                    acc[c] += vals[i][c];
                                }
                                n += 1.0;
                            }
                            OWN => any_own = true,
                            _ => {}
                        }
                    }
                }
                let ci = cy * cw + cx;
                if n > 0.0 {
                    cvals[ci] = [acc[0] / n, acc[1] / n, acc[2] / n];
                    cstate[ci] = KNOWN;
                } else if any_own {
                    cstate[ci] = OWN;
                }
            }
        }
        membrane(&mut cvals, &cstate, cw, ch);
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if state[i] == OWN {
                    vals[i] = cvals[(y / 2) * cw + x / 2];
                }
            }
        }
    } else {
        // base: start from the mean of the known pixels
        let (mut acc, mut n) = ([0.0f64; 3], 0.0f64);
        for (v, &s) in vals.iter().zip(state) {
            if s == KNOWN {
                for c in 0..3 {
                    acc[c] += f64::from(v[c]);
                }
                n += 1.0;
            }
        }
        let mean = if n > 0.0 { [(acc[0] / n) as f32, (acc[1] / n) as f32, (acc[2] / n) as f32] } else { [0.0; 3] };
        for (v, &s) in vals.iter_mut().zip(state) {
            if s == OWN {
                *v = mean;
            }
        }
    }
    // over-relaxed Gauss-Seidel on the unknowns (4-neighbour Laplace, foreign = Neumann)
    let idx: Vec<usize> = (0..w * h).filter(|&i| state[i] == OWN).collect();
    let sweeps = 24 + 2 * w.max(h).min(64);
    let omega = 1.7f32;
    for _ in 0..sweeps {
        let mut change = 0.0f32;
        for &i in &idx {
            let (x, y) = (i % w, i / w);
            let mut acc = [0.0f32; 3];
            let mut n = 0.0f32;
            let mut add = |j: usize| {
                if state[j] != FOREIGN {
                    for c in 0..3 {
                        acc[c] += vals[j][c];
                    }
                    n += 1.0;
                }
            };
            if x > 0 {
                add(i - 1);
            }
            if x + 1 < w {
                add(i + 1);
            }
            if y > 0 {
                add(i - w);
            }
            if y + 1 < h {
                add(i + w);
            }
            if n > 0.0 {
                for c in 0..3 {
                    let target = acc[c] / n;
                    let d = omega * (target - vals[i][c]);
                    vals[i][c] += d;
                    change = change.max(d.abs());
                }
            }
        }
        if change < 1e-6 {
            break;
        }
    }
}

/// Multiplicative grain matched to the clean surroundings, identical in every channel.
fn add_grain(ctx: &FillContext, win: &mut Window, targets: &[bool], seed: u64) {
    if !ctx.cfg.grain || ctx.cfg.grain_strength <= 0.0 {
        return;
    }
    let (w, h) = (win.w, win.h);
    // relative high-pass of clean pixels near the defect (8-neighbour mean of clean neighbours)
    let mut rel = Vec::new();
    for li in 0..w * h {
        if win.state[li] != KNOWN {
            continue;
        }
        let (x, y) = (li % w, li / w);
        if x == 0 || y == 0 || x + 1 >= w || y + 1 >= h {
            continue;
        }
        let mut acc = 0.0f32;
        let mut n = 0.0f32;
        for ny in y - 1..=y + 1 {
            for nx in x - 1..=x + 1 {
                let j = ny * w + nx;
                if j != li && win.state[j] == KNOWN {
                    acc += win.luma[j];
                    n += 1.0;
                }
            }
        }
        let v = win.luma[li];
        if n >= 5.0 && v > 1e-6 {
            rel.push((v - acc / n) / v);
        }
    }
    if rel.len() < 12 {
        return;
    }
    let Some((_, sigma)) = util::median_sigma(rel) else {
        return;
    };
    // a pixel minus the mean of 8 independent neighbours has variance σ²(1 + 1/8)
    let sigma = (sigma / 1.0607 * ctx.cfg.grain_strength as f32).clamp(0.0, 0.5);
    if sigma <= 0.0 {
        return;
    }
    for li in 0..w * h {
        if win.state[li] == OWN && targets[li] {
            let g = win.global(li, ctx.width) as u64;
            let n = 1.0 + sigma * util::gauss(g ^ seed.rotate_left(17));
            for c in 0..3 {
                win.rgb[li][c] *= n.max(0.0);
            }
        }
    }
}

/// Relative detail beyond this is clamped (a patch never more than doubles or quarters the
/// membrane tone).
const DETAIL_RANGE: (f32, f32) = (-0.75, 1.0);
/// Copied detail is clamped to this many grain sigmas of the surroundings.
const DETAIL_SIGMAS: f32 = 3.0;
/// A match whose detail misfit is below this fraction of the template's detail variance continues
/// the texture of the surroundings: its fine structure is copied whole.
const STRONG_MATCH: f32 = 0.5;
/// Textured copies only where the target's log low-pass tone varies less than this over the block.
const FLAT_TONE: f32 = 0.2;
/// A patch whose low-pass tone differs from the target's by more than this (rms of the log) is
/// not a convincing source: that block gets synthetic grain instead.
const TONE_TOLERANCE: f32 = 0.06;

/// Texture for the membrane-filled OWN pixels (`done`): each pixel becomes
/// `membrane · (1 + d)`, where `d = v / lowpass(v) − 1` is the relative high-pass detail (at grain
/// scale, clamped to a few grain sigmas of the surroundings) of a best-matching clean patch. Tone
/// therefore comes from the boundary-matched membrane (no block can bring its own brightness),
/// and only grain and fine texture are copied. Where the target's tone is flat and the patch
/// continues the surrounding texture closely (`STRONG_MATCH`), the patch is copied whole with its
/// tone moved to the target's (low-pass ratio), so regular texture keeps its full contrast.
///
/// The OWN area is cut into `patch` blocks, processed from the boundary inwards. Each block is
/// matched over its block grown by half a patch, within `search` px, on two terms: the detail of
/// the known (and already textured) pixels, and the log low-pass tone of the whole grown block
/// (membrane inside the defect). The tone term keeps sources to regions of the same local tone
/// and structure, so the texture of a light background is never copied into dark hair. Each
/// block's contribution is spread over its grown box with feathered weights, so overlapping
/// blocks blend instead of meeting at a hard seam. A block with no source within
/// `TONE_TOLERANCE` gets grain matched to the surroundings (when grain is on) instead of a wrong
/// patch. `stride` subsamples the matching on large scans.
fn texture_fill(ctx: &FillContext, win: &mut Window, done: &[bool], patch: usize, search: usize, stride: usize, seed: u64) {
    let scale = ctx.scale.max(0.1);
    let (w, h) = (win.w, win.h);
    let n = w * h;
    if !done.iter().any(|&d| d) {
        return;
    }
    // tone (luma) with the membrane inside the defect; `usable` = known or membrane-filled
    let usable: Vec<bool> = (0..n).map(|i| win.state[i] == KNOWN || (win.state[i] == OWN && done[i])).collect();
    let tone: Vec<f32> = win.rgb.iter().map(|v| (v[0] + v[1] + v[2]) / 3.0).collect();
    // masked box low-pass of the tone (summed-area tables)
    let r = (patch / 2).max(2);
    let ww = w + 1;
    let mut sat_v = vec![0.0f64; ww * (h + 1)];
    let mut sat_n = vec![0.0f64; ww * (h + 1)];
    let mut sat_bad = vec![0u32; ww * (h + 1)];
    for y in 0..h {
        let (mut rv, mut rn, mut rb) = (0.0f64, 0.0f64, 0u32);
        for x in 0..w {
            let i = y * w + x;
            if usable[i] && tone[i].is_finite() {
                rv += f64::from(tone[i]);
                rn += 1.0;
            }
            rb += u32::from(win.state[i] != KNOWN);
            let o = (y + 1) * ww + x + 1;
            sat_v[o] = sat_v[y * ww + x + 1] + rv;
            sat_n[o] = sat_n[y * ww + x + 1] + rn;
            sat_bad[o] = sat_bad[y * ww + x + 1] + rb;
        }
    }
    let rect = |t: &[f64], x0: usize, y0: usize, x1: usize, y1: usize| t[y1 * ww + x1] + t[y0 * ww + x0] - t[y0 * ww + x1] - t[y1 * ww + x0];
    let bad =
        |x0: usize, y0: usize, x1: usize, y1: usize| sat_bad[y1 * ww + x1] + sat_bad[y0 * ww + x0] - sat_bad[y0 * ww + x1] - sat_bad[y1 * ww + x0];
    let mean_at = |x: usize, y: usize, r: usize| -> Option<f32> {
        let (x0, y0, x1, y1) = (x.saturating_sub(r), y.saturating_sub(r), (x + r + 1).min(w), (y + r + 1).min(h));
        let cnt = rect(&sat_n, x0, y0, x1, y1);
        let low = if cnt > 0.0 { (rect(&sat_v, x0, y0, x1, y1) / cnt) as f32 } else { f32::NAN };
        (low > 1e-9 && low.is_finite()).then_some(low)
    };
    // tone for matching: log low-pass at half a patch; detail: relative high-pass at grain scale
    // (a narrow low-pass, so the detail carries grain and fine texture, not edges)
    let rf = ((1.5 * scale).round() as usize).max(1);
    let mut lnlow = vec![f32::NAN; n];
    let mut detail = vec![0.0f32; n];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if !usable[i] {
                continue;
            }
            if let Some(low) = mean_at(x, y, r) {
                lnlow[i] = low.ln();
            }
            if win.state[i] == KNOWN
                && tone[i].is_finite()
                && let Some(fine) = mean_at(x, y, rf)
            {
                detail[i] = (tone[i] / fine - 1.0).clamp(DETAIL_RANGE.0, DETAIL_RANGE.1);
            }
        }
    }
    // the grain of the surroundings: amplitude of synthetic grain, and the detail clamp (what is
    // beyond a few grain sigmas is structure, which the membrane already carries)
    let near = near_own(&win.state, w, h, patch);
    let ring: Vec<f32> = (0..n).filter(|&i| win.state[i] == KNOWN && near[i] && lnlow[i].is_finite()).map(|i| detail[i]).collect();
    let sigma = if ring.len() >= 12 { util::median_sigma(ring).map_or(0.0, |(_, sg)| sg) } else { 0.0 };
    if sigma > 0.0 {
        let lim = DETAIL_SIGMAS * sigma;
        for d in &mut detail {
            *d = d.clamp(-lim, lim);
        }
    }
    let grain_sigma = if ctx.cfg.grain && ctx.cfg.grain_strength > 0.0 { sigma * ctx.cfg.grain_strength as f32 } else { 0.0 };
    // onion distance of OWN pixels from the known boundary (BFS, 8-connected)
    let mut dist = vec![u32::MAX; n];
    let mut queue = std::collections::VecDeque::new();
    for (i, d) in dist.iter_mut().enumerate() {
        if win.state[i] == KNOWN {
            *d = 0;
            queue.push_back(i);
        }
    }
    while let Some(i) = queue.pop_front() {
        let (x, y) = (i % w, i / w);
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                let j = ny * w + nx;
                if win.state[j] == OWN && dist[j] == u32::MAX {
                    dist[j] = dist[i] + 1;
                    queue.push_back(j);
                }
            }
        }
    }
    // blocks with OWN pixels to texture, inner-most last
    let mut blocks: Vec<(u32, usize, usize)> = Vec::new();
    for by in (0..h).step_by(patch) {
        for bx in (0..w).step_by(patch) {
            let mut best = u32::MAX;
            let mut any = false;
            for y in by..(by + patch).min(h) {
                for x in bx..(bx + patch).min(w) {
                    let i = y * w + x;
                    if win.state[i] == OWN && done[i] {
                        best = best.min(dist[i]);
                        any = true;
                    }
                }
            }
            if any {
                blocks.push((best, bx, by));
            }
        }
    }
    blocks.sort_unstable();
    let grow = (patch / 2).max(2);
    let mut dacc = vec![0.0f32; n];
    let mut wacc = vec![0.0f32; n];
    let mut lacc = vec![0.0f32; n];
    for &(_, bx, by) in &blocks {
        let (bx1, by1) = ((bx + patch).min(w), (by + patch).min(h));
        let (tx0, ty0) = (bx.saturating_sub(grow), by.saturating_sub(grow));
        let (tx1, ty1) = ((bx1 + grow).min(w), (by1 + grow).min(h));
        let (tw, th) = (tx1 - tx0, ty1 - ty0);
        // samples: (x, y, detail weight, target detail, tone weight, target log tone)
        let mut tpl: Vec<(usize, usize, f32, f32, f32, f32)> = Vec::new();
        let (mut sum_d, mut sum_t) = (0.0f32, 0.0f32);
        for y in (ty0..ty1).step_by(stride) {
            for x in (tx0..tx1).step_by(stride) {
                let i = y * w + x;
                let (wd, dt) = match win.state[i] {
                    KNOWN if lnlow[i].is_finite() => (1.0, detail[i]),
                    OWN if wacc[i] > 0.0 => (0.5, dacc[i] / wacc[i]),
                    _ => (0.0, 0.0),
                };
                let wt = if lnlow[i].is_finite() { 1.0 } else { 0.0 };
                if wd > 0.0 || wt > 0.0 {
                    tpl.push((x - tx0, y - ty0, wd, dt, wt, if wt > 0.0 { lnlow[i] } else { 0.0 }));
                    sum_d += wd;
                    sum_t += wt;
                }
            }
        }
        if sum_t <= 0.0 {
            continue;
        }
        let kd = if sum_d > 0.0 { 1.0 / sum_d } else { 0.0 };
        let kt = 1.0 / sum_t;
        for t in &mut tpl {
            t.2 *= kd;
            t.4 *= kt;
        }
        let s = search as isize;
        let cost = |ox: isize, oy: isize, limit: f32| -> Option<f32> {
            let (sx0, sy0) = (tx0 as isize + ox, ty0 as isize + oy);
            if sx0 < 0 || sy0 < 0 || sx0 as usize + tw > w || sy0 as usize + th > h || (ox == 0 && oy == 0) {
                return None;
            }
            let (sx0, sy0) = (sx0 as usize, sy0 as usize);
            if bad(sx0, sy0, sx0 + tw, sy0 + th) != 0 {
                return None;
            }
            let mut acc = 0.0f32;
            for &(x, y, wd, dt, wt, lt) in &tpl {
                let j = (sy0 + y) * w + sx0 + x;
                let ls = lnlow[j];
                if !ls.is_finite() {
                    return None;
                }
                let (a, b) = (detail[j] - dt, ls - lt);
                acc += wd * a * a + wt * b * b;
                if acc > limit {
                    return Some(acc);
                }
            }
            Some(acc)
        };
        let mut best: Option<(f32, isize, isize)> = None;
        let coarse = (2 * stride) as isize;
        let mut oy = -s;
        while oy <= s {
            let mut ox = -s;
            while ox <= s {
                let lim = best.map_or(f32::INFINITY, |b| b.0);
                if let Some(c) = cost(ox, oy, lim)
                    && c < lim
                {
                    best = Some((c, ox, oy));
                }
                ox += coarse;
            }
            oy += coarse;
        }
        let mut step = coarse / 2;
        while step >= 1 {
            if let Some((_, cx, cy)) = best {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let lim = best.map_or(f32::INFINITY, |b| b.0);
                        if let Some(c) = cost(cx + dx * step, cy + dy * step, lim)
                            && c < lim
                        {
                            best = Some((c, cx + dx * step, cy + dy * step));
                        }
                    }
                }
            }
            step /= 2;
        }
        // tone mismatch of the winner alone (rms of the log low-pass over the grown block)
        // and whether its detail really continues the texture of the surroundings (then its fine
        // structure is copied whole, not just its grain)
        let source = best.and_then(|(_, ox, oy)| {
            let (mut e, mut mis, mut var) = (0.0f32, 0.0f32, 0.0f32);
            for &(x, y, wd, dt, wt, lt) in &tpl {
                let j = ((ty0 + y) as isize + oy) as usize * w + ((tx0 + x) as isize + ox) as usize;
                e += wt * (lnlow[j] - lt).powi(2);
                mis += wd * (detail[j] - dt).powi(2);
                var += wd * dt * dt;
            }
            // ...where the target's tone is locally flat: at an edge, a copied patch's own edge
            // never lines up exactly, and its low-pass ratio would blur the membrane's edge
            let (lo, hi) = tpl.iter().filter(|t| t.4 > 0.0).fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), t| (lo.min(t.5), hi.max(t.5)));
            let textured = sum_d > 0.0 && var > 1e-8 && mis < STRONG_MATCH * var && hi - lo < FLAT_TONE;
            (e.sqrt() <= TONE_TOLERANCE).then_some((ox, oy, textured))
        });
        if source.is_none() && grain_sigma <= 0.0 {
            continue;
        }
        // feathered contribution over the grown block: 1 on the block, falling off outside it
        let ramp = |v: usize, a: usize, b: usize| -> f32 {
            let out = if v < a {
                a - v
            } else if v >= b {
                v + 1 - b
            } else {
                0
            };
            1.0 - out as f32 / (grow + 1) as f32
        };
        for y in ty0..ty1 {
            for x in tx0..tx1 {
                let i = y * w + x;
                if win.state[i] != OWN || !done[i] {
                    continue;
                }
                // target luma: membrane × grain-scale detail; for a textured match, the source
                // itself with its tone moved to the target's (low-pass ratio)
                let (base, d) = match source {
                    Some((ox, oy, textured)) => {
                        let j = ((y as isize + oy) as usize) * w + (x as isize + ox) as usize;
                        if textured && lnlow[i].is_finite() && lnlow[j].is_finite() {
                            (lnlow[i].exp(), (tone[j] / lnlow[j].exp() - 1.0).clamp(DETAIL_RANGE.0, DETAIL_RANGE.1))
                        } else {
                            (tone[i], detail[j])
                        }
                    }
                    None => (tone[i], grain_sigma * util::gauss(win.global(i, ctx.width) as u64 ^ seed.rotate_left(17))),
                };
                let v = base * (1.0 + d);
                if !v.is_finite() {
                    continue;
                }
                let k = ramp(x, bx, bx1) * ramp(y, by, by1);
                lacc[i] += k * v;
                dacc[i] += k * d;
                wacc[i] += k;
            }
        }
    }
    for i in 0..n {
        if win.state[i] == OWN && done[i] && wacc[i] > 0.0 && tone[i] > 0.0 {
            // the same factor in every channel: the membrane's colour ratios are kept
            let m = (lacc[i] / wacc[i] / tone[i]).clamp(1.0 + DETAIL_RANGE.0, 1.0 + DETAIL_RANGE.1);
            if m.is_finite() {
                for c in 0..3 {
                    win.rgb[i][c] *= m;
                }
            }
        }
    }
}

/// Pixels within `r` (square) of an OWN pixel (separable running counts, O(window)).
fn near_own(state: &[u8], w: usize, h: usize, r: usize) -> Vec<bool> {
    let dilate_1d = |src: &[bool], dst: &mut [bool]| {
        let n = src.len();
        let mut pre = vec![0u32; n + 1];
        for i in 0..n {
            pre[i + 1] = pre[i] + u32::from(src[i]);
        }
        for (i, d) in dst.iter_mut().enumerate() {
            *d = pre[(i + r + 1).min(n)] > pre[i.saturating_sub(r)];
        }
    };
    let own: Vec<bool> = state.iter().map(|&s| s == OWN).collect();
    let mut rows = vec![false; w * h];
    for y in 0..h {
        dilate_1d(&own[y * w..(y + 1) * w], &mut rows[y * w..(y + 1) * w]);
    }
    let mut out = vec![false; w * h];
    let (mut col, mut res) = (vec![false; h], vec![false; h]);
    for x in 0..w {
        for y in 0..h {
            col[y] = rows[y * w + x];
        }
        dilate_1d(&col, &mut res);
        for y in 0..h {
            out[y * w + x] = res[y];
        }
    }
    out
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn ctx_parts(w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> Vec<f32> {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let v = f(x, y);
                rgb.extend_from_slice(&[v, v * 0.8, v * 1.2]);
            }
        }
        rgb
    }

    pub fn run(tier: Tier, f: impl Fn(usize, usize) -> f32, hole: impl Fn(usize, usize) -> bool) -> (Vec<f32>, Vec<f32>, Vec<usize>) {
        let (w, h) = (96, 80);
        let truth = ctx_parts(w, h, &f);
        let mut rgb = truth.clone();
        let mut mask = Mask::empty(w, h).unwrap();
        let mut pixels = Vec::new();
        let mut bbox = [usize::MAX, usize::MAX, 0, 0];
        for y in 0..h {
            for x in 0..w {
                if hole(x, y) {
                    mask.set(x, y, true);
                    pixels.push(y * w + x);
                    rgb[3 * (y * w + x)..3 * (y * w + x) + 3].copy_from_slice(&[0.01, 0.01, 0.01]);
                    bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x + 1), bbox[3].max(y + 1)];
                }
            }
        }
        let luma = crate::util::luma(w, h, &rgb).unwrap();
        let cfg = HealConfig { grain: false, ..HealConfig::default() };
        let ctx = FillContext { width: w, height: h, rgb: &rgb, forbidden: &mask, luma: &luma, cfg: &cfg, scale: 1.0 };
        let writes = fill_component(&ctx, &pixels, bbox, tier, 1);
        assert_eq!(writes.len(), pixels.len(), "every pixel filled");
        let mut out = rgb.clone();
        for (i, v) in writes {
            assert!(mask.data()[i], "wrote outside the component");
            out[3 * i..3 * i + 3].copy_from_slice(&v);
        }
        (out, truth, pixels)
    }

    pub fn mean_err(out: &[f32], truth: &[f32], px: &[usize]) -> f32 {
        px.iter().map(|&i| (0..3).map(|c| (out[3 * i + c] - truth[3 * i + c]).abs()).sum::<f32>() / 3.0).sum::<f32>() / px.len() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn smooth_tiers_reproduce_gradients() {
        let ramp = |x: usize, y: usize| 0.2 + 0.004 * x as f32 + 0.002 * y as f32;
        let (o, t, p) = run(Tier::Small, ramp, |x, y| (x as i32 - 40).pow(2) + (y as i32 - 40).pow(2) <= 4);
        assert!(mean_err(&o, &t, &p) < 0.003, "{}", mean_err(&o, &t, &p));
        let (o, t, p) = run(Tier::Medium, ramp, |x, y| (30..42).contains(&x) && (30..45).contains(&y));
        assert!(mean_err(&o, &t, &p) < 0.003, "{}", mean_err(&o, &t, &p));
        // colour ratios of the surroundings are kept (all channels filled consistently)
        let i = p[p.len() / 2];
        assert!((o[3 * i + 1] / o[3 * i] - 0.8).abs() < 1e-3);
    }

    #[test]
    fn exemplar_continues_texture() {
        // vertical stripes of period 6: a smooth fill would average them to grey
        let stripes = |x: usize, _y: usize| if (x / 3).is_multiple_of(2) { 0.6 } else { 0.3 };
        let hair = |x: usize, y: usize| (y as i32 - (20 + x as i32 / 3)).abs() <= 2 && (10..80).contains(&x);
        let (o, t, p) = run(Tier::Large, stripes, hair);
        let err = mean_err(&o, &t, &p);
        let (om, tm, pm) = run(Tier::Medium, stripes, hair);
        let smooth = mean_err(&om, &tm, &pm);
        assert!(err < 0.02, "exemplar {err}");
        assert!(smooth > 4.0 * err, "membrane {smooth} vs exemplar {err}");
    }
}
