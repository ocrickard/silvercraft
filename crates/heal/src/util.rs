//! Small shared helpers: masked large-scale blur, robust statistics, deterministic noise.

use rayon::prelude::*;
use silvercraft_imgproc::{Mask, Plane, filter, resample};

/// Weighted local-linear Gaussian regression (normalised convolution of order 1): at every
/// pixel, the value at that pixel of the plane `a + b·x + c·y` fitted to `values` with weights
/// `weight · G_sigma`. Unlike a plain weighted blur it does not bend toward the inside at image
/// edges, holder edges or around large excluded defects on a sloped background (lamp falloff).
/// Evaluated on a block-averaged grid and upsampled bilinearly, so the cost does not grow with
/// `sigma`. Non-finite values carry no weight; where no weight reaches, the weighted global mean
/// is used, and where the fit is ill-conditioned the plain weighted mean.
pub fn masked_blur(values: &Plane, weight: &Plane, sigma: f32) -> Plane {
    let (w, h) = (values.width(), values.height());
    let sigma = if sigma.is_finite() { sigma.max(0.5) } else { 0.5 };
    let factor = ((sigma / 3.0).floor() as usize).clamp(1, w.max(h));
    let ww = values.zip_map(weight, |v, k| if v.is_finite() && k.is_finite() && k > 0.0 { k } else { 0.0 });
    let Ok(ww) = ww else {
        return values.clone();
    };
    let vw = values.zip_map(&ww, |v, k| if k > 0.0 { v * k } else { 0.0 }).unwrap_or_else(|_| ww.clone());
    let (total_vw, total_w) = vw.data().iter().zip(ww.data()).fold((0.0f64, 0.0f64), |(a, b), (&x, &y)| (a + f64::from(x), b + f64::from(y)));
    let fallback = if total_w > 0.0 { (total_vw / total_w) as f32 } else { 0.0 };
    let (Ok(dv), Ok(dw)) = (resample::downsample(&vw, factor), resample::downsample(&ww, factor)) else {
        return Plane::filled(w, h, fallback).unwrap_or_else(|_| values.clone());
    };
    let (cw, ch) = (dw.width(), dw.height());
    // coarse coordinates normalised to about [-1, 1]
    let half = (cw.max(ch) as f32 / 2.0).max(1.0);
    let nx = |x: usize| (x as f32 - cw as f32 / 2.0) / half;
    let ny = |y: usize| (y as f32 - ch as f32 / 2.0) / half;
    let prod = |f: &(dyn Fn(usize, usize) -> f32 + Sync), base: &Plane| {
        Plane::from_fn(cw, ch, |x, y| base.get(x, y) * f(x, y)).unwrap_or_else(|_| base.clone())
    };
    let s = sigma / factor as f32;
    let g = |p: &Plane| filter::gaussian(p, s);
    let g_w = g(&dw);
    let g_wx = g(&prod(&|x, _| nx(x), &dw));
    let g_wy = g(&prod(&|_, y| ny(y), &dw));
    let g_wxx = g(&prod(&|x, _| nx(x) * nx(x), &dw));
    let g_wxy = g(&prod(&|x, y| nx(x) * ny(y), &dw));
    let g_wyy = g(&prod(&|_, y| ny(y) * ny(y), &dw));
    let g_v = g(&dv);
    let g_vx = g(&prod(&|x, _| nx(x), &dv));
    let g_vy = g(&prod(&|_, y| ny(y), &dv));
    let coarse = Plane::from_fn(cw, ch, |x, y| {
        let i = y * cw + x;
        let sw = f64::from(g_w.data()[i]);
        if sw <= 1e-6 {
            return fallback;
        }
        let mean = (f64::from(g_v.data()[i]) / sw) as f32;
        // centred moments at this pixel
        let (x0, y0) = (f64::from(nx(x)), f64::from(ny(y)));
        let (sx, sy) = (f64::from(g_wx.data()[i]) - x0 * sw, f64::from(g_wy.data()[i]) - y0 * sw);
        let sxx = f64::from(g_wxx.data()[i]) - 2.0 * x0 * f64::from(g_wx.data()[i]) + x0 * x0 * sw;
        let syy = f64::from(g_wyy.data()[i]) - 2.0 * y0 * f64::from(g_wy.data()[i]) + y0 * y0 * sw;
        let sxy = f64::from(g_wxy.data()[i]) - x0 * f64::from(g_wy.data()[i]) - y0 * f64::from(g_wx.data()[i]) + x0 * y0 * sw;
        let sv = f64::from(g_v.data()[i]);
        let svx = f64::from(g_vx.data()[i]) - x0 * sv;
        let svy = f64::from(g_vy.data()[i]) - y0 * sv;
        // ill-conditioned (weights on a line or a point): plain weighted mean
        let scale2 = (sxx + syy) / sw;
        let det_rel = (sxx * syy - sxy * sxy) / (sw * sw);
        if scale2 <= 0.0 || det_rel < 1e-4 * scale2 * scale2 {
            return mean;
        }
        match solve3([[sw, sx, sy], [sx, sxx, sxy], [sy, sxy, syy]], [sv, svx, svy]) {
            Some(sol) if sol[0].is_finite() => sol[0] as f32,
            _ => mean,
        }
    })
    .unwrap_or_else(|_| g_v.clone());
    if factor == 1 {
        return coarse;
    }
    upsample(&coarse, factor, w, h)
}

