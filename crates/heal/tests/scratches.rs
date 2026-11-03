//! Long-scratch ("rain") repair on synthetic grainy negatives: light and dark scratches 1-3 px
//! wide, slanted, with gaps and with varying contrast, next to genuine image lines that must be
//! left alone (a wire between two backgrounds, a wire that ends, a building edge, a tone boundary,
//! the frame edge and the holder).

use silvercraft_film::RgbImage;
use silvercraft_heal::scratches::{LineAxis, LineRepair, LineScratch};
use silvercraft_heal::{HealConfig, IrPlane, Kind, ScratchAxis, ScratchRepair, Tier, heal};

const W: usize = 420;
const H: usize = 1000;

fn hash(x: usize, y: usize, k: u64) -> f32 {
    let mut z = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ k;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

/// Approximately normal grain deviate (sum of four uniforms, unit variance).
fn grain(x: usize, y: usize) -> f32 {
    grain_seeded(x, y, 0)
}

fn grain_seeded(x: usize, y: usize, seed: u64) -> f32 {
    let s: f32 = (0..4).map(|k| hash(x, y, k + 4 * seed)).sum::<f32>() - 2.0;
    s * 3.0f32.sqrt()
}

/// One injected scratch: centre `x0 + slope·y`, Gaussian-ish profile of `width` px, transmittance
/// factor `1 + gain(y)` (positive gain = more transparent = dark in the positive).
#[derive(Clone, Copy)]
struct Scratch {
    x0: f32,
    slope: f32,
    width: f32,
    gain: fn(usize) -> f32,
    gaps: &'static [(usize, usize)],
}

impl Scratch {
    fn centre(&self, y: usize) -> f32 {
        self.x0 + self.slope * y as f32
    }
    fn present(&self, y: usize) -> bool {
        !self.gaps.iter().any(|&(a, b)| y >= a && y < b)
    }
    /// Coverage (0..1) of pixel `x` by the scratch on row `y` (box profile, 8× supersampled).
    fn cover(&self, x: usize, y: usize) -> f32 {
        let c = self.centre(y);
        (0..8).filter(|k| ((x as f32 - 0.5 + (*k as f32 + 0.5) / 8.0) - c).abs() <= self.width / 2.0).count() as f32 / 8.0
    }
}

fn scratches() -> Vec<Scratch> {
    vec![
        // a light (emulsion) scratch on the negative, 1 px, vertical, strong
        Scratch { x0: 50.0, slope: 0.0, width: 1.2, gain: |_| 0.08, gaps: &[] },
        // a dark (base) scratch, 2 px, slanted 1.5°
        Scratch { x0: 75.0, slope: 0.0262, width: 2.0, gain: |_| -0.035, gaps: &[] },
        // 3 px, slanted -1°, with two gaps
        Scratch { x0: 150.0, slope: -0.01745, width: 3.0, gain: |_| -0.03, gaps: &[(300, 360), (650, 700)] },
        // 2 px, varying contrast
        Scratch { x0: 240.0, slope: 0.0, width: 2.0, gain: |y| 0.015 + 0.03 * (0.5 + 0.5 * (y as f32 / 120.0).sin()), gaps: &[] },
    ]
}

/// Genuine image lines near which nothing may change: `(x at row y, rows)`.
fn features() -> Vec<(fn(usize) -> f32, std::ops::Range<usize>)> {
    vec![
        (|_| 30.0, 0..H),                                 // frame edge (rebate | image)
        (|_| 190.0, 0..H),                                // wire between two backgrounds
        (|_| 215.0, 0..220),                              // wire that ends
        (|_| 290.0, 0..H),                                // tone boundary
        (|y| 340.0 + 0.012 * (y as f32 - 400.0), 400..H), // building edge
        (|_| 315.0, 0..500),                              // a pole that ends half way
        (|_| 265.0, 0..H),                                // a soft 5 px band, too wide
        (|_| 378.0, 0..H),                                // a wire whose contrast follows the image
    ]
}

/// Clean negative transmittance (before grain) at `(x, y)`.
fn scene(x: usize, y: usize) -> f32 {
    let (xf, yf) = (x as f32, y as f32);
    if !(12..408).contains(&x) {
        return 0.002; // opaque holder
    }
    if x < 30 {
        return 0.85; // clear rebate
    }
    // a faint fringe (halation) 6 px inside the strong frame edge
    let fringe = if (35..=36).contains(&x) { 0.97 } else { 1.0 };
    let mut t = 0.35 * (1.0 + 0.12 * (yf / 90.0).sin()) * (1.0 + 0.1 * xf / W as f32) * fringe;
    let step = |e: f32, soft: f32| 1.0 / (1.0 + (-(xf - e) / soft).exp());
    // wire on a boundary: right of 190 is darker, and a dense 2 px wire sits on it
    t *= 1.0 - 0.3 * step(190.0, 0.6);
    if (xf - 190.0).abs() <= 1.0 {
        t *= 0.75;
    }
    // a wire that ends
    if (xf - 215.0).abs() <= 1.0 && y < 220 {
        t *= 0.7;
    }
    // tone boundary
    t *= 1.0 - 0.5 * step(290.0, 1.5);
    // building edge, from row 400 down
    if y >= 400 {
        t *= 1.0 - 0.45 * step(340.0 + 0.012 * (yf - 400.0), 0.8);
    }
    // a faint 5 px band (an out-of-focus line, a processing streak), full length
    if (xf - 265.0).abs() <= 2.0 {
        t *= 0.97;
    }
    // a 2 px pole over the same background on both sides, ending half way down
    if (xf - 315.0).abs() <= 1.0 && y < 500 {
        t *= 0.8;
    }
    // a full-length 2 px wire whose contrast comes and goes with the scene
    if (xf - 378.0).abs() <= 1.0 {
        t *= 1.0 - 0.25 * (0.5 + 0.5 * (yf / 150.0).sin());
    }
    t
}

struct Fixture {
    clean: RgbImage,
    dirty: RgbImage,
}

fn fixture(scr: &[Scratch]) -> Fixture {
    let mut clean = Vec::with_capacity(W * H * 3);
    let mut dirty = Vec::with_capacity(W * H * 3);
    for y in 0..H {
        for x in 0..W {
            let t = scene(x, y) * (1.0 + 0.015 * grain(x, y));
            let mut f = 1.0;
            for s in scr {
                if s.present(y) {
                    f *= 1.0 + (s.gain)(y) * s.cover(x, y);
                }
            }
            for k in [1.0, 0.8, 0.6] {
                clean.push(t * k);
                dirty.push(t * k * f);
            }
        }
    }
    Fixture { clean: RgbImage::new(W, H, clean).unwrap(), dirty: RgbImage::new(W, H, dirty).unwrap() }
}

fn cfg() -> HealConfig {
    HealConfig { dpi: Some(600.0), scratches: true, scratch_axis: ScratchAxis::Vertical, ..HealConfig::default() }
}

fn density(img: &RgbImage, x: usize, y: usize) -> f32 {
    let p = &img.data()[3 * (y * W + x)..3 * (y * W + x) + 3];
    -((p[0] + p[1] + p[2]) / 3.0).ln()
}

/// Centre of a found line at `along`, from its knots.
fn line_at(l: &LineScratch, along: f32) -> f32 {
    let k = &l.knots;
    let i = k.iter().position(|p| p[0] >= along).unwrap_or(k.len() - 1).max(1).min(k.len() - 1);
    if k.len() == 1 {
        return k[0][1];
    }
    let (a, b) = (k[i - 1], k[i]);
    a[1] + (b[1] - a[1]) * (along - a[0]) / (b[0] - a[0])
}

fn found<'a>(lines: &'a [LineScratch], s: &Scratch) -> Option<&'a LineScratch> {
    let mid = H / 2;
    lines.iter().find(|l| (l.start as f32) < mid as f32 && l.end > mid && (line_at(l, mid as f32) - s.centre(mid)).abs() <= 2.0)
}

