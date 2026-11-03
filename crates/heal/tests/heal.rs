//! End-to-end tests on synthetic captures: a textured negative with grain, synthetic dust and
//! hairs in both the visible and the IR pass, lamp falloff and image leakage in the IR.

use silvercraft_film::RgbImage;
use silvercraft_heal::{HealConfig, HealError, IrPlane, Kind, MaskSource, Tier, heal, heal_capture};

const W: usize = 240;
const H: usize = 180;

fn hash(x: usize, y: usize, k: u64) -> f32 {
    let mut z = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ k;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
}

/// Smooth scene transmittance (no grain) at a continuous position: gradients, a soft-edged
/// dense region and some mid-frequency texture.
fn scene(x: f32, y: f32) -> f32 {
    let base = 0.30 + 0.15 * (x / W as f32) + 0.05 * (y / 23.0).sin() * (x / 31.0).cos();
    let dense = 1.0 / (1.0 + (-(x - 150.0 - 0.3 * y) / 1.5).exp());
    base * (1.0 - 0.55 * dense)
}

#[derive(Clone, Copy)]
enum Shape {
    Disc {
        cx: f32,
        cy: f32,
        r: f32,
    },
    /// A sinusoidal hair from (x0, y0) along x for `len` pixels.
    Hair {
        x0: f32,
        y0: f32,
        len: f32,
        amp: f32,
        width: f32,
    },
}

#[derive(Clone, Copy)]
struct Defect {
    shape: Shape,
    /// Fraction of visible light blocked.
    opacity: f32,
    /// IR opacity relative to the visible one (1 = an ordinary grey occluder).
    ir: f32,
}

fn inside(s: Shape, x: f32, y: f32) -> bool {
    match s {
        Shape::Disc { cx, cy, r } => (x - cx).powi(2) + (y - cy).powi(2) <= r * r,
        Shape::Hair { x0, y0, len, amp, width } => {
            if x < x0 || x > x0 + len {
                return false;
            }
            let yc = y0 + amp * ((x - x0) / 9.0).sin();
            let slope = amp / 9.0 * ((x - x0) / 9.0).cos();
            (y - yc).abs() / (1.0 + slope * slope).sqrt() <= width / 2.0
        }
    }
}

/// Occlusion (0..1) of pixel (x, y) by the defects shifted by `off`, 4×4 supersampled.
fn occlusion(defects: &[Defect], x: usize, y: usize, off: (f32, f32)) -> f32 {
    occlusion_by(defects, x, y, off, |d| d.opacity)
}

fn occlusion_by(defects: &[Defect], x: usize, y: usize, off: (f32, f32), opacity: impl Fn(&Defect) -> f32) -> f32 {
    let mut t = 1.0;
    for d in defects {
        let mut cover = 0.0;
        for sy in 0..4 {
            for sx in 0..4 {
                let px = x as f32 + (sx as f32 + 0.5) / 4.0 - 0.5 - off.0;
                let py = y as f32 + (sy as f32 + 0.5) / 4.0 - 0.5 - off.1;
                if inside(d.shape, px, py) {
                    cover += 1.0 / 16.0;
                }
            }
        }
        t *= 1.0 - opacity(d) * cover;
    }
    1.0 - t
}

fn defects() -> Vec<Defect> {
    vec![
        Defect { shape: Shape::Disc { cx: 40.0, cy: 40.0, r: 1.5 }, opacity: 0.95, ir: 1.0 },
        Defect { shape: Shape::Disc { cx: 90.0, cy: 30.0, r: 2.5 }, opacity: 0.9, ir: 1.0 },
        Defect { shape: Shape::Disc { cx: 60.0, cy: 130.0, r: 4.0 }, opacity: 0.95, ir: 1.0 },
        Defect { shape: Shape::Disc { cx: 120.0, cy: 90.0, r: 7.0 }, opacity: 0.97, ir: 1.0 },
        Defect { shape: Shape::Disc { cx: 200.0, cy: 140.0, r: 2.0 }, opacity: 0.8, ir: 1.0 },
        Defect { shape: Shape::Hair { x0: 20.0, y0: 80.0, len: 70.0, amp: 6.0, width: 1.6 }, opacity: 0.9, ir: 1.0 },
        Defect { shape: Shape::Hair { x0: 140.0, y0: 40.0, len: 60.0, amp: 4.0, width: 1.4 }, opacity: 0.85, ir: 1.0 },
    ]
}

