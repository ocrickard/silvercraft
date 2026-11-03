//! Fill quality on synthetic textured scenes: no blocky patchwork, the tone of each side of an
//! edge kept, edges continued through a defect, and clean frame borders.

use silvercraft_film::RgbImage;
use silvercraft_heal::fill::{FillContext, fill_component};
use silvercraft_heal::{DefectMask, HealConfig, IrPlane, Tier, heal};

fn hash(x: usize, y: usize, k: u64) -> f32 {
    let mut z = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ k;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
}

/// Smooth value noise in [-0.5, 0.5] with features about `cell` pixels across.
fn value_noise(x: usize, y: usize, cell: f32, k: u64) -> f32 {
    let (fx, fy) = (x as f32 / cell, y as f32 / cell);
    let (ix, iy) = (fx.floor() as usize, fy.floor() as usize);
    let (tx, ty) = (fx - ix as f32, fy - iy as f32);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (s(tx), s(ty));
    let a = hash(ix, iy, k) * (1.0 - sx) + hash(ix + 1, iy, k) * sx;
    let b = hash(ix, iy + 1, k) * (1.0 - sx) + hash(ix + 1, iy + 1, k) * sx;
    a * (1.0 - sy) + b * sy
}

const W: usize = 200;
const H: usize = 170;

/// A dark textured disc (a head of dark hair) on a light textured background, both with grain
/// (linear transmittance, so "dark" is just the lower value).
#[derive(Clone, Copy)]
struct Head {
    cx: f32,
    cy: f32,
    r: f32,
}

/// A child's head: its outline curves within a hair's length.
const SMALL: Head = Head { cx: 95.0, cy: 90.0, r: 42.0 };
/// A larger dark region with a gentler outline.
const LARGE: Head = Head { cx: 47.0, cy: 90.0, r: 90.0 };

impl Head {
    /// Signed distance from the outline, positive outside.
    fn dist(&self, x: usize, y: usize) -> f32 {
        ((x as f32 - self.cx).powi(2) + (y as f32 - self.cy).powi(2)).sqrt() - self.r
    }

    fn tone(&self, x: usize, y: usize) -> f32 {
        let base = if self.dist(x, y) < 0.0 { 0.06 } else { 0.32 };
        base * (1.0 + 0.25 * value_noise(x, y, 6.0, 7)) * (1.0 + 0.08 * hash(x, y, 3))
    }
}

struct Filled {
    out: Vec<f32>,
    truth: Vec<f32>,
    mask: Vec<bool>,
}

/// Fill one component (as `heal` would) and return the result next to the truth.
fn fill(tier: Tier, f: impl Fn(usize, usize) -> f32, hole: impl Fn(usize, usize) -> bool) -> Filled {
    let mut truth = Vec::with_capacity(W * H * 3);
    for y in 0..H {
        for x in 0..W {
            let v = f(x, y);
            truth.extend_from_slice(&[v * 1.1, v, v * 0.85]);
        }
    }
    let mut rgb = truth.clone();
    let mut mask = DefectMask::empty(W, H).unwrap();
    let mut pixels = Vec::new();
    let mut bbox = [usize::MAX, usize::MAX, 0, 0];
    for y in 0..H {
        for x in 0..W {
            if hole(x, y) {
                mask.set(x, y, true);
                let i = y * W + x;
                pixels.push(i);
                rgb[3 * i..3 * i + 3].copy_from_slice(&[0.01, 0.01, 0.01]);
                bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x + 1), bbox[3].max(y + 1)];
            }
        }
    }
    let luma = IrPlane::new(W, H, rgb.chunks_exact(3).map(|p| (p[0] + p[1] + p[2]) / 3.0).collect()).unwrap();
    let cfg = HealConfig::default();
    let ctx = FillContext { width: W, height: H, rgb: &rgb, forbidden: &mask, luma: &luma, cfg: &cfg, scale: 1.0 };
    let writes = fill_component(&ctx, &pixels, bbox, tier, 11);
    assert_eq!(writes.len(), pixels.len(), "every pixel filled");
    let mut out = rgb.clone();
    for (i, v) in writes {
        assert!(mask.data()[i], "wrote outside the component");
        out[3 * i..3 * i + 3].copy_from_slice(&v);
    }
    Filled { out, truth, mask: mask.data().to_vec() }
}

