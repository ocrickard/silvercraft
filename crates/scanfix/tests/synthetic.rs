//! Synthetic scans: textured film-like images with injected scanner artifacts.

use silvercraft_imgproc::Plane;
use silvercraft_scanfix::*;

const W: usize = 384;
const H: usize = 288;

/// splitmix64 PRNG.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn normal(&mut self) -> f32 {
        (0..4).map(|_| self.uniform()).sum::<f32>() - 2.0
    }
}

/// A film-like textured plane: smooth tones, anisotropic blobs (including thin vertical "poles"
/// and walls), a full-height frame border (a vertical step to the dark holder), a horizontal
/// frame gap, a slanted thin line, grain, and 16-bit quantisation.
fn texture(w: usize, h: usize, seed: u64, channel_gain: f32) -> Plane {
    let mut rng = Rng(seed);
    let blobs: Vec<(f32, f32, f32, f32, f32, f32)> = (0..40)
        .map(|i| {
            let tall = i % 5 == 0;
            // Tall blobs are poles: thin, up to 40 % of the frame, some tilted.
            let (sx, sy) =
                if tall { (1.0 + 2.0 * rng.uniform(), 8.0 + 12.0 * rng.uniform()) } else { (4.0 + 20.0 * rng.uniform(), 4.0 + 20.0 * rng.uniform()) };
            let tilt = if tall && i % 10 == 0 { 0.03 * rng.normal() } else { 0.0 };
            (rng.uniform() * w as f32, rng.uniform() * h as f32, sx, sy, 0.3 * rng.normal(), tilt)
        })
        .collect();
    let grain: Vec<f32> = (0..w * h).map(|_| rng.normal()).collect();
    let border = (w as f32 * 0.06) as usize;
    let gap = (h as f32 * 0.55) as usize;
    let p = Plane::from_fn(w, h, |x, y| {
        let (xf, yf) = (x as f32, y as f32);
        let mut v = 0.4 + 0.15 * (xf / 37.0).sin() * (yf / 53.0).cos() + 0.1 * (xf / w as f32);
        for &(bx, by, sx, sy, a, tilt) in &blobs {
            let d = ((xf - bx - tilt * (yf - by)) / sx).powi(2) + ((yf - by) / sy).powi(2);
            if d < 9.0 {
                v += a * (-0.5 * d).exp();
            }
        }
        // Slanted thin dark line (a wire), about 4° off vertical.
        if ((xf - 0.7 * w as f32) - 0.07 * yf).abs() < 1.0 {
            v *= 0.7;
        }
        // A painted wall: a flat 20 px band over the top 60 %, column-aligned (wider than any
        // streak, with step edges on both sides).
        if x >= w / 2 + 20 && x < w / 2 + 40 && y < h * 6 / 10 {
            v = 0.75;
        }
        v = v.clamp(0.03, 0.95) * channel_gain;
        v *= 1.0 + 0.008 * grain[y * w + x];
        if x < border {
            v = 0.004 + 0.0005 * grain[y * w + x]; // holder: full-height vertical step edge
        }
        if y >= gap && y < gap + 6 {
            v = 0.02; // inter-frame gap: a horizontal step pair
        }
        (v.clamp(0.0, 1.0) * 65535.0).round() / 65535.0
    });
    p.unwrap()
}

fn scan(seed: u64, with_ir: bool) -> ScanImage {
    let rgb = [texture(W, H, seed, 1.0), texture(W, H, seed + 1, 0.8), texture(W, H, seed + 2, 0.6)];
    // IR shows no dye image: a flat field with grain.
    let ir = with_ir.then(|| {
        let mut rng = Rng(seed + 99);
        let data: Vec<f32> = (0..W * H).map(|_| 0.85 * (1.0 + 0.006 * rng.normal())).collect();
        Plane::new(W, H, data).unwrap()
    });
    ScanImage::new(rgb, ir).unwrap()
}

fn scan_placeholder() -> ScanImage {
    let p = Plane::filled(1, 1, 0.0).unwrap();
    ScanImage::new([p.clone(), p.clone(), p], None).unwrap()
}