/// Weighted Gaussian mean `blur(v·w) / blur(w)` (order-0 normalised convolution) on a
/// block-averaged grid, upsampled bilinearly. Where no weight reaches, the weighted global mean.
pub fn masked_mean(values: &Plane, weight: &Plane, sigma: f32) -> Plane {
    let (w, h) = (values.width(), values.height());
    let sigma = if sigma.is_finite() { sigma.max(0.5) } else { 0.5 };
    let factor = ((sigma / 3.0).floor() as usize).clamp(1, w.max(h));
    let ok = |v: f32, k: f32| v.is_finite() && k.is_finite() && k > 0.0;
    let (Ok(vw), Ok(ww)) =
        (values.zip_map(weight, |v, k| if ok(v, k) { v * k } else { 0.0 }), values.zip_map(weight, |v, k| if ok(v, k) { k } else { 0.0 }))
    else {
        return values.clone();
    };
    let (tv, tw) = vw.data().iter().zip(ww.data()).fold((0.0f64, 0.0f64), |(a, b), (&x, &y)| (a + f64::from(x), b + f64::from(y)));
    let fallback = if tw > 0.0 { (tv / tw) as f32 } else { 0.0 };
    let (Ok(dv), Ok(dw)) = (resample::downsample(&vw, factor), resample::downsample(&ww, factor)) else {
        return values.clone();
    };
    let s = sigma / factor as f32;
    let (gv, gw) = (filter::gaussian(&dv, s), filter::gaussian(&dw, s));
    let coarse = gv.zip_map(&gw, |a, b| if b > 1e-6 { a / b } else { fallback }).unwrap_or(gv);
    if factor == 1 { coarse } else { upsample(&coarse, factor, w, h) }
}

/// Bilinear upsampling of a `factor`-block-averaged grid back onto `w × h` (pixel-centre aligned
/// with the blocks that `resample::downsample` produced).
pub fn upsample(coarse: &Plane, factor: usize, w: usize, h: usize) -> Plane {
    let f = factor as f32;
    Plane::from_fn(w, h, |x, y| coarse.sample((x as f32 + 0.5) / f - 0.5, (y as f32 + 0.5) / f - 0.5)).unwrap_or_else(|_| coarse.clone())
}

/// Median and 1.4826·MAD of at most `max_samples` strided finite values selected by `keep`.
pub fn robust_stats(values: &[f32], keep: impl Fn(usize) -> bool, max_samples: usize) -> Option<(f32, f32)> {
    let step = (values.len() / max_samples.max(1)).max(1);
    let sample: Vec<f32> = (0..values.len()).step_by(step).filter(|&i| keep(i)).map(|i| values[i]).filter(|v| v.is_finite()).collect();
    median_sigma(sample)
}

/// Median of finite values by selection (O(n)), `None` when there are none.
pub fn median(mut v: Vec<f32>) -> Option<f32> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    let m = v.len() / 2;
    let (_, &mut x, _) = v.select_nth_unstable_by(m, f32::total_cmp);
    Some(x)
}

/// Median and 1.4826·MAD of the finite values, by selection (O(n)).
pub fn median_sigma(v: Vec<f32>) -> Option<(f32, f32)> {
    let mut v: Vec<f32> = v.into_iter().filter(|x| x.is_finite()).collect();
    let med = median(v.clone())?;
    for x in &mut v {
        *x = (*x - med).abs();
    }
    Some((med, 1.4826 * median(v)?))
}