struct Fixture {
    clean: RgbImage,
    dusty: RgbImage,
    ir: IrPlane,
    /// Pixels materially covered by a defect (occlusion > 5%).
    truth: Vec<bool>,
}

struct Options {
    ir_offset: (f32, f32),
    /// Log slope of IR on the scene (0.2 = XP2-like leakage, 0.9 = silver).
    leakage: f32,
    rings: f32,
    defects: Vec<Defect>,
}

impl Default for Options {
    fn default() -> Self {
        Self { ir_offset: (0.0, 0.0), leakage: 0.2, rings: 0.0, defects: defects() }
    }
}

fn fixture(o: &Options) -> Fixture {
    let mut clean = Vec::with_capacity(W * H * 3);
    let mut dusty = Vec::with_capacity(W * H * 3);
    let mut ir = Vec::with_capacity(W * H);
    let mut truth = Vec::with_capacity(W * H);
    for y in 0..H {
        for x in 0..W {
            let grain = 1.0 + 0.06 * hash(x, y, 1);
            let t = scene(x as f32, y as f32) * grain;
            let px = [t * 1.15, t, t * 0.8];
            let occ = occlusion(&o.defects, x, y, (0.0, 0.0));
            truth.push(occ > 0.05);
            clean.extend_from_slice(&px);
            dusty.extend(px.iter().map(|v| v * (1.0 - occ)));
            // IR: lamp falloff × leaked image × occlusion, at the IR plane's own coordinates
            let (fx, fy) = (x as f32 - o.ir_offset.0, y as f32 - o.ir_offset.1);
            let lamp = 0.75 - 0.08 * ((fx - 120.0).powi(2) + (fy - 90.0).powi(2)) / 15_000.0;
            let leak = (scene(fx, fy) / 0.3).powf(o.leakage);
            let r = ((fx - 70.0).powi(2) + (fy - 120.0).powi(2)).sqrt();
            let ring = 1.0 - o.rings * (0.5 + 0.5 * (r * r / 200.0).cos());
            let occ_ir = occlusion_by(&o.defects, x, y, o.ir_offset, |d| d.opacity * d.ir);
            ir.push(lamp * leak * ring * (1.0 - occ_ir) * (1.0 + 0.01 * hash(x, y, 2)));
        }
    }
    Fixture { clean: RgbImage::new(W, H, clean).unwrap(), dusty: RgbImage::new(W, H, dusty).unwrap(), ir: IrPlane::new(W, H, ir).unwrap(), truth }
}

/// Mean relative error over truth pixels, and the worst per-defect mean error.
fn errors(img: &RgbImage, f: &Fixture) -> f32 {
    let mut acc = 0.0;
    let mut n = 0.0;
    for (i, &t) in f.truth.iter().enumerate() {
        if t {
            for c in 0..3 {
                acc += (img.data()[3 * i + c] - f.clean.data()[3 * i + c]).abs() / f.clean.data()[3 * i + c];
            }
            n += 3.0;
        }
    }
    acc / n
}