/// Apply `f` to channel `c` (0..3 RGB, 3 = IR) of `img`.
fn edit(img: &mut ScanImage, c: usize, f: impl FnOnce(&mut Plane)) {
    let (mut rgb, mut ir) = std::mem::replace(img, scan_placeholder()).into_parts();
    match c {
        0..=2 => f(&mut rgb[c]),
        _ => f(ir.as_mut().unwrap()),
    }
    *img = ScanImage::new(rgb, ir).unwrap();
}

fn column_map(p: &mut Plane, x: usize, rows: std::ops::Range<usize>, f: impl Fn(f32) -> f32) {
    for y in rows {
        let v = p.get(x, y);
        p.set(x, y, f(v));
    }
}

fn max_err(a: &Plane, b: &Plane) -> f32 {
    a.data().iter().zip(b.data()).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

fn column_rms_finite(a: &Plane, b: &Plane, x: usize) -> f32 {
    let d: Vec<f32> = (0..a.height()).map(|y| a.get(x, y) - b.get(x, y)).filter(|v| v.is_finite()).collect();
    (d.iter().map(|v| v * v).sum::<f32>() / d.len().max(1) as f32).sqrt()
}

fn column_rms(a: &Plane, b: &Plane, x: usize) -> f32 {
    let s: f32 = (0..a.height()).map(|y| (a.get(x, y) - b.get(x, y)).powi(2)).sum();
    (s / a.height() as f32).sqrt()
}

#[test]
fn clean_textured_scans_are_bit_identical() {
    for seed in [1u64, 17, 99, 1234, 101, 157, 163, 199] {
        let clean = scan(seed, true);
        let mut img = clean.clone();
        let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
        assert!(report.line_defects.is_empty(), "seed {seed}: {:#?}", report.line_defects);
        assert!(report.banding.iter().all(|b| b.banded.is_empty() && b.strong.is_empty()), "seed {seed}: {:#?}", report.banding);
        // The banding score measures the residual row-to-row noise: small but not snapped to 0.
        assert!(report.banding.iter().all(|b| b.rms > 0.0 && b.rms < 0.001), "seed {seed}: {:#?}", report.banding);
        assert!(!report.repaired);
        assert_eq!(img, clean, "seed {seed}");
        assert!(report.warnings.iter().all(|w| !matches!(w.kind, WarningKind::LineDefect | WarningKind::Banding)), "{:#?}", report.warnings);
    }
}

#[test]
fn gain_streak_full_length_is_detected_and_corrected() {
    let clean = scan(3, false);
    let mut img = clean.clone();
    edit(&mut img, 1, |p| column_map(p, 100, 0..H, |v| v * 0.9));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert_eq!((d.channel, d.position, d.width, d.full_length, d.polarity), (Channel::Green, 100, 1, true, Polarity::Dark));
    assert!((d.deviation + 0.1).abs() < 0.02, "{}", d.deviation);
    match d.repairs[0] {
        LineRepair::Gain { gain, offset } => assert!((gain - 0.9).abs() < 0.02 && offset.abs() < 0.01, "{gain} {offset}"),
        r => panic!("{r:?}"),
    }
    assert!(d.repaired);
    // Near ground truth in the streak (its own texture kept), untouched elsewhere.
    assert!(column_rms(&img.rgb()[1], &clean.rgb()[1], 100) < 0.004, "{}", column_rms(&img.rgb()[1], &clean.rgb()[1], 100));
    for x in (0..W).filter(|&x| x != 100) {
        assert_eq!(column_rms(&img.rgb()[1], &clean.rgb()[1], x), 0.0, "column {x}");
    }
    assert_eq!(img.rgb()[0], clean.rgb()[0]);
    assert!(report.warnings.iter().any(|w| w.kind == WarningKind::LineDefect && w.message.contains("column 100")), "{:#?}", report.warnings);
}

#[test]
fn mild_streak_is_found() {
    let clean = scan(4, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| column_map(p, 330, 0..H, |v| v * 1.02));
    let before = column_rms(&img.rgb()[0], &clean.rgb()[0], 330);
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    assert_eq!(report.line_defects[0].polarity, Polarity::Bright);
    let after = column_rms(&img.rgb()[0], &clean.rgb()[0], 330);
    // 2 % against 0.8 % grain: texture hides the tail, so the repair covers what was seen.
    assert!(after < 0.6 * before, "{before} -> {after}");
}

#[test]
fn faint_full_length_column_is_found_by_the_column_profile() {
    // A weak CCD element: +0.6 % on every row, below the per-pixel grain (0.8 %), as measured on
    // the V800 (blue column 109 of a 600 dpi frame, the same in two scans).
    let clean = scan(26, false);
    let mut img = clean.clone();
    edit(&mut img, 2, |p| column_map(p, 140, 0..H, |v| v * 1.006));
    let before = column_rms(&img.rgb()[2], &clean.rgb()[2], 140);
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert_eq!((d.position, d.width, d.full_length, d.polarity), (140, 1, true, Polarity::Bright));
    let after = column_rms(&img.rgb()[2], &clean.rgb()[2], 140);
    assert!(after < 0.3 * before, "{before} -> {after}");
    // The local test alone misses it.
    let mut cfg = ScanfixConfig::default();
    cfg.lines.full_z_threshold = 100.0;
    assert!(analyze(&img, &cfg).unwrap().line_defects.is_empty());
}

#[test]
fn offset_streak_two_wide_is_corrected() {
    let clean = scan(5, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        column_map(p, 150, 0..H, |v| v + 0.03);
        column_map(p, 151, 0..H, |v| v + 0.03);
    });
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert_eq!((d.position, d.width, d.polarity), (150, 2, Polarity::Bright));
    for x in [150, 151] {
        let e = column_rms(&img.rgb()[0], &clean.rgb()[0], x);
        assert!(e < 0.006, "column {x}: {e}");
    }
}

