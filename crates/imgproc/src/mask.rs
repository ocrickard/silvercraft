//! Binary masks.

use crate::plane::check_dims;
use crate::{ImgError, Plane, Result};

/// Row-major binary mask, same invariants as [`Plane`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    width: usize,
    height: usize,
    data: Vec<bool>,
}

impl Mask {
    pub fn new(width: usize, height: usize, data: Vec<bool>) -> Result<Self> {
        let n = check_dims(width, height, 1)?;
        if data.len() != n {
            return Err(ImgError::BufferSize { width, height, channels: 1, got: data.len() });
        }
        Ok(Self { width, height, data })
    }

    pub fn empty(width: usize, height: usize) -> Result<Self> {
        let n = check_dims(width, height, 1)?;
        Ok(Self { width, height, data: vec![false; n] })
    }

    /// `true` where `pred(value)` holds.
    pub fn from_plane(p: &Plane, pred: impl Fn(f32) -> bool) -> Self {
        Self { width: p.width(), height: p.height(), data: p.data().iter().map(|&v| pred(v)).collect() }
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn data(&self) -> &[bool] {
        &self.data
    }
    pub fn data_mut(&mut self) -> &mut [bool] {
        &mut self.data
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.data[y * self.width + x]
    }
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: bool) {
        self.data[y * self.width + x] = v;
    }

    pub fn count(&self) -> usize {
        self.data.iter().filter(|&&b| b).count()
    }

    pub fn fraction(&self) -> f64 {
        self.count() as f64 / self.data.len() as f64
    }

    fn check(&self, o: &Self) -> Result<()> {
        if self.width != o.width || self.height != o.height {
            return Err(ImgError::SizeMismatch { a_w: self.width, a_h: self.height, b_w: o.width, b_h: o.height });
        }
        Ok(())
    }

    pub fn or(&self, o: &Self) -> Result<Self> {
        self.check(o)?;
        Ok(Self { width: self.width, height: self.height, data: self.data.iter().zip(&o.data).map(|(a, b)| *a || *b).collect() })
    }

    pub fn and(&self, o: &Self) -> Result<Self> {
        self.check(o)?;
        Ok(Self { width: self.width, height: self.height, data: self.data.iter().zip(&o.data).map(|(a, b)| *a && *b).collect() })
    }

    pub fn not(&self) -> Self {
        Self { width: self.width, height: self.height, data: self.data.iter().map(|b| !b).collect() }
    }

    /// 1.0 / 0.0 plane.
    pub fn to_plane(&self) -> Plane {
        let data = self.data.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect();
        Plane::from_parts(self.width, self.height, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logic() {
        let a = Mask::new(2, 1, vec![true, false]).unwrap();
        let b = Mask::new(2, 1, vec![true, true]).unwrap();
        assert_eq!(a.and(&b).unwrap().count(), 1);
        assert_eq!(a.or(&b).unwrap().count(), 2);
        assert_eq!(a.not().count(), 1);
        assert_eq!(a.to_plane().data(), &[1.0, 0.0]);
    }
}