#[test]
fn finds_and_repairs_rain_but_not_image_lines() {
    let scr = scratches();
    let f = fixture(&scr);
    let res = heal(&f.dirty, None, &cfg()).unwrap();
    assert_eq!(res.lines.len(), scr.len(), "exactly the injected scratches: {:#?}", res.lines);
    assert_eq!(res.stats.line_scratches, scr.len());
    assert!(res.stats.line_coverage_pct > 0.5 && res.stats.line_coverage_pct < 5.0, "{}", res.stats.line_coverage_pct);
    assert_eq!(res.defects.iter().filter(|d| d.kind == Kind::Scratch).count(), scr.len());
    for (k, s) in scr.iter().enumerate() {
        let l = found(&res.lines, s).unwrap_or_else(|| panic!("scratch {k} not found: {:#?}", res.lines));
        assert_eq!(l.axis, LineAxis::Vertical);
        assert!(l.end - l.start >= H * 8 / 10, "scratch {k} covers {}..{}", l.start, l.end);
        assert!(l.width <= 4.5, "scratch {k} width {}", l.width);
        let light = (s.gain)(0) > 0.0;
        assert_eq!(l.contrast < 0.0, light, "scratch {k} polarity");
        // near-ground-truth repair on the scratch core (where it is present)
        let (mut before, mut after, mut n) = (0.0f32, 0.0f32, 0.0f32);
        for y in (l.start..l.end).filter(|&y| s.present(y) && y > 40 && y < H - 40) {
            let x = s.centre(y).round() as usize;
            if s.cover(x, y) < 0.9 {
                continue;
            }
            before += (density(&f.dirty, x, y) - density(&f.clean, x, y)).abs();
            after += (density(&res.image, x, y) - density(&f.clean, x, y)).abs();
            n += 1.0;
        }
        let (before, after) = (before / n, after / n);
        let limit = match l.repair {
            LineRepair::Corrected => 0.3 * before,
            // interpolation replaces the pixel: its error is the grain it cannot know
            LineRepair::Interpolated => 0.6 * before,
        };
        assert!(after < limit, "scratch {k} ({:?}): error {before:.4} -> {after:.4}", l.repair);
    }
    // the strong scratch is interpolated, the faint ones corrected
    assert_eq!(found(&res.lines, &scr[0]).unwrap().repair, LineRepair::Interpolated);
    assert_eq!(found(&res.lines, &scr[1]).unwrap().repair, LineRepair::Corrected);
    assert!(res.defects.iter().any(|d| d.tier == Tier::Interpolated) && res.defects.iter().any(|d| d.tier == Tier::Corrected));
    // genuine lines are untouched
    for (fx, rows) in features() {
        for y in rows {
            let x = fx(y).round() as isize;
            for dx in -3..=3 {
                let xx = (x + dx) as usize;
                assert!(!res.mask.get(xx, y), "image line at ({xx}, {y}) was masked");
            }
        }
    }
    // bit-identical outside the mask
    for i in 0..W * H {
        if !res.mask.data()[i] {
            for c in 0..3 {
                assert_eq!(res.image.data()[3 * i + c].to_bits(), f.dirty.data()[3 * i + c].to_bits(), "pixel {i} changed outside the mask");
            }
        }
    }
}