#[test]
fn dead_column_is_interpolated() {
    let clean = scan(6, false);
    let mut img = clean.clone();
    edit(&mut img, 2, |p| column_map(p, 200, 0..H, |_| 0.0));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert_eq!(d.repairs, vec![LineRepair::Interpolate]);
    let e = column_rms(&img.rgb()[2], &clean.rgb()[2], 200);
    assert!(e < 0.01, "{e}");
    // A dead-bright (stuck) element too.
    let mut img = clean.clone();
    edit(&mut img, 2, |p| column_map(p, 300, 0..H, |_| 0.7));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    assert!(column_rms(&img.rgb()[2], &clean.rgb()[2], 300) < 0.01);
}

#[test]
fn partial_streak_from_the_scan_start() {
    let clean = scan(7, false);
    let end = H * 6 / 10;
    let mut img = clean.clone();
    edit(&mut img, 1, |p| column_map(p, 250, 0..end, |v| v * 0.88));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert!(!d.full_length);
    assert_eq!(d.start, 0);
    assert!(d.end.abs_diff(end - 1) <= 3, "end {}", d.end);
    assert!(column_rms(&img.rgb()[1], &clean.rgb()[1], 250) < 0.006);
    // Rows clearly past the streak are untouched.
    for y in end + 4..H {
        assert_eq!(img.rgb()[1].get(250, y), clean.rgb()[1].get(250, y));
    }
}

#[test]
fn partial_streak_to_the_scan_end() {
    let clean = scan(8, false);
    let start = H * 3 / 10;
    let mut img = clean.clone();
    edit(&mut img, 0, |p| column_map(p, 90, start..H, |v| v * 1.1));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert!(d.start.abs_diff(start) <= 3 && d.end == H - 1, "{d:?}");
}

#[test]
fn floating_vertical_line_is_content_not_a_streak() {
    // A thin perfectly vertical feature in the middle 60 % of the frame (a pole): it touches
    // neither the scan start nor end, so it is left alone by default ...
    let clean = scan(9, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| column_map(p, 180, H / 5..H * 4 / 5, |v| v * 0.8));
    let damaged = img.clone();
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert!(report.line_defects.is_empty(), "{:#?}", report.line_defects);
    assert_eq!(img, damaged);
    // ... and found once the edge requirement is lifted (the requirement is what rejects it).
    let mut cfg = ScanfixConfig::default();
    cfg.lines.partial_must_touch_edge = false;
    assert_eq!(analyze(&damaged, &cfg).unwrap().line_defects.len(), 1);
}