#[test]
fn detects_and_fills_close_to_ground_truth() {
    let f = fixture(&Options::default());
    let res = heal(&f.dusty, Some(&f.ir), &HealConfig::default()).unwrap();
    assert!(res.warnings.is_empty(), "{:?} {:?}", res.warnings, res.stats);
    assert_eq!(res.stats.source, MaskSource::Infrared);
    // every materially occluded pixel is in the mask
    let missed = f.truth.iter().zip(res.mask.data()).filter(|(t, m)| **t && !**m).count();
    assert_eq!(missed, 0, "missed {missed} defect pixels");
    // and the mask is the defects plus their halo, nothing else (no leakage edges, no lamp)
    let near = silvercraft_imgproc::morph::dilate(&silvercraft_imgproc::Mask::new(W, H, f.truth.clone()).unwrap(), 4);
    let stray = res.mask.data().iter().zip(near.data()).filter(|(m, n)| **m && !**n).count();
    assert_eq!(stray, 0, "{stray} mask pixels far from any defect");
    let before = errors(&f.dusty, &f);
    let after = errors(&res.image, &f);
    assert!(before > 0.4, "fixture defects are strong ({before})");
    assert!(after < 0.07, "healed error {after} (grain alone is ~0.035)");
    // the hairs are scratches routed to the exemplar fill, specks to the smooth tiers
    let hairs: Vec<_> = res.defects.iter().filter(|d| d.kind == Kind::Scratch).collect();
    assert_eq!(hairs.len(), 2, "{:#?}", res.defects);
    assert!(hairs.iter().all(|d| d.tier == Tier::Large && d.deep));
    assert!(res.defects.iter().any(|d| d.tier == Tier::Small));
    assert_eq!(res.stats.defects, 7);
}

#[test]
fn clean_pixels_are_bit_identical() {
    let f = fixture(&Options::default());
    let res = heal(&f.dusty, Some(&f.ir), &HealConfig::default()).unwrap();
    let mut changed_inside = 0;
    for i in 0..W * H {
        let same = (0..3).all(|c| res.image.data()[3 * i + c].to_bits() == f.dusty.data()[3 * i + c].to_bits());
        if res.mask.data()[i] {
            changed_inside += usize::from(!same);
        } else {
            assert!(same, "pixel {i} outside the mask changed");
        }
    }
    assert!(changed_inside > 100);
}

#[test]
fn registration_recovers_subpixel_offset() {
    let f = fixture(&Options { ir_offset: (1.3, -0.6), ..Options::default() });
    let res = heal(&f.dusty, Some(&f.ir), &HealConfig::default()).unwrap();
    let reg = res.stats.registration.unwrap();
    assert!(reg.estimated && reg.confidence > 0.3, "{reg:?}");
    assert!((reg.dx - 1.3).abs() < 0.15 && (reg.dy + 0.6).abs() < 0.15, "{reg:?}");
    let missed = f.truth.iter().zip(res.mask.data()).filter(|(t, m)| **t && !**m).count();
    assert_eq!(missed, 0);
    assert!(errors(&res.image, &f) < 0.07);
    // without registration the mask misses the defect edges at a 1.3 px offset
    let off = HealConfig { register: false, halo: 0.0, visible_grow: 0.0, ..HealConfig::default() };
    let on = HealConfig { halo: 0.0, visible_grow: 0.0, ..HealConfig::default() };
    let miss = |cfg: &HealConfig| {
        let r = heal(&f.dusty, Some(&f.ir), cfg).unwrap();
        f.truth.iter().zip(r.mask.data()).filter(|(t, m)| **t && !**m).count()
    };
    assert!(miss(&off) > 2 * miss(&on) + 10, "registered {} vs not {}", miss(&on), miss(&off));
}

#[test]
fn silver_film_is_refused() {
    let f = fixture(&Options { leakage: 1.0, ..Options::default() });
    let res = heal(&f.dusty, Some(&f.ir), &HealConfig::default()).unwrap();
    assert!(res.warnings.iter().any(|w| w.contains("IR looks like image")), "{:?}", res.warnings);
    assert_eq!(res.image, f.dusty);
    assert_eq!(res.mask.count(), 0);
    assert_eq!(res.stats.source, MaskSource::None);
    assert!(res.stats.leakage.iter().sum::<f32>() > 0.8);
}

