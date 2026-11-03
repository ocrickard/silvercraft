//! The normative scene-linear output transform (2.3, 2.5).
//!
//! Colour negative, per pixel:
//!
//! ```text
//! D_c = clamp(-log10(max(T_c, SENSOR_FLOOR) / b_c), 0, Dcap_c)
//! e   = alpha/gamma curve( M . D )             (roll calibration, or diag fallback)
//! L_c = k * (10^(e_c / G_ref) - 1)              (base -> exactly 0)
//! k   = mid_grey / (10^(e_mid / G_ref) - 1)     (roll-level)
//! ```
//!
//! No tone curve, paper, shoulder or encode. The per-frame residual white
//! balance is MEASURED on `L` and returned as `AsShotNeutral` (never baked),
//! and per-frame exposure as a `BaselineExposure` delta. Every knob comes
//! from [`FilmConfig`].
//!
//! Colour primaries: nothing here assumes sRGB. Channels are treated as
//! three independent records; only the Rec.709 luma weights are used, for
//! neutral selection and medians.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::base::{SENSOR_FLOOR, base_is_usable};
use crate::calibrate::{RollCalibrationDocument, apply_calibration};
use crate::config::{EMidEstimator, ExposureConfig, ExposureMode, FilmConfig, PositiveMode, SceneConfig, WhiteBalanceConfig, WhiteSource};
use crate::error::FilmError;
use crate::image::RgbImage;
use crate::invert::{NeutralParams, neutral_balance_gains_with};
use crate::stats::{LUMA, LUMA_F32, channel_percentiles_finite, clamp_f64, median_f64, percentile_f32};

/// Default scene mid-grey (`scene.mid_grey`).
pub const MID_GREY: f64 = 0.18;
/// Default `scene.e_mid_default`.
pub const DEFAULT_E_MID: f64 = 0.65;
/// Default `scene.e_mid_range`.
pub const E_MID_RANGE: (f64, f64) = (0.05, 4.0);
/// Default `exposure.clamp_ev`.
pub const MAX_BASELINE_EV: f64 = 3.0;

/// Whether the pixel data is scene-referred (negatives, unrendered slides)
/// or a display-referred print passed through (as-is slides).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Referred {
    Scene,
    Display,
}

/// Roll-level scale of the transform.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RollScale {
    pub g_ref: f64,
    pub k: f64,
}

/// Everything the handoff writes besides the pixels (DNG tags + the
/// `sc:` metadata blob).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneMetadata {
    pub referred: Referred,
    /// `AsShotNeutral` = 1/g normalized so G = 1 (not baked into pixels).
    pub as_shot_neutral: [f64; 3],
    /// The (confidence-shrunk, bounded) residual WB multipliers `g`.
    pub wb_gains: [f64; 3],
    pub scene_confidence: f64,
    /// Per-frame exposure for BaselineExposure (see `[exposure]`).
    pub baseline_exposure_ev: f64,
    pub median_neutral_l: f64,
    /// Fraction of pixels with at least one channel at the noise floor (Dcap).
    pub clipped_fraction: f64,
    /// Per-channel resolvable density cap (`None` for slides).
    pub dcap: Option<[f64; 3]>,
    /// G_ref (`None` for slides).
    pub g_ref: Option<f64>,
    pub k: f64,
    /// Film base (negatives) or slide white (positives).
    pub film_base: [f64; 3],
    pub calibration: Option<RollCalibrationDocument>,
}

/// A developed frame: f32 linear RGB + metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneFrame {
    pub image: RgbImage,
    pub meta: SceneMetadata,
}

/// Roll-level scale with default `[scene]` settings.
pub fn k_from_e_mid(e_mid: Option<f64>, g_ref: f64) -> f64 {
    k_from_e_mid_with(e_mid, g_ref, &SceneConfig::default())
}