#[test]
fn rebate_between_frame_edge_and_film_edge_is_content() {
    // Measured V800 geometry (600 dpi dual scan, red): image at 0.37, a frame edge drifting 2 px
    // over the top 55 % of the scan, a 12 px rebate at 0.45, then the film edge drops to a 0.045
    // plateau and the holder at 0.002. Below 55 % the image matches the rebate (no frame edge).
    // The column beside the frame edge must not read as a stuck bright streak.
    let mut rng = Rng(9);
    let grain: Vec<f32> = (0..W * H).map(|_| rng.normal()).collect();
    let plateau = [0.25, 0.064, 0.052, 0.049, 0.047, 0.044, 0.043, 0.042, 0.041, 0.039, 0.036, 0.025, 0.012, 0.006, 0.004];
    let p = Plane::from_fn(W, H, |x, y| {
        let top = y < H * 55 / 100;
        let edge = 330 + if top { (2 * y) / (H * 55 / 100) } else { 0 };
        let v = if x < edge {
            if top { 0.366 } else { 0.455 }
        } else if x < 342 {
            0.453
        } else {
            plateau.get(x - 342).copied().unwrap_or(0.002)
        };
        v * (1.0 + 0.005 * grain[y * W + x])
    })
    .unwrap();
    let img = ScanImage::new([p.clone(), p.clone(), p], None).unwrap();
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    assert!(report.line_defects.is_empty(), "{:#?}", report.line_defects);
}

#[test]
fn glow_beside_a_film_edge_is_not_a_streak() {
    // Real prescan geometry: black holder (~40 codes, with the sensor's column pattern) beside a
    // bright film strip; the strip's glow decays over a few pixels into the holder and
    // strengthens down the strip.
    let edge = 200;
    let mut rng = Rng(5);
    let pattern: Vec<f32> = (0..W).map(|_| 2.5 * rng.normal()).collect();
    let glow = Plane::from_fn(W, H, |x, y| {
        let grain = 1.5 * (((x * 7 + y * 13) % 5) as f32 - 2.0);
        let amp = 1.0 + 3.0 * (y as f32 / H as f32).powi(3);
        let codes = if x >= edge {
            30_000.0 + 400.0 * ((x as f32 / 9.0).sin() + (y as f32 / 23.0).cos())
        } else {
            40.0 + amp * (40.0 * (-((edge - x) as f32) / 4.0).exp() + 25.0 * (-((edge - x) as f32) / 15.0).exp())
        };
        (codes + pattern[x] + grain).round() / 65535.0
    })
    .unwrap();
    let img = ScanImage::new([glow.clone(), glow.clone(), glow], None).unwrap();
    assert!(analyze(&img, &ScanfixConfig::default()).unwrap().line_defects.is_empty());
    // The halo guard (with the absolute noise floor) is what rejects it.
    let mut cfg = ScanfixConfig::default();
    cfg.lines.halo_ratio = 1e6;
    cfg.lines.min_abs_noise = 0.0;
    assert!(!analyze(&img, &cfg).unwrap().line_defects.is_empty());
}

#[test]
fn bell_shaped_pole_is_content() {
    // A perfectly column-aligned thin dark feature from the top of the frame whose contrast fades
    // along its length (a pole in a photo), unlike a streak's constant gain.
    let clean = scan(25, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        for y in 0..H * 7 / 10 {
            let depth = 0.35 * (-((y as f32) / (0.35 * H as f32)).powi(2)).exp();
            let v = p.get(180, y);
            p.set(180, y, v * (1.0 - depth));
        }
    });
    assert!(analyze(&img, &ScanfixConfig::default()).unwrap().line_defects.is_empty());
    let mut cfg = ScanfixConfig::default();
    cfg.lines.max_variation = 10.0;
    let found = analyze(&img, &cfg).unwrap().line_defects;
    assert!(found.iter().any(|d| d.position <= 180 && 180 < d.position + d.width), "the stationarity test is what rejects it: {found:#?}");
}