/// Luminance-like mean of the three channels (non-finite stays non-finite).
pub fn luma(width: usize, height: usize, rgb: &[f32]) -> Option<Plane> {
    let data: Vec<f32> = rgb.par_chunks_exact(3).map(|p| (p[0] + p[1] + p[2]) / 3.0).collect();
    Plane::new(width, height, data).ok()
}

/// Components of `low` that touch the image edge and are at least `min_area` pixels: the opaque
/// film holder or frame surround.
pub fn edge_connected(low: &Mask, min_area: usize) -> Mask {
    let (w, h) = (low.width(), low.height());
    let (labels, comps) = silvercraft_imgproc::label(low);
    let keep: Vec<bool> = std::iter::once(false)
        .chain(comps.iter().map(|c| c.area >= min_area && (c.bbox[0] == 0 || c.bbox[1] == 0 || c.bbox[2] >= w || c.bbox[3] >= h)))
        .collect();
    let mut out = low.clone();
    for (o, &l) in out.data_mut().iter_mut().zip(&labels) {
        *o = keep.get(l as usize).copied().unwrap_or(false);
    }
    out
}

/// The opaque holder or frame surround seen in the visible luma: edge-connected regions darker
/// than 35 % of the median.
pub fn surround(luma: &Plane, scale: f32) -> Mask {
    let median = robust_stats(luma.data(), |_| true, 200_000).map_or(0.0, |(m, _)| m);
    let low = Mask::from_plane(luma, |v| v.is_nan() || v <= 0.35 * median);
    edge_connected(&low, ((64.0 * scale * scale) as usize).max(16))
}

/// SplitMix64: a tiny deterministic hash for per-pixel noise (independent of thread order).
pub fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Standard normal deviate keyed by `key` (Box-Muller on two hashed uniforms).
pub fn gauss(key: u64) -> f32 {
    let a = splitmix(key);
    let b = splitmix(a ^ 0x5851_F42D_4C95_7F2D);
    let u1 = ((a >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    let u2 = (b >> 11) as f64 / (1u64 << 53) as f64;
    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
}

/// Solve the 3×3 system `a·x = b` (Cramer); `None` when singular.
pub fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(a);
    if !d.is_finite() || d.abs() < 1e-300 {
        return None;
    }
    let mut x = [0.0; 3];
    for (k, xk) in x.iter_mut().enumerate() {
        let mut m = a;
        for r in 0..3 {
            m[r][k] = b[r];
        }
        *xk = det(m) / d;
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_blur_ignores_masked_pixels() {
        let v = Plane::from_fn(64, 48, |x, _| if (30..34).contains(&x) { 100.0 } else { 1.0 }).unwrap();
        let wt = Plane::from_fn(64, 48, |x, _| if (30..34).contains(&x) { 0.0 } else { 1.0 }).unwrap();
        for sigma in [2.0, 12.0] {
            let b = masked_blur(&v, &wt, sigma);
            assert!(b.data().iter().all(|x| (x - 1.0).abs() < 1e-3), "sigma {sigma}");
        }
        let unweighted = masked_blur(&v, &Plane::filled(64, 48, 1.0).unwrap(), 12.0);
        assert!(unweighted.get(32, 24) > 2.0, "without the mask the spike leaks in");
    }

    #[test]
    fn gauss_is_standard_normal() {
        let v: Vec<f32> = (0..20_000u64).map(gauss).collect();
        let mean = v.iter().sum::<f32>() / v.len() as f32;
        let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / v.len() as f32;
        assert!(mean.abs() < 0.03 && (var - 1.0).abs() < 0.05, "{mean} {var}");
    }

    #[test]
    fn solve3_solves() {
        let x = solve3([[2.0, 0.0, 1.0], [0.0, 3.0, 0.0], [1.0, 0.0, 4.0]], [3.0, 3.0, 5.0]).unwrap();
        assert!((x[0] - 1.0).abs() < 1e-12 && (x[1] - 1.0).abs() < 1e-12 && (x[2] - 1.0).abs() < 1e-12);
        assert!(solve3([[0.0; 3]; 3], [1.0; 3]).is_none());
    }
}
