//! Spec 2.7 / 11.3(5): which direction of `density_scale` equalizes layer
//! gammas?
//!
//! FINDING (documented by these tests): an early stored convention kept
//! `density_scale_c = grad_c / grad_G` and MULTIPLIED density by it. On a
//! synthetic negative with known
//! layer gammas that direction makes the layer imbalance WORSE (roughly
//! squares it); the equalizer is the reciprocal `gamma_G / gamma_c`.
//! SilverCraft profiles store the measured layer gammas (R,G,B) and
//! `FilmProfile::layer_gain()` derives the reciprocal (equalizing) direction.

#![allow(clippy::field_reassign_with_default)] // option tweaks read clearer as assignments in tests

mod common;

use common::{Rng, Stock};
use silvercraft_film::{FilmConfig, FilmKind, FilmProfile, RgbImage, develop_single};

/// Layer gammas of the sign test: R 0.60, G 0.55, B 0.62.
const GAMMA: [f64; 3] = [0.60, 0.55, 0.62];

/// A colour-negative profile with scene_gamma = gamma_G whose layer gain is
/// `1 / density_scale`, i.e. the profile built from a stored `density_scale`
/// (layer_gamma_c = density_scale_c * gamma_G).
fn derived_stock(density_scale: [f64; 3]) -> FilmProfile {
    let mut p = FilmProfile::template(FilmKind::ColorNegative);
    let neg = p.negative.as_mut().unwrap();
    neg.scene_gamma = Some(GAMMA[1]);
    neg.layer_gamma = Some(density_scale.map(|d| d * GAMMA[1]));
    p
}

fn stored_convention() -> [f64; 3] {
    [GAMMA[0] / GAMMA[1], 1.0, GAMMA[2] / GAMMA[1]]
}

/// A neutral ramp over 6 EV (-3..+3 around 0.18).
fn ramp() -> (RgbImage, Stock) {
    let stock = Stock { base: common::ORANGE_BASE, gamma: GAMMA, noise: 0.0 };
    let mut rng = Rng(1);
    let n = 601;
    let data: Vec<f32> = (0..n)
        .flat_map(|i| {
            let e = 0.18 * 2f64.powf(-3.0 + 6.0 * i as f64 / (n - 1) as f64);
            stock.transmittance([e; 3], &mut rng)
        })
        .collect();
    (RgbImage::new(n, 1, data).unwrap(), stock)
}

/// Worst channel peak-to-peak / mean over the ramp after a develop with the
/// calibration fit and residual WB switched OFF, so only the layer gain acts.
fn worst_ptp(spec: &FilmProfile) -> f64 {
    let (frame, stock) = ramp();
    let mut opts = FilmConfig::default();
    opts.base.dmin = Some(stock.base.map(|v| f64::from(v as f32)));
    opts.calibration.neutralize = Some(false);
    opts.white_balance.residual = false;
    opts.scene.highlight_cap = false;
    let out = develop_single(&frame, None, spec, &opts).unwrap();
    out.image
        .data()
        .chunks_exact(3)
        .map(|p| {
            let mean = (p[0] + p[1] + p[2]) as f64 / 3.0;
            let ptp = p.iter().copied().fold(f32::MIN, f32::max) - p.iter().copied().fold(f32::MAX, f32::min);
            f64::from(ptp) / mean
        })
        .fold(0.0, f64::max)
}

#[test]
fn layer_gain_reciprocal_of_stored_density_scale_equalizes_layers() {
    let spec = derived_stock(stored_convention());
    let gain = spec.layer_gain();
    assert!((gain[0] - GAMMA[1] / GAMMA[0]).abs() < 1e-6 && (gain[2] - GAMMA[1] / GAMMA[2]).abs() < 1e-6);
    assert_eq!(spec.scene_gamma(), GAMMA[1]);
    let ptp = worst_ptp(&spec);
    assert!(ptp < 0.01, "SilverCraft direction must be neutral across 6 EV: ptp {ptp}");
}

#[test]
fn multiplying_by_the_stored_scale_amplifies_the_imbalance() {
    // Make layer_gain() equal the STORED value, i.e. reproduce the old
    // `density * density_scale` direction.
    let stored = stored_convention();
    let stored_like = derived_stock(stored.map(|v| 1.0 / v));
    let wrong = worst_ptp(&stored_like);
    let uncorrected = worst_ptp(&derived_stock([1.0; 3]));
    let right = worst_ptp(&derived_stock(stored));
    assert!(right < 0.01);
    assert!(uncorrected > 0.10, "the raw layers really are unequal: {uncorrected}");
    assert!(wrong > uncorrected * 1.5, "stored direction worsens it: wrong {wrong} vs uncorrected {uncorrected}");
}

#[test]
fn equalizing_direction_in_density_terms() {
    // The bare algebra, independent of the pipeline: D_c = gamma_c * x.
    for x in [0.2, 0.7, 1.5] {
        let d = GAMMA.map(|g| g * x);
        let fixed = [0, 1, 2].map(|c| d[c] * GAMMA[1] / GAMMA[c]);
        let stored_dir = [0, 1, 2].map(|c| d[c] * stored_convention()[c]);
        assert!((fixed[0] - fixed[1]).abs() < 1e-12 && (fixed[2] - fixed[1]).abs() < 1e-12);
        assert!((stored_dir[2] / stored_dir[1] - (GAMMA[2] / GAMMA[1]).powi(2)).abs() < 1e-12);
    }
}