#[test]
fn black_level_column_pattern_is_not_a_streak() {
    // The V800's holder black sits at ~40 codes with a fixed pattern of ±3 codes per column.
    let mut rng = Rng(77);
    let pattern: Vec<f32> = (0..W).map(|_| 3.0 * rng.normal()).collect();
    let dark = Plane::from_fn(W, H, |x, y| {
        let ramp = 40.0 + 0.05 * x as f32 + 0.02 * y as f32;
        (ramp + pattern[x] + 2.0 * (((x * 7 + y * 13) % 5) as f32 - 2.0)).round() / 65535.0
    })
    .unwrap();
    let img = ScanImage::new([dark.clone(), dark.clone(), dark], None).unwrap();
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    assert!(report.line_defects.is_empty(), "{:#?}", report.line_defects);
    // Without the absolute noise floor the pattern is flagged (the floor is what rejects it).
    let mut cfg = ScanfixConfig::default();
    cfg.lines.min_abs_noise = 0.0;
    assert!(!analyze(&img, &cfg).unwrap().line_defects.is_empty());
}

#[test]
fn wide_band_is_content_not_a_streak() {
    let clean = scan(10, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        for x in 120..129 {
            column_map(p, x, 0..H, |v| v * 0.85);
        }
    });
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    assert!(report.line_defects.is_empty(), "{:#?}", report.line_defects);
    // A 6 px streak (max_width) is still a defect.
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        for x in 120..126 {
            column_map(p, x, 0..H, |v| v * 0.85);
        }
    });
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    assert_eq!(report.line_defects[0].width, 6);
}

#[test]
fn row_flicker_is_corrected() {
    let clean = scan(11, false);
    let mut img = clean.clone();
    let mut rng = Rng(5);
    let gains: Vec<f32> = (0..H).map(|_| 1.0 + 0.012 * rng.normal()).collect();
    for c in 0..3 {
        edit(&mut img, c, |p| {
            for y in 0..H {
                for x in 0..W {
                    let v = p.get(x, y);
                    p.set(x, y, v * gains[y]);
                }
            }
        });
    }
    let err_before = max_err(&img.rgb()[1], &clean.rgb()[1]);
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    let b = report.banding.iter().find(|b| b.channel == Channel::Green).unwrap();
    assert!(b.rms > 0.006 && b.rms < 0.02, "{}", b.rms);
    assert!(b.banded.len() > H / 2, "{}", b.banded.len());
    assert!(report.warnings.iter().any(|w| w.kind == WarningKind::Banding));
    // Residual row gain after repair (rows relative to the clean scan).
    let row_gain = |a: &Plane, y: usize| {
        let mut r: Vec<f32> = (W / 10..W).map(|x| a.get(x, y) / clean.rgb()[1].get(x, y)).collect();
        r.sort_by(f32::total_cmp);
        r[r.len() / 2]
    };
    // The visible part of flicker is row to row: RMS of each row's gain against the median of
    // its ±3 neighbours (slow variation is indistinguishable from the scene and is left alone).
    let spread = |f: &dyn Fn(usize) -> f32| {
        let g: Vec<f32> = (0..H).map(f).collect();
        let gap = H * 55 / 100;
        let d: Vec<f32> = (3..H - 3)
            .filter(|y| !(gap - 1..gap + 7).contains(y))
            .map(|y| {
                let mut w: Vec<f32> = g[y - 3..=y + 3].to_vec();
                w.sort_by(f32::total_cmp);
                g[y] - w[3]
            })
            .collect();
        (d.iter().map(|v| v * v).sum::<f32>() / d.len() as f32).sqrt()
    };
    let before = spread(&|y| gains[y]);
    let after = spread(&|y| row_gain(&img.rgb()[1], y));
    assert!(after < 0.35 * before, "row-to-row gain RMS {before} -> {after}");
    assert!(max_err(&img.rgb()[1], &clean.rgb()[1]) < err_before);
}

#[test]
fn isolated_banded_rows_are_corrected_exactly_there() {
    let clean = scan(12, false);
    let rows = [20usize, 21, 100, 201];
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        for &y in &rows {
            for x in 0..W {
                let v = p.get(x, y);
                p.set(x, y, v * 1.02);
            }
        }
    });
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    let b = &report.banding[0];
    let found: Vec<usize> = b.banded.iter().map(|g| g.index).collect();
    assert_eq!(found, rows, "{b:#?}");
    for g in &b.banded {
        assert!((g.gain - 1.02).abs() < 0.003, "{g:?}");
    }
    for &y in &rows {
        let e: f32 = (0..W).map(|x| (img.rgb()[0].get(x, y) - clean.rgb()[0].get(x, y)).abs()).fold(0.0, f32::max);
        assert!(e < 0.003, "row {y}: {e}");
    }
    assert_eq!(img.rgb()[1], clean.rgb()[1]);
}

