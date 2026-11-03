//! Per-roll layer-gamma measurement, plus the per-strip residual gain.
//!
//! Each dye layer has its own gamma (density per stop). The datasheet value
//! (folded into the profile's layer gain) is right only for the datasheet's
//! process; this measures the RESIDUAL per roll by regressing each channel's
//! density on green's, frame by frame, and takes the median over frames.
//! Never fit per frame: scene content biases single frames badly.

use std::collections::BTreeMap;

use crate::calibrate::{RollCalibrationDocument, apply_calibration, geomean_neutral_gain_with};
use crate::config::{CalibrationConfig, LayerGammaConfig};
use crate::stats::{median_f64, percentile_f64};

/// Regression band on green density (percentiles).
pub const BAND_LOW: f64 = 2.0;
pub const BAND_HIGH: f64 = 98.0;
/// Fewest samples worth regressing.
pub const MIN_SAMPLES: usize = 2000;
/// Plausible slope range; outside is a broken measurement.
pub const GAMMA_MIN: f64 = 0.4;
pub const GAMMA_MAX: f64 = 2.5;
/// Blue-slope IQR below which the median is trusted fully / above which not at all.
pub const SPREAD_SOFT: f64 = 0.10;
pub const SPREAD_HARD: f64 = 0.30;
/// Fewest usable frames for a roll estimate.
pub const MIN_FRAMES: usize = 3;

/// One frame's per-channel gamma relative to green (`[R, 1, B]`), or `None`
/// if unusable. `density` is per-pixel density AFTER the profile's layer gain,
/// so the result is the residual the datasheet did not account for.
pub fn frame_layer_gamma(density: &[[f64; 3]]) -> Option<[f64; 3]> {
    frame_layer_gamma_with(density, &LayerGammaConfig::default())
}

/// [`frame_layer_gamma`] with explicit `[layer_gamma]` knobs.
pub fn frame_layer_gamma_with(density: &[[f64; 3]], cfg: &LayerGammaConfig) -> Option<[f64; 3]> {
    let min_samples = cfg.min_samples.max(2);
    if density.len() < min_samples || !density.iter().all(|p| p.iter().all(|v| v.is_finite())) {
        return None;
    }
    let mut green: Vec<f64> = density.iter().map(|p| p[1]).collect();
    let lo = percentile_f64(&mut green, cfg.band_low);
    let hi = percentile_f64(&mut green, cfg.band_high);
    let kept: Vec<&[f64; 3]> = density.iter().filter(|p| p[1] >= lo && p[1] <= hi).collect();
    if kept.len() < min_samples {
        return None;
    }
    let n = kept.len() as f64;
    let g_mean = kept.iter().map(|p| p[1]).sum::<f64>() / n;
    let g_var = kept.iter().map(|p| (p[1] - g_mean).powi(2)).sum::<f64>() / n;
    if g_var.sqrt() < 1e-6 {
        return None;
    }
    let mut out = [1.0; 3];
    for channel in [0usize, 2] {
        let c_mean = kept.iter().map(|p| p[channel]).sum::<f64>() / n;
        let cov = kept.iter().map(|p| (p[1] - g_mean) * (p[channel] - c_mean)).sum::<f64>() / n;
        let slope = cov / g_var;
        if !slope.is_finite() || !(cfg.slope_range[0]..=cfg.slope_range[1]).contains(&slope) {
            return None;
        }
        out[channel] = slope;
    }
    Some(out)
}

/// Roll layer gamma with its trust.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LayerGammaDetail {
    /// Confidence-shrunk median gamma (G = 1), `None` when nothing usable.
    pub gamma: Option<[f64; 3]>,
    /// IQR of the per-frame blue slopes (`None` with < 3 usable frames).
    pub spread: Option<f64>,
    pub frames_used: usize,
    pub confidence: f64,
}

/// Median over frames (>= 3 usable), normalized to G = 1, shrunk toward 1 by
/// the blue-slope IQR confidence.
pub fn roll_layer_gamma_detail(frame_densities: &[Vec<[f64; 3]>]) -> LayerGammaDetail {
    roll_layer_gamma_detail_with(frame_densities, &LayerGammaConfig::default())
}