fn luma(v: &[f32], i: usize) -> f32 {
    (v[3 * i] + v[3 * i + 1] + v[3 * i + 2]) / 3.0
}

/// Relative tone error of the fill, low-passed (box radius 3, over filled pixels only), at filled
/// pixels at least `keep_off` px from the edge: (rms, max). Grain averages out; a block of the
/// wrong tone does not.
fn lowpass_error(f: &Filled, head: Head, keep_off: f32) -> (f32, f32) {
    let rel: Vec<f32> = (0..W * H).map(|i| if f.mask[i] { luma(&f.out, i) / luma(&f.truth, i) - 1.0 } else { 0.0 }).collect();
    let (mut acc, mut n, mut worst) = (0.0f32, 0.0f32, 0.0f32);
    for y in 3..H - 3 {
        for x in 3..W - 3 {
            let i = y * W + x;
            if !f.mask[i] || head.dist(x, y).abs() < keep_off {
                continue;
            }
            let (mut s, mut c) = (0.0f32, 0.0f32);
            for yy in y - 3..=y + 3 {
                for xx in x - 3..=x + 3 {
                    let j = yy * W + xx;
                    // only pixels on the same side of the edge
                    if f.mask[j] && (head.dist(xx, yy) < 0.0) == (head.dist(x, y) < 0.0) {
                        s += rel[j];
                        c += 1.0;
                    }
                }
            }
            let e = s / c;
            acc += e * e;
            n += 1.0;
            worst = worst.max(e.abs());
        }
    }
    ((acc / n.max(1.0)).sqrt(), worst)
}

/// Mean filled tone over mean true tone, minus 1, on each side of the edge (pixels at least
/// `keep_off` px from it).
fn side_bias(f: &Filled, head: Head, keep_off: f32) -> [f32; 2] {
    let mut acc = [[0.0f32; 2]; 2];
    for y in 0..H {
        for x in 0..W {
            let i = y * W + x;
            let d = head.dist(x, y);
            if !f.mask[i] || d.abs() < keep_off {
                continue;
            }
            let side = usize::from(d > 0.0);
            acc[side][0] += luma(&f.out, i);
            acc[side][1] += luma(&f.truth, i);
        }
    }
    [acc[0][0] / acc[0][1] - 1.0, acc[1][0] / acc[1][1] - 1.0]
}

/// A long wavy hair, 5 px wide, crossing the small head and its outline twice.
fn hair(x: usize, y: usize) -> bool {
    let yc = 70.0 + 10.0 * (x as f32 / 17.0).sin();
    (20..185).contains(&x) && (y as f32 - yc).abs() <= 2.5
}

/// A big blob (28 px across) on the outline of the large head.
fn blob(x: usize, y: usize) -> bool {
    let a = 0.6f32;
    let (cx, cy) = (LARGE.cx + LARGE.r * a.cos(), LARGE.cy + LARGE.r * a.sin());
    (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2) <= 14.0f32.powi(2)
}

#[test]
fn large_fill_across_an_edge_is_not_blocky() {
    // the old block-wise patch copy left squares of the wrong tone (light background texture in
    // the dark hair): its low-passed error was several times these limits on both
    let cases: [(&str, Head, &dyn Fn(usize, usize) -> bool, f32); 2] = [("hair", SMALL, &hair, 4.0), ("blob", LARGE, &blob, 6.0)];
    for (name, head, hole, keep_off) in cases {
        let f = fill(Tier::Large, |x, y| head.tone(x, y), hole);
        let (rms, worst) = lowpass_error(&f, head, keep_off);
        let bias = side_bias(&f, head, keep_off);
        assert!(rms < 0.06, "{name}: low-passed tone error rms {rms}");
        assert!(worst < 0.2, "{name}: low-passed tone error max {worst}");
        assert!(bias[0].abs() < 0.05 && bias[1].abs() < 0.05, "{name}: tone of the dark / light side off by {bias:?}");
    }
}