/// `k = mid_grey / (10^(e_mid / G_ref) - 1)`; `e_mid` falls back to
/// `e_mid_default` when absent/non-finite and is clamped to `e_mid_range`.
pub fn k_from_e_mid_with(e_mid: Option<f64>, g_ref: f64, scene: &SceneConfig) -> f64 {
    let g_ref = sanitize_g_ref(g_ref);
    let default = if scene.e_mid_default.is_finite() && scene.e_mid_default > 0.0 { scene.e_mid_default } else { DEFAULT_E_MID };
    let e = e_mid.filter(|v| v.is_finite()).unwrap_or(default);
    let lo = if scene.e_mid_range[0] > 0.0 { scene.e_mid_range[0] } else { E_MID_RANGE.0 };
    let e = clamp_f64(e, lo, scene.e_mid_range[1]);
    let e = if e.is_finite() && e > 0.0 { e } else { DEFAULT_E_MID };
    let mid = if scene.mid_grey.is_finite() && scene.mid_grey > 0.0 { scene.mid_grey } else { MID_GREY };
    mid / (10f64.powf(e / g_ref) - 1.0)
}

/// `L = k (10^(e / G_ref) - 1)`, with `e` floored at 0 (base -> 0).
#[inline]
pub fn excitation_to_linear(e: f64, g_ref: f64, k: f64) -> f64 {
    let e = if e.is_nan() { 0.0 } else { e.max(0.0) };
    k * (10f64.powf(e / g_ref) - 1.0)
}

/// Inverse of [`excitation_to_linear`]: `e = G_ref log10(L / k + 1)`.
#[inline]
pub fn linear_to_excitation(l: f64, g_ref: f64, k: f64) -> f64 {
    g_ref * (l.max(0.0) / k + 1.0).log10()
}

fn sanitize_g_ref(g_ref: f64) -> f64 {
    if g_ref.is_finite() && g_ref > 1e-3 { g_ref } else { crate::profiles::DEFAULT_COLOR_NEGATIVE_GAMMA }
}

/// Auto exposure with defaults: `log2(0.18 / median)`, +-3 EV.
pub fn baseline_exposure_ev(median_neutral_l: f64) -> f64 {
    frame_exposure_ev(median_neutral_l, &ExposureConfig::default(), MID_GREY)
}

/// Per-frame BaselineExposure per `[exposure]`.
pub fn frame_exposure_ev(median_neutral_l: f64, exposure: &ExposureConfig, mid_grey: f64) -> f64 {
    let limit = if exposure.clamp_ev.is_finite() { exposure.clamp_ev.max(0.0) } else { MAX_BASELINE_EV };
    match exposure.mode {
        ExposureMode::Roll => 0.0,
        ExposureMode::Fixed => {
            if exposure.fixed_ev.is_finite() {
                exposure.fixed_ev
            } else {
                0.0
            }
        }
        ExposureMode::Auto => {
            if median_neutral_l.is_nan() || mid_grey.is_nan() || mid_grey <= 0.0 {
                0.0
            } else if median_neutral_l <= 0.0 {
                limit
            } else {
                clamp_f64((mid_grey / median_neutral_l).log2(), -limit, limit)
            }
        }
    }
}

/// [`scene_confidence_with`] with default thresholds.
pub fn scene_confidence(rgb: &[f32]) -> f64 {
    scene_confidence_with(rgb, &WhiteBalanceConfig::default())
}

/// How far the scene satisfies
/// the grey-world family's assumption, 0..1. A tight chromaticity cluster far
/// from neutral (lawn, red wall) is dangerous; a spread or neutral one is safe.
pub fn scene_confidence_with(rgb: &[f32], wb: &WhiteBalanceConfig) -> f64 {
    let mut chroma: Vec<[f64; 3]> = Vec::with_capacity(rgb.len() / 3);
    for pixel in rgb.chunks_exact(3) {
        let p = [f64::from(pixel[0]), f64::from(pixel[1]), f64::from(pixel[2])];
        let total = p[0] + p[1] + p[2];
        if total > 1e-6 && total.is_finite() {
            chroma.push(p.map(|v| v / total));
        }
    }
    if chroma.len() < wb.confidence_min_pixels.max(1) {
        return 0.0;
    }
    let mut centroid = [0.0; 3];
    for (c, slot) in centroid.iter_mut().enumerate() {
        let mut values: Vec<f64> = chroma.iter().map(|x| x[c]).collect();
        *slot = median_f64(&mut values);
    }
    let mut deviations: Vec<f64> = chroma.iter().map(|x| (0..3).map(|c| (x[c] - centroid[c]).abs()).fold(0.0, f64::max)).collect();
    let spread = median_f64(&mut deviations);
    let bias = centroid.iter().map(|v| (v - 1.0 / 3.0).abs()).fold(0.0, f64::max);
    let danger = clamp_f64(1.0 - spread / wb.confidence_spread, 0.0, 1.0) * clamp_f64(bias / wb.confidence_bias, 0.0, 1.0);
    let conf = clamp_f64(1.0 - danger, 0.0, 1.0);
    if conf.is_nan() { 0.0 } else { conf }
}