/// [`roll_layer_gamma_detail`] with explicit `[layer_gamma]` knobs.
pub fn roll_layer_gamma_detail_with(frame_densities: &[Vec<[f64; 3]>], cfg: &LayerGammaConfig) -> LayerGammaDetail {
    let fits: Vec<[f64; 3]> = frame_densities.iter().filter_map(|d| frame_layer_gamma_with(d, cfg)).collect();
    if fits.len() < cfg.min_frames.max(1) {
        return LayerGammaDetail { gamma: None, spread: None, frames_used: fits.len(), confidence: 0.0 };
    }
    let mut median = [0.0; 3];
    for (c, slot) in median.iter_mut().enumerate() {
        let mut values: Vec<f64> = fits.iter().map(|f| f[c]).collect();
        *slot = median_f64(&mut values);
    }
    let green = median[1];
    let median = median.map(|v| v / green);
    let mut blue: Vec<f64> = fits.iter().map(|f| f[2]).collect();
    let spread = percentile_f64(&mut blue.clone(), 75.0) - percentile_f64(&mut blue, 25.0);
    if !median.iter().all(|v| (cfg.slope_range[0]..=cfg.slope_range[1]).contains(v)) {
        return LayerGammaDetail { gamma: None, spread: Some(spread), frames_used: fits.len(), confidence: 0.0 };
    }
    let (soft, hard) = (cfg.confidence_spread[0], cfg.confidence_spread[1]);
    let confidence = if spread <= soft {
        1.0
    } else if spread >= hard {
        0.0
    } else {
        (hard - spread) / (hard - soft)
    };
    let shrunk = median.map(|v| 1.0 + (v - 1.0) * confidence);
    LayerGammaDetail { gamma: Some(shrunk), spread: Some(spread), frames_used: fits.len(), confidence }
}

/// Fold a measured layer gamma into a density MULTIPLIER: `scale / gamma`.
/// Invalid gammas leave the
/// scale unchanged.
pub fn corrected_density_scale(scale: [f64; 3], layer_gamma: Option<[f64; 3]>) -> [f64; 3] {
    match layer_gamma {
        Some(g) if g.iter().all(|v| v.is_finite() && *v > 0.0) => [scale[0] / g[0], scale[1] / g[1], scale[2] / g[2]],
        _ => scale,
    }
}

/// `strip-(\d+)` key of a frame label (roll_pipeline `_strip_key`); labels
/// without one share the "" strip.
pub fn strip_key(label: &str) -> String {
    strip_key_with(label, "strip-")
}

/// Strip key with a configurable prefix (`strip_balance.label_prefix`).
pub fn strip_key_with(label: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return String::new();
    }
    let Some(start) = label.find(prefix) else { return String::new() };
    let digits: String = label[start + prefix.len()..].chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() { String::new() } else { format!("{prefix}{digits}") }
}

/// Per-strip residual gain:
/// geomean-normalized neutral gain on the curve-corrected densities of every
/// frame of a strip, applied as a multiplier on excitation. Corrects scanner
/// bed spatial non-uniformity. `frames` are subsampled interleaved RGB.
pub fn strip_residual_gains(frames: &[(&str, &[f32])], calibration: &RollCalibrationDocument) -> BTreeMap<String, [f64; 3]> {
    strip_residual_gains_with(frames, calibration, "strip-", &CalibrationConfig::default())
}

/// [`strip_residual_gains`] with the label prefix and fallback-gain knobs.
pub fn strip_residual_gains_with(
    frames: &[(&str, &[f32])],
    calibration: &RollCalibrationDocument,
    prefix: &str,
    cfg: &CalibrationConfig,
) -> BTreeMap<String, [f64; 3]> {
    let mut by_strip: BTreeMap<String, Vec<[f64; 3]>> = BTreeMap::new();
    for (label, rgb) in frames {
        let entry = by_strip.entry(strip_key_with(label, prefix)).or_default();
        for pixel in rgb.chunks_exact(3) {
            entry.push(apply_calibration(pixel_f64(pixel), calibration, 1e-6, [f64::INFINITY; 3]));
        }
    }
    by_strip.into_iter().map(|(key, dens)| (key, geomean_neutral_gain_with(&dens, cfg))).collect()
}

