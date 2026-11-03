//! Small numpy-compatible statistics shared by every stage.
//!
//! Percentiles use numpy's default (linear interpolation between order
//! statistics) so thresholds land on stable, well-understood numbers.
//! Every helper is total: empty input yields 0, NaN sorts last
//! (`total_cmp`) and never panics.

/// Rec.709 luma weights (linear light).
pub const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];
pub(crate) const LUMA_F32: [f32; 3] = [0.2126, 0.7152, 0.0722];

#[inline]
pub(crate) fn luminance_f32(rgb: &[f32]) -> f32 {
    match rgb {
        [r, g, b, ..] => LUMA_F32[0] * r + LUMA_F32[1] * g + LUMA_F32[2] * b,
        _ => 0.0,
    }
}

/// `clamp` that never panics: `max(lo)` then `min(hi)` (std `clamp` panics on
/// NaN or inverted bounds, which hostile parameters can produce).
#[inline]
pub(crate) fn clamp_f64(value: f64, lo: f64, hi: f64) -> f64 {
    value.max(lo).min(hi)
}

#[inline]
pub(crate) fn clamp_f32(value: f32, lo: f32, hi: f32) -> f32 {
    value.max(lo).min(hi)
}

/// The `k`th and `k + 1`th smallest of `values` under `cmp` (the `k`th twice when it is the
/// last), as a full sort would place them, without sorting: a selection, then the least of what
/// lies above it. Reorders `values`.
fn order_pair<T: Copy>(values: &mut [T], k: usize, cmp: fn(&T, &T) -> std::cmp::Ordering) -> (T, T) {
    let (_, kth, above) = values.select_nth_unstable_by(k, cmp);
    let kth = *kth;
    let next = above.iter().copied().min_by(cmp).unwrap_or(kth);
    (kth, next)
}

/// numpy-default (linear) percentile in f32, reordering `values` in place.
pub fn percentile_f32(values: &mut [f32], percentile: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    if values.len() == 1 {
        return values[0];
    }
    let position = (percentile.clamp(0.0, 100.0) / 100.0) * (values.len() - 1) as f32;
    let low = (position.floor() as usize).min(values.len() - 1);
    let fraction = position - low as f32;
    let (lo, high) = order_pair(values, low, f32::total_cmp);
    lo + (high - lo) * fraction
}

/// numpy-default (linear) percentile in f64, reordering `values` in place.
pub fn percentile_f64(values: &mut [f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    if values.len() == 1 {
        return values[0];
    }
    let position = (percentile.clamp(0.0, 100.0) / 100.0) * (values.len() - 1) as f64;
    let low = (position.floor() as usize).min(values.len() - 1);
    let fraction = position - low as f64;
    let (lo, high) = order_pair(values, low, f64::total_cmp);
    lo + (high - lo) * fraction
}

/// numpy `median`: average of the two middle order statistics for even n.
/// Empty input returns 0.
pub fn median_f64(values: &mut [f64]) -> f64 {
    let n = values.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        *values.select_nth_unstable_by(n / 2, f64::total_cmp).1
    } else {
        let (a, b) = order_pair(values, n / 2 - 1, f64::total_cmp);
        (a + b) / 2.0
    }
}

/// Per-channel percentile of an interleaved RGB buffer (f32, reference parity).
pub fn channel_percentiles(rgb: &[f32], percentile_value: f32) -> [f32; 3] {
    let pixels = rgb.len() / 3;
    let mut channels = [Vec::with_capacity(pixels), Vec::with_capacity(pixels), Vec::with_capacity(pixels)];
    for pixel in rgb.chunks_exact(3) {
        for channel in 0..3 {
            channels[channel].push(pixel[channel]);
        }
    }
    channels.map(|mut values| percentile_f32(&mut values, percentile_value))
}

