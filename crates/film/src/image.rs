//! Minimal interleaved linear RGB f32 image used across the crate.

use crate::error::FilmError;

/// Row-major, interleaved RGB, linear light, nominally 0..1 for scans
/// (scene-linear outputs may exceed 1). The invariant
/// `data.len() == width * height * 3` with `width, height > 0` is enforced by
/// every constructor, so downstream code can index without re-checking.
#[derive(Debug, Clone, PartialEq)]
pub struct RgbImage {
    width: usize,
    height: usize,
    data: Vec<f32>,
}

impl RgbImage {
    /// Wrap a buffer, validating dimensions. Non-finite VALUES are allowed
    /// (they are capture data); every stage treats them as unmeasured.
    pub fn new(width: usize, height: usize, data: Vec<f32>) -> Result<Self, FilmError> {
        if width == 0 || height == 0 {
            return Err(FilmError::EmptyImage { width, height });
        }
        let expected = width.checked_mul(height).and_then(|pixels| pixels.checked_mul(3)).ok_or(FilmError::DimensionOverflow { width, height })?;
        if data.len() != expected {
            return Err(FilmError::BufferSize { width, height, expected, got: data.len() });
        }
        Ok(Self { width, height, data })
    }

    /// A constant-colour image.
    pub fn filled(width: usize, height: usize, rgb: [f32; 3]) -> Result<Self, FilmError> {
        let pixels = width.checked_mul(height).ok_or(FilmError::DimensionOverflow { width, height })?;
        let data = std::iter::repeat_n(rgb, pixels).flatten().collect();
        Self::new(width, height, data)
    }

    /// Convert a 16-bit capture to 0..1 by `max_code` (65535 for full-range).
    pub fn from_u16(width: usize, height: usize, data: &[u16], max_code: f32) -> Result<Self, FilmError> {
        if !(max_code.is_finite() && max_code > 0.0) {
            return Err(FilmError::InvalidParameter("max_code must be positive"));
        }
        Self::new(width, height, data.iter().map(|&v| f32::from(v) / max_code).collect())
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixel_count(&self) -> usize {
        self.width * self.height
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

    /// One pixel, or `None` out of bounds.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[f32; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * 3;
        Some([self.data[i], self.data[i + 1], self.data[i + 2]])
    }

    /// Interleaved pixels of row `y` (empty when out of bounds).
    pub fn row(&self, y: usize) -> &[f32] {
        if y >= self.height {
            return &[];
        }
        let start = y * self.width * 3;
        &self.data[start..start + self.width * 3]
    }

    /// Copy of the rectangle `[x0, x1) x [y0, y1)`, clamped to the image.
    /// Errors when the clamped rectangle is empty.
    pub fn crop(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> Result<Self, FilmError> {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        let (x0, y0) = (x0.min(x1), y0.min(y1));
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0) * 3);
        for y in y0..y1 {
            data.extend_from_slice(&self.data[(y * self.width + x0) * 3..(y * self.width + x1) * 3]);
        }
        Self::new(x1 - x0, y1 - y0, data)
    }

    /// Strided pixel subsample with at most `max_samples` pixels (interleaved
    /// RGB). Mirrors the roll pipeline's `flat[::step]` sampling.
    pub fn subsample(&self, max_samples: usize) -> Vec<f32> {
        let pixels = self.pixel_count();
        let step = (pixels / max_samples.max(1)).max(1);
        let mut out = Vec::with_capacity((pixels / step + 1) * 3);
        for index in (0..pixels).step_by(step) {
            out.extend_from_slice(&self.data[index * 3..index * 3 + 3]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_mismatched_buffers() {
        assert!(matches!(RgbImage::new(0, 4, vec![]), Err(FilmError::EmptyImage { .. })));
        assert!(matches!(RgbImage::new(2, 2, vec![0.0; 11]), Err(FilmError::BufferSize { .. })));
        assert!(RgbImage::new(usize::MAX, 3, vec![]).is_err());
        assert!(RgbImage::from_u16(1, 1, &[1, 2, 3], 0.0).is_err());
    }

    #[test]
    fn accessors_and_crop() {
        let img = RgbImage::new(3, 2, (0..18).map(|v| v as f32).collect()).unwrap();
        assert_eq!(img.pixel(1, 1), Some([12.0, 13.0, 14.0]));
        assert_eq!(img.pixel(3, 0), None);
        assert_eq!(img.row(1).len(), 9);
        assert!(img.row(5).is_empty());
        let c = img.crop(1, 0, 3, 2).unwrap();
        assert_eq!((c.width(), c.height()), (2, 2));
        assert_eq!(c.pixel(0, 0), Some([3.0, 4.0, 5.0]));
        assert!(img.crop(5, 5, 9, 9).is_err());
        assert_eq!(img.subsample(2).len(), 6);
        assert_eq!(img.subsample(0).len(), 3);
    }
}
