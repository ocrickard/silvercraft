//! Connected components (8-connected) of a mask with per-component statistics.

use crate::Mask;

/// One connected region.
#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    /// Label in the label image (1-based; 0 is background).
    pub label: u32,
    pub area: usize,
    /// Bounding box `[x0, y0, x1, y1)`.
    pub bbox: [usize; 4],
    pub centroid: (f64, f64),
}

impl Component {
    pub fn width(&self) -> usize {
        self.bbox[2] - self.bbox[0]
    }
    pub fn height(&self) -> usize {
        self.bbox[3] - self.bbox[1]
    }
    /// Elongation: longer bbox side over shorter (≥ 1). Scratches and hairs are long and thin.
    pub fn elongation(&self) -> f64 {
        let (a, b) = (self.width().max(1) as f64, self.height().max(1) as f64);
        a.max(b) / a.min(b)
    }
    /// Fraction of the bounding box the region fills.
    pub fn fill(&self) -> f64 {
        self.area as f64 / (self.width().max(1) * self.height().max(1)) as f64
    }
}

/// Label 8-connected `true` regions. Returns the label image (0 = background) and components in
/// label order.
pub fn label(m: &Mask) -> (Vec<u32>, Vec<Component>) {
    let (w, h) = (m.width(), m.height());
    let mut labels = vec![0u32; w * h];
    let mut comps = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..w * h {
        if !m.data()[start] || labels[start] != 0 {
            continue;
        }
        let id = comps.len() as u32 + 1;
        labels[start] = id;
        stack.push(start);
        let (mut area, mut sx, mut sy) = (0usize, 0f64, 0f64);
        let mut bbox = [usize::MAX, usize::MAX, 0, 0];
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            area += 1;
            sx += x as f64;
            sy += y as f64;
            bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x + 1), bbox[3].max(y + 1)];
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    let (nx, ny) = (x as isize + dx, y as isize + dy);
                    if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if m.data()[j] && labels[j] == 0 {
                        labels[j] = id;
                        stack.push(j);
                    }
                }
            }
        }
        comps.push(Component { label: id, area, bbox, centroid: (sx / area as f64, sy / area as f64) });
    }
    (labels, comps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_two_regions() {
        let mut m = Mask::empty(6, 4).unwrap();
        m.set(0, 0, true);
        m.set(1, 1, true); // diagonal: same region
        for x in 3..6 {
            m.set(x, 3, true);
        }
        let (lab, comps) = label(&m);
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].area, 2);
        assert_eq!(comps[1].bbox, [3, 3, 6, 4]);
        assert_eq!(comps[1].elongation(), 3.0);
        assert_eq!(lab[6 + 1], 1);
    }
}
