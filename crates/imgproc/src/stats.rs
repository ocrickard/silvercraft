//! Robust statistics over finite values.

/// The `q`-th percentile (0..100, linear interpolation) of the finite values; `None` if there are
/// none or `q` is not finite.
pub fn percentile(values: &[f32], q: f64) -> Option<f32> {
    if !q.is_finite() {
        return None;
    }
    let mut v: Vec<f32> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable_by(f32::total_cmp);
    let pos = (q.clamp(0.0, 100.0) / 100.0) * (v.len() - 1) as f64;
    let i = pos.floor() as usize;
    let f = (pos - i as f64) as f32;
    let j = (i + 1).min(v.len() - 1);
    Some(v[i] * (1.0 - f) + v[j] * f)
}

/// Several percentiles from one sort.
pub fn percentiles(values: &[f32], qs: &[f64]) -> Option<Vec<f32>> {
    let mut v: Vec<f32> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable_by(f32::total_cmp);
    Some(
        qs.iter()
            .map(|&q| {
                let pos = (q.clamp(0.0, 100.0) / 100.0) * (v.len() - 1) as f64;
                let i = pos.floor() as usize;
                let f = (pos - i as f64) as f32;
                v[i] * (1.0 - f) + v[(i + 1).min(v.len() - 1)] * f
            })
            .collect(),
    )
}

/// Median and MAD-based sigma (1.4826·MAD) of the finite values.
pub fn median_sigma(values: &[f32]) -> Option<(f32, f32)> {
    let m = percentile(values, 50.0)?;
    let dev: Vec<f32> = values.iter().filter(|x| x.is_finite()).map(|x| (x - m).abs()).collect();
    let mad = percentile(&dev, 50.0)?;
    Some((m, 1.4826 * mad))
}

/// Every `step`-th value, for cheap statistics over large images.
pub fn subsample(values: &[f32], max_samples: usize) -> Vec<f32> {
    let step = (values.len() / max_samples.max(1)).max(1);
    values.iter().step_by(step).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolates_and_skips_nan() {
        let v = [3.0, f32::NAN, 1.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&v, 100.0), Some(4.0));
        assert_eq!(percentile(&v, 50.0), Some(2.5));
        assert_eq!(percentile(&[f32::NAN], 50.0), None);
        assert_eq!(percentiles(&v, &[0.0, 100.0]), Some(vec![1.0, 4.0]));
    }

    #[test]
    fn mad_sigma_of_normalish() {
        let v: Vec<f32> = (0..1001).map(|i| i as f32).collect();
        let (m, s) = median_sigma(&v).unwrap();
        assert_eq!(m, 500.0);
        assert!((s - 1.4826 * 250.0).abs() < 1.0);
    }
}