fn pixel_f64(pixel: &[f32]) -> [f64; 3] {
    [f64::from(pixel[0]), f64::from(pixel[1]), f64::from(pixel[2])]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    fn frame(rng: &mut Rng, gamma: [f64; 3], n: usize) -> Vec<[f64; 3]> {
        (0..n)
            .map(|_| {
                let e = 0.1 + 1.5 * rng.next();
                let noise = |r: &mut Rng| 0.01 * (r.next() - 0.5);
                [gamma[0] * e + noise(rng), gamma[1] * e + noise(rng), gamma[2] * e + noise(rng)]
            })
            .collect()
    }

    #[test]
    fn recovers_known_layer_gammas_with_green_exactly_one() {
        let mut rng = Rng(42);
        let frames: Vec<_> = (0..4).map(|_| frame(&mut rng, [0.60, 1.0, 1.13], 5000)).collect();
        let detail = roll_layer_gamma_detail(&frames);
        let g = detail.gamma.unwrap();
        assert_eq!(g[1], 1.0);
        assert!((g[0] - 0.60).abs() < 0.01 && (g[2] - 1.13).abs() < 0.01, "{g:?}");
        assert_eq!(detail.confidence, 1.0);
        assert_eq!(detail.frames_used, 4);
    }

    #[test]
    fn refuses_flat_few_and_implausible_frames() {
        let mut rng = Rng(7);
        assert!(frame_layer_gamma(&frame(&mut rng, [1.0; 3], 100)).is_none(), "few samples");
        assert!(frame_layer_gamma(&vec![[0.5; 3]; 5000]).is_none(), "flat");
        assert!(frame_layer_gamma(&frame(&mut rng, [3.0, 1.0, 1.0], 5000)).is_none(), "implausible");
        let mut nan = frame(&mut rng, [1.0; 3], 5000);
        nan[3][0] = f64::NAN;
        assert!(frame_layer_gamma(&nan).is_none());
        let two: Vec<_> = (0..2).map(|_| frame(&mut rng, [0.9, 1.0, 1.1], 5000)).collect();
        let detail = roll_layer_gamma_detail(&two);
        assert!(detail.gamma.is_none() && detail.spread.is_none() && detail.confidence == 0.0);
    }

    #[test]
    fn median_survives_an_outlier_frame() {
        let mut rng = Rng(9);
        let mut frames: Vec<_> = (0..4).map(|_| frame(&mut rng, [1.0, 1.0, 1.13], 5000)).collect();
        frames.push(frame(&mut rng, [1.0, 1.0, 0.869], 5000));
        let g = roll_layer_gamma_detail(&frames).gamma.unwrap();
        assert!((g[2] - 1.13).abs() < 0.01, "{g:?}");
    }

    #[test]
    fn confidence_shrinks_with_disagreement() {
        let mut rng = Rng(11);
        let frames: Vec<_> = [0.9, 1.1, 1.3, 1.5].iter().map(|b| frame(&mut rng, [1.0, 1.0, *b], 5000)).collect();
        let detail = roll_layer_gamma_detail(&frames);
        // IQR of (0.9, 1.1, 1.3, 1.5) = 0.3 -> confidence 0 -> no correction.
        assert!(detail.spread.unwrap() >= 0.29, "{detail:?}");
        let g = detail.gamma.unwrap();
        assert!((g[2] - 1.0).abs() < 0.05, "{g:?}");
        let frames: Vec<_> = [1.1, 1.15, 1.2, 1.25].iter().map(|b| frame(&mut rng, [1.0, 1.0, *b], 5000)).collect();
        let detail = roll_layer_gamma_detail(&frames);
        assert!(detail.confidence > 0.0 && detail.confidence <= 1.0);
    }

    #[test]
    fn corrected_scale_divides_by_gamma() {
        assert_eq!(corrected_density_scale([1.0, 1.0, 1.2], Some([0.5, 1.0, 1.2])), [2.0, 1.0, 1.0]);
        assert_eq!(corrected_density_scale([1.0, 1.0, 1.2], None), [1.0, 1.0, 1.2]);
        assert_eq!(corrected_density_scale([1.0; 3], Some([0.0, 1.0, 1.0])), [1.0; 3]);
    }

    #[test]
    fn strip_keys_and_gains() {
        assert_eq!(strip_key("roll1-strip-03-frame2"), "strip-03");
        assert_eq!(strip_key("frame7"), "");
        assert_eq!(strip_key("strip-x"), "");
        // Two strips: the second scanned 5% dimmer in blue (bed falloff).
        let base = [0.8, 0.6, 0.4];
        let calib = crate::calibrate::diagonal_calibration(base, [1.0; 3]);
        let mk = |blue: f64| -> Vec<f32> {
            (0..4000)
                .flat_map(|i| {
                    let d = 0.1 + 1.2 * (i as f64 / 4000.0);
                    let t = 10f64.powf(-d);
                    [(base[0] * t) as f32, (base[1] * t) as f32, (base[2] * t * blue) as f32]
                })
                .collect()
        };
        let (a, b) = (mk(1.0), mk(0.95));
        let gains = strip_residual_gains(&[("s-strip-1-f1", &a), ("s-strip-2-f1", &b)], &calib);
        let g1 = gains["strip-1"];
        let g2 = gains["strip-2"];
        assert!((g1[2] - 1.0).abs() < 1e-3, "{g1:?}");
        assert!(g2[2] < g1[2], "denser blue must get a smaller gain: {g2:?}");
    }
}
