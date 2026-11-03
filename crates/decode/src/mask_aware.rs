//! Orange-mask-aware camera → linear sRGB conversion for camera scans of negatives.
//!
//! A camera → sRGB matrix has large negative off-diagonals. On an orange negative at unity white
//! balance, red is several times blue at the sensor, so the matrix drives blue negative and a plain
//! conversion clips it: on the original R-041 that meant 24 % of blue pixels at zero and a wrecked
//! blue density. Instead each pixel is divided by the per-channel mask level (p95), converted, and
//! the result compressed about the neutral axis into the measured channel envelope before the
//! mask is put back.

use rayon::prelude::*;

/// Linear sRGB (D65) from CIE XYZ (D65), IEC 61966-2-1.
const XYZ_TO_SRGB: [[f64; 3]; 3] = [[3.2404542, -1.5371385, -0.4985314], [-0.9692660, 1.8760108, 0.0415560], [0.0556434, -0.2040259, 1.0572252]];

fn mul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

fn inverse(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let inv = [
        [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det],
        [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
        [(d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det],
    ];
    inv.iter().flatten().all(|v| v.is_finite()).then_some(inv)
}

/// Camera → linear sRGB with rows normalised so camera neutral stays neutral; `None` when the
/// matrix is singular or not finite.
pub fn camera_to_srgb(xyz_to_camera: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let cam_to_xyz = inverse(xyz_to_camera)?;
    let mut m = mul(&XYZ_TO_SRGB, &cam_to_xyz);
    for row in &mut m {
        let s: f64 = row.iter().sum();
        if !s.is_finite() || s.abs() < 1e-9 {
            return None;
        }
        row.iter_mut().for_each(|v| *v /= s);
    }
    Some(m)
}

/// Per-channel percentile (p95 by default, the film-base statistic) over a stride-7 subsample
/// (all pixels below 4096). Non-finite samples are skipped and a non-finite `percentile`
/// means the p95 default: hostile input yields a finite, positive scale, never a panic
/// (`f32::clamp` panics on NaN *bounds*, and a NaN percentile rank would select one).
pub fn mask_scale(rgb: &[f32], percentile: f64) -> [f32; 3] {
    let percentile = if percentile.is_finite() { percentile.clamp(0.0, 100.0) } else { 95.0 };
    let q = (percentile / 100.0) as f32;
    let px = rgb.len() / 3;
    let stride = if px < 4096 { 1 } else { 7 };
    let mut ch: [Vec<f32>; 3] = Default::default();
    for p in rgb.chunks_exact(3).step_by(stride) {
        for c in 0..3 {
            if p[c].is_finite() {
                ch[c].push(p[c]);
            }
        }
    }
    ch.map(|mut v| {
        if v.is_empty() {
            return 1.0;
        }
        v.sort_unstable_by(f32::total_cmp);
        let pos = q * (v.len() - 1) as f32;
        let lo = pos.floor() as usize;
        let hi = (lo + 1).min(v.len() - 1);
        let s = v[lo] + (v[hi] - v[lo]) * (pos - lo as f32);
        if !s.is_finite() || s < 1e-9 { 1.0 } else { s }
    })
}

/// One pixel through the mask-aware conversion (`n` = camera RGB divided by the mask scale).
/// Non-finite inputs are treated as 0 (unmeasured): an all-NaN pixel previously reached
/// `clamp(NaN, NaN)`, which panics.
pub fn convert_pixel(n: [f64; 3], m: &[[f64; 3]; 3]) -> [f64; 3] {
    let n = n.map(|v| if v.is_finite() { v } else { 0.0 });
    let mapped = [0, 1, 2].map(|r| m[r][0] * n[0] + m[r][1] * n[1] + m[r][2] * n[2]);
    let lo = n[0].min(n[1]).min(n[2]);
    let hi = n[0].max(n[1]).max(n[2]);
    let ctr = ((n[0] + n[1] + n[2]) / 3.0).clamp(lo, hi);
    let delta = mapped.map(|v| v - ctr);
    let mut f = 1.0f64;
    for d in delta {
        let down = (ctr - lo) / (-d).max(1e-12);
        let up = (hi - ctr) / d.max(1e-12);
        f = f.min(down.min(up));
    }
    let f = if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 };
    delta.map(|d| {
        let v = ctr + d * f;
        if v.is_finite() { v } else { ctr }
    })
}

/// Convert interleaved camera RGB in place to linear sRGB primaries through the mask.
/// Output is finite for any input (non-finite samples read as unmeasured black).
pub fn convert(rgb: &mut [f32], camera_to_srgb: &[[f64; 3]; 3], mask_percentile: f64) {
    let s = mask_scale(rgb, mask_percentile);
    let peak = rgb.iter().copied().filter(|v| v.is_finite()).fold(0.0f32, f32::max);
    rgb.par_chunks_exact_mut(3).for_each(|p| {
        let n = [0, 1, 2].map(|c| p[c] as f64 / s[c] as f64);
        let out = convert_pixel(n, camera_to_srgb);
        for c in 0..3 {
            let v = (out[c] * s[c] as f64) as f32;
            p[c] = if v.is_finite() { v.clamp(0.0, peak) } else { 0.0 };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera whose XYZ→camera matrix is the sRGB one, perturbed: rows sum to 1 after
    /// normalisation and the conversion has the negative off-diagonals real cameras have.
    fn camera() -> [[f64; 3]; 3] {
        let srgb_to_xyz = inverse(&XYZ_TO_SRGB).unwrap();
        let skew = [[0.75, 0.30, -0.05], [0.10, 0.85, 0.05], [0.02, 0.18, 0.80]];
        inverse(&mul(&srgb_to_xyz, &skew)).unwrap()
    }

    #[test]
    fn rows_are_normalised_and_singular_matrices_refused() {
        let m = camera_to_srgb(&camera()).unwrap();
        for row in m {
            assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        }
        assert!(m.iter().flatten().any(|v| *v < -0.05), "{m:?}");
        assert!(camera_to_srgb(&[[0.0; 3]; 3]).is_none());
        assert!(camera_to_srgb(&[[f64::NAN; 3]; 3]).is_none());
    }

    #[test]
    fn orange_negative_keeps_its_blue_record() {
        // camera → sRGB with the strong crosstalk of a real body (the A1's green row is about
        // -0.63 R + 2.51 G - 0.88 B); rows sum to 1
        let m = [[1.90, -0.70, -0.20], [-0.63, 2.51, -0.88], [0.05, -0.85, 1.80]];
        // orange base: R ≈ 5.6 × B at the sensor, with image content modulating each record
        let mut rgb = Vec::new();
        for i in 0..5000 {
            let t = (i % 100) as f32 / 100.0;
            rgb.extend([0.85 * (0.3 + 0.7 * t), 0.45 * (0.2 + 0.8 * (1.0 - t)), 0.15 * (0.25 + 0.75 * t * t)]);
        }
        let mut plain = rgb.clone();
        for p in plain.chunks_exact_mut(3) {
            let o = [0, 1, 2].map(|r| m[r][0] * p[0] as f64 + m[r][1] * p[1] as f64 + m[r][2] * p[2] as f64);
            for c in 0..3 {
                p[c] = o[c].max(0.0) as f32;
            }
        }
        let zero_plain = plain.chunks_exact(3).filter(|p| p[2] <= 0.0).count();
        convert(&mut rgb, &m, 95.0);
        let zero_aware = rgb.chunks_exact(3).filter(|p| p[2] <= 0.0).count();
        assert!(zero_plain > 500, "the test negative should clip a plain matrix ({zero_plain})");
        assert_eq!(zero_aware, 0);
        assert!(rgb.iter().all(|v| v.is_finite() && *v >= 0.0));
    }

    #[test]
    fn non_finite_input_never_panics_and_stays_finite() {
        let m = camera_to_srgb(&camera()).unwrap();
        // The reproduced crash: an all-NaN pixel reached clamp(NaN, NaN).
        assert_eq!(convert_pixel([f64::NAN; 3], &m), [0.0; 3]);
        assert!(convert_pixel([f64::INFINITY, 0.5, f64::NEG_INFINITY], &m).iter().all(|v| v.is_finite()));
        assert_eq!(mask_scale(&[f32::NAN; 9], f64::NAN), [1.0; 3]);
        assert!(mask_scale(&[0.5, f32::INFINITY, 0.25], f64::INFINITY).iter().all(|v| v.is_finite() && *v > 0.0));
        let mut rgb = vec![f32::NAN, f32::INFINITY, 0.2, 0.8, f32::NEG_INFINITY, 0.1];
        convert(&mut rgb, &m, f64::NAN);
        assert!(rgb.iter().all(|v| v.is_finite() && *v >= 0.0), "{rgb:?}");
        convert(&mut [], &m, 95.0);
        let mut tail = vec![0.5, 0.4, 0.3, 0.9]; // not a multiple of 3: tail untouched, no panic
        convert(&mut tail, &m, 95.0);
        assert_eq!(tail[3], 0.9);
    }

    #[test]
    fn neutral_and_mask_coloured_pixels_are_fixed_points() {
        let m = camera_to_srgb(&camera()).unwrap();
        // a pixel exactly at the mask scale is neutral after division: unchanged
        let out = convert_pixel([1.0, 1.0, 1.0], &m);
        for v in out {
            assert!((v - 1.0).abs() < 1e-9);
        }
        // the conversion never leaves the channel envelope
        let out = convert_pixel([0.9, 0.2, 0.05], &m);
        assert!(out.iter().all(|v| (0.05 - 1e-9..=0.9 + 1e-9).contains(v)), "{out:?}");
    }
}
