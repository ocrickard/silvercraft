//! A single-channel float image.

use rayon::prelude::*;

use crate::{ImgError, Result};

/// Row-major single-channel f32 image with `width, height > 0` and `data.len() == width * height`
/// enforced by every constructor.
#[derive(Clone, Debug, PartialEq)]
pub struct Plane {
    width: usize,
    height: usize,
    data: Vec<f32>,
}

pub(crate) fn check_dims(width: usize, height: usize, channels: usize) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(ImgError::Empty { width, height });
    }
    width.checked_mul(height).and_then(|p| p.checked_mul(channels)).ok_or(ImgError::Overflow { width, height })
}

impl Plane {
    pub fn new(width: usize, height: usize, data: Vec<f32>) -> Result<Self> {
        let n = check_dims(width, height, 1)?;
        if data.len() != n {
            return Err(ImgError::BufferSize { width, height, channels: 1, got: data.len() });
        }
        Ok(Self { width, height, data })
    }

    /// Build from parts whose invariant the caller already holds (same-size derived data).
    pub(crate) fn from_parts(width: usize, height: usize, data: Vec<f32>) -> Self {
        debug_assert_eq!(data.len(), width * height);
        Self { width, height, data }
    }

    pub fn filled(width: usize, height: usize, v: f32) -> Result<Self> {
        let n = check_dims(width, height, 1)?;
        Ok(Self { width, height, data: vec![v; n] })
    }

    /// Build from `f(x, y)`, rows in parallel.
    pub fn from_fn(width: usize, height: usize, f: impl Fn(usize, usize) -> f32 + Sync) -> Result<Self> {
        let n = check_dims(width, height, 1)?;
        let mut data = vec![0.0; n];
        data.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            for (x, v) in row.iter_mut().enumerate() {
                *v = f(x, y);
            }
        });
        Ok(Self { width, height, data })
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    pub fn data(&self) -> &[f32] {
        &self.data
    }
    pub fn data_mut(&mut self) -> &mut [f32] {
        &mut self.data
    }
    pub fn into_data(self) -> Vec<f32> {
        self.data
    }

    /// Value at (x, y); coordinates outside the image are clamped to the edge.
    #[inline]
    pub fn at(&self, x: isize, y: isize) -> f32 {
        let xc = x.clamp(0, self.width as isize - 1) as usize;
        let yc = y.clamp(0, self.height as isize - 1) as usize;
        self.data[yc * self.width + xc]
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: f32) {
        self.data[y * self.width + x] = v;
    }

    pub fn row(&self, y: usize) -> &[f32] {
        &self.data[y * self.width..(y + 1) * self.width]
    }

    /// Bilinear sample at a fractional position (pixel centres at integers), edge-clamped.
    pub fn sample(&self, x: f32, y: f32) -> f32 {
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (xi, yi) = (x0 as isize, y0 as isize);
        let a = self.at(xi, yi) * (1.0 - fx) + self.at(xi + 1, yi) * fx;
        let b = self.at(xi, yi + 1) * (1.0 - fx) + self.at(xi + 1, yi + 1) * fx;
        a * (1.0 - fy) + b * fy
    }

    pub fn map(&self, f: impl Fn(f32) -> f32 + Sync) -> Self {
        Self { width: self.width, height: self.height, data: self.data.par_iter().map(|&v| f(v)).collect() }
    }

    /// Combine two same-size planes value by value.
    pub fn zip_map(&self, other: &Self, f: impl Fn(f32, f32) -> f32 + Sync) -> Result<Self> {
        self.same_size(other)?;
        Ok(Self { width: self.width, height: self.height, data: self.data.par_iter().zip(&other.data).map(|(&a, &b)| f(a, b)).collect() })
    }

    pub fn same_size(&self, other: &Self) -> Result<()> {
        if self.width != other.width || self.height != other.height {
            return Err(ImgError::SizeMismatch { a_w: self.width, a_h: self.height, b_w: other.width, b_h: other.height });
        }
        Ok(())
    }

    /// Copy of the rectangle `[x0, x0+w) × [y0, y0+h)` (clipped to the image).
    pub fn crop(&self, x0: usize, y0: usize, w: usize, h: usize) -> Result<Self> {
        let x1 = (x0.saturating_add(w)).min(self.width);
        let y1 = (y0.saturating_add(h)).min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return Err(ImgError::Param("crop outside the image"));
        }
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            data.extend_from_slice(&self.data[y * self.width + x0..y * self.width + x1]);
        }
        Self::new(x1 - x0, y1 - y0, data)
    }

    /// Mean of the finite values (NaN when there are none).
    pub fn mean(&self) -> f32 {
        let (s, n) = self.data.iter().filter(|v| v.is_finite()).fold((0.0f64, 0usize), |(s, n), &v| (s + f64::from(v), n + 1));
        if n == 0 { f32::NAN } else { (s / n as f64) as f32 }
    }
}

/// Split an interleaved buffer of `channels` channels into planes.
pub fn split_channels(width: usize, height: usize, channels: usize, data: &[f32]) -> Result<Vec<Plane>> {
    if channels == 0 {
        return Err(ImgError::Param("zero channels"));
    }
    let n = check_dims(width, height, channels)?;
    if data.len() != n {
        return Err(ImgError::BufferSize { width, height, channels, got: data.len() });
    }
    (0..channels).map(|c| Plane::new(width, height, data.iter().skip(c).step_by(channels).copied().collect())).collect()
}

/// Interleave same-size planes back into one buffer.
pub fn interleave(planes: &[Plane]) -> Result<Vec<f32>> {
    let first = planes.first().ok_or(ImgError::Param("no planes"))?;
    for p in planes {
        first.same_size(p)?;
    }
    let c = planes.len();
    let mut out = vec![0.0; first.len() * c];
    for (i, px) in out.chunks_exact_mut(c).enumerate() {
        for (k, p) in planes.iter().enumerate() {
            px[k] = p.data[i];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_validate() {
        assert!(Plane::new(0, 3, vec![]).is_err());
        assert!(Plane::new(2, 2, vec![0.0; 3]).is_err());
        assert!(Plane::new(2, 2, vec![0.0; 4]).is_ok());
    }

    #[test]
    fn split_and_interleave_round_trip() {
        let data: Vec<f32> = (0..24).map(|v| v as f32).collect();
        let planes = split_channels(4, 2, 3, &data).unwrap();
        assert_eq!(planes[1].get(0, 0), 1.0);
        assert_eq!(interleave(&planes).unwrap(), data);
    }

    #[test]
    fn bilinear_sample_interpolates() {
        let p = Plane::new(2, 1, vec![0.0, 1.0]).unwrap();
        assert!((p.sample(0.25, 0.0) - 0.25).abs() < 1e-6);
        assert_eq!(p.sample(-5.0, 0.0), 0.0);
    }

    #[test]
    fn crop_clips() {
        let p = Plane::from_fn(4, 4, |x, y| (x + 10 * y) as f32).unwrap();
        let c = p.crop(2, 1, 10, 2).unwrap();
        assert_eq!((c.width(), c.height()), (2, 2));
        assert_eq!(c.get(0, 0), 12.0);
        assert!(p.crop(4, 0, 1, 1).is_err());
    }
}