/// Residual white balance measured on scene-linear data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResidualWhiteBalance {
    pub wb_gains: [f64; 3],
    pub as_shot_neutral: [f64; 3],
    pub scene_confidence: f64,
    pub median_neutral_l: f64,
}

/// Spec 0.3 "White balance" + the exposure median: two-pass neutral gains on
/// `L` (bounded), shrunk toward 1 by `g^conf`, reported as `AsShotNeutral =
/// 1/g` normalized to G = 1. `median_neutral_l` is the median luminance of the
/// WB-corrected near-neutral pixels (log spread under the ceiling, L > 0),
/// else of every lit pixel.
pub fn residual_white_balance(linear: &[f32], wb: &WhiteBalanceConfig) -> ResidualWhiteBalance {
    let (wb_gains, confidence) = if wb.residual {
        let params = NeutralParams::from_white_balance(wb);
        let raw = neutral_balance_gains_with(linear, wb.gray_percentile as f32, &params).map(f64::from);
        let conf = if wb.confidence_shrink { scene_confidence_with(linear, wb) } else { 1.0 };
        (raw.map(|g| clamp_f64(g.powf(conf), wb.gain_min, wb.gain_max)), conf)
    } else {
        ([1.0; 3], 0.0)
    };
    let wb_gains = wb_gains.map(|g| if g.is_finite() && g > 0.0 { g } else { 1.0 });
    let as_shot_neutral = [wb_gains[1] / wb_gains[0], 1.0, wb_gains[1] / wb_gains[2]];
    let mut neutral_lum = Vec::new();
    let mut lit_lum = Vec::new();
    for pixel in linear.chunks_exact(3) {
        let p = [0, 1, 2].map(|c| f64::from(pixel[c]) * wb_gains[c]);
        let lum = LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2];
        if !(lum.is_finite() && p.iter().all(|v| *v > 0.0)) {
            continue;
        }
        lit_lum.push(lum);
        let z = p.map(f64::ln);
        let mean = (z[0] + z[1] + z[2]) / 3.0;
        let achro = (((z[0] - mean).powi(2) + (z[1] - mean).powi(2) + (z[2] - mean).powi(2)) / 3.0).sqrt();
        if achro < wb.neutral_achro_ceiling {
            neutral_lum.push(lum);
        }
    }
    let median_neutral_l = if neutral_lum.len() > wb.neutral_min_count {
        median_f64(&mut neutral_lum)
    } else if !lit_lum.is_empty() {
        median_f64(&mut lit_lum)
    } else {
        0.0
    };
    ResidualWhiteBalance { wb_gains, as_shot_neutral, scene_confidence: confidence, median_neutral_l }
}

/// `Dcap_c = max(-log10(noise_floor_c / b_c), 0)` with `noise_floor_c` =
/// max(`percentile` of the frame's finite values, SENSOR_FLOOR) unless
/// overridden. Returns (Dcap, noise floor).
pub fn highlight_cap(frame: &RgbImage, film_base: [f64; 3], noise_floor: Option<[f64; 3]>, percentile: f64) -> ([f64; 3], [f64; 3]) {
    let floor = f64::from(SENSOR_FLOOR);
    let nf = noise_floor.unwrap_or_else(|| channel_percentiles_finite(frame.data(), percentile as f32).map(|p| p.map_or(floor, f64::from)));
    let nf = nf.map(|v| if v.is_nan() { floor } else { v.max(floor) });
    let dcap = [0, 1, 2].map(|c| {
        let d = -(nf[c] / film_base[c]).log10();
        if d.is_nan() { 0.0 } else { d.max(0.0) }
    });
    (dcap, nf)
}

