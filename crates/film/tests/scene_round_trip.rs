//! Scene-linear round trip: a synthetic negative of a KNOWN
//! scene-linear image, developed through the full pipeline, must recover the
//! scene's ratios.

#![allow(clippy::field_reassign_with_default)] // option tweaks read clearer as assignments in tests

mod common;

use common::{E0, PATCHES, Stock, capture_with_rebate, negative, patch_center, window_mean};
use silvercraft_film::base::{FilmBaseSampleOptions, sample_film_base};
use silvercraft_film::scene::{Referred, excitation_to_linear, linear_to_excitation};
use silvercraft_film::{FilmConfig, FrameInput, develop_roll, develop_single, film_profile};

const W: usize = 240;
const H: usize = 120;

/// Ramp pixel x for a given EV around 0.18 (left half spans -4..+4 EV).
fn ramp_x(ev: f64) -> usize {
    let half = W / 2;
    ((ev + 4.0) / 8.0 * (half - 1) as f64).round() as usize
}

fn lum(p: [f64; 3]) -> f64 {
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

fn ratio_spread(p: [f64; 3]) -> f64 {
    let mean = (p[0] + p[1] + p[2]) / 3.0;
    p.iter().map(|v| (v / mean - 1.0).abs()).fold(0.0, f64::max)
}

#[test]
fn single_frame_recovers_scene_ratios() {
    let stock = Stock::even(0.60);
    let frame = negative(W, H, &stock, 0.0, 0x51C0);
    let (capture, y0, y1) = capture_with_rebate(&frame, &stock, 20, 1);
    let gap = sample_film_base(&capture, &FilmBaseSampleOptions::new(y0, y1)).unwrap();
    let profile = film_profile("generic-c41").unwrap();
    let out = develop_single(&frame, Some(gap), profile, &FilmConfig::default()).unwrap();
    let meta = &out.meta;
    assert_eq!(meta.referred, Referred::Scene);
    assert_eq!(meta.g_ref, Some(0.60));

    // A 1-stop step in the scene is a 2x step in L, everywhere on the ramp.
    for ev in [-2.0, -1.0, 0.0, 1.0, 2.0] {
        let a = lum(window_mean(&out.image, ramp_x(ev), H / 2, 0));
        let b = lum(window_mean(&out.image, ramp_x(ev + 1.0), H / 2, 0));
        // The ramp is sampled at whole pixels: correct for the sampled EV.
        let half = W / 2;
        let ev_at = |x: usize| -4.0 + 8.0 * x as f64 / (half - 1) as f64;
        let expected = 2f64.powf(ev_at(ramp_x(ev + 1.0)) - ev_at(ramp_x(ev)));
        assert!((b / a / expected - 1.0).abs() < 0.04, "EV {ev}: ratio {} vs {expected}", b / a);
    }
    // Grey patches: neutral and in scene proportion.
    let grey = |i: usize| {
        let (x, y) = patch_center(W, H, i);
        window_mean(&out.image, x, y, 5)
    };
    for i in 0..4 {
        assert!(ratio_spread(grey(i)) < 0.02, "{} not neutral: {:?}", PATCHES[i].0, grey(i));
    }
    let g18 = lum(grey(1));
    assert!((lum(grey(2)) / g18 / 2.0 - 1.0).abs() < 0.03);
    assert!((lum(grey(3)) / g18 / 4.0 - 1.0).abs() < 0.03);
    assert!((lum(grey(0)) / g18 / (0.05 / 0.18) - 1.0).abs() < 0.04);
    // Coloured patches keep their channel ratios (relative to the scene).
    for i in 4..9 {
        let (x, y) = patch_center(W, H, i);
        let got = window_mean(&out.image, x, y, 5);
        let want = PATCHES[i].1;
        for c in 0..3 {
            let r = (got[c] / g18) / (want[c] / 0.18);
            assert!((r - 1.0).abs() < 0.05, "{} channel {c}: {r}", PATCHES[i].0);
        }
    }
    // Mid grey lands near 0.18 at the roll level, and BaselineExposure
    // brings the frame's neutral median to 0.18 exactly.
    // Default e_mid estimator (neutral-range-midpoint in log exposure) puts
    // the middle of this -4..+4 EV neutral range at 0.18. (The old binned
    // density median gave 0.134, -0.4 EV: see `e_mid_estimators_compared`.)
    assert!((g18 / 0.18 - 1.0).abs() < 0.03, "mid grey L = {g18}");
    let auto = meta.median_neutral_l * 2f64.powf(meta.baseline_exposure_ev);
    assert!((auto - 0.18).abs() < 1e-9 || meta.baseline_exposure_ev.abs() == 3.0);
    // The roll calibration had nothing to fix: residual WB ~ 1.
    for c in [0, 2] {
        assert!((meta.as_shot_neutral[c] - 1.0).abs() < 0.03, "{:?}", meta.as_shot_neutral);
    }
    assert!(meta.clipped_fraction < 0.01);
}

#[test]
fn film_base_maps_to_exact_black_and_brighter_than_base_clamps() {
    let stock = Stock::even(0.60);
    let frame = negative(W, H, &stock, 0.0, 3);
    let mut data = frame.data().to_vec();
    // First pixel = clear base, second = brighter than base (sprocket hole).
    data[..3].copy_from_slice(&stock.base.map(|v| v as f32));
    data[3..6].copy_from_slice(&[1.0, 1.0, 1.0]);
    let frame = silvercraft_film::RgbImage::new(W, H, data).unwrap();
    let mut opts = FilmConfig::default();
    // The base as the scan stores it (f32), so clear film is exactly T = b.
    opts.base.dmin = Some(stock.base.map(|v| f64::from(v as f32)));
    let out = develop_single(&frame, None, film_profile("generic-c41").unwrap(), &opts).unwrap();
    assert_eq!(out.image.pixel(0, 0).unwrap(), [0.0; 3]);
    assert_eq!(out.image.pixel(1, 0).unwrap(), [0.0; 3]);
    assert!(out.image.data().iter().all(|v| v.is_finite() && *v >= 0.0));
}

#[test]
fn stock_path_equalizes_portra_layer_gammas() {
    // Portra 400 datasheet gammas (R 0.57, G 0.55, B 0.62). The stock's
    // corrected layer gain + G_ref 0.55 must give neutral, ratio-true output
    // even with the calibration fit switched off.
    let stock = Stock { base: common::ORANGE_BASE, gamma: [0.57, 0.55, 0.62], noise: 0.003 };
    let frame = negative(W, H, &stock, 0.0, 77);
    let profile = film_profile("kodak-portra-400").unwrap();
    let mut opts = FilmConfig::default();
    opts.base.dmin = Some(stock.base);
    opts.calibration.neutralize = Some(false);
    opts.white_balance.residual = false;
    let out = develop_single(&frame, None, profile, &opts).unwrap();
    assert_eq!(out.meta.g_ref, Some(0.55));
    for ev in [-3.0, -1.0, 0.0, 1.0, 3.0] {
        let p = window_mean(&out.image, ramp_x(ev), H / 2, 0);
        assert!(ratio_spread(p) < 0.02, "EV {ev}: {p:?}");
    }
    let a = lum(window_mean(&out.image, ramp_x(-2.0), H / 2, 0));
    let b = lum(window_mean(&out.image, ramp_x(2.0), H / 2, 0));
    let half = W / 2;
    let ev_at = |x: usize| -4.0 + 8.0 * x as f64 / (half - 1) as f64;
    let expected = 2f64.powf(ev_at(ramp_x(2.0)) - ev_at(ramp_x(-2.0)));
    assert!((b / a / expected - 1.0).abs() < 0.05, "4-stop ratio {} vs {expected}", b / a);
}

#[test]
fn roll_exposure_differences_survive_and_baseline_exposure_compensates() {
    // Three frames of the same scene at -1, 0, +1 EV: roll-level k keeps the
    // bracket in the pixels (2x per stop), BaselineExposure undoes it.
    let stock = Stock::even(0.60);
    let frames: Vec<_> = [-1.0, 0.0, 1.0].iter().enumerate().map(|(i, ev)| negative(W, H, &stock, *ev, 100 + i as u64)).collect();
    let inputs: Vec<FrameInput> = frames.iter().map(|f| FrameInput { image: f, gap_base: Some(stock.base), label: None }).collect();
    let mut opts = FilmConfig::default();
    opts.layer_gamma.measure = true;
    let roll = develop_roll(&inputs, film_profile("generic-c41").unwrap(), &opts).unwrap();
    assert_eq!(roll.frames.len(), 3);
    assert!(roll.frames.iter().all(|f| f.meta.k == roll.model.k), "k is roll-level");
    let (x, y) = patch_center(W, H, 1);
    let g: Vec<f64> = roll.frames.iter().map(|f| lum(window_mean(&f.image, x, y, 5))).collect();
    assert!((g[1] / g[0] / 2.0 - 1.0).abs() < 0.04, "{g:?}");
    assert!((g[2] / g[1] / 2.0 - 1.0).abs() < 0.04, "{g:?}");
    let ev: Vec<f64> = roll.frames.iter().map(|f| f.meta.baseline_exposure_ev).collect();
    assert!(ev[0] > ev[1] && ev[1] > ev[2], "{ev:?}");
    assert!(((ev[0] - ev[2]) - 2.0).abs() < 0.15, "{ev:?}");
    // Layer gamma was measured over the 3 frames (its value is scene-biased
    // on identical frames; the calibration fit absorbs that, as asserted
    // above by the clean 2x steps).
    assert_eq!(roll.model.layer_gamma.unwrap().frames_used, 3);
}

#[test]
fn roll_layer_gamma_measures_and_removes_a_process_residual() {
    // Grey-only frames from a roll whose chemistry pushed R +10% and pulled
    // B -10% relative to the profile (generic: layer gain 1). The pipeline
    // measures the residual and folds it into the calibration anchor.
    let stock = Stock { base: common::ORANGE_BASE, gamma: [0.66, 0.60, 0.54], noise: 0.002 };
    let frames: Vec<_> = (0..4)
        .map(|i| {
            let mut rng = common::Rng(500 + i);
            let data: Vec<f32> = (0..100 * 60)
                .flat_map(|_| {
                    let e = 0.18 * 2f64.powf(rng.uniform(-4.0, 4.0));
                    stock.transmittance([e; 3], &mut rng)
                })
                .collect();
            silvercraft_film::RgbImage::new(100, 60, data).unwrap()
        })
        .collect();
    let inputs: Vec<FrameInput> = frames.iter().map(|f| FrameInput { image: f, gap_base: Some(stock.base), label: None }).collect();
    let mut opts = FilmConfig::default();
    opts.calibration.neutralize = Some(false);
    opts.white_balance.residual = false;
    let roll = develop_roll(&inputs, film_profile("generic-c41").unwrap(), &opts).unwrap();
    let lg = roll.model.layer_gamma.unwrap().gamma.unwrap();
    assert!((lg[0] - 1.1).abs() < 0.01 && lg[1] == 1.0 && (lg[2] - 0.9).abs() < 0.01, "{lg:?}");
    assert!((roll.model.density_anchor[0] - 1.0 / 1.1).abs() < 0.01, "{:?}", roll.model.density_anchor);
    // With the fit OFF, only the measured anchor neutralizes the greys.
    for f in &roll.frames {
        for p in f.image.data().chunks_exact(3).take(500) {
            let p = [p[0] as f64, p[1] as f64, p[2] as f64];
            if lum(p) > 0.01 {
                assert!(ratio_spread(p) < 0.04, "{p:?}");
            }
        }
    }
}

#[test]
fn excitation_is_recoverable_from_l() {
    // Invariant 11.3(3): e' = G_ref log10(L/k + 1) recovers e.
    let (g, k) = (0.55, 0.0123);
    for e in [0.0, 0.05, 0.6, 1.4, 2.9] {
        let l = excitation_to_linear(e, g, k) as f32;
        assert!((linear_to_excitation(f64::from(l), g, k) - e).abs() < 1e-5);
    }
    // E0 sanity: the generator's mid grey density.
    assert!((0.60 * (1.0 + 0.18 / E0).log10() - 0.738).abs() < 1e-3);
}

#[test]
fn e_mid_estimators_compared() {
    // Same frame, each estimator: only the default lands mid grey at 0.18;
    // the density-domain binned median is toe-biased dark, the area median
    // follows this scene's surplus of bright grey patches.
    let stock = Stock::even(0.60);
    let frame = negative(W, H, &stock, 0.0, 0x51C0);
    let grey = |estimator: &str| {
        let mut cfg = silvercraft_film::FilmConfig::from_toml_str(&format!("[scene]\ne_mid = \"{estimator}\"")).unwrap();
        cfg.base.dmin = Some(stock.base.map(|v| f64::from(v as f32)));
        let out = develop_single(&frame, None, film_profile("generic-c41").unwrap(), &cfg).unwrap();
        let (x, y) = patch_center(W, H, 1);
        lum(window_mean(&out.image, x, y, 5))
    };
    let midpoint = grey("neutral-range-midpoint");
    let binned = grey("binned-median");
    let area = grey("all-neutral-median");
    assert!((midpoint / 0.18 - 1.0).abs() < 0.03, "{midpoint}");
    assert!(binned < 0.15 && area < 0.15, "binned {binned}, area {area}");
    let mut cfg = silvercraft_film::FilmConfig::from_toml_str("[scene]\ne_mid = \"fixed\"\ne_mid_fixed = 0.7382693528269643").unwrap();
    cfg.base.dmin = Some(stock.base.map(|v| f64::from(v as f32)));
    let out = develop_single(&frame, None, film_profile("generic-c41").unwrap(), &cfg).unwrap();
    assert_eq!(out.meta.calibration.unwrap().e_mid, Some(0.7382693528269643));
}