#[test]
fn horizontal_edge_shoulders_are_not_banding() {
    // Real prescan geometry: the strip ends are horizontal steps from the black holder to the
    // bright film leader, with one transition row and a shoulder row; neither is banding.
    let mut img = scan(24, false);
    for c in 0..3 {
        edit(&mut img, c, |p| {
            for x in 0..W {
                for y in 0..60 {
                    p.set(x, y, 0.004);
                }
                let v = p.get(x, 62);
                p.set(x, 60, 0.45 * v);
                p.set(x, 61, 0.96 * v);
            }
        });
    }
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    for b in &report.banding {
        assert!(b.banded.iter().chain(&b.strong).all(|g| g.index > 70), "{b:#?}");
    }
}

#[test]
fn strong_transient_row_is_reported_and_optionally_interpolated() {
    let clean = scan(13, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        for x in 0..W {
            let v = p.get(x, 77);
            p.set(x, 77, v * 1.3);
        }
    });
    let damaged = img.clone();
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.banding[0].strong.iter().map(|s| s.index).collect::<Vec<_>>(), vec![77]);
    assert_eq!(img, damaged, "strong rows are not touched by default");
    assert!(report.warnings.iter().any(|w| w.kind == WarningKind::StrongRows));
    let mut cfg = ScanfixConfig::default();
    cfg.banding.interpolate_strong_rows = true;
    repair(&mut img, &cfg).unwrap();
    let e: f32 = (W / 10..W).map(|x| (img.rgb()[0].get(x, 77) - clean.rgb()[0].get(x, 77)).abs()).sum::<f32>() / (W as f32 * 0.9);
    assert!(e < 0.01, "{e}");
}

#[test]
fn rows_axis_finds_horizontal_streaks() {
    // The same scan rotated a quarter turn: the streak becomes a row.
    let clean = scan(14, false);
    let t = |p: &Plane| Plane::from_fn(p.height(), p.width(), |x, y| p.get(y, x)).unwrap();
    let rgb = clean.rgb();
    let rotated = ScanImage::new([t(&rgb[0]), t(&rgb[1]), t(&rgb[2])], None).unwrap();
    let mut img = rotated.clone();
    edit(&mut img, 1, |p| {
        for x in 0..p.width() {
            let v = p.get(x, 60);
            p.set(x, 60, v * 0.9);
        }
    });
    assert!(analyze(&img, &ScanfixConfig::default()).unwrap().line_defects.is_empty(), "columns axis must not see a row streak");
    let cfg = ScanfixConfig { axis: StreakAxis::Rows, ..ScanfixConfig::default() };
    let report = repair(&mut img, &cfg).unwrap();
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    let d = &report.line_defects[0];
    assert_eq!((d.axis, d.position, d.full_length), (StreakAxis::Rows, 60, true));
    assert!(d.describe().contains("row 60"), "{}", d.describe());
    let e: f32 = (0..img.width()).map(|x| (img.rgb()[1].get(x, 60) - rotated.rgb()[1].get(x, 60)).abs()).fold(0.0, f32::max);
    assert!(e < 0.02, "{e}");
}

#[test]
fn infrared_plane_is_repaired_too() {
    let clean = scan(15, true);
    let mut img = clean.clone();
    edit(&mut img, 3, |p| column_map(p, 140, 0..H, |v| v * 0.8));
    let report = repair(&mut img, &ScanfixConfig::default()).unwrap();
    assert_eq!(report.line_defects.len(), 1);
    assert_eq!(report.line_defects[0].channel, Channel::Ir);
    assert!(column_rms(img.ir().unwrap(), clean.ir().unwrap(), 140) < 0.01);
    assert_eq!(report.channels.len(), 4);
    // Skipped when IR processing is off.
    let mut img2 = clean.clone();
    edit(&mut img2, 3, |p| column_map(p, 140, 0..H, |v| v * 0.8));
    let cfg = ScanfixConfig { process_ir: false, ..ScanfixConfig::default() };
    assert!(repair(&mut img2, &cfg).unwrap().line_defects.is_empty());
}

