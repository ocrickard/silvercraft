//! Hostile input never panics: empty, 1x1, all-zero, NaN, inf, garbage
//! parameters. Every entry point returns an error or a safe default.

#![allow(clippy::field_reassign_with_default)] // option tweaks read clearer as assignments in tests

use silvercraft_film::base::{BaseBand, FilmBaseSampleOptions, auto_film_base, capture_channel_health, film_base_trims, sample_film_base};
use silvercraft_film::calibrate::{FitParams, diagonal_calibration, fit_roll_calibration};
use silvercraft_film::invert::{InversionProfile, estimate_inversion_params};
use silvercraft_film::scene::{RollScale, bw_negative_to_scene_linear, negative_to_scene_linear, slide_as_is};
use silvercraft_film::{FilmConfig, FilmError, FrameInput, RgbImage, develop_roll, develop_single, film_profile};

fn images() -> Vec<RgbImage> {
    vec![
        RgbImage::filled(1, 1, [0.5, 0.4, 0.3]).unwrap(),
        RgbImage::filled(1, 1, [0.0; 3]).unwrap(),
        RgbImage::filled(7, 5, [0.0; 3]).unwrap(),
        RgbImage::filled(7, 5, [f32::NAN; 3]).unwrap(),
        RgbImage::filled(3, 3, [f32::INFINITY, f32::NEG_INFINITY, f32::NAN]).unwrap(),
        RgbImage::new(4, 1, vec![0.5, f32::NAN, 0.2, 1e30, -5.0, 0.1, 0.0, 0.3, f32::MIN_POSITIVE, 0.4, 0.4, 0.4]).unwrap(),
        RgbImage::filled(64, 48, [0.8, 0.5, 0.3]).unwrap(),
    ]
}

const STOCKS: [&str; 6] = ["generic-c41", "kodak-portra-400", "ilford-hp5-plus", "fuji-velvia-50", "kodak-tri-x-400", "ilford-xp2-super"];

#[test]
fn empty_images_are_rejected_at_construction() {
    assert!(matches!(RgbImage::new(0, 0, vec![]), Err(FilmError::EmptyImage { .. })));
    assert!(RgbImage::filled(0, 10, [0.0; 3]).is_err());
    assert!(develop_roll(&[], film_profile("generic-c41").unwrap(), &FilmConfig::default()).is_err());
}

#[test]
fn develop_never_panics_and_outputs_are_finite() {
    for image in images() {
        for stock in STOCKS {
            let profile = film_profile(stock).unwrap();
            for gap in [None, Some([0.0; 3]), Some([f64::NAN; 3]), Some([0.8, 0.5, 0.3])] {
                if let Ok(out) = develop_single(&image, gap, profile, &FilmConfig::default()) {
                    assert!(out.image.data().iter().all(|v| v.is_finite() && *v >= 0.0), "{stock}: non-finite output");
                    assert!(out.meta.as_shot_neutral.iter().all(|v| v.is_finite() && *v > 0.0));
                    assert!(out.meta.baseline_exposure_ev.abs() <= 3.0);
                    assert!((0.0..=1.0).contains(&out.meta.clipped_fraction));
                }
            }
        }
    }
}

#[test]
fn zero_and_nan_bases_are_errors_not_panics() {
    let image = RgbImage::filled(7, 5, [0.0; 3]).unwrap();
    let err = develop_single(&image, None, film_profile("generic-c41").unwrap(), &FilmConfig::default()).unwrap_err();
    assert!(matches!(err, FilmError::InvalidBase(_)));
    let mut opts = FilmConfig::default();
    opts.base.dmin = Some([0.5, 0.0, 0.5]);
    let good = RgbImage::filled(7, 5, [0.3; 3]).unwrap();
    assert!(develop_single(&good, None, film_profile("generic-c41").unwrap(), &opts).is_err());
    let cfg = FilmConfig::default();
    let scale = RollScale { g_ref: 0.6, k: 0.01 };
    assert!(negative_to_scene_linear(&good, &diagonal_calibration([f64::NAN; 3], [1.0; 3]), scale, [1.0; 3], &cfg).is_err());
    assert!(bw_negative_to_scene_linear(&good, [0.0; 3], scale, &cfg).is_err());
    for bad in [
        RollScale { g_ref: f64::NAN, k: 0.01 },
        RollScale { g_ref: 0.0, k: 0.01 },
        RollScale { g_ref: 0.6, k: f64::INFINITY },
        RollScale { g_ref: 0.6, k: -1.0 },
    ] {
        assert!(negative_to_scene_linear(&good, &diagonal_calibration([0.5; 3], [1.0; 3]), bad, [1.0; 3], &cfg).is_err());
    }
    assert!(slide_as_is(&RgbImage::filled(2, 2, [f32::NAN; 3]).unwrap(), None, false).is_err());
    assert!(film_profile("digital").is_none(), "digital originals belong to LightCraft");
    // An invalid config is refused up front with diagnostics.
    let mut bad_cfg = FilmConfig::default();
    bad_cfg.scene.mid_grey = f64::NAN;
    assert!(matches!(develop_single(&good, None, film_profile("generic-c41").unwrap(), &bad_cfg), Err(FilmError::InvalidConfig(_))));
}

