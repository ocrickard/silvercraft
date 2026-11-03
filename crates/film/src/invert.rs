//! Per-frame negative inversion — the LIVE PREVIEW / pre-calibration
//! path (`estimate_inversion_params`: record rescue, shared green scale,
//! two-pass neutral gains) minus every display-referred stage (soft shoulder,
//! paper, sRGB). Export goes through [`crate::scene`] instead.
//!
//! The math is kept bit-for-bit stable for finite input.

use crate::base::{SENSOR_FLOOR, estimate_frame_base};
use crate::profiles::{FilmKind, FilmProfile};
use crate::stats::{channel_percentiles, luminance_f32, percentile_f32};

/// Record-rescue triggers (both required): channel density range under 40% of
/// green's AND median density under 0.10. Gain clamped to [1, 4].
pub const RESCUE_RANGE_RATIO: f32 = 0.4;
pub const RESCUE_P50_FLOOR: f32 = 0.10;
pub const RESCUE_MAX_GAIN: f32 = 4.0;

/// Gray-gain bounds.
pub const GRAY_GAIN_MIN: f32 = 0.75;
pub const GRAY_GAIN_MAX: f32 = 4.0 / 3.0;

/// Two-pass neutral selection + gain bounds. `Default` is the
/// Preview constants; [`NeutralParams::from_white_balance`] maps the
/// `[white_balance]` config section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NeutralParams {
    pub achro_ceiling: f32,
    pub bins: usize,
    pub per_bin_cap: usize,
    pub min_count: usize,
    pub luma_range: [f32; 2],
    pub gain_min: f32,
    pub gain_max: f32,
}

impl Default for NeutralParams {
    fn default() -> Self {
        Self {
            achro_ceiling: 0.12,
            bins: 6,
            per_bin_cap: 1200,
            min_count: 100,
            luma_range: [5.0, 97.0],
            gain_min: GRAY_GAIN_MIN,
            gain_max: GRAY_GAIN_MAX,
        }
    }
}

impl NeutralParams {
    pub fn from_white_balance(wb: &crate::config::WhiteBalanceConfig) -> Self {
        Self {
            achro_ceiling: wb.neutral_achro_ceiling as f32,
            bins: wb.neutral_bins,
            per_bin_cap: wb.neutral_per_bin_cap,
            min_count: wb.neutral_min_count,
            luma_range: wb.neutral_luma_range.map(|v| v as f32),
            gain_min: wb.gain_min as f32,
            gain_max: wb.gain_max as f32,
        }
    }
}

/// Inversion inputs: the inversion half of a stock profile.
#[derive(Debug, Clone, Copy)]
pub struct InversionProfile {
    pub kind: FilmKind,
    /// Per-channel MULTIPLIER on density. Built from a
    /// stock with [`InversionProfile::from_spec`] this is the corrected
    /// [`crate::profiles::FilmProfile::layer_gain`], not raw `density_scale`.
    pub density_scale: [f32; 3],
    pub dmin: Option<[f32; 3]>,
    /// Roll-level base hint: the per-frame p95 base is clamped to `hint_range`x of it.
    pub base_hint: Option<[f32; 3]>,
    /// Hint clamp ratio (`base.hint_range`, default 1.25).
    pub hint_range: f32,
    pub low_percentile: f32,
    pub high_percentile: f32,
    pub gray_percentile: f32,
    pub neutralize_gray: bool,
    pub epsilon: f32,
}

