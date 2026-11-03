//! Separable linear filters: Gaussian blur, box mean (integral image), gradients, and a small
//! median.

use rayon::prelude::*;

use crate::{ImgError, Plane, Result};

/// Hard bound on the Gaussian kernel radius: a hostile/config sigma (1e30) must not
/// turn into a multi-gigabyte kernel allocation.
const MAX_GAUSSIAN_RADIUS: isize = 8192;

fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let r = ((3.0 * sigma).ceil().max(1.0) as isize).min(MAX_GAUSSIAN_RADIUS);
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

/// Convolve rows then columns with a symmetric 1-D kernel (edge-clamped).
pub fn separable(p: &Plane, kernel: &[f32]) -> Plane {
    let (w, h) = (p.width(), p.height());
    let r = (kernel.len() / 2) as isize;
    let mut tmp = vec![0.0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = 0.0;
            for (i, k) in kernel.iter().enumerate() {
                s += k * p.at(x as isize + i as isize - r, y as isize);
            }
            *o = s;
        }
    });
    let tmp = Plane::from_parts(w, h, tmp);
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = 0.0;
            for (i, k) in kernel.iter().enumerate() {
                s += k * tmp.at(x as isize, y as isize + i as isize - r);
            }
            *o = s;
        }
    });
    Plane::from_parts(w, h, out)
}

/// Gaussian blur with standard deviation `sigma` pixels (`sigma <= 0` or non-finite
/// returns a copy). Sigma is capped at the plane's larger dimension (a wider blur is
/// a flat mean) and the kernel radius at 8192 px, so any finite
/// sigma is safe to pass.
pub fn gaussian(p: &Plane, sigma: f32) -> Plane {
    if !(sigma.is_finite() && sigma > 0.0) {
        return p.clone();
    }
    let sigma = sigma.min((p.width().max(p.height()).max(1)) as f32);
    separable(p, &gaussian_kernel(sigma))
}

/// Summed-area table with an extra zero row/column, f64 for precision on large images.
pub struct Integral {
    w: usize,
    h: usize,
    s: Vec<f64>,
}

impl Integral {
    pub fn new(p: &Plane) -> Self {
        let (w, h) = (p.width(), p.height());
        let mut s = vec![0.0f64; (w + 1) * (h + 1)];
        for y in 0..h {
            let mut run = 0.0f64;
            for x in 0..w {
                run += f64::from(p.get(x, y));
                s[(y + 1) * (w + 1) + x + 1] = s[y * (w + 1) + x + 1] + run;
            }
        }
        Self { w, h, s }
    }

    /// Sum over `[x0, x1) × [y0, y1)` (clipped).
    pub fn sum(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
        let (x1, y1) = (x1.min(self.w), y1.min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return 0.0;
        }
        let ww = self.w + 1;
        self.s[y1 * ww + x1] - self.s[y0 * ww + x1] - self.s[y1 * ww + x0] + self.s[y0 * ww + x0]
    }
}

/// Mean over a `(2r+1)²` window clipped to the image.
pub fn box_mean(p: &Plane, r: usize) -> Plane {
    let ii = Integral::new(p);
    let (w, h) = (p.width(), p.height());
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (y0, y1) = (y.saturating_sub(r), (y.saturating_add(r).saturating_add(1)).min(h));
        for (x, o) in row.iter_mut().enumerate() {
            let (x0, x1) = (x.saturating_sub(r), (x.saturating_add(r).saturating_add(1)).min(w));
            *o = (ii.sum(x0, y0, x1, y1) / ((x1 - x0) * (y1 - y0)) as f64) as f32;
        }
    });
    Plane::from_parts(w, h, out)
}

/// Mean of `p` over a window, counting only pixels where `weight > 0` (weights are 0/1 or soft).
/// Pixels with no weight in the window get `fallback`.
pub fn masked_box_mean(p: &Plane, weight: &Plane, r: usize, fallback: f32) -> Result<Plane> {
    p.same_size(weight)?;
    let pw = p.zip_map(weight, |a, b| if a.is_finite() { a * b } else { 0.0 })?;
    let num = Integral::new(&pw);
    let den = Integral::new(&weight.zip_map(p, |b, a| if a.is_finite() { b } else { 0.0 })?);
    let (w, h) = (p.width(), p.height());
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (y0, y1) = (y.saturating_sub(r), (y.saturating_add(r).saturating_add(1)).min(h));
        for (x, o) in row.iter_mut().enumerate() {
            let (x0, x1) = (x.saturating_sub(r), (x.saturating_add(r).saturating_add(1)).min(w));
            let d = den.sum(x0, y0, x1, y1);
            *o = if d > 1e-9 { (num.sum(x0, y0, x1, y1) / d) as f32 } else { fallback };
        }
    });
    Ok(Plane::from_parts(w, h, out))
}