#[test]
fn corrupt_calibration_documents_do_not_panic() {
    let good = RgbImage::filled(5, 5, [0.3, 0.2, 0.1]).unwrap();
    let mut doc = diagonal_calibration([0.8, 0.5, 0.3], [1.0; 3]);
    // Inverted and NaN curve bounds (std clamp would panic), NaN matrix.
    doc.curve_log_min = [5.0, f64::NAN, 1.0];
    doc.curve_log_max = [-5.0, 0.0, f64::NAN];
    doc.density_matrix[1][1] = f64::NAN;
    doc.curve_alpha = [f64::INFINITY, 0.0, f64::NAN];
    doc.curve_max_log_correction = f64::NAN;
    doc.e_mid = Some(f64::NAN);
    let scale = RollScale { g_ref: 0.6, k: 0.01 };
    let out = negative_to_scene_linear(&good, &doc, scale, [1.0; 3], &FilmConfig::default()).unwrap();
    assert!(out.image.data().iter().all(|v| !v.is_nan()));
    // Unvalidated hostile config values reach the transform: still no panic.
    let mut cfg = FilmConfig::default();
    cfg.scene.epsilon = f64::NAN;
    cfg.scene.noise_floor = Some([f64::NAN, -1.0, 2.0]);
    cfg.scene.single_channel_highlight_blend = true;
    cfg.white_balance.gain_min = f64::NAN;
    cfg.white_balance.neutral_bins = 0;
    cfg.white_balance.confidence_spread = 0.0;
    cfg.exposure.clamp_ev = f64::NAN;
    assert!(negative_to_scene_linear(&good, &doc, scale, [f64::NAN, -1.0, 0.0], &cfg).is_ok());
}

#[test]
fn low_level_helpers_are_total() {
    for image in images() {
        let opts = FilmBaseSampleOptions {
            y0: 1,
            y1: 0,
            percentile: f64::NAN,
            min_margin_pixels: 0,
            max_samples: 0,
            band: BaseBand::Auto,
            auto_reach: f64::NAN,
            auto_keep: -1.0,
        };
        let _ = sample_film_base(&image, &opts);
        for band in [BaseBand::FullMargin, BaseBand::Rows(0), BaseBand::Rows(usize::MAX)] {
            let opts = FilmBaseSampleOptions { y0: usize::MAX, y1: 0, band, ..FilmBaseSampleOptions::new(0, 0) };
            let _ = sample_film_base(&image, &opts);
            let opts = FilmBaseSampleOptions { y0: 1, y1: usize::MAX, band, min_margin_pixels: 0, ..FilmBaseSampleOptions::new(0, 0) };
            let _ = sample_film_base(&image, &opts);
        }
        let _ = auto_film_base(&image, usize::MAX, usize::MAX);
        let _ = auto_film_base(&image, 0, 0);
        let _ = film_base_trims(&image, usize::MAX, usize::MAX);
        let _ = capture_channel_health(image.data(), image.data());
        let mut p = InversionProfile::from_spec(film_profile("kodak-portra-400").unwrap());
        p.epsilon = f32::NAN;
        p.base_hint = Some([f32::NAN, 0.0, f32::INFINITY]);
        let _ = estimate_inversion_params(image.data(), &p);
        let frames = [image.data()];
        for eps in [f64::NAN, 5.0, -1.0] {
            let params =
                FitParams { density_scale: [f64::NAN, 0.0, 1.0], low_percentile: -5.0, high_percentile: 500.0, epsilon: eps, neutralize: true };
            let _ = fit_roll_calibration(&frames, &[[0.0, f64::NAN, 0.5]], &params);
        }
    }
    let frames: [&[f32]; 1] = [&[0.5; 3]];
    assert!(
        fit_roll_calibration(
            &frames,
            &[],
            &FitParams { density_scale: [1.0; 3], low_percentile: 1.0, high_percentile: 99.0, epsilon: 1e-6, neutralize: true }
        )
        .is_err()
    );
}

#[test]
fn roll_of_mixed_garbage_frames() {
    let imgs = images();
    let inputs: Vec<FrameInput> = imgs.iter().map(FrameInput::new).collect();
    for stock in STOCKS {
        let mut opts = FilmConfig::default();
        opts.strip_balance.enabled = true;
        let _ = develop_roll(&inputs, film_profile(stock).unwrap(), &opts);
        opts.base.dmin = Some([0.8, 0.5, 0.3]);
        if let Ok(roll) = develop_roll(&inputs, film_profile(stock).unwrap(), &opts) {
            assert_eq!(roll.frames.len(), inputs.len());
        }
    }
}