#[test]
fn image_lines_alone_are_not_scratches() {
    let f = fixture(&[]);
    let res = heal(&f.dirty, None, &cfg()).unwrap();
    assert!(res.lines.is_empty(), "{:#?}", res.lines);
    assert_eq!(res.image.data(), f.dirty.data());
    let auto = heal(&f.dirty, None, &HealConfig { scratch_axis: ScratchAxis::Auto, ..cfg() }).unwrap();
    assert!(auto.lines.is_empty(), "{:#?}", auto.lines);
}

#[test]
fn clumpy_grain_alone_is_not_scratches() {
    // dye-cloud-like grain (white noise blurred over 3×3) on a 1200 dpi frame with soft image
    // structure; several grain seeds, both orientations searched
    let (w, h) = (700, 2400);
    for seed in 0..3 {
        let white: Vec<f32> = (0..w * h).map(|i| grain_seeded(i % w, i / w, seed + 1)).collect();
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let mut g = 0.0;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (xx, yy) = ((x as i32 + dx).clamp(0, w as i32 - 1) as usize, (y as i32 + dy).clamp(0, h as i32 - 1) as usize);
                        g += white[yy * w + xx] / 3.0;
                    }
                }
                let t = 0.3 * (1.0 + 0.3 * (x as f32 / 150.0).sin() * (y as f32 / 400.0).cos()) * (1.0 + 0.012 * g);
                rgb.extend_from_slice(&[t, 0.8 * t, 0.6 * t]);
            }
        }
        let img = RgbImage::new(w, h, rgb).unwrap();
        let c = HealConfig { dpi: Some(1200.0), scratch_axis: ScratchAxis::Auto, ..cfg() };
        let res = heal(&img, None, &c).unwrap();
        assert!(res.lines.is_empty(), "seed {seed}: {:#?}", res.lines);
    }
}

#[test]
fn minimum_length_is_honoured() {
    // one faint 2 px scratch over 550 rows: a scratch at the default 300 px, image detail at 600
    let (w, h) = (200, 1000);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let line = (150..700).contains(&y) && (100..102).contains(&x);
            let t = 0.35 * (1.0 + 0.015 * grain(x, y)) * if line { 0.988 } else { 1.0 };
            rgb.extend_from_slice(&[t, 0.8 * t, 0.6 * t]);
        }
    }
    let img = RgbImage::new(w, h, rgb).unwrap();
    let res = heal(&img, None, &cfg()).unwrap();
    assert_eq!(res.lines.len(), 1, "{:#?}", res.lines);
    let l = &res.lines[0];
    assert!(l.start <= 192 && l.end >= 704 && l.end - l.start <= 704, "{}..{}", l.start, l.end);
    assert!(res.mask.get(100, 400) && !res.mask.get(100, 900), "only the scratch's extent is written");
    let long = heal(&img, None, &HealConfig { scratch_length: 600.0, ..cfg() }).unwrap();
    assert!(long.lines.is_empty(), "{:#?}", long.lines);
}

