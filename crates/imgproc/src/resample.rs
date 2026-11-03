//! Resampling: area downsampling, bilinear resize and sub-pixel shifts.

use rayon::prelude::*;

use crate::{ImgError, Plane, Result};

/// Average `factor × factor` blocks (partial blocks at the edges are averaged over what exists).
pub fn downsample(p: &Plane, factor: usize) -> Result<Plane> {
    if factor == 0 {
        return Err(ImgError::Param("downsample factor 0"));
    }
    if factor == 1 {
        return Ok(p.clone());
    }
    let (w, h) = (p.width().div_ceil(factor), p.height().div_ceil(factor));
    Plane::from_fn(w, h, |x, y| {
        let (x0, y0) = (x * factor, y * factor);
        let (x1, y1) = ((x0 + factor).min(p.width()), (y0 + factor).min(p.height()));
        let mut s = 0.0f32;
        for yy in y0..y1 {
            for xx in x0..x1 {
                s += p.get(xx, yy);
            }
        }
        s / ((x1 - x0) * (y1 - y0)) as f32
    })
}

/// Bilinear resize to `w × h` (pixel-centre aligned).
pub fn resize(p: &Plane, w: usize, h: usize) -> Result<Plane> {
    let sx = p.width() as f32 / w.max(1) as f32;
    let sy = p.height() as f32 / h.max(1) as f32;
    Plane::from_fn(w, h, |x, y| p.sample((x as f32 + 0.5) * sx - 0.5, (y as f32 + 0.5) * sy - 0.5))
}

/// `out(x, y) = p(x - dx, y - dy)`: move content by (dx, dy), bilinear, edge-clamped.
pub fn shift(p: &Plane, dx: f32, dy: f32) -> Plane {
    let (w, h) = (p.width(), p.height());
    let mut data = vec![0.0f32; w * h];
    data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            *o = p.sample(x as f32 - dx, y as f32 - dy);
        }
    });
    Plane::from_parts(w, h, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_averages_blocks() {
        let p = Plane::from_fn(4, 2, |x, _| x as f32).unwrap();
        let d = downsample(&p, 2).unwrap();
        assert_eq!((d.width(), d.height()), (2, 1));
        assert_eq!(d.data(), &[0.5, 2.5]);
        let odd = downsample(&Plane::filled(5, 5, 1.0).unwrap(), 2).unwrap();
        assert_eq!((odd.width(), odd.height()), (3, 3));
        assert!(odd.data().iter().all(|v| (v - 1.0).abs() < 1e-6));
    }

    #[test]
    fn shift_moves_content() {
        let mut p = Plane::filled(8, 8, 0.0).unwrap();
        p.set(3, 3, 1.0);
        let s = shift(&p, 2.0, 1.0);
        assert_eq!(s.get(5, 4), 1.0);
        assert_eq!(s.get(3, 3), 0.0);
    }

    #[test]
    fn resize_constant() {
        let r = resize(&Plane::filled(10, 6, 0.3).unwrap(), 4, 3).unwrap();
        assert!(r.data().iter().all(|v| (v - 0.3).abs() < 1e-6));
    }
}