/// Per-channel percentile over FINITE values only (robust variant used where
/// the input is untrusted capture data rather than a parity path).
pub fn channel_percentiles_finite(rgb: &[f32], percentile_value: f32) -> [Option<f32>; 3] {
    let pixels = rgb.len() / 3;
    let mut channels = [Vec::with_capacity(pixels), Vec::with_capacity(pixels), Vec::with_capacity(pixels)];
    for pixel in rgb.chunks_exact(3) {
        for channel in 0..3 {
            if pixel[channel].is_finite() {
                channels[channel].push(pixel[channel]);
            }
        }
    }
    channels.map(|mut values| if values.is_empty() { None } else { Some(percentile_f32(&mut values, percentile_value)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_gives_what_a_full_sort_gives() {
        fn sorted_percentile(v: &[f64], p: f64) -> f64 {
            let mut v = v.to_vec();
            v.sort_unstable_by(f64::total_cmp);
            if v.len() == 1 {
                return v[0];
            }
            let pos = (p / 100.0) * (v.len() - 1) as f64;
            let lo = (pos.floor() as usize).min(v.len() - 1);
            let hi = (lo + 1).min(v.len() - 1);
            v[lo] + (v[hi] - v[lo]) * (pos - lo as f64)
        }
        // duplicates, signed zeros, infinities, NaN; odd and even lengths
        let mut seed = 7u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / f64::from(1u32 << 31)
        };
        for n in [1usize, 2, 3, 10, 11, 257, 1000] {
            let v: Vec<f64> = (0..n)
                .map(|i| match i % 17 {
                    0 => 0.0,
                    1 => -0.0,
                    2 => 0.5,
                    3 if n > 100 => f64::INFINITY,
                    4 if n > 100 => f64::NAN,
                    _ => (next() * 8.0).floor() / 4.0 - 1.0,
                })
                .collect();
            for p in [0.0, 5.0, 25.0, 50.0, 73.3, 97.0, 99.9, 100.0] {
                let want = sorted_percentile(&v, p);
                let got = percentile_f64(&mut v.clone(), p);
                assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "n {n} p {p}: {got} vs {want}");
                let w32: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                let mut s32 = w32.clone();
                s32.sort_unstable_by(f32::total_cmp);
                let got32 = percentile_f32(&mut w32.clone(), p as f32);
                let mut again = s32.clone();
                assert!(got32.to_bits() == percentile_f32(&mut again, p as f32).to_bits() || got32.is_nan());
            }
            let mut s = v.clone();
            s.sort_unstable_by(f64::total_cmp);
            let want = if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 };
            let got = median_f64(&mut v.clone());
            assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "median n {n}: {got} vs {want}");
        }
    }

    #[test]
    fn percentile_matches_numpy_linear() {
        let mut v = [4.0_f64, 1.0, 3.0, 2.0];
        assert!((percentile_f64(&mut v, 50.0) - 2.5).abs() < 1e-12);
        assert!((percentile_f64(&mut v, 80.0) - 3.4).abs() < 1e-12);
        let mut w = [4.0_f32, 1.0, 3.0, 2.0];
        assert!((percentile_f32(&mut w, 25.0) - 1.75).abs() < 1e-6);
    }

    #[test]
    fn helpers_are_total_on_empty_and_nan() {
        assert_eq!(percentile_f64(&mut [], 50.0), 0.0);
        assert_eq!(percentile_f32(&mut [], 50.0), 0.0);
        assert_eq!(median_f64(&mut []), 0.0);
        let mut nan = [f32::NAN, 1.0, 2.0];
        let _ = percentile_f32(&mut nan, 100.0);
        assert_eq!(channel_percentiles_finite(&[f32::NAN; 3], 50.0), [None; 3]);
        assert_eq!(channel_percentiles(&[], 50.0), [0.0; 3]);
        assert!((median_f64(&mut [3.0, 1.0, 2.0, 10.0]) - 2.5).abs() < 1e-12);
    }
}