#[test]
fn large_fill_keeps_texture_on_each_side() {
    // the fill is not a smooth blob: its local contrast is of the order of the surroundings'
    let f = fill(Tier::Large, |x, y| SMALL.tone(x, y), hair);
    let contrast = |v: &[f32]| {
        let (mut acc, mut n) = (0.0f32, 0.0f32);
        for y in 1..H - 1 {
            for x in 1..W - 1 {
                let i = y * W + x;
                if f.mask[i] && f.mask[i + 1] && SMALL.dist(x, y).abs() > 4.0 {
                    let r = luma(v, i + 1) / luma(v, i) - 1.0;
                    acc += r * r;
                    n += 1.0;
                }
            }
        }
        (acc / n).sqrt()
    };
    let (filled, truth) = (contrast(&f.out), contrast(&f.truth));
    assert!(filled > 0.5 * truth && filled < 1.6 * truth, "pixel-to-pixel contrast {filled} vs {truth}");
}

#[test]
fn smooth_fill_follows_an_edge_through_the_defect() {
    // a speck straddling a straight, sharp edge (the clear rebate next to the image): filled
    // along the edge, not blended across it into a grey bump
    let step = |x: usize, y: usize| (if x < 120 { 0.06 } else { 0.14 }) * (1.0 + 0.04 * hash(x, y, 5));
    let hole = |x: usize, y: usize| (106..124).contains(&x) && (70..84).contains(&y);
    for tier in [Tier::Small, Tier::Medium, Tier::Large] {
        let f = fill(tier, step, hole);
        let mut worst = 0.0f32;
        for y in 70..84 {
            for x in 106..124 {
                let i = y * W + x;
                let want = if x < 120 { 0.06 } else { 0.14 };
                worst = worst.max(((luma(&f.out, i) / 0.983) / want - 1.0).abs());
            }
        }
        // grain alone is a few percent; a blend across the edge is tens of percent
        assert!(worst < 0.25, "{tier:?}: worst relative error {worst}");
    }
}

/// A synthetic 600 dpi capture of a frame corner: film (grain, a smooth scene) with a clear
/// rebate band, the opaque holder on the right (with a semi-opaque lip in the IR) and open lamp
/// below the end of the strip. Returns (rgb, ir, a speck mask).
fn border_frame() -> (RgbImage, IrPlane, Vec<bool>) {
    let (w, h) = (220, 200);
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut ir = Vec::with_capacity(w * h);
    let mut speck = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let grain = 1.0 + 0.05 * hash(x, y, 9);
            // regions: image (x < 170), rebate (170..185), lip (185..189), holder (>= 189);
            // open lamp below y >= 175 (where the strip ends)
            let (t, i) = if y >= 175 && x < 189 {
                (0.85, 1.25)
            } else if x >= 189 {
                (0.002, 0.03)
            } else if x >= 185 {
                (0.004, 0.45)
            } else if x >= 170 {
                (0.20 * grain, 1.0)
            } else {
                (0.07 * (1.0 + 0.2 * value_noise(x, y, 12.0, 4)) * grain, 1.0)
            };
            // a dust speck on the rebate, a few pixels from the holder, and one in the image
            let on = ((x as f32 - 178.0).powi(2) + (y as f32 - 80.0).powi(2)).sqrt() <= 2.5
                || ((x as f32 - 90.0).powi(2) + (y as f32 - 90.0).powi(2)).sqrt() <= 2.5;
            let occ = if on { 0.9 } else { 0.0 };
            speck.push(on);
            rgb.extend_from_slice(&[t * 1.1 * (1.0 - occ), t * (1.0 - occ), t * 0.85 * (1.0 - occ)]);
            ir.push(0.7 * i * (1.0 - occ) * (1.0 + 0.01 * hash(x, y, 2)));
        }
    }
    (RgbImage::new(w, h, rgb).unwrap(), IrPlane::new(w, h, ir).unwrap(), speck)
}

