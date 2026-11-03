//! Shared helpers: sanitising, area-downsampling interleaved RGB into planes, small statistics.

use silvercraft_imgproc::Plane;

use crate::{Result, Rgb};

/// Untrusted sample to a usable value: non-finite → 0, negative → 0, huge → capped.
#[inline]
pub(crate) fn clean(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0e6) } else { 0.0 }
}

/// Area-average `factor × factor` blocks of an interleaved RGB image into three planes
/// (`factor` = 1 copies). `conv` maps a raw sample to f32.
pub(crate) fn downsample_rgb<T: Copy + Sync>(img: &Rgb<'_, T>, factor: usize, conv: impl Fn(T) -> f32 + Sync) -> Result<[Plane; 3]> {
    let f = factor.max(1);
    let (w, h) = (img.width(), img.height());
    let (ow, oh) = (w.div_ceil(f), h.div_ceil(f));
    let data = img.data();
    let channel = |c: usize| {
        Plane::from_fn(ow, oh, |x, y| {
            let (x0, y0) = (x * f, y * f);
            let (x1, y1) = ((x0 + f).min(w), (y0 + f).min(h));
            let mut s = 0.0f64;
            for yy in y0..y1 {
                let row = yy * w;
                for xx in x0..x1 {
                    s += f64::from(clean(data.get((row + xx) * 3 + c).map_or(0.0, |&v| conv(v))));
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1);
            (s / n as f64) as f32
        })
    };
    Ok([channel(0)?, channel(1)?, channel(2)?])
}

/// Rec.709 luminance of three same-size planes.
pub(crate) fn luma(p: &[Plane; 3]) -> Result<Plane> {
    let rg = p[0].zip_map(&p[1], |r, g| 0.2126 * r + 0.7152 * g)?;
    Ok(rg.zip_map(&p[2], |a, b| a + 0.0722 * b)?)
}

/// Several percentiles of a scratch buffer, sorting it in place (no allocation per call).
pub(crate) fn pcts_in_place(v: &mut [f32], qs: &[f64], out: &mut [f32]) -> bool {
    if v.is_empty() || out.len() < qs.len() {
        return false;
    }
    v.sort_unstable_by(f32::total_cmp);
    let n = v.len() - 1;
    for (o, &q) in out.iter_mut().zip(qs) {
        let pos = (q.clamp(0.0, 100.0) / 100.0) * n as f64;
        let i = pos.floor() as usize;
        let fr = (pos - i as f64) as f32;
        let a = v.get(i).copied().unwrap_or(0.0);
        let b = v.get((i + 1).min(n)).copied().unwrap_or(a);
        *o = a * (1.0 - fr) + b * fr;
    }
    true
}

/// Least-squares line `y = a + b x`, refit once without points beyond `3·MAD + 1` of the first
/// fit. `None` with fewer than 3 points or a degenerate spread.
pub(crate) fn robust_line(xs: &[f64], ys: &[f64]) -> Option<(f64, f64)> {
    fn fit(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
        if pts.len() < 3 {
            return None;
        }
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        if sxx <= 1e-9 {
            return None;
        }
        let b = sxy / sxx;
        let r = (my - b * mx, b);
        (r.0.is_finite() && r.1.is_finite()).then_some(r)
    }
    let pts: Vec<(f64, f64)> = xs.iter().copied().zip(ys.iter().copied()).filter(|p| p.0.is_finite() && p.1.is_finite()).collect();
    let (a, b) = fit(&pts)?;
    let mut res: Vec<f64> = pts.iter().map(|p| (p.1 - a - b * p.0).abs()).collect();
    res.sort_unstable_by(f64::total_cmp);
    let mad = res.get(res.len() / 2).copied().unwrap_or(0.0);
    let keep: Vec<(f64, f64)> = pts.iter().copied().filter(|p| (p.1 - a - b * p.0).abs() <= 3.0 * mad + 1.0).collect();
    fit(&keep).or(Some((a, b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_and_clean() {
        let data = [0.5f32, f32::NAN, -1.0, 1.0, f32::INFINITY, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let img = Rgb::new(2, 2, &data).unwrap();
        let [r, g, b] = downsample_rgb(&img, 2, |v| v).unwrap();
        assert_eq!((r.width(), r.height()), (1, 1));
        assert!((r.get(0, 0) - 0.625).abs() < 1e-6);
        assert!(g.get(0, 0).abs() < 1e-6 && (b.get(0, 0) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn robust_line_ignores_outliers() {
        let xs: Vec<f64> = (0..50).map(f64::from).collect();
        let mut ys: Vec<f64> = xs.iter().map(|x| 3.0 + 0.1 * x).collect();
        ys[10] = 500.0;
        let (a, b) = robust_line(&xs, &ys).unwrap();
        assert!((a - 3.0).abs() < 1e-6 && (b - 0.1).abs() < 1e-6);
        assert!(robust_line(&[1.0, 1.0, 1.0], &[0.0, 1.0, 2.0]).is_none());
    }
}