impl InversionProfile {
    /// Inversion profile for a stock, using the corrected layer gain.
    pub fn from_spec(spec: &FilmProfile) -> Self {
        let neg = spec.negative_params();
        Self {
            kind: spec.kind(),
            density_scale: spec.layer_gain().map(|v| v as f32),
            dmin: spec.dmin_f64().map(|d| d.map(|v| v as f32)),
            base_hint: None,
            hint_range: crate::base::BASE_HINT_RANGE,
            low_percentile: neg.low_percentile as f32,
            high_percentile: neg.high_percentile as f32,
            gray_percentile: neg.gray_percentile as f32,
            neutralize_gray: neg.neutralize,
            epsilon: 1e-6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InversionParams {
    pub film_base: [f32; 3],
    pub low: [f32; 3],
    /// Shared normalization scale (green anchor range) for all channels.
    pub scale: f32,
    /// Bounded per-channel record rescue (1.0 = none).
    pub rescue: [f32; 3],
    /// Gray-neutralization gains on normalized density (`None` when the
    /// profile does not neutralize, or for B&W).
    pub gray_scales: Option<[f32; 3]>,
}

/// Density above base of one transmittance sample, preview
/// convention: `-log10(max(max(T, floor) / max(b, eps), eps))`. Can be
/// negative for pixels brighter than the base.
#[inline]
pub fn density(transmittance: f32, base: f32, epsilon: f32) -> f32 {
    let t = (transmittance.max(SENSOR_FLOOR) / base.max(epsilon)).max(epsilon);
    -t.log10()
}

/// Density above base, export convention: T/b is
/// clamped to `[eps, 1]` so the result is in `[0, -log10 eps]`. NaN reads as
/// the sensor floor.
#[inline]
pub fn density_clamped(transmittance: f64, base: f64, epsilon: f64) -> f64 {
    let t = transmittance.max(f64::from(SENSOR_FLOOR));
    let ratio = (t / base).max(epsilon).min(1.0);
    if ratio.is_nan() { 0.0 } else { (-ratio.log10()).max(0.0) }
}

/// B&W density: mean over channels of `log10(b_c / max(T_c, floor))`.
/// Dividing by the per-channel base first neutralizes a tinted (XP2 pink) base.
#[inline]
pub fn bw_density(pixel: &[f32], film_base: [f32; 3]) -> f32 {
    match pixel {
        [r, g, b, ..] => {
            ((film_base[0] / r.max(SENSOR_FLOOR)).log10()
                + (film_base[1] / g.max(SENSOR_FLOOR)).log10()
                + (film_base[2] / b.max(SENSOR_FLOOR)).log10())
                / 3.0
        }
        _ => 0.0,
    }
}

/// Estimate the per-frame inversion statistics. Positive and digital kinds
/// return identity parameters (they are never inverted).
pub fn estimate_inversion_params(rgb: &[f32], profile: &InversionProfile) -> InversionParams {
    if profile.kind == FilmKind::ColorPositive {
        return InversionParams { film_base: [1.0; 3], low: [0.0; 3], scale: 1.0, rescue: [1.0; 3], gray_scales: None };
    }
    let film_base = estimate_frame_base(rgb, profile.dmin, profile.base_hint, profile.hint_range, profile.epsilon);
    if matches!(profile.kind, FilmKind::BlackAndWhiteNegative | FilmKind::BlackAndWhitePositive) {
        let mut density = Vec::with_capacity(rgb.len());
        for pixel in rgb.chunks_exact(3) {
            let value = bw_density(pixel, film_base);
            density.extend([value, value, value]);
        }
        let low = channel_percentiles(&density, profile.low_percentile);
        let high = channel_percentiles(&density, profile.high_percentile);
        let scale = (high[0] - low[0]).max(1e-12);
        return InversionParams { film_base, low, scale, rescue: [1.0; 3], gray_scales: None };
    }

    let mut density_values = Vec::with_capacity(rgb.len());
    for pixel in rgb.chunks_exact(3) {
        for channel in 0..3 {
            density_values.push(density(pixel[channel], film_base[channel], profile.epsilon) * profile.density_scale[channel]);
        }
    }
    let mut low = channel_percentiles(&density_values, profile.low_percentile);
    let mut rescue = [1.0_f32; 3];
    if profile.kind == FilmKind::ColorNegative {
        let rlow = channel_percentiles(&density_values, 2.0);
        let rhigh = channel_percentiles(&density_values, 98.0);
        let anchor_range = (rhigh[1] - rlow[1]).max(1e-12);
        let median = channel_percentiles(&density_values, 50.0);
        for channel in [0usize, 2usize] {
            let range = (rhigh[channel] - rlow[channel]).max(1e-12);
            if range < RESCUE_RANGE_RATIO * anchor_range && median[channel] < RESCUE_P50_FLOOR {
                rescue[channel] = (anchor_range / range).clamp(1.0, RESCUE_MAX_GAIN);
            }
        }
        if rescue != [1.0; 3] {
            for (index, value) in density_values.iter_mut().enumerate() {
                *value *= rescue[index % 3];
            }
            low = channel_percentiles(&density_values, profile.low_percentile);
        }
    }
    let scale = (channel_percentiles(&density_values, profile.high_percentile)[1] - low[1]).max(1e-12);
    let gray_scales = if profile.neutralize_gray {
        let normalized: Vec<f32> = density_values.iter().enumerate().map(|(index, value)| (value - low[index % 3]) / scale).collect();
        Some(neutral_balance_gains(&normalized, profile.gray_percentile))
    } else {
        None
    };
    InversionParams { film_base, low, scale, rescue, gray_scales }
}

/// Indices of the two-pass neutral subset: per luminance bin over
/// `[p5, p97]` (6 bins), pixels with log-value spread < 0.12, most achromatic
/// first, at most 1200 per bin. `values` is interleaved RGB.
pub fn select_neutral_indices(values: &[f32]) -> Vec<usize> {
    select_neutral_indices_with(values, &NeutralParams::default())
}

/// [`select_neutral_indices`] with explicit parameters.
pub fn select_neutral_indices_with(values: &[f32], params: &NeutralParams) -> Vec<usize> {
    let bins = params.bins.max(1);
    let luma: Vec<f32> = values.chunks_exact(3).map(luminance_f32).collect();
    let luma_lo = percentile_f32(&mut luma.clone(), params.luma_range[0]);
    let luma_hi = percentile_f32(&mut luma.clone(), params.luma_range[1]);
    let achro: Vec<f32> = values
        .chunks_exact(3)
        .map(|pixel| {
            let z: [f32; 3] = [pixel[0], pixel[1], pixel[2]].map(|value| value.max(1e-4).ln());
            let mean = (z[0] + z[1] + z[2]) / 3.0;
            (((z[0] - mean).powi(2) + (z[1] - mean).powi(2) + (z[2] - mean).powi(2)) / 3.0).sqrt()
        })
        .collect();
    let mut out = Vec::new();
    for bin in 0..bins {
        let edge_lo = luma_lo + (luma_hi - luma_lo) * bin as f32 / bins as f32;
        let edge_hi = luma_lo + (luma_hi - luma_lo) * (bin + 1) as f32 / bins as f32;
        let mut indices: Vec<usize> =
            (0..luma.len()).filter(|&index| luma[index] >= edge_lo && luma[index] <= edge_hi && achro[index] < params.achro_ceiling).collect();
        // the `per_bin_cap` most neutral, in a stable sort's order (ties by index), without
        // sorting the whole bin
        let order = |a: &usize, b: &usize| achro[*a].total_cmp(&achro[*b]).then(a.cmp(b));
        if indices.len() > params.per_bin_cap && params.per_bin_cap > 0 {
            indices.select_nth_unstable_by(params.per_bin_cap - 1, order);
        }
        indices.truncate(params.per_bin_cap);
        indices.sort_unstable_by(order);
        out.extend(indices);
    }
    out
}

/// Two-pass neutral gray gains:
/// pass 1 is the `gray_percentile` balance on the 0..1-clipped values, pass 2
/// re-references on the neutral-selected subset of the pass-1-corrected
/// values when more than 100 neutrals exist. Clamped to [0.75, 4/3].
///
/// Domain-agnostic: The classic form runs it on normalized density; SilverCraft's
/// export runs it on scene-linear `L` (where the gains are true WB).
pub fn neutral_balance_gains(values: &[f32], gray_percentile: f32) -> [f32; 3] {
    neutral_balance_gains_with(values, gray_percentile, &NeutralParams::default())
}

/// [`neutral_balance_gains`] with explicit selection and bounds.
pub fn neutral_balance_gains_with(values: &[f32], gray_percentile: f32, params: &NeutralParams) -> [f32; 3] {
    let clipped: Vec<f32> = values.iter().map(|value| value.clamp(0.0, 1.0)).collect();
    let reference = channel_percentiles(&clipped, gray_percentile);
    let target = (reference[0] + reference[1] + reference[2]) / 3.0;
    let pass1 = reference.map(|r| target / r.max(1e-12));
    let corrected: Vec<f32> = values.iter().enumerate().map(|(index, value)| value * pass1[index % 3]).collect();
    let indices = select_neutral_indices_with(&corrected, params);
    let gains = if indices.len() > params.min_count {
        let mut subset = [0.0_f32; 3];
        for &index in &indices {
            for channel in 0..3 {
                subset[channel] += corrected[index * 3 + channel];
            }
        }
        let reference = subset.map(|value| value / indices.len() as f32);
        let target = (reference[0] + reference[1] + reference[2]) / 3.0;
        [0, 1, 2].map(|c| pass1[c] * (target / reference[c].max(1e-12)))
    } else {
        pass1
    };
    gains.map(|gain| if gain.is_nan() { 1.0 } else { crate::stats::clamp_f32(gain, params.gain_min, params.gain_max) })
}

/// Normalized density positive `P`, WITHOUT the
/// display shoulder/paper. B&W writes the same value to all three channels;
/// positives pass through. For a live preview only.
pub fn apply_inversion_density(rgb: &[f32], profile: &InversionProfile, params: &InversionParams) -> Vec<f32> {
    match profile.kind {
        FilmKind::ColorPositive => rgb.to_vec(),
        FilmKind::BlackAndWhiteNegative | FilmKind::BlackAndWhitePositive => {
            let mut output = Vec::with_capacity(rgb.len());
            for pixel in rgb.chunks_exact(3) {
                let normalized = (bw_density(pixel, params.film_base) - params.low[0]) / params.scale;
                let value = if profile.kind == FilmKind::BlackAndWhitePositive { 1.0 - normalized } else { normalized };
                output.extend([value, value, value]);
            }
            output
        }
        FilmKind::ColorNegative => {
            let gray = params.gray_scales.unwrap_or([1.0; 3]);
            rgb.iter()
                .enumerate()
                .map(|(index, &value)| {
                    let c = index % 3;
                    let d = density(value, params.film_base[c], profile.epsilon) * profile.density_scale[c] * params.rescue[c];
                    (d - params.low[c]) / params.scale * gray[c]
                })
                .collect()
        }
    }
}

/// Scene-referred excitation of the preview path, `e_c = max(D_c * rescue_c, 0)`
/// ("To get a scene-linear preview from this path"). Feed it to
/// [`crate::scene::excitation_to_linear`].
pub fn preview_excitation(rgb: &[f32], profile: &InversionProfile, params: &InversionParams) -> Vec<f32> {
    rgb.iter()
        .enumerate()
        .map(|(index, &value)| {
            let c = index % 3;
            (density(value, params.film_base[c], profile.epsilon) * profile.density_scale[c] * params.rescue[c]).max(0.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(neutralize: bool, base: [f32; 3]) -> InversionProfile {
        InversionProfile {
            kind: FilmKind::ColorNegative,
            density_scale: [1.0; 3],
            dmin: Some(base),
            base_hint: None,
            hint_range: crate::base::BASE_HINT_RANGE,
            low_percentile: 0.5,
            high_percentile: 99.5,
            gray_percentile: 50.0,
            neutralize_gray: neutralize,
            epsilon: 1e-6,
        }
    }

    #[test]
    fn inversion_rescue_fires_on_compressed_record_and_recovers_balance() {
        let base = [0.60_f32, 0.34, 0.28];
        let mut frame = Vec::new();
        for i in 0..20000 {
            let t = i as f32 / 20000.0;
            let g = 0.05 + 0.85 * t;
            frame.extend([base[0] * (0.65 + 0.35 * t), base[1] * g, base[2] * g]);
        }
        let profile = profile(false, base);
        let params = estimate_inversion_params(&frame, &profile);
        assert!(params.rescue[0] > 1.5, "rescue must fire on R: {:?}", params.rescue);
        assert_eq!(params.rescue[1], 1.0);
        assert!(params.rescue[0] <= RESCUE_MAX_GAIN);
        let positive = apply_inversion_density(&frame, &profile, &params);
        let mid = &positive[3 * 10000..3 * 10000 + 3];
        let spread = (mid[0] - mid[1]).abs().max((mid[2] - mid[1]).abs());
        assert!(spread < 0.06, "rescued midtone balance: {mid:?}");
    }

    #[test]
    fn inversion_rescue_never_fires_on_healthy_or_one_color_scenes() {
        let base = [0.60_f32, 0.34, 0.28];
        let profile = profile(false, base);
        let healthy: Vec<f32> = (0..20000)
            .flat_map(|i| {
                let g = 0.05 + 0.85 * (i as f32 / 20000.0);
                [base[0] * g, base[1] * g, base[2] * g]
            })
            .collect();
        assert_eq!(estimate_inversion_params(&healthy, &profile).rescue, [1.0; 3]);
        let poppies: Vec<f32> = (0..20000)
            .flat_map(|i| {
                let t = 0.3 + 0.5 * (i as f32 / 20000.0);
                [base[0] * (0.05 + 0.9 * (i as f32 / 20000.0)), base[1] * t, base[2] * t]
            })
            .collect();
        assert_eq!(estimate_inversion_params(&poppies, &profile).rescue, [1.0; 3]);
    }

    #[test]
    fn shared_scale_keeps_hue_across_tones() {
        // A tinted ramp whose channel densities are in a fixed RATIO: with one
        // shared scale the normalized positive keeps that ratio at every tone
        // (per-channel scales would rotate hue with tonality).
        let base = [0.7_f32, 0.5, 0.3];
        let frame: Vec<f32> = (0..10000)
            .flat_map(|i| {
                let d = 0.1 + 1.4 * i as f32 / 10000.0;
                [base[0] * 10f32.powf(-d * 0.8), base[1] * 10f32.powf(-d), base[2] * 10f32.powf(-d * 1.1)]
            })
            .collect();
        let mut p = profile(false, base);
        p.low_percentile = 0.0;
        let params = estimate_inversion_params(&frame, &p);
        let e = preview_excitation(&frame, &p, &params);
        for i in [2000usize, 5000, 9000] {
            let px = &e[i * 3..i * 3 + 3];
            assert!((px[0] / px[1] - 0.8).abs() < 0.8 * 0.03, "{px:?}");
            assert!((px[2] / px[1] - 1.1).abs() < 1.1 * 0.03, "{px:?}");
        }
    }

    #[test]
    fn gray_balance_follows_neutrals_not_the_median() {
        // Port of test_color_science_v2 A3: ~80% dense orange + warm-cast true
        // neutrals; the neutral-subset balance targets the NEUTRALS.
        let (h, w) = (96usize, 96usize);
        let base = [0.85_f32, 0.72, 0.50];
        let neutral_t = [1.0_f32, 0.9, 0.8];
        let mut image = vec![0.0_f32; h * w * 3];
        for y in 0..h {
            for x in 0..w {
                let px = if y < 8 {
                    base
                } else if y >= h / 2 && x < w / 4 {
                    [0, 1, 2].map(|c| base[c] * neutral_t[c] * 0.5)
                } else if y >= h / 2 && x < w / 2 {
                    [0, 1, 2].map(|c| base[c] * neutral_t[c] * 0.25)
                } else {
                    [base[0] * 0.25, base[1] * 0.55 * 0.25, base[2] * 0.30 * 0.25]
                };
                image[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&px);
            }
        }
        let mut p = profile(true, base);
        p.dmin = None;
        let params = estimate_inversion_params(&image, &p);
        let positive = apply_inversion_density(&image, &p, &params);
        let mut means = [0.0_f32; 3];
        let mut n = 0.0;
        for y in h / 2..h {
            for x in 0..w / 2 {
                for c in 0..3 {
                    means[c] += positive[(y * w + x) * 3 + c];
                }
                n += 1.0;
            }
        }
        let means = means.map(|m| m / n);
        let mean = means.iter().sum::<f32>() / 3.0;
        let spread = (means.iter().copied().fold(f32::MIN, f32::max) - means.iter().copied().fold(f32::MAX, f32::min)) / mean;
        assert!(spread < 0.10, "neutrals must come out near-neutral, got {means:?}");
    }

    #[test]
    fn hostile_input_is_total() {
        let p = profile(true, [0.5; 3]);
        for rgb in [vec![], vec![0.0; 3], vec![f32::NAN; 30], vec![f32::INFINITY; 6]] {
            let params = estimate_inversion_params(&rgb, &p);
            let out = apply_inversion_density(&rgb, &p, &params);
            assert_eq!(out.len(), rgb.len());
            let gains = params.gray_scales.unwrap();
            assert!(gains.iter().all(|g| g.is_finite()));
        }
        assert_eq!(bw_density(&[0.5], [0.5; 3]), 0.0);
        assert!(density_clamped(f64::NAN, 0.5, 1e-6) > 0.0);
        assert_eq!(density_clamped(0.9, 0.5, 1e-6), 0.0);
        assert_eq!(density_clamped(0.5, 0.0, 1e-6), 0.0);
    }
}