#[test]
fn railings_are_not_scratches() {
    // a railing: eleven thin bars, 8 px apart, through 90 % of the frame, each a strong, steady,
    // narrow line between matching backgrounds; only their busy surroundings give them away
    let (w, h) = (300, 1000);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let bar = (50..950).contains(&y) && (100..188).contains(&x) && (x - 100) % 8 < 2;
            let t = 0.35 * (1.0 + 0.015 * grain(x, y)) * if bar { 0.8 } else { 1.0 };
            rgb.extend_from_slice(&[t, 0.8 * t, 0.6 * t]);
        }
    }
    let img = RgbImage::new(w, h, rgb).unwrap();
    let res = heal(&img, None, &cfg()).unwrap();
    assert!(res.lines.is_empty(), "{:#?}", res.lines);
}

#[test]
fn off_by_default_and_when_disabled() {
    let f = fixture(&scratches());
    assert!(!HealConfig::default().scratches, "long-scratch repair is opt-in");
    for c in [HealConfig { scratches: false, ..cfg() }, HealConfig { enabled: false, ..cfg() }] {
        let res = heal(&f.dirty, None, &c).unwrap();
        assert!(res.lines.is_empty());
        assert_eq!(res.image.data(), f.dirty.data());
    }
}

#[test]
fn horizontal_strips_are_found_by_auto() {
    let f = fixture(&scratches());
    // transpose: the strip now runs left to right
    let t = |img: &RgbImage| {
        let mut out = vec![0.0f32; W * H * 3];
        for y in 0..H {
            for x in 0..W {
                out[3 * (x * H + y)..3 * (x * H + y) + 3].copy_from_slice(&img.data()[3 * (y * W + x)..3 * (y * W + x) + 3]);
            }
        }
        RgbImage::new(H, W, out).unwrap()
    };
    let dirty = t(&f.dirty);
    let res = heal(&dirty, None, &HealConfig { scratch_axis: ScratchAxis::Auto, ..cfg() }).unwrap();
    assert_eq!(res.lines.len(), 4, "{:#?}", res.lines);
    assert!(res.lines.iter().all(|l| l.axis == LineAxis::Horizontal));
    let forced = heal(&dirty, None, &HealConfig { scratch_axis: ScratchAxis::Horizontal, ..cfg() }).unwrap();
    assert_eq!(forced.lines.len(), 4);
    let wrong = heal(&dirty, None, &cfg()).unwrap();
    assert!(wrong.lines.is_empty(), "no vertical scratches in a horizontal strip: {:#?}", wrong.lines);
}

#[test]
fn auto_follows_decisive_evidence_over_the_longer_side() {
    // the vertical strip padded with plain grainy film to 1200 px wide: the longer side is now
    // horizontal, but the four scratches are vertical
    let f = fixture(&scratches());
    let pw = 1200;
    let mut rgb = Vec::with_capacity(pw * H * 3);
    for y in 0..H {
        for x in 0..pw {
            if x < W {
                rgb.extend_from_slice(&f.dirty.data()[3 * (y * W + x)..3 * (y * W + x) + 3]);
            } else {
                let t = 0.35 * (1.0 + 0.015 * grain(x, y));
                rgb.extend_from_slice(&[t, 0.8 * t, 0.6 * t]);
            }
        }
    }
    let wide = RgbImage::new(pw, H, rgb).unwrap();
    let res = heal(&wide, None, &HealConfig { scratch_axis: ScratchAxis::Auto, ..cfg() }).unwrap();
    assert_eq!(res.lines.len(), 4, "{:#?}", res.lines);
    assert!(res.lines.iter().all(|l| l.axis == LineAxis::Vertical));
}