#[test]
fn newton_rings_are_not_dust() {
    let f = fixture(&Options { rings: 0.08, defects: vec![], ..Options::default() });
    let res = heal(&f.dusty, Some(&f.ir), &HealConfig::default()).unwrap();
    assert!(!res.warnings.iter().any(|w| w.contains("misdetection")), "{:?}", res.warnings);
    assert_eq!(res.mask.count(), 0, "{} ring pixels masked", res.mask.count());
    // with dust on top of the rings, the dust is still found
    let g = fixture(&Options { rings: 0.08, ..Options::default() });
    let res = heal(&g.dusty, Some(&g.ir), &HealConfig::default()).unwrap();
    let missed = g.truth.iter().zip(res.mask.data()).filter(|(t, m)| **t && !**m).count();
    assert_eq!(missed, 0);
}

#[test]
fn partial_occluders_can_be_divided_out() {
    let defects = vec![Defect { shape: Shape::Disc { cx: 60.0, cy: 60.0, r: 5.0 }, opacity: 0.3, ir: 1.0 }];
    let f = fixture(&Options { defects, ..Options::default() });
    let cfg = HealConfig { partial_correction: true, halo: 1.0, visible_grow: 0.0, ..HealConfig::default() };
    let res = heal(&f.dusty, Some(&f.ir), &cfg).unwrap();
    assert_eq!(res.stats.corrected, 1, "{:#?}", res.defects);
    assert!(!res.defects[0].deep && (res.defects[0].min_attenuation - 0.7).abs() < 0.1);
    // division keeps the real grain underneath: error is close to the IR noise, not the grain
    assert!(errors(&res.image, &f) < 0.03, "{} {:#?}", errors(&res.image, &f), res.defects);
    let filled = heal(&f.dusty, Some(&f.ir), &HealConfig { partial_correction: false, ..cfg }).unwrap();
    assert_eq!(filled.stats.corrected, 0);
}

#[test]
fn grain_option_matches_surroundings() {
    let defects = vec![Defect { shape: Shape::Disc { cx: 60.0, cy: 60.0, r: 6.0 }, opacity: 0.97, ir: 1.0 }];
    let f = fixture(&Options { defects, ..Options::default() });
    let stdev = |img: &RgbImage| {
        let v: Vec<f32> = (55..66).flat_map(|y| (55..66).map(move |x| (x, y))).map(|(x, y)| img.pixel(x, y).unwrap()[1]).collect();
        let hp: Vec<f32> = v.windows(2).map(|p| p[1] - p[0]).collect();
        let m = hp.iter().sum::<f32>() / hp.len() as f32;
        (hp.iter().map(|d| (d - m).powi(2)).sum::<f32>() / hp.len() as f32).sqrt()
    };
    let with = heal(&f.dusty, Some(&f.ir), &HealConfig { large_min_area: 1e6, ..HealConfig::default() }).unwrap();
    let without = heal(&f.dusty, Some(&f.ir), &HealConfig { large_min_area: 1e6, grain: false, ..HealConfig::default() }).unwrap();
    assert!(with.defects.iter().all(|d| d.tier == Tier::Medium));
    let reference = stdev(&f.clean);
    assert!(stdev(&without.image) < 0.3 * reference, "smooth fill");
    assert!((stdev(&with.image) / reference - 1.0).abs() < 0.35, "{} vs {reference}", stdev(&with.image));
}

#[test]
fn no_ir_detector_is_opt_in() {
    let defects = vec![
        Defect { shape: Shape::Disc { cx: 40.0, cy: 40.0, r: 1.5 }, opacity: 0.8, ir: 1.0 },
        Defect { shape: Shape::Disc { cx: 100.0, cy: 120.0, r: 1.2 }, opacity: 0.7, ir: 1.0 },
    ];
    let f = fixture(&Options { defects, ..Options::default() });
    let off = heal(&f.dusty, None, &HealConfig::default()).unwrap();
    assert_eq!(off.stats.source, MaskSource::None);
    assert_eq!(off.image, f.dusty);
    let on = heal(&f.dusty, None, &HealConfig { no_ir_detect: true, ..HealConfig::default() }).unwrap();
    assert_eq!(on.stats.source, MaskSource::Visible);
    assert_eq!(on.stats.defects, 2, "{:#?}", on.defects);
    assert!(errors(&on.image, &f) < 0.08, "{}", errors(&on.image, &f));
}

