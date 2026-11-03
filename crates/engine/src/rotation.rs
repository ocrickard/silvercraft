//! Quarter-turn rotation: how a capture is turned to stand upright. The capture itself is never
//! rotated; the turn travels in `roll.json` and the DNG's orientation, and displays apply it.

use serde::{Deserialize, Serialize};

/// Clockwise quarter turns, 0..=3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "u16", into = "u16")]
pub struct Rotation(u8);

impl Rotation {
    pub const NONE: Rotation = Rotation(0);

    /// `degrees` clockwise, rounded to the nearest quarter turn.
    pub fn from_degrees(degrees: i32) -> Self {
        Rotation(((degrees as f64 / 90.0).round() as i32).rem_euclid(4) as u8)
    }

    pub fn degrees(self) -> u16 {
        u16::from(self.0) * 90
    }

    pub fn quarter_turns(self) -> u8 {
        self.0
    }

    // by reference: serde's `skip_serializing_if` passes one
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn is_none(&self) -> bool {
        self.0 == 0
    }

    /// A further quarter turn clockwise (`cw`) or counter-clockwise.
    pub fn turned(self, cw: bool) -> Self {
        Rotation((self.0 + if cw { 1 } else { 3 }) % 4)
    }

    pub fn inverse(self) -> Self {
        Rotation((4 - self.0) % 4)
    }

    /// Size of a `w × h` image after the turn.
    pub fn size(self, w: usize, h: usize) -> (usize, usize) {
        if self.0 % 2 == 1 { (h, w) } else { (w, h) }
    }

    /// Where point `(x, y)` of a `w × h` image lands after the turn (continuous coordinates:
    /// the image spans `0..w`, `0..h`).
    pub fn point(self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
        match self.0 {
            1 => (h - y, x),
            2 => (w - x, h - y),
            3 => (y, w - x),
            _ => (x, y),
        }
    }

    /// Box `[x0, y0, x1, y1]` of a `w × h` image after the turn.
    pub fn bbox(self, b: [f64; 4], w: f64, h: f64) -> [f64; 4] {
        let (ax, ay) = self.point(b[0], b[1], w, h);
        let (bx, by) = self.point(b[2], b[3], w, h);
        [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
    }

    /// Turn an interleaved `w × h` image with `channels` samples per pixel.
    pub fn pixels<T: Copy>(self, w: usize, h: usize, channels: usize, data: &[T]) -> (usize, usize, Vec<T>) {
        if self.0 == 0 || data.len() < w * h * channels {
            return (w, h, data.to_vec());
        }
        let (nw, nh) = self.size(w, h);
        let mut out = Vec::with_capacity(w * h * channels);
        for ny in 0..nh {
            for nx in 0..nw {
                // the source pixel that lands at (nx, ny)
                let (x, y) = match self.0 {
                    1 => (ny, h - 1 - nx),
                    2 => (w - 1 - nx, h - 1 - ny),
                    _ => (w - 1 - ny, nx),
                };
                let i = (y * w + x) * channels;
                out.extend_from_slice(&data[i..i + channels]);
            }
        }
        (nw, nh, out)
    }

    /// The DNG orientation that shows a capture turned this way.
    pub fn orientation(self) -> silvercraft_handoff::Orientation {
        use silvercraft_handoff::Orientation::*;
        match self.0 {
            1 => Rotate90,
            2 => Rotate180,
            3 => Rotate270,
            _ => Normal,
        }
    }

    /// The turn a DNG orientation applies (flips are ignored: SilverCraft never writes them).
    pub fn of_orientation(o: silvercraft_handoff::Orientation) -> Self {
        Rotation(o.to_parts().1 % 4)
    }
}

impl From<u16> for Rotation {
    fn from(degrees: u16) -> Self {
        Rotation::from_degrees(i32::from(degrees))
    }
}

impl From<Rotation> for u16 {
    fn from(r: Rotation) -> u16 {
        r.degrees()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_compose_and_invert() {
        let r = Rotation::NONE.turned(true);
        assert_eq!(r.degrees(), 90);
        assert_eq!(r.turned(false), Rotation::NONE);
        assert_eq!(Rotation::NONE.turned(false).degrees(), 270);
        assert_eq!(Rotation::from_degrees(-90).degrees(), 270);
        assert_eq!(Rotation::from(450).degrees(), 90);
        for q in 0..4 {
            let r = Rotation(q);
            assert_eq!(Rotation::of_orientation(r.orientation()), r);
            assert_eq!(serde_json::from_str::<Rotation>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
        }
    }

    #[test]
    fn pixels_and_points_agree() {
        // a 3 × 2 image, one channel, each pixel its own index
        let (w, h) = (3usize, 2usize);
        let data: Vec<u32> = (0..6).collect();
        for q in 0..4u8 {
            let r = Rotation(q);
            let (nw, nh, out) = r.pixels(w, h, 1, &data);
            assert_eq!((nw, nh), r.size(w, h));
            for y in 0..h {
                for x in 0..w {
                    // the pixel centre's new place
                    let (px, py) = r.point(x as f64 + 0.5, y as f64 + 0.5, w as f64, h as f64);
                    let (nx, ny) = (px.floor() as usize, py.floor() as usize);
                    assert_eq!(out[ny * nw + nx], data[y * w + x], "q {q} at {x},{y}");
                }
            }
            // a box goes there and back
            let b = [0.5, 0.25, 2.0, 1.5];
            let there = r.bbox(b, w as f64, h as f64);
            let (rw, rh) = r.size(w, h);
            assert_eq!(r.inverse().bbox(there, rw as f64, rh as f64), b);
        }
        // a quarter turn clockwise: the top-left pixel goes to the top-right
        let (_, _, out) = Rotation(1).pixels(w, h, 1, &data);
        assert_eq!(out, vec![3, 0, 4, 1, 5, 2]);
    }
}