fn check_g_ref(g_ref: f64) -> Result<f64, FilmError> {
    if g_ref.is_finite() && g_ref > 1e-3 { Ok(g_ref) } else { Err(FilmError::InvalidParameter("g_ref must be positive and finite")) }
}

fn check_k(k: f64) -> Result<f64, FilmError> {
    if k.is_finite() && k > 0.0 { Ok(k) } else { Err(FilmError::InvalidParameter("k must be positive and finite")) }
}

fn metadata(image: &RgbImage, cfg: &FilmConfig, residual: bool) -> ResidualWhiteBalance {
    let mut wb = cfg.white_balance.clone();
    wb.residual = wb.residual && residual;
    residual_white_balance(image.data(), &wb)
}

/// Colour negative -> scene-linear `L` with a roll calibration
/// (use [`crate::calibrate::diagonal_calibration`] as the fallback).
/// `residual_gain` multiplies the excitation (per-strip balance.1).
pub fn negative_to_scene_linear(
    frame: &RgbImage,
    calibration: &RollCalibrationDocument,
    scale: RollScale,
    residual_gain: [f64; 3],
    cfg: &FilmConfig,
) -> Result<SceneFrame, FilmError> {
    let base = calibration.film_base;
    if !base_is_usable(base) {
        return Err(FilmError::InvalidBase(base));
    }
    let g_ref = check_g_ref(scale.g_ref)?;
    let k = check_k(scale.k)?;
    let sc = &cfg.scene;
    let (dcap, noise_floor) = highlight_cap(frame, base, sc.noise_floor, sc.noise_floor_percentile);
    let applied_cap = if sc.highlight_cap { dcap } else { [f64::INFINITY; 3] };
    let blend = sc.highlight_cap && sc.single_channel_highlight_blend;
    let eps = clamp_f64(sc.epsilon, 1e-12, 1.0);
    let gain = residual_gain.map(|g| if g.is_finite() && g > 0.0 { g } else { 1.0 });
    let results: Vec<([f32; 3], bool)> = frame
        .data()
        .par_chunks_exact(3)
        .map(|pixel| {
            let t = [f64::from(pixel[0]), f64::from(pixel[1]), f64::from(pixel[2])];
            let at_floor = [0, 1, 2].map(|c| t[c].is_nan() || t[c] <= noise_floor[c]);
            let mut e = apply_calibration(t, calibration, eps, applied_cap);
            if blend && at_floor.iter().filter(|x| **x).count() == 1 {
                // Hue-preserving highlight blend: the capped record's true
                // density is at least the cap; take the neutral-axis estimate
                // from the other two when it is higher. Never lowers.
                if let Some(c) = at_floor.iter().position(|x| *x) {
                    let others = (e[(c + 1) % 3] + e[(c + 2) % 3]) / 2.0;
                    e[c] = e[c].max(others);
                }
            }
            let l = [0, 1, 2].map(|c| excitation_to_linear(e[c] * gain[c], g_ref, k) as f32);
            (l, at_floor.iter().any(|x| *x))
        })
        .collect();
    let clipped = results.iter().filter(|(_, c)| *c).count();
    let data: Vec<f32> = results.iter().flat_map(|(l, _)| *l).collect();
    let image = RgbImage::new(frame.width(), frame.height(), data)?;
    let wb = metadata(&image, cfg, true);
    Ok(SceneFrame {
        meta: SceneMetadata {
            referred: Referred::Scene,
            as_shot_neutral: wb.as_shot_neutral,
            wb_gains: wb.wb_gains,
            scene_confidence: wb.scene_confidence,
            baseline_exposure_ev: frame_exposure_ev(wb.median_neutral_l, &cfg.exposure, sc.mid_grey),
            median_neutral_l: wb.median_neutral_l,
            clipped_fraction: clipped as f64 / frame.pixel_count() as f64,
            dcap: Some(dcap),
            g_ref: Some(g_ref),
            k,
            film_base: base,
            calibration: Some(calibration.clone()),
        },
        image,
    })
}