#[test]
fn switches_disable_repairs() {
    let clean = scan(16, false);
    let mut damaged = clean.clone();
    edit(&mut damaged, 0, |p| column_map(p, 100, 0..H, |v| v * 0.9));
    edit(&mut damaged, 0, |p| column_map(p, 300, 0..H, |_| 0.0));
    for cfg in [
        ScanfixConfig { enabled: false, ..ScanfixConfig::default() },
        ScanfixConfig { lines: LineConfig { detect: false, ..LineConfig::default() }, ..ScanfixConfig::default() },
        ScanfixConfig { lines: LineConfig { correct_gain: false, interpolate_dead: false, ..LineConfig::default() }, ..ScanfixConfig::default() },
    ] {
        let mut img = damaged.clone();
        repair(&mut img, &cfg).unwrap();
        assert_eq!(img, damaged);
    }
    // Only the gain correction: the dead column stays.
    let cfg = ScanfixConfig { lines: LineConfig { interpolate_dead: false, ..LineConfig::default() }, ..ScanfixConfig::default() };
    let mut img = damaged.clone();
    let report = repair(&mut img, &cfg).unwrap();
    assert_eq!(report.line_defects.len(), 2);
    assert!(column_rms(&img.rgb()[0], &clean.rgb()[0], 100) < 0.004);
    assert_eq!(img.rgb()[0].get(300, 10), 0.0);
}

#[test]
fn health_report_warns_about_clipping() {
    let mut img = scan(18, false);
    // Saturate 2.1 % of the blue channel.
    let n = (W * H) * 21 / 1000;
    edit(&mut img, 2, |p| {
        for v in p.data_mut().iter_mut().skip(W * 50).take(n) {
            *v = 1.0;
        }
    });
    let report = analyze(&img, &ScanfixConfig::default()).unwrap();
    let blue = &report.channels[2];
    assert!((blue.clipped_high - 0.021).abs() < 0.0005, "{}", blue.clipped_high);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.kind == WarningKind::HighlightsClipped && w.message == "Highlights clipped in blue: 2.1% of frame (sensor saturated)"),
        "{:#?}",
        report.warnings
    );
    // The 6 % holder strip is black-clipped? No: it sits at 0.004, above clip_low.
    assert!(report.channels[0].clipped_low < 0.001);
    assert!(report.channels.iter().all(|c| c.noise_floor > 0.0 && c.dynamic_range_stops > 4.0));
    // Serialises.
    let json = serde_json::to_string(&report).unwrap();
    let back: ScanReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report);
}

#[test]
fn weak_signal_is_reported() {
    let img = scan(19, false);
    let (rgb, _) = img.into_parts();
    let dim = rgb.map(|p| p.map(|v| v * 0.15));
    let report = analyze(&ScanImage::new(dim, None).unwrap(), &ScanfixConfig::default()).unwrap();
    assert!(report.warnings.iter().any(|w| w.kind == WarningKind::WeakSignal && w.channel == Some(Channel::Red)), "{:#?}", report.warnings);
}

#[test]
fn interleaved_entry_point() {
    let clean = scan(20, true);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| column_map(p, 100, 0..H, |v| v * 0.9));
    let mut rgb = img.rgb_interleaved().unwrap();
    let mut ir = img.ir().unwrap().data().to_vec();
    let report = repair_interleaved(W, H, &mut rgb, Some(&mut ir), &ScanfixConfig::default()).unwrap();
    assert!(report.repaired);
    let fixed = ScanImage::from_interleaved(W, H, &rgb, Some(&ir)).unwrap();
    assert!(column_rms(&fixed.rgb()[0], &clean.rgb()[0], 100) < 0.004);
    // Clean buffers stay bit-identical.
    let mut rgb = clean.rgb_interleaved().unwrap();
    let orig = rgb.clone();
    repair_interleaved(W, H, &mut rgb, None, &ScanfixConfig::default()).unwrap();
    assert_eq!(rgb, orig);
}

// ---- hostile input ----