#[test]
fn repair_modes_can_be_forced() {
    let scr = scratches();
    let f = fixture(&scr);
    for (mode, want) in [(ScratchRepair::Correct, LineRepair::Corrected), (ScratchRepair::Interpolate, LineRepair::Interpolated)] {
        let res = heal(&f.dirty, None, &HealConfig { scratch_repair: mode, ..cfg() }).unwrap();
        assert_eq!(res.lines.len(), scr.len());
        assert!(res.lines.iter().all(|l| l.repair == want), "{mode:?}");
        // the varying-contrast scratch is reduced either way
        let s = &scr[3];
        let err = |img: &RgbImage| (100..900).map(|y| (density(img, 240, y) - density(&f.clean, 240, y)).abs()).sum::<f32>() / 800.0;
        // interpolation cannot know the grain under the scratch, correction keeps it
        let limit = if mode == ScratchRepair::Correct { 0.35 } else { 0.75 };
        assert!(err(&res.image) < limit * err(&f.dirty), "{mode:?}: {} vs {}", err(&res.image), err(&f.dirty));
        assert!(found(&res.lines, s).is_some());
    }
}

#[test]
fn runs_after_infrared_repair() {
    let scr = scratches();
    let f = fixture(&scr);
    // IR sees a dust speck but not the scratches
    let ir = IrPlane::from_fn(W, H, |x, y| {
        let film = if (12..408).contains(&x) { 0.9 } else { 0.001 };
        if (x as i32 - 100).pow(2) + (y as i32 - 600).pow(2) <= 9 { film * 0.1 } else { film }
    })
    .unwrap();
    let mut dusty = f.dirty.data().to_vec();
    for y in 597..=603 {
        for x in 97..=103 {
            if (x as i32 - 100).pow(2) + (y as i32 - 600).pow(2) <= 9 {
                for c in 0..3 {
                    dusty[3 * (y * W + x) + c] *= 0.1;
                }
            }
        }
    }
    let dusty = RgbImage::new(W, H, dusty).unwrap();
    let res = heal(&dusty, Some(&ir), &HealConfig { register: false, ..cfg() }).unwrap();
    assert_eq!(res.stats.source, silvercraft_heal::MaskSource::Infrared);
    assert!(res.mask.get(100, 600), "the speck is repaired");
    assert_eq!(res.lines.len(), scr.len(), "{:#?}", res.lines);
    assert!(res.stats.defects > scr.len());
    assert!(res.stats.coverage_pct >= res.stats.line_coverage_pct);
}

#[test]
fn hostile_inputs_never_panic() {
    let f = fixture(&scratches());
    // non-finite, zero and negative pixels inside a scratch and across the frame
    let mut bad = f.dirty.data().to_vec();
    for y in 0..H {
        for (k, x) in [(0, 50usize), (1, 76), (2, 200), (3, 241)] {
            if y % (7 + k) == 0 {
                bad[3 * (y * W + x) + k % 3] = [f32::NAN, f32::INFINITY, 0.0, -1.0][k];
            }
        }
    }
    for x in 0..W {
        bad[3 * (500 * W + x)] = f32::NAN;
    }
    let bad = RgbImage::new(W, H, bad).unwrap();
    let res = heal(&bad, None, &cfg()).unwrap();
    for i in 0..W * H {
        if !res.mask.data()[i] {
            for c in 0..3 {
                assert_eq!(res.image.data()[3 * i + c].to_bits(), bad.data()[3 * i + c].to_bits());
            }
        }
    }
    // degenerate sizes and contents, extreme settings
    let extreme = HealConfig {
        scratch_length: 20.0,
        scratch_max_width: 20.0,
        scratch_sensitivity: 4.0,
        scratch_max_angle: 10.0,
        dpi: Some(4800.0),
        scratch_axis: ScratchAxis::Auto,
        ..cfg()
    };
    for (w, h, v) in [(8, 8, 0.5), (9, 400, 0.5), (400, 9, 0.5), (64, 64, 0.0), (64, 300, f32::NAN), (200, 300, 0.3)] {
        let img = RgbImage::new(w, h, vec![v; w * h * 3]).unwrap();
        for c in [cfg(), extreme.clone(), HealConfig { dpi: Some(50.0), ..cfg() }] {
            let r = heal(&img, None, &c).unwrap();
            assert!(r.lines.is_empty());
        }
    }
    let r = heal(&f.dirty, None, &extreme).unwrap();
    assert!(r.image.data().len() == f.dirty.data().len());
    // invalid scratch settings are config errors, not panics
    for c in [
        HealConfig { scratch_length: f64::NAN, ..cfg() },
        HealConfig { scratch_max_width: 0.0, ..cfg() },
        HealConfig { scratch_sensitivity: 100.0, ..cfg() },
        HealConfig { scratch_max_angle: -1.0, ..cfg() },
    ] {
        assert!(heal(&f.dirty, None, &c).is_err());
    }
}