/// B&W density above base for one pixel: mean over channels of
/// `log10(b_c / max(T_c, floor))`, floored at 0.
#[inline]
pub fn bw_excitation(pixel: &[f32], base: [f64; 3]) -> f64 {
    let floor = f64::from(SENSOR_FLOOR);
    let mut sum = 0.0;
    for c in 0..3 {
        let t = pixel.get(c).map_or(floor, |v| f64::from(*v).max(floor));
        sum += (base[c] / t).log10();
    }
    let d = sum / 3.0;
    if d.is_nan() { 0.0 } else { d.max(0.0) }
}

/// Roll `e_mid` for B&W: every pixel is neutral, so both median estimators
/// are the median B&W density over the pooled samples; "fixed" uses
/// `e_mid_fixed`. `None` when there are no samples.
pub fn bw_e_mid(samples: &[&[f32]], base: [f64; 3], scene: &SceneConfig) -> Option<f64> {
    if scene.e_mid == EMidEstimator::Fixed {
        return scene.e_mid_fixed;
    }
    let mut values: Vec<f64> = samples.iter().flat_map(|s| s.chunks_exact(3).map(|p| bw_excitation(p, base))).collect();
    if values.is_empty() { None } else { Some(median_f64(&mut values)) }
}

/// B&W negative -> scene-linear: `S = D_mean / G_ref`,
/// `L = k (10^S - 1)` on all three channels, `AsShotNeutral = 1,1,1`.
/// Chromogenic (XP2) tinted bases are neutralized by the per-channel division.
pub fn bw_negative_to_scene_linear(frame: &RgbImage, film_base: [f64; 3], scale: RollScale, cfg: &FilmConfig) -> Result<SceneFrame, FilmError> {
    if !base_is_usable(film_base) {
        return Err(FilmError::InvalidBase(film_base));
    }
    let g_ref = check_g_ref(scale.g_ref)?;
    let k = check_k(scale.k)?;
    let sc = &cfg.scene;
    let (dcap3, noise_floor) = highlight_cap(frame, film_base, sc.noise_floor, sc.noise_floor_percentile);
    let dcap = (dcap3[0] + dcap3[1] + dcap3[2]) / 3.0;
    let results: Vec<(f32, bool)> = frame
        .data()
        .par_chunks_exact(3)
        .map(|pixel| {
            let clipped = (0..3).any(|c| pixel[c].is_nan() || f64::from(pixel[c]) <= noise_floor[c]);
            let mut d = bw_excitation(pixel, film_base);
            if sc.highlight_cap {
                d = d.min(dcap);
            }
            (excitation_to_linear(d, g_ref, k) as f32, clipped)
        })
        .collect();
    let clipped = results.iter().filter(|(_, c)| *c).count();
    let data: Vec<f32> = results.iter().flat_map(|(l, _)| [*l, *l, *l]).collect();
    let image = RgbImage::new(frame.width(), frame.height(), data)?;
    let wb = metadata(&image, cfg, false);
    Ok(SceneFrame {
        meta: SceneMetadata {
            referred: Referred::Scene,
            as_shot_neutral: [1.0; 3],
            wb_gains: [1.0; 3],
            scene_confidence: 0.0,
            baseline_exposure_ev: frame_exposure_ev(wb.median_neutral_l, &cfg.exposure, sc.mid_grey),
            median_neutral_l: wb.median_neutral_l,
            clipped_fraction: clipped as f64 / frame.pixel_count() as f64,
            dcap: Some([dcap; 3]),
            g_ref: Some(g_ref),
            k,
            film_base,
            calibration: None,
        },
        image,
    })
}

/// Slide "as-is" with a given (or p99.9 auto) white; see [`positive_to_linear`].
pub fn slide_as_is(frame: &RgbImage, t_white: Option<[f64; 3]>, monochrome: bool) -> Result<SceneFrame, FilmError> {
    let mut cfg = FilmConfig::default();
    if let Some(white) = t_white {
        cfg.positive.white_source = WhiteSource::Explicit;
        cfg.positive.white = Some(white);
    }
    positive_to_linear(frame, PositiveMode::AsIs, None, monochrome, &cfg)
}