#[test]
fn hostile_sizes_do_not_panic() {
    let cfg = ScanfixConfig::default();
    assert!(ScanImage::from_interleaved(0, 0, &[], None).is_err());
    assert!(ScanImage::from_interleaved(0, 5, &[], None).is_err());
    assert!(ScanImage::from_interleaved(2, 2, &[0.5; 11], None).is_err());
    assert!(ScanImage::from_interleaved(2, 2, &[0.5; 12], Some(&[0.5; 3])).is_err());
    assert!(matches!(
        ScanImage::new(
            [Plane::filled(2, 2, 0.5).unwrap(), Plane::filled(2, 2, 0.5).unwrap(), Plane::filled(2, 2, 0.5).unwrap()],
            Some(Plane::filled(3, 2, 0.5).unwrap())
        ),
        Err(ScanfixError::IrSize { .. })
    ));
    assert!(repair_interleaved(usize::MAX, 2, &mut [0.5; 12], None, &cfg).is_err());
    for (w, h) in [(1, 1), (1, 7), (7, 1), (3, 3), (16, 16), (40, 17), (2, 300), (300, 2)] {
        let mut rgb: Vec<f32> = (0..w * h * 3).map(|i| ((i * 37) % 101) as f32 / 101.0).collect();
        let mut ir: Vec<f32> = (0..w * h).map(|i| (i % 7) as f32 / 7.0).collect();
        let r = repair_interleaved(w, h, &mut rgb, Some(&mut ir), &cfg).unwrap();
        assert_eq!(r.channels.len(), 4);
        for axis in [StreakAxis::Columns, StreakAxis::Rows] {
            let cfg = ScanfixConfig { axis, ..cfg.clone() };
            repair_interleaved(w, h, &mut rgb, Some(&mut ir), &cfg).unwrap();
        }
    }
}

#[test]
fn hostile_values_do_not_panic_or_spread() {
    let cfg = ScanfixConfig::default();
    for fill in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 1e30, 0.0, 1.0] {
        let mut rgb = vec![fill; W * H * 3];
        let r = repair_interleaved(W, H, &mut rgb, None, &cfg).unwrap();
        assert!(r.line_defects.is_empty());
        serde_json::to_string(&r).unwrap();
    }
    // A real scan with NaN and infinities sprinkled in, plus a streak.
    let clean = scan(21, false);
    let mut img = clean.clone();
    edit(&mut img, 0, |p| {
        column_map(p, 100, 0..H, |v| v * 0.9);
        let mut rng = Rng(3);
        for _ in 0..500 {
            let i = (rng.next() % (W * H) as u64) as usize;
            p.data_mut()[i] = if i.is_multiple_of(2) { f32::NAN } else { f32::INFINITY };
        }
    });
    let bad_before: Vec<usize> = img.rgb()[0].data().iter().enumerate().filter(|(_, v)| !v.is_finite()).map(|(i, _)| i).collect();
    assert!(bad_before.len() > 450);
    let report = repair(&mut img, &cfg).unwrap();
    assert_eq!(report.channels[0].non_finite, bad_before.len());
    assert!(report.warnings.iter().any(|w| w.kind == WarningKind::NonFinite));
    assert_eq!(report.line_defects.len(), 1, "{:#?}", report.line_defects);
    assert!(column_rms_finite(&img.rgb()[0], &clean.rgb()[0], 100) < 0.004);
    let bad_after: Vec<usize> = img.rgb()[0].data().iter().enumerate().filter(|(_, v)| !v.is_finite()).map(|(i, _)| i).collect();
    assert_eq!(bad_after, bad_before, "repair must not create, move or remove non-finite values");
    // Every finite value stays finite.
    assert!(img.rgb()[1..].iter().all(|p| p.data().iter().all(|v| v.is_finite())));
}

#[test]
fn bad_config_is_rejected_not_panicking() {
    let img = scan(22, false);
    let mut cfg = ScanfixConfig::default();
    cfg.lines.baseline_radius = 0;
    assert!(matches!(analyze(&img, &cfg), Err(ScanfixError::Config(_))));
    let mut cfg = ScanfixConfig::default();
    cfg.banding.max_gain_deviation = f32::NAN;
    assert!(analyze(&img, &cfg).is_err());
    let mut cfg = ScanfixConfig::default();
    cfg.health.max_samples = 0;
    assert!(analyze(&img, &cfg).is_err());
}
