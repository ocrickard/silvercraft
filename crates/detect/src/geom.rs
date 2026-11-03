//! Rectangles: axis-aligned pixel boxes, millimetre boxes and rotated rectangles.

use serde::{Deserialize, Serialize};

/// An axis-aligned box in image pixels (`x, y` = top-left; same layout as `epson::PixelRect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PxRect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// The box covering `[x0, x1) × [y0, y1)` (floats, outward-rounded), clamped to a
    /// `w × h` image. `None` when nothing of it is inside or a value is not finite.
    pub fn from_bounds(x0: f64, y0: f64, x1: f64, y1: f64, w: usize, h: usize) -> Option<Self> {
        if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
            return None;
        }
        let cx = |v: f64| v.clamp(0.0, w as f64);
        let cy = |v: f64| v.clamp(0.0, h as f64);
        let (ax, bx) = (cx(x0.min(x1)).floor(), cx(x0.max(x1)).ceil());
        let (ay, by) = (cy(y0.min(y1)).floor(), cy(y0.max(y1)).ceil());
        if bx - ax < 1.0 || by - ay < 1.0 {
            return None;
        }
        let u = |v: f64| v.min(f64::from(u32::MAX)) as u32;
        Some(Self { x: u(ax), y: u(ay), width: u(bx - ax), height: u(by - ay) })
    }

    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Right edge (exclusive).
    pub fn right(&self) -> u64 {
        u64::from(self.x) + u64::from(self.width)
    }

    /// Bottom edge (exclusive).
    pub fn bottom(&self) -> u64 {
        u64::from(self.y) + u64::from(self.height)
    }

    /// Intersection over union (0 when either box is empty).
    pub fn iou(&self, o: &Self) -> f64 {
        let x0 = u64::from(self.x.max(o.x));
        let y0 = u64::from(self.y.max(o.y));
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        let inter = ((x1 - x0) * (y1 - y0)) as f64;
        let union = self.area() as f64 + o.area() as f64 - inter;
        if union > 0.0 { inter / union } else { 0.0 }
    }

    /// The box in millimetres at `dpi`.
    pub fn to_mm(&self, dpi: f64) -> MmRect {
        let k = if dpi > 0.0 && dpi.is_finite() { 25.4 / dpi } else { 0.0 };
        MmRect { x: f64::from(self.x) * k, y: f64::from(self.y) * k, width: f64::from(self.width) * k, height: f64::from(self.height) * k }
    }
}

/// An axis-aligned box in millimetres from the image origin.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MmRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A rotated rectangle in pixels: centre, size, and rotation.
///
/// `angle_deg` is the rotation of the rectangle's `width` side from the +x axis, in image
/// coordinates (y down), so a positive angle turns clockwise on screen. Frame skews are small
/// (a few degrees).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RotRect {
    pub cx: f64,
    pub cy: f64,
    pub width: f64,
    pub height: f64,
    pub angle_deg: f64,
}

impl RotRect {
    /// The four corners (top-left, top-right, bottom-right, bottom-left before rotation).
    pub fn corners(&self) -> [(f64, f64); 4] {
        let (s, c) = self.angle_deg.to_radians().sin_cos();
        let (hw, hh) = (self.width / 2.0, self.height / 2.0);
        [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)].map(|(u, v)| (self.cx + u * c - v * s, self.cy + u * s + v * c))
    }

    /// Axis-aligned bounding box, clamped to a `w × h` image.
    pub fn bounding_box(&self, w: usize, h: usize) -> Option<PxRect> {
        let cs = self.corners();
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in cs {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        PxRect::from_bounds(x0, y0, x1, y1, w, h)
    }

    /// The unrotated box with the same centre and size, clamped to a `w × h` image (what a
    /// crop that is deskewed separately covers).
    pub fn upright_box(&self, w: usize, h: usize) -> Option<PxRect> {
        PxRect::from_bounds(self.cx - self.width / 2.0, self.cy - self.height / 2.0, self.cx + self.width / 2.0, self.cy + self.height / 2.0, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_and_bounds() {
        let a = PxRect::new(0, 0, 10, 10);
        assert_eq!(a.iou(&a), 1.0);
        assert!((a.iou(&PxRect::new(5, 0, 10, 10)) - 50.0 / 150.0).abs() < 1e-12);
        assert_eq!(a.iou(&PxRect::new(20, 20, 1, 1)), 0.0);
        assert_eq!(PxRect::from_bounds(-5.0, -5.0, 3.2, 4.0, 10, 10), Some(PxRect::new(0, 0, 4, 4)));
        assert_eq!(PxRect::from_bounds(f64::NAN, 0.0, 1.0, 1.0, 10, 10), None);
        assert_eq!(PxRect::from_bounds(20.0, 20.0, 30.0, 30.0, 10, 10), None);
        let mm = PxRect::new(200, 0, 200, 100).to_mm(200.0);
        assert!((mm.x - 25.4).abs() < 1e-9 && (mm.height - 12.7).abs() < 1e-9);
    }

    #[test]
    fn rotated_rect_bbox_grows_with_angle() {
        let r = RotRect { cx: 50.0, cy: 50.0, width: 40.0, height: 20.0, angle_deg: 0.0 };
        assert_eq!(r.bounding_box(100, 100), Some(PxRect::new(30, 40, 40, 20)));
        let r90 = RotRect { angle_deg: 90.0, ..r };
        let b = r90.bounding_box(100, 100).unwrap();
        assert!((b.width as i64 - 20).abs() <= 1 && (b.height as i64 - 40).abs() <= 1);
        let small = RotRect { angle_deg: 3.0, ..r };
        assert!(small.bounding_box(100, 100).unwrap().height > 20);
        assert_eq!(small.upright_box(100, 100), Some(PxRect::new(30, 40, 40, 20)));
    }
}