/// Positives. `white` = explicit `positive.white` or the
/// per-channel `positive.white_percentile` of the frame.
///
/// - as-is: `L_c = T_c / T_white_c`, display-referred (the backlight colour is
///   the slide's "base").
/// - unrender (experimental): `t = T / T_white`, `L_c = mid_grey *
///   (t_c / t_mid)^(1/gamma)` with `t_mid` the frame's median luminance of `t`;
///   scene-referred, toe/shoulder remain.
///
/// `monochrome` averages the channels (positive B&W). No gray balance, no
/// rescue; exposure follows `[exposure]` (auto measures the median for
/// unrender only).
pub fn positive_to_linear(
    frame: &RgbImage,
    mode: PositiveMode,
    gamma: Option<f64>,
    monochrome: bool,
    cfg: &FilmConfig,
) -> Result<SceneFrame, FilmError> {
    let pc = &cfg.positive;
    let white = match (pc.white_source, pc.white) {
        (WhiteSource::Explicit, Some(w)) => w,
        _ => channel_percentiles_finite(frame.data(), pc.white_percentile as f32).map(|p| p.map_or(0.0, f64::from)),
    };
    if !base_is_usable(white) {
        return Err(FilmError::InvalidBase(white));
    }
    let gamma = gamma.or(pc.gamma).filter(|g| g.is_finite() && *g > 0.1).unwrap_or(if pc.default_gamma > 0.1 { pc.default_gamma } else { 2.0 });
    let normalized: Vec<f32> = frame
        .data()
        .par_chunks_exact(3)
        .flat_map_iter(|pixel| {
            let l = [0, 1, 2].map(|c| {
                let v = f64::from(pixel[c]) / white[c];
                if v.is_finite() { v.max(0.0) } else { 0.0 }
            });
            if monochrome {
                let m = ((l[0] + l[1] + l[2]) / 3.0) as f32;
                [m, m, m]
            } else {
                l.map(|v| v as f32)
            }
        })
        .collect();
    let mid_grey = if cfg.scene.mid_grey > 0.0 { cfg.scene.mid_grey } else { MID_GREY };
    let (data, referred, k) = match mode {
        PositiveMode::AsIs => (normalized, Referred::Display, 1.0),
        PositiveMode::Unrender => {
            let t_mid = f64::from(median_luminance(&normalized)).max(1e-6);
            let data = normalized.par_iter().map(|t| (mid_grey * (f64::from(*t) / t_mid).powf(1.0 / gamma)) as f32).collect();
            (data, Referred::Scene, mid_grey)
        }
    };
    let image = RgbImage::new(frame.width(), frame.height(), data)?;
    let median = f64::from(median_luminance(image.data()));
    let exposure = match (referred, cfg.exposure.mode) {
        (Referred::Display, ExposureMode::Auto) => 0.0,
        _ => frame_exposure_ev(median, &cfg.exposure, mid_grey),
    };
    Ok(SceneFrame {
        meta: SceneMetadata {
            referred,
            as_shot_neutral: [1.0; 3],
            wb_gains: [1.0; 3],
            scene_confidence: 0.0,
            baseline_exposure_ev: exposure,
            median_neutral_l: median,
            clipped_fraction: 0.0,
            dcap: None,
            g_ref: None,
            k,
            film_base: white,
            calibration: None,
        },
        image,
    })
}

