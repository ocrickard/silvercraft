//! Small helpers: orientation and robust statistics over finite values.

use rayon::prelude::*;
use silvercraft_imgproc::Plane;

use crate::Result;

/// The transpose of `p` (width and height swapped).
pub(crate) fn transpose(p: &Plane) -> Result<Plane> {
    let (w, h) = (p.width(), p.height());
    let src = p.data();
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        for (y, v) in col.iter_mut().enumerate() {
            *v = src[y * w + x];
        }
    });
    Ok(Plane::new(h, w, out)?)
}

/// Median of the finite values (reorders `buf`); `None` when there are none.
pub(crate) fn median_in_place(buf: &mut Vec<f32>) -> Option<f32> {
    buf.retain(|v| v.is_finite());
    if buf.is_empty() {
        return None;
    }
    let mid = buf.len() / 2;
    let (_, m, _) = buf.select_nth_unstable_by(mid, f32::total_cmp);
    let hi = *m;
    if buf.len() % 2 == 1 {
        return Some(hi);
    }
    // Even count: average with the largest value of the lower half.
    let lo = buf[..mid].iter().copied().fold(f32::NEG_INFINITY, f32::max);
    Some(0.5 * (lo + hi))
}

/// Median of the finite values of a slice.
pub(crate) fn median(values: &[f32]) -> Option<f32> {
    let mut v = values.to_vec();
    median_in_place(&mut v)
}

/// Median and MAD (not scaled) of the finite values.
pub(crate) fn median_mad(values: &[f32]) -> Option<(f32, f32)> {
    let m = median(values)?;
    let mut dev: Vec<f32> = values.iter().filter(|v| v.is_finite()).map(|v| (v - m).abs()).collect();
    let mad = median_in_place(&mut dev)?;
    Some((m, mad))
}

/// 1.4826: MAD to Gaussian sigma.
pub(crate) const MAD_SIGMA: f32 = 1.4826;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_skips_nan_and_averages_even() {
        assert_eq!(median(&[3.0, f32::NAN, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[f32::NAN, f32::INFINITY]), None);
        assert_eq!(median(&[]), None);
        assert_eq!(median_mad(&[1.0, 2.0, 3.0, 100.0, 2.0]), Some((2.0, 1.0)));
    }

    #[test]
    fn transpose_round_trips() {
        let p = Plane::from_fn(5, 3, |x, y| (x * 10 + y) as f32).unwrap();
        let t = transpose(&p).unwrap();
        assert_eq!((t.width(), t.height()), (3, 5));
        assert_eq!(t.get(2, 4), p.get(4, 2));
        assert_eq!(transpose(&t).unwrap(), p);
    }
}