#[test]
fn frame_borders_are_not_defects_and_stay_clean() {
    let (rgb, ir, speck) = border_frame();
    let (w, h) = (rgb.width(), rgb.height());
    let res = heal(&rgb, Some(&ir), &HealConfig::default()).unwrap();
    // the holder lip and the film next to the open lamp are not detected as long "scratches"
    let along_border = (0..w * h).filter(|&i| res.mask.data()[i] && !speck[i] && (i % w >= 183 || i / w >= 170)).count();
    let speck_px = speck.iter().filter(|&&s| s).count();
    assert!(along_border < speck_px, "{along_border} mask pixels along the frame border");
    assert!(res.defects.iter().all(|d| d.length < 40.0), "{:?}", res.defects);
    // both specks found
    let missed = (0..w * h).filter(|&i| speck[i] && !res.mask.data()[i]).count();
    assert!(missed <= 2, "{missed} speck pixels missed");
    // the rebate speck is filled with rebate tone, not image tone or holder tone: no notch
    let mut worst = 0.0f32;
    for y in 74..87 {
        for x in 174..184 {
            let i = y * w + x;
            if res.mask.data()[i] {
                let v = res.image.data()[3 * i + 1];
                worst = worst.max((v / 0.20 - 1.0).abs());
            }
        }
    }
    assert!(worst < 0.2, "rebate fill off by {worst}");
}

/// A dense object on the negative (a light float in the positive) with a tiny IR speck inside it,
/// near its edge. Returns (rgb, ir, object mask).
fn dense_object() -> (RgbImage, IrPlane, Vec<bool>) {
    let (w, h) = (160, 140);
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut ir = Vec::with_capacity(w * h);
    let mut object = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let grain = 1.0 + 0.05 * hash(x, y, 13);
            let inside = ((x as f32 - 80.0) / 6.0).powi(2) + ((y as f32 - 70.0) / 12.0).powi(2) <= 1.0;
            object.push(inside);
            let t = if inside { 0.10 } else { 0.18 } * grain;
            // the speck: IR-opaque, 2 px, a few px inside the object's lower end
            let speck = ((x as f32 - 80.0).powi(2) + (y as f32 - 78.0).powi(2)).sqrt() <= 1.5;
            let occ = if speck { 0.6 } else { 0.0 };
            rgb.extend_from_slice(&[t * 1.1 * (1.0 - occ), t * (1.0 - occ), t * 0.85 * (1.0 - occ)]);
            // XP2-like leakage: the object is a little denser in IR as well
            let leak = (t / 0.18f32).powf(0.08);
            ir.push(0.7 * leak * (1.0 - occ) * (1.0 + 0.01 * hash(x, y, 2)));
        }
    }
    (RgbImage::new(w, h, rgb).unwrap(), IrPlane::new(w, h, ir).unwrap(), object)
}

#[test]
fn visible_growth_does_not_erase_dense_image_around_a_speck() {
    let (rgb, ir, object) = dense_object();
    let masked = |cfg: &HealConfig| {
        let res = heal(&rgb, Some(&ir), cfg).unwrap();
        assert!(res.mask.get(80, 78), "the speck is repaired");
        object.iter().zip(res.mask.data()).filter(|(o, m)| **o && **m).count()
    };
    let object_px = object.iter().filter(|&&o| o).count();
    // the IR defect alone (no visible growth) is the most that should be rewritten
    let ir_only = masked(&HealConfig { visible_grow: 0.0, ..HealConfig::default() });
    let with = masked(&HealConfig::default());
    assert!(with <= ir_only + 4 && 4 * with < object_px, "{with} of {object_px} object pixels masked (IR alone: {ir_only})");
}