/// Sobel gradients (gx, gy).
pub fn sobel(p: &Plane) -> (Plane, Plane) {
    let (w, h) = (p.width(), p.height());
    let gx = Plane::from_parts(
        w,
        h,
        (0..w * h)
            .into_par_iter()
            .map(|i| {
                let (x, y) = ((i % w) as isize, (i / w) as isize);
                (p.at(x + 1, y - 1) + 2.0 * p.at(x + 1, y) + p.at(x + 1, y + 1)) - (p.at(x - 1, y - 1) + 2.0 * p.at(x - 1, y) + p.at(x - 1, y + 1))
            })
            .collect(),
    );
    let gy = Plane::from_parts(
        w,
        h,
        (0..w * h)
            .into_par_iter()
            .map(|i| {
                let (x, y) = ((i % w) as isize, (i / w) as isize);
                (p.at(x - 1, y + 1) + 2.0 * p.at(x, y + 1) + p.at(x + 1, y + 1)) - (p.at(x - 1, y - 1) + 2.0 * p.at(x, y - 1) + p.at(x + 1, y - 1))
            })
            .collect(),
    );
    (gx, gy)
}

/// Median over a `(2r+1)²` window (edge-clamped). Intended for small radii (≤ 3).
pub fn median(p: &Plane, r: usize) -> Result<Plane> {
    if r > 7 {
        return Err(ImgError::Param("median radius above 7"));
    }
    let (w, h) = (p.width(), p.height());
    let ri = r as isize;
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let mut buf = Vec::with_capacity((2 * r + 1) * (2 * r + 1));
        for (x, o) in row.iter_mut().enumerate() {
            buf.clear();
            for dy in -ri..=ri {
                for dx in -ri..=ri {
                    buf.push(p.at(x as isize + dx, y as isize + dy));
                }
            }
            let m = buf.len() / 2;
            buf.select_nth_unstable_by(m, f32::total_cmp);
            *o = buf[m];
        }
    });
    Ok(Plane::from_parts(w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_preserves_constant_and_mass() {
        let p = Plane::filled(9, 7, 2.5).unwrap();
        assert!(gaussian(&p, 1.5).data().iter().all(|v| (v - 2.5).abs() < 1e-5));
        let mut d = Plane::filled(31, 31, 0.0).unwrap();
        d.set(15, 15, 1.0);
        let g = gaussian(&d, 2.0);
        assert!((g.data().iter().sum::<f32>() - 1.0).abs() < 1e-4);
        assert!(g.get(15, 15) > g.get(17, 15));
    }

    #[test]
    fn huge_sigma_and_radii_are_safe() {
        let p = Plane::filled(9, 7, 2.5).unwrap();
        // Previously: kernel radius 3e30 → allocation abort.
        assert!(gaussian(&p, 1e30).data().iter().all(|v| (v - 2.5).abs() < 1e-5));
        assert!(gaussian(&p, f32::NAN).data().iter().all(|v| *v == 2.5));
        // Radius arithmetic must saturate, not overflow.
        assert!(box_mean(&p, usize::MAX).data().iter().all(|v| (v - 2.5).abs() < 1e-6));
        let w = Plane::filled(9, 7, 1.0).unwrap();
        assert!(masked_box_mean(&p, &w, usize::MAX, 0.0).unwrap().data().iter().all(|v| (v - 2.5).abs() < 1e-6));
    }

    #[test]
    fn box_mean_matches_brute_force() {
        let p = Plane::from_fn(11, 9, |x, y| ((x * 7 + y * 3) % 5) as f32).unwrap();
        let b = box_mean(&p, 2);
        let (x, y) = (4usize, 4usize);
        let mut s = 0.0;
        for yy in 2..=6 {
            for xx in 2..=6 {
                s += p.get(xx, yy);
            }
        }
        assert!((b.get(x, y) - s / 25.0).abs() < 1e-5);
    }

    #[test]
    fn masked_mean_ignores_masked_pixels() {
        let p = Plane::from_fn(5, 5, |x, _| if x == 2 { 100.0 } else { 1.0 }).unwrap();
        let wgt = Plane::from_fn(5, 5, |x, _| if x == 2 { 0.0 } else { 1.0 }).unwrap();
        let m = masked_box_mean(&p, &wgt, 1, -1.0).unwrap();
        assert!((m.get(2, 2) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn median_removes_impulse() {
        let mut p = Plane::filled(7, 7, 1.0).unwrap();
        p.set(3, 3, 50.0);
        assert_eq!(median(&p, 1).unwrap().get(3, 3), 1.0);
    }

    #[test]
    fn sobel_sees_vertical_edge() {
        let p = Plane::from_fn(6, 6, |x, _| if x >= 3 { 1.0 } else { 0.0 }).unwrap();
        let (gx, gy) = sobel(&p);
        assert!(gx.get(3, 3) > 1.0);
        assert!(gy.get(3, 3).abs() < 1e-6);
    }
}