#[test]
fn disabled_and_coverage_cap_return_input() {
    let f = fixture(&Options::default());
    let off = heal(&f.dusty, Some(&f.ir), &HealConfig { enabled: false, ..HealConfig::default() }).unwrap();
    assert_eq!(off.image, f.dusty);
    let capped = heal(&f.dusty, Some(&f.ir), &HealConfig { max_coverage: 0.1, ..HealConfig::default() }).unwrap();
    assert_eq!(capped.image, f.dusty);
    assert!(capped.warnings.iter().any(|w| w.contains("misdetection")));
}

#[test]
fn hostile_inputs_never_panic() {
    let f = fixture(&Options::default());
    let cfg = HealConfig::default();
    // mismatched sizes
    let small = IrPlane::filled(10, 10, 0.5).unwrap();
    assert!(matches!(heal(&f.dusty, Some(&small), &cfg), Err(HealError::SizeMismatch { .. })));
    assert!(matches!(heal_capture(&f.dusty, Some(&[0.5; 7]), &cfg), Err(HealError::SizeMismatch { .. })));
    // bad config
    assert!(matches!(heal(&f.dusty, None, &HealConfig { halo: f64::NAN, ..cfg.clone() }), Err(HealError::Config(_))));
    // all-zero, NaN, infinite and constant IR
    for v in [0.0, f32::NAN, f32::INFINITY, -1.0, 0.5] {
        let ir = IrPlane::filled(W, H, v).unwrap();
        let res = heal(&f.dusty, Some(&ir), &cfg).unwrap();
        assert!(res.mask.count() == 0 || v == 0.5, "IR {v}");
        assert_eq!(res.image, f.dusty, "IR {v}");
    }
    // NaN / inf sprinkled into both passes
    let mut rgb = f.dusty.data().to_vec();
    let mut ir = f.ir.data().to_vec();
    for k in (0..W * H).step_by(97) {
        rgb[3 * k] = f32::NAN;
        ir[(k * 7) % (W * H)] = if k % 2 == 0 { f32::NAN } else { f32::INFINITY };
    }
    let rgb = RgbImage::new(W, H, rgb).unwrap();
    let res = heal_capture(&rgb, Some(&ir), &cfg).unwrap();
    assert!(res.image.data().iter().zip(rgb.data()).all(|(a, b)| a.to_bits() == b.to_bits() || a.is_finite()));
    let res = heal(&rgb, None, &HealConfig { no_ir_detect: true, ..cfg.clone() }).unwrap();
    assert!(res.stats.defects < 50);
    // tiny and degenerate images
    for (w, h) in [(1, 1), (1, 9), (9, 1), (3, 3), (8, 8), (16, 2)] {
        let img = RgbImage::filled(w, h, [0.4, 0.3, 0.2]).unwrap();
        let ir = IrPlane::filled(w, h, 0.7).unwrap();
        let r = heal(&img, Some(&ir), &cfg).unwrap();
        assert_eq!(r.image, img);
        let r = heal(&img, None, &HealConfig { no_ir_detect: true, ..cfg.clone() }).unwrap();
        assert_eq!(r.image, img);
    }
    // all-black and all-white captures
    for v in [0.0, 1.0, 1e30] {
        let img = RgbImage::filled(64, 48, [v, v, v]).unwrap();
        let ir = IrPlane::filled(64, 48, v).unwrap();
        heal(&img, Some(&ir), &cfg).unwrap();
        heal(&img, None, &HealConfig { no_ir_detect: true, ..cfg.clone() }).unwrap();
    }
}