/// Rec.709 luminance median of an interleaved buffer.
pub fn median_luminance(rgb: &[f32]) -> f32 {
    let mut lum: Vec<f32> = rgb.chunks_exact(3).map(|p| LUMA_F32[0] * p[0] + LUMA_F32[1] * p[1] + LUMA_F32[2] * p[2]).collect();
    percentile_f32(&mut lum, 50.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibrate::diagonal_calibration;

    #[test]
    fn transform_round_trips_and_pins_mid_grey() {
        let g = 0.55;
        let k = k_from_e_mid(Some(0.7), g);
        assert!((excitation_to_linear(0.7, g, k) - MID_GREY).abs() < 1e-12);
        assert_eq!(excitation_to_linear(0.0, g, k), 0.0);
        assert_eq!(excitation_to_linear(-1.0, g, k), 0.0);
        assert_eq!(excitation_to_linear(f64::NAN, g, k), 0.0);
        for e in [0.01, 0.3, 1.0, 2.5] {
            let l = excitation_to_linear(e, g, k);
            assert!((linear_to_excitation(l, g, k) - e).abs() < 1e-12);
        }
        // e_mid fallbacks and clamps keep k finite.
        for e_mid in [None, Some(f64::NAN), Some(0.0), Some(-3.0), Some(100.0)] {
            let k = k_from_e_mid(e_mid, g);
            assert!(k.is_finite() && k > 0.0, "{e_mid:?} -> {k}");
        }
        assert!(k_from_e_mid(Some(0.7), f64::NAN).is_finite());
    }

    #[test]
    fn baseline_exposure_is_bounded() {
        assert!((baseline_exposure_ev(0.09) - 1.0).abs() < 1e-12);
        assert!((baseline_exposure_ev(0.36) + 1.0).abs() < 1e-12);
        assert_eq!(baseline_exposure_ev(1e-9), 3.0);
        assert_eq!(baseline_exposure_ev(1e9), -3.0);
        assert_eq!(baseline_exposure_ev(0.0), 3.0);
        assert_eq!(baseline_exposure_ev(f64::NAN), 0.0);
    }

    #[test]
    fn scene_confidence_flags_dominant_colour_only() {
        // Varied hues: safe.
        let varied: Vec<f32> = (0..2000).flat_map(|i| [(i % 7) as f32 / 7.0 + 0.1, (i % 5) as f32 / 5.0 + 0.1, (i % 3) as f32 / 3.0 + 0.1]).collect();
        assert!(scene_confidence(&varied) > 0.9);
        // Tight neutral cluster: safe.
        let grey: Vec<f32> = (0..2000).flat_map(|i| [0.3 + 0.0001 * i as f32; 3]).collect();
        assert!((scene_confidence(&grey) - 1.0).abs() < 1e-9);
        // A lawn: tight cluster far from neutral -> no trust.
        let lawn: Vec<f32> = (0..2000).flat_map(|i| [0.1, 0.5 + 0.00001 * i as f32, 0.08]).collect();
        assert!(scene_confidence(&lawn) < 0.05);
        assert_eq!(scene_confidence(&lawn[..300]), 0.0, "fewer than 500 pixels");
    }

    #[test]
    fn residual_wb_is_reported_not_baked_and_bounded() {
        // A neutral ramp under a cool cast in L, plus varied colour so the
        // scene confidence is high.
        let cast = [0.9_f32, 1.0, 1.15];
        let mut l = Vec::new();
        for i in 0..4000 {
            let t = 0.02 + 1.5 * i as f32 / 4000.0;
            l.extend(cast.map(|c| c * t));
        }
        for i in 0..2000 {
            l.extend([(i % 7) as f32 / 7.0 + 0.05, (i % 5) as f32 / 5.0 + 0.05, (i % 3) as f32 / 3.0 + 0.05]);
        }
        let wb = residual_white_balance(&l, &WhiteBalanceConfig::default());
        assert_eq!(wb.as_shot_neutral[1], 1.0);
        // AsShotNeutral points at the cast (normalized to G), shrunk toward 1
        // by the scene confidence (the cast ramp is itself a tight cluster).
        assert!(wb.scene_confidence > 0.3 && wb.scene_confidence < 1.0, "{wb:?}");
        assert!(wb.as_shot_neutral[0] > 0.88 && wb.as_shot_neutral[0] < 0.99, "{wb:?}");
        assert!(wb.as_shot_neutral[2] > 1.02 && wb.as_shot_neutral[2] < 1.17, "{wb:?}");
        assert!((wb.as_shot_neutral[0] - wb.wb_gains[1] / wb.wb_gains[0]).abs() < 1e-12);
        assert!(wb.wb_gains.iter().all(|g| (0.75..=4.0 / 3.0).contains(g)));
        let off = residual_white_balance(&l, &WhiteBalanceConfig { residual: false, ..WhiteBalanceConfig::default() });
        assert_eq!(off.as_shot_neutral, [1.0; 3]);
    }

    #[test]
    fn highlight_cap_counts_and_holds_clipped_pixels() {
        let base = [0.8, 0.6, 0.4];
        // Half the pixels sit at the sensor floor in blue (blown highlight
        // on the negative), the rest are a normal ramp.
        let mut data = Vec::new();
        for i in 0..2000 {
            let t = 0.05 + 0.9 * (i as f64 / 2000.0);
            let blue = if i % 2 == 0 { 0.0 } else { base[2] * t };
            data.extend([(base[0] * t) as f32, (base[1] * t) as f32, blue as f32]);
        }
        let frame = RgbImage::new(2000, 1, data).unwrap();
        let calib = diagonal_calibration(base, [1.0; 3]);
        let scale = RollScale { g_ref: 0.6, k: k_from_e_mid(None, 0.6) };
        let out = negative_to_scene_linear(&frame, &calib, scale, [1.0; 3], &FilmConfig::default()).unwrap();
        assert!((out.meta.clipped_fraction - 0.5).abs() < 0.01, "{}", out.meta.clipped_fraction);
        let expected_cap = -(f64::from(SENSOR_FLOOR) / 0.4).log10();
        let dcap = out.meta.dcap.unwrap();
        assert!((dcap[2] - expected_cap).abs() < 1e-9);
        // All clipped pixels hold the same capped value (no invented detail).
        let capped: Vec<f32> = out.image.data().chunks_exact(3).step_by(2).map(|p| p[2]).collect();
        assert!(capped.windows(2).all(|w| w[0] == w[1]));
        let l_cap = excitation_to_linear(expected_cap, out.meta.g_ref.unwrap(), out.meta.k) as f32;
        assert!((capped[0] - l_cap).abs() <= l_cap * 1e-5);
    }

    #[test]
    fn slide_as_is_divides_by_the_light_and_is_display_referred() {
        let light = [0.9_f32, 0.8, 0.6];
        let data: Vec<f32> = (0..1000).flat_map(|i| light.map(|w| w * (i as f32 / 999.0))).collect();
        let frame = RgbImage::new(1000, 1, data).unwrap();
        let out = slide_as_is(&frame, Some(light.map(f64::from)), false).unwrap();
        assert_eq!(out.meta.referred, Referred::Display);
        let px = out.image.pixel(500, 0).unwrap();
        assert!((px[0] - px[1]).abs() < 1e-6 && (px[1] - px[2]).abs() < 1e-6, "backlight colour removed: {px:?}");
        assert!((px[1] - 500.0 / 999.0).abs() < 1e-5, "values pass through un-normalized");
        let auto = slide_as_is(&frame, None, true).unwrap();
        assert!((auto.image.pixel(999, 0).unwrap()[0] - 1.0).abs() < 0.01);
        let black = RgbImage::filled(4, 4, [0.0; 3]).unwrap();
        assert!(slide_as_is(&black, None, false).is_err());
    }

    #[test]
    fn bw_neutralizes_a_tinted_base() {
        // XP2 pink base: per-channel division makes R = G = B exactly.
        let base = [0.85, 0.62, 0.70];
        let data: Vec<f32> = (0..3000)
            .flat_map(|i| {
                let d = 0.05 + 1.5 * (i as f64 / 3000.0);
                base.map(|b| (b * 10f64.powf(-d)) as f32)
            })
            .collect();
        let frame = RgbImage::new(3000, 1, data).unwrap();
        let scale = RollScale { g_ref: 0.6, k: k_from_e_mid(None, 0.6) };
        let out = bw_negative_to_scene_linear(&frame, base, scale, &FilmConfig::default()).unwrap();
        for p in out.image.data().chunks_exact(3) {
            assert!(p[0] == p[1] && p[1] == p[2]);
        }
        assert_eq!(out.meta.as_shot_neutral, [1.0; 3]);
        // Monotone: denser negative -> brighter scene.
        let a = out.image.pixel(100, 0).unwrap()[0];
        let b = out.image.pixel(2000, 0).unwrap()[0];
        assert!(b > a);
    }
}
