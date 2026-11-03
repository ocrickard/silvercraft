//! Morphology on masks and planes with square structuring elements (separable, O(n·r)).

use rayon::prelude::*;

use crate::{Mask, Plane};

fn run_1d(src: &[bool], dst: &mut [bool], r: usize, any: bool) {
    let n = src.len();
    // prefix counts of `true`
    let mut pre = vec![0u32; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + u32::from(src[i]);
    }
    for i in 0..n {
        let a = i.saturating_sub(r);
        let b = (i.saturating_add(r).saturating_add(1)).min(n);
        let c = pre[b] - pre[a];
        dst[i] = if any { c > 0 } else { c as usize == b - a };
    }
}

fn morph(m: &Mask, r: usize, dilate: bool) -> Mask {
    if r == 0 {
        return m.clone();
    }
    let (w, h) = (m.width(), m.height());
    let mut rows = vec![false; w * h];
    rows.par_chunks_mut(w).enumerate().for_each(|(y, out)| run_1d(&m.data()[y * w..(y + 1) * w], out, r, dilate));
    // columns: transpose-free, process column by column
    let cols: Vec<Vec<bool>> = (0..w)
        .into_par_iter()
        .map(|x| {
            let col: Vec<bool> = (0..h).map(|y| rows[y * w + x]).collect();
            let mut out = vec![false; h];
            run_1d(&col, &mut out, r, dilate);
            out
        })
        .collect();
    let mut data = vec![false; w * h];
    for (x, col) in cols.iter().enumerate() {
        for (y, &v) in col.iter().enumerate() {
            data[y * w + x] = v;
        }
    }
    let mut out = m.clone();
    out.data_mut().copy_from_slice(&data);
    out
}

/// Grow `true` regions by `r` pixels (square element).
pub fn dilate(m: &Mask, r: usize) -> Mask {
    morph(m, r, true)
}

/// Shrink `true` regions by `r` pixels (square element; the border counts as `true`-neutral).
pub fn erode(m: &Mask, r: usize) -> Mask {
    morph(m, r, false)
}

pub fn open(m: &Mask, r: usize) -> Mask {
    dilate(&erode(m, r), r)
}

pub fn close(m: &Mask, r: usize) -> Mask {
    erode(&dilate(m, r), r)
}

fn extreme_1d(src: &[f32], dst: &mut [f32], r: usize, max: bool) {
    let n = src.len();
    for i in 0..n {
        let a = i.saturating_sub(r);
        let b = (i.saturating_add(r).saturating_add(1)).min(n);
        let mut v = src[a];
        for &s in &src[a + 1..b] {
            v = if max { v.max(s) } else { v.min(s) };
        }
        dst[i] = v;
    }
}

fn extreme(p: &Plane, r: usize, max: bool) -> Plane {
    if r == 0 {
        return p.clone();
    }
    let (w, h) = (p.width(), p.height());
    let mut rows = vec![0.0f32; w * h];
    rows.par_chunks_mut(w).enumerate().for_each(|(y, out)| extreme_1d(p.row(y), out, r, max));
    let cols: Vec<Vec<f32>> = (0..w)
        .into_par_iter()
        .map(|x| {
            let col: Vec<f32> = (0..h).map(|y| rows[y * w + x]).collect();
            let mut out = vec![0.0; h];
            extreme_1d(&col, &mut out, r, max);
            out
        })
        .collect();
    let mut data = vec![0.0f32; w * h];
    for (x, col) in cols.iter().enumerate() {
        for (y, &v) in col.iter().enumerate() {
            data[y * w + x] = v;
        }
    }
    Plane::from_parts(w, h, data)
}

/// Grey-level dilation (local max over a square window).
pub fn max_filter(p: &Plane, r: usize) -> Plane {
    extreme(p, r, true)
}

/// Grey-level erosion (local min over a square window).
pub fn min_filter(p: &Plane, r: usize) -> Plane {
    extreme(p, r, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dilate_then_erode() {
        let mut m = Mask::empty(9, 9).unwrap();
        m.set(4, 4, true);
        let d = dilate(&m, 1);
        assert_eq!(d.count(), 9);
        assert_eq!(erode(&d, 1).count(), 1);
        assert_eq!(open(&m, 1).count(), 0, "opening removes specks smaller than the element");
    }

    #[test]
    fn grey_min_max() {
        let p = Plane::from_fn(5, 5, |x, y| (x + y) as f32).unwrap();
        assert_eq!(max_filter(&p, 1).get(2, 2), 6.0);
        assert_eq!(min_filter(&p, 1).get(2, 2), 2.0);
    }

    #[test]
    fn huge_radii_saturate_to_the_whole_image() {
        let p = Plane::from_fn(5, 5, |x, y| (x + y) as f32).unwrap();
        assert_eq!(max_filter(&p, usize::MAX).get(0, 0), 8.0);
        assert_eq!(min_filter(&p, usize::MAX).get(4, 4), 0.0);
        let mut m = Mask::empty(5, 5).unwrap();
        m.set(2, 2, true);
        assert_eq!(dilate(&m, usize::MAX).count(), 25);
    }
}