#[test]
fn larger_dpi_scales_geometry() {
    // the same scene at 2× resolution with dpi = 1200 finds the same defects
    let f = fixture(&Options::default());
    let up = |img: &RgbImage| {
        let mut d = Vec::with_capacity(4 * W * H * 3);
        for y in 0..2 * H {
            for x in 0..2 * W {
                d.extend_from_slice(&img.pixel(x / 2, y / 2).unwrap());
            }
        }
        RgbImage::new(2 * W, 2 * H, d).unwrap()
    };
    let ir = IrPlane::from_fn(2 * W, 2 * H, |x, y| f.ir.get(x / 2, y / 2)).unwrap();
    let res = heal(&up(&f.dusty), Some(&ir), &HealConfig { dpi: Some(1200.0), ..HealConfig::default() }).unwrap();
    assert_eq!(res.stats.defects, 7, "{:#?}", res.defects);
    assert_eq!(res.stats.scratches, 2);
}

#[test]
fn attenuation_is_flat_on_clean_film_despite_falloff_and_leakage() {
    let f = fixture(&Options::default());
    let near = silvercraft_imgproc::morph::dilate(&silvercraft_imgproc::Mask::new(W, H, f.truth.clone()).unwrap(), 6);
    let worst = |cfg: &HealConfig| {
        let a = heal(&f.dusty, Some(&f.ir), cfg).unwrap().attenuation.unwrap();
        (8..H - 8)
            .flat_map(|y| (8..W - 8).map(move |x| (x, y)))
            .filter(|&(x, y)| !near.get(x, y))
            .map(|(x, y)| (1.0 - a.get(x, y)).abs())
            .fold(0.0f32, f32::max)
    };
    let with = worst(&HealConfig::default());
    let without = worst(&HealConfig { leakage: false, ..HealConfig::default() });
    assert!(with < 0.03, "{with}");
    assert!(without > 0.08, "the dense region leaks into IR: {without}");
}

#[test]
fn visible_growth_covers_tinted_flakes() {
    // a flake that blocks visible light but is nearly IR-transparent, with an opaque grain inside
    let defects = vec![
        Defect { shape: Shape::Disc { cx: 80.0, cy: 60.0, r: 6.0 }, opacity: 0.5, ir: 0.05 },
        Defect { shape: Shape::Disc { cx: 80.0, cy: 60.0, r: 1.5 }, opacity: 0.9, ir: 1.0 },
    ];
    let f = fixture(&Options { defects, ..Options::default() });
    let missed = |cfg: &HealConfig| {
        let res = heal(&f.dusty, Some(&f.ir), cfg).unwrap();
        f.truth.iter().zip(res.mask.data()).filter(|(t, m)| **t && !**m).count()
    };
    assert_eq!(missed(&HealConfig::default()), 0);
    assert!(missed(&HealConfig { visible_grow: 0.0, ..HealConfig::default() }) > 20);
}

mod fuzz {
    use super::*;
    use proptest::prelude::*;

    fn value() -> impl Strategy<Value = f32> {
        prop_oneof![
            8 => 0.0f32..1.2,
            1 => Just(f32::NAN),
            1 => Just(f32::INFINITY),
            1 => Just(-1.0f32),
            1 => Just(0.0f32),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn arbitrary_captures_never_panic(w in 1usize..40, h in 1usize..40, seed in proptest::collection::vec(value(), 16), no_ir in any::<bool>(), partial in any::<bool>()) {
            let rgb: Vec<f32> = (0..w * h * 3).map(|i| seed[(i * 7 + i / 5) % 16] * (1.0 + 0.1 * hash(i, 0, 3))).collect();
            let ir: Vec<f32> = (0..w * h).map(|i| seed[(i * 3 + 1) % 16]).collect();
            let img = RgbImage::new(w, h, rgb).unwrap();
            let cfg = HealConfig { no_ir_detect: no_ir, partial_correction: partial, ..HealConfig::default() };
            let res = heal_capture(&img, Some(&ir), &cfg).unwrap();
            prop_assert_eq!(res.image.width(), w);
            let res = heal(&img, None, &cfg).unwrap();
            for i in 0..w * h {
                if !res.mask.data()[i] {
                    prop_assert!((0..3).all(|c| res.image.data()[3 * i + c].to_bits() == img.data()[3 * i + c].to_bits()));
                }
            }
        }
    }
}
