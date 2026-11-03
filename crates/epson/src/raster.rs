//! Raster assembly and host-side corrections: visible BIL → RGB16 (§3.8), IR 3-sample
//! collapse (§4.3), tile level matching (§7.1), overlapped dual-pass stitching (§4.5) and the
//! IR↔visible residual offset estimate (§4.4). Pure functions; no Windows de-framing (§3.9 is
//! never applied to the clean nusb/libusb stream).

use crate::EpsonError;

/// A 16-bit RGB image, row-major, pixel-interleaved `[h][w][3]`.
///
/// Fields are private and every constructor checks `data.len() == width * height * 3`,
/// so the slicing in assembly and stitching cannot be panicked by a hand-built image
/// whose geometry disagrees with its buffer. A 0×0 image (empty buffer) is the
/// "no image" value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    width: usize,
    height: usize,
    data: Vec<u16>,
}

/// A 16-bit single-channel image, row-major. Sealed like [`RgbImage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayImage {
    width: usize,
    height: usize,
    data: Vec<u16>,
}

impl RgbImage {
    /// Wrap a buffer, validating its length against the geometry.
    pub fn new(width: usize, height: usize, data: Vec<u16>) -> Result<RgbImage, EpsonError> {
        let expected = width.checked_mul(height).and_then(|n| n.checked_mul(3));
        if expected != Some(data.len()) {
            return Err(EpsonError::Raster(format!("{width}x{height} RGB image needs {expected:?} samples, got {}", data.len())));
        }
        Ok(RgbImage { width, height, data })
    }
    /// The empty (0×0) image.
    pub fn empty() -> RgbImage {
        RgbImage { width: 0, height: 0, data: Vec::new() }
    }
    /// Width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }
    /// Height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }
    /// `width * height * 3` samples, pixel-interleaved.
    pub fn data(&self) -> &[u16] {
        &self.data
    }
    /// Sample at (x, y, c); 0 when out of range.
    pub fn get(&self, x: usize, y: usize, c: usize) -> u16 {
        self.data.get((y * self.width + x) * 3 + c).copied().unwrap_or(0)
    }
    fn crop_rows(&self, h: usize) -> RgbImage {
        let h = h.min(self.height);
        RgbImage { width: self.width, height: h, data: self.data[..h * self.width * 3].to_vec() }
    }
}

impl GrayImage {
    /// Wrap a buffer, validating its length against the geometry.
    pub fn new(width: usize, height: usize, data: Vec<u16>) -> Result<GrayImage, EpsonError> {
        if width.checked_mul(height) != Some(data.len()) {
            return Err(EpsonError::Raster(format!("{width}x{height} gray image has {} samples", data.len())));
        }
        Ok(GrayImage { width, height, data })
    }
    /// The empty (0×0) image.
    pub fn empty() -> GrayImage {
        GrayImage { width: 0, height: 0, data: Vec::new() }
    }
    /// Width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }
    /// Height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }
    /// `width * height` samples.
    pub fn data(&self) -> &[u16] {
        &self.data
    }
    /// Sample at (x, y); 0 when out of range.
    pub fn get(&self, x: usize, y: usize) -> u16 {
        self.data.get(y * self.width + x).copied().unwrap_or(0)
    }
}

fn need(raw: &[u8], width: usize, height: usize, what: &str) -> Result<usize, EpsonError> {
    if width == 0 || height == 0 {
        return Err(EpsonError::Raster(format!("{what}: empty geometry {width}x{height}")));
    }
    let n = width.checked_mul(height).and_then(|v| v.checked_mul(6)).ok_or_else(|| EpsonError::Raster("geometry overflow".into()))?;
    if raw.len() < n {
        return Err(EpsonError::Raster(format!("{what}: raster has {} bytes; need {n} for {width}x{height} 3-sample 16-bit", raw.len())));
    }
    Ok(n)
}

/// Visible raster: band-interleaved by line (`[h][3][w]` u16le, hardware-confirmed EP:1813-1822)
/// → `[h][w][3]`. Fails if too short or all-zero. Extra bytes are ignored.
pub fn assemble_bil(raw: &[u8], width: usize, height: usize) -> Result<RgbImage, EpsonError> {
    let n = need(raw, width, height, "visible")?;
    let raw = &raw[..n];
    if raw.iter().all(|&b| b == 0) {
        return Err(EpsonError::Raster("visible raster is all zero".into()));
    }
    let mut data = vec![0u16; width * height * 3];
    for y in 0..height {
        for c in 0..3 {
            let line = &raw[(y * 3 + c) * width * 2..(y * 3 + c + 1) * width * 2];
            for (x, s) in line.chunks_exact(2).enumerate() {
                data[(y * width + x) * 3 + c] = u16::from_le_bytes([s[0], s[1]]);
            }
        }
    }
    Ok(RgbImage { width, height, data })
}

/// IR raster: pixel-interleaved `[h][w][3]` u16le (NOT BIL; EP:1847-1888) collapsed to one plane
/// by `rint(mean of 3)`. The sum of three integers over 3 never lands on .5, so
/// `(s + 1) / 3` is exactly numpy's round-half-even here.
pub fn collapse_ir(raw: &[u8], width: usize, height: usize) -> Result<GrayImage, EpsonError> {
    let n = need(raw, width, height, "infrared")?;
    let data = raw[..n]
        .chunks_exact(6)
        .map(|p| {
            let s = u32::from(u16::from_le_bytes([p[0], p[1]]))
                + u32::from(u16::from_le_bytes([p[2], p[3]]))
                + u32::from(u16::from_le_bytes([p[4], p[5]]));
            ((s + 1) / 3) as u16
        })
        .collect();
    Ok(GrayImage { width, height, data })
}

/// numpy-compatible median (mean of the middle two for even counts); 0 for empty input.
fn median(mut v: Vec<f64>) -> f64 {
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn apply_gain(col: &RgbImage, gain: [f64; 3]) -> RgbImage {
    let data = col.data.iter().enumerate().map(|(i, &s)| (f64::from(s) * gain[i % 3]).clamp(0.0, 65535.0) as u16).collect();
    RgbImage { width: col.width, height: col.height, data }
}

/// Level-match abutting visible tiles (§7.1, ED:4998-5028): per channel,
/// `gain = median(prev[:rows, -6:]) / median(col[:rows, :6])` clipped to [0.85, 1.18].
pub fn level_match_columns(columns: Vec<RgbImage>) -> Vec<RgbImage> {
    const EDGE: usize = 6;
    let mut out: Vec<RgbImage> = Vec::with_capacity(columns.len());
    for col in columns {
        let Some(prev) = out.last() else {
            out.push(col);
            continue;
        };
        let rows = prev.height.min(col.height);
        let mut gain = [1.0; 3];
        for (c, g) in gain.iter_mut().enumerate() {
            let mut right = Vec::new();
            let mut left = Vec::new();
            for y in 0..rows {
                for x in prev.width.saturating_sub(EDGE)..prev.width {
                    right.push(f64::from(prev.get(x, y, c)));
                }
                for x in 0..EDGE.min(col.width) {
                    left.push(f64::from(col.get(x, y, c)));
                }
            }
            let (r, l) = (median(right), median(left));
            *g = if l > 1e-6 { (r / l).clamp(0.85, 1.18) } else { 1.0 };
        }
        out.push(apply_gain(&col, gain));
    }
    out
}

/// Crop to the smallest height and concatenate horizontally.
pub fn concat_columns(columns: &[RgbImage]) -> RgbImage {
    let height = columns.iter().map(|c| c.height).min().unwrap_or(0);
    let width: usize = columns.iter().map(|c| c.width).sum();
    let mut data = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for c in columns {
            data.extend_from_slice(&c.data[y * c.width * 3..(y + 1) * c.width * 3]);
        }
    }
    RgbImage { width, height, data }
}

/// One stitched seam of a wide dual pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seam {
    /// Columns of overlap consumed.
    pub dx: usize,
    /// Row shift applied to the new tile (np.roll semantics).
    pub dy: i32,
    /// Normalised cross-correlation score of the chosen alignment.
    pub score: f64,
    /// Per-channel RGB gain applied to the new tile.
    pub gain: [f64; 3],
}

fn roll_index(i: usize, dy: i32, n: usize) -> usize {
    // np.roll(a, dy, axis=0): out[i] = a[(i - dy) mod n]
    let n_i = n as i64;
    (((i as i64 - i64::from(dy)) % n_i + n_i) % n_i) as usize
}

/// Stitch overlapping dual-pass tiles (§4.5, ED:5339-5472), applying each seam's RGB-derived
/// (dx, dy) to the IR plane too. Rows wrap circularly exactly like `np.roll` (bit-compatible
/// with the golden vectors).
pub fn stitch_overlapped(rgb_cols: Vec<RgbImage>, ir_cols: Vec<GrayImage>, nominal_overlap: usize) -> (RgbImage, GrayImage, Vec<Seam>) {
    let mut rgb_iter = rgb_cols.into_iter();
    let mut ir_iter = ir_cols.into_iter();
    let (Some(mut acc), Some(mut acc_ir)) = (rgb_iter.next(), ir_iter.next()) else {
        return (RgbImage { width: 0, height: 0, data: vec![] }, GrayImage { width: 0, height: 0, data: vec![] }, vec![]);
    };
    let mut seams = Vec::new();
    for (col, col_ir) in rgb_iter.zip(ir_iter) {
        let h = acc.height.min(col.height);
        if h == 0 || col.width == 0 || acc.width == 0 {
            break;
        }
        let lum =
            |img: &RgbImage, x: usize, y: usize| (f64::from(img.get(x, y, 0)) + f64::from(img.get(x, y, 1)) + f64::from(img.get(x, y, 2))) / 3.0;
        let step = (h / 1000).max(1);
        let rows: Vec<usize> = (0..h).step_by(step).collect();
        let nr = rows.len();
        let (wa, wc) = (acc.width, col.width);
        let mut best = (-1e18f64, nominal_overlap.min(wa).min(wc), 0i32);
        let lo = 8.max(nominal_overlap.saturating_sub(24));
        let hi = wa.min(wc).min(nominal_overlap + 24);
        for dx in lo..=hi {
            if dx == 0 {
                continue;
            }
            let a: Vec<f64> = rows.iter().flat_map(|&y| (wa - dx..wa).map(move |x| (x, y))).map(|(x, y)| lum(&acc, x, y)).collect();
            let am = a.iter().sum::<f64>() / a.len() as f64;
            let an: Vec<f64> = a.iter().map(|v| v - am).collect();
            let ad = an.iter().map(|v| v * v).sum::<f64>().sqrt();
            for dy in -8..=8 {
                // roll applied to the row-subsampled array, as before.
                let rows = &rows;
                let c: Vec<f64> =
                    (0..nr).flat_map(|i| (0..dx).map(move |x| (x, rows[roll_index(i, dy, nr)]))).map(|(x, y)| lum(&col, x, y)).collect();
                let cm = c.iter().sum::<f64>() / c.len() as f64;
                let mut num = 0.0;
                let mut cd = 0.0;
                for (av, cv) in an.iter().zip(&c) {
                    let cn = cv - cm;
                    num += av * cn;
                    cd += cn * cn;
                }
                let score = num / (ad * cd.sqrt() + 1e-9);
                if score > best.0 {
                    best = (score, dx, dy);
                }
            }
        }
        let (score, dx, dy) = best;
        let dx = dx.min(col.width).min(acc.width);
        let mut gain = [1.0; 3];
        for (ch, g) in gain.iter_mut().enumerate() {
            let mut a = Vec::with_capacity(h * dx);
            let mut b = Vec::with_capacity(h * dx);
            for y in 0..h {
                for x in acc.width - dx..acc.width {
                    a.push(f64::from(acc.get(x, y, ch)));
                }
                let sy = roll_index(y, dy, col.height);
                for x in 0..dx {
                    b.push(f64::from(col.get(x, sy, ch)));
                }
            }
            *g = (median(a) / median(b).max(1e-6)).clamp(0.85, 1.18);
        }
        let new_w = acc.width + col.width - dx;
        let mut data = Vec::with_capacity(new_w * h * 3);
        let mut ir = Vec::with_capacity(new_w * h);
        for y in 0..h {
            data.extend_from_slice(&acc.data[y * acc.width * 3..(y + 1) * acc.width * 3]);
            let sy = roll_index(y, dy, col.height);
            for x in dx..col.width {
                for (ch, g) in gain.iter().enumerate() {
                    data.push((f64::from(col.get(x, sy, ch)) * g).clamp(0.0, 65535.0) as u16);
                }
            }
            ir.extend_from_slice(acc_ir.data.get(y * acc_ir.width..(y + 1) * acc_ir.width).unwrap_or(&[]));
            let sy_ir = roll_index(y, dy, col_ir.height.max(1));
            for x in dx..col_ir.width {
                ir.push(col_ir.get(x, sy_ir));
            }
        }
        acc = RgbImage { width: new_w, height: h, data };
        acc_ir = GrayImage { width: acc_ir.width + col_ir.width.saturating_sub(dx), height: h, data: ir };
        seams.push(Seam { dx, dy, score, gain });
    }
    (acc, acc_ir, seams)
}

/// Crop helper for callers that need equal heights.
pub fn crop_rgb_rows(img: &RgbImage, h: usize) -> RgbImage {
    img.crop_rows(h)
}

// ---------------------------------------------------------------------------------------------
// IR ↔ visible residual offset (§4.4 layer 2).

fn gaussian_kernel(sigma: f64) -> Vec<f64> {
    // OpenCV getGaussianKernel for float images: ksize = round(sigma*8+1) | 1.
    let ksize = ((sigma * 8.0 + 1.0).round() as usize) | 1;
    let half = (ksize / 2) as f64;
    let mut k: Vec<f64> = (0..ksize).map(|i| (-((i as f64 - half).powi(2)) / (2.0 * sigma * sigma)).exp()).collect();
    let s: f64 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

fn reflect101(i: i64, n: usize) -> usize {
    let n = n as i64;
    if n == 1 {
        return 0;
    }
    let period = 2 * (n - 1);
    let mut m = i.rem_euclid(period);
    if m >= n {
        m = period - m;
    }
    m as usize
}

fn blur(a: &[f64], w: usize, h: usize, sigma: f64) -> Vec<f64> {
    let k = gaussian_kernel(sigma);
    let r = (k.len() / 2) as i64;
    let mut tmp = vec![0.0; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0.0;
            for (j, kv) in k.iter().enumerate() {
                s += kv * a[y * w + reflect101(x as i64 + j as i64 - r, w)];
            }
            tmp[y * w + x] = s;
        }
    }
    let mut out = vec![0.0; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0.0;
            for (j, kv) in k.iter().enumerate() {
                s += kv * tmp[reflect101(y as i64 + j as i64 - r, h) * w + x];
            }
            out[y * w + x] = s;
        }
    }
    out
}

fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let pos = q / 100.0 * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    s[lo] + (s[hi] - s[lo]) * (pos - lo as f64)
}

/// Estimate the small IR→visible translation `(dx, dy)` from dark dust defects in up to six
/// 512² patches. `(0, 0)` when ambiguous; the
/// planes themselves are never shifted or resampled.
pub fn transmission_ir_offset(rgb: &RgbImage, ir: &GrayImage, radius: usize) -> (i32, i32) {
    if rgb.width != ir.width || rgb.height != ir.height {
        return (0, 0);
    }
    let (height, width) = (rgb.height, rgb.width);
    let (side_y, side_x) = (height.min(512), width.min(512));
    if side_y.min(side_x) <= 2 * radius + 16 {
        return (0, 0);
    }
    let mut origins: Vec<(usize, usize)> = Vec::new();
    for fy in [1.0 / 6.0, 0.5, 5.0 / 6.0] {
        for fx in [0.25, 0.75] {
            let oy = (height as f64 * fy - side_y as f64 / 2.0).round_ties_even().clamp(0.0, (height - side_y) as f64) as usize;
            let ox = (width as f64 * fx - side_x as f64 / 2.0).round_ties_even().clamp(0.0, (width - side_x) as f64) as usize;
            if !origins.contains(&(oy, ox)) {
                origins.push((oy, ox));
            }
        }
    }
    origins.sort_unstable();
    let mut votes: Vec<((i32, i32), usize)> = Vec::new();
    for &(y0, x0) in &origins {
        let mut a = vec![0.0; side_x * side_y];
        let mut b = vec![0.0; side_x * side_y];
        for y in 0..side_y {
            for x in 0..side_x {
                let (sx, sy) = (x0 + x, y0 + y);
                a[y * side_x + x] = (f64::from(rgb.get(sx, sy, 0)) + f64::from(rgb.get(sx, sy, 1)) + f64::from(rgb.get(sx, sy, 2))) / 3.0;
                b[y * side_x + x] = f64::from(ir.get(sx, sy));
            }
        }
        let ab = blur(&a, side_x, side_y, 3.0);
        let bb = blur(&b, side_x, side_y, 3.0);
        let ah: Vec<f64> = ab.iter().zip(&a).map(|(bl, v)| (bl - v).max(0.0)).collect();
        let mut bh: Vec<f64> = bb.iter().zip(&b).map(|(bl, v)| (bl - v).max(0.0)).collect();
        let p99 = percentile(&bh, 99.0);
        bh.iter_mut().for_each(|v| {
            if *v < p99 {
                *v = 0.0
            }
        });
        let (tw, th) = (side_x - 2 * radius, side_y - 2 * radius);
        let mut tmpl: Vec<(usize, usize, f64)> = Vec::new();
        for y in 0..th {
            for x in 0..tw {
                let v = bh[(y + radius) * side_x + x + radius];
                if v != 0.0 {
                    tmpl.push((x, y, v));
                }
            }
        }
        if tmpl.len() < 8 || !ah.iter().any(|&v| v != 0.0) {
            continue;
        }
        let tnorm = tmpl.iter().map(|t| t.2 * t.2).sum::<f64>();
        // Integral image of ah² for the CCORR_NORMED denominator.
        let iw = side_x + 1;
        let mut integ = vec![0.0; iw * (side_y + 1)];
        for y in 0..side_y {
            let mut row = 0.0;
            for x in 0..side_x {
                row += ah[y * side_x + x] * ah[y * side_x + x];
                integ[(y + 1) * iw + x + 1] = integ[y * iw + x + 1] + row;
            }
        }
        let n = 2 * radius + 1;
        let mut scores = vec![0.0; n * n];
        let mut best = (f64::MIN, 0usize, 0usize);
        for sy in 0..n {
            for sx in 0..n {
                let mut num = 0.0;
                for &(x, y, v) in &tmpl {
                    num += v * ah[(y + sy) * side_x + x + sx];
                }
                let sum2 = integ[(sy + th) * iw + sx + tw] - integ[sy * iw + sx + tw] - integ[(sy + th) * iw + sx] + integ[sy * iw + sx];
                let den = (tnorm * sum2.max(0.0)).sqrt();
                let s = if den > 0.0 { num / den } else { 0.0 };
                scores[sy * n + sx] = s;
                if s > best.0 {
                    best = (s, sx, sy);
                }
            }
        }
        let (score, bx, by) = best;
        let (dx, dy) = (bx as i32 - radius as i32, by as i32 - radius as i32);
        if score < 0.2 || dx.abs().max(dy.abs()) >= radius as i32 {
            continue;
        }
        let vote = if (dx, dy) != (0, 0) && score < scores[radius * n + radius] + 0.03 { (0, 0) } else { (dx, dy) };
        match votes.iter_mut().find(|(v, _)| *v == vote) {
            Some((_, c)) => *c += 1,
            None => votes.push((vote, 1)),
        }
    }
    let mut top: Option<((i32, i32), usize)> = None;
    for &(v, c) in &votes {
        if top.is_none_or(|(_, tc)| c > tc) {
            top = Some((v, c));
        }
    }
    let Some((offset, count)) = top else { return (0, 0) };
    let required = (if origins.len() == 1 { 1 } else { 2 }).max((0.6 * origins.len() as f64).ceil() as usize);
    if count >= required { offset } else { (0, 0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(samples: &[u16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn bil_assembly_matches_fixture() {
        // 8x3, samples 1..=72 in wire order: row 0 is R=1..8, G=9..16, B=17..24.
        let samples: Vec<u16> = (1..=72).collect();
        let img = assemble_bil(&le(&samples), 8, 3).unwrap();
        assert_eq!([img.get(0, 0, 0), img.get(0, 0, 1), img.get(0, 0, 2)], [1, 9, 17]);
        assert_eq!([img.get(7, 2, 0), img.get(7, 2, 1), img.get(7, 2, 2)], [56, 64, 72]);
        assert!(assemble_bil(&le(&samples[..70]), 8, 3).is_err());
        assert!(assemble_bil(&[0; 144], 8, 3).is_err());
        assert!(assemble_bil(&[1; 144], 0, 3).is_err());
    }

    #[test]
    fn ir_collapse_is_pixel_interleaved_rint_mean() {
        // pixel 0: (1,2,4) -> 7/3=2.33 -> 2 ; pixel 1: (1,1,3) -> 5/3=1.67 -> 2; pixel 2: (65535 x3)
        let img = collapse_ir(&le(&[1, 2, 4, 1, 1, 3, 65535, 65535, 65535, 0, 0, 1]), 2, 2).unwrap();
        assert_eq!(img.data, vec![2, 2, 65535, 0]);
        assert!(collapse_ir(&[0; 10], 2, 2).is_err());
    }

    fn flat(w: usize, h: usize, v: u16) -> RgbImage {
        RgbImage { width: w, height: h, data: vec![v; w * h * 3] }
    }

    #[test]
    fn level_match_flattens_a_12_percent_seam() {
        // Level-matching removes per-tile level steps.
        let left = flat(30, 40, 20000);
        let right = flat(30, 40, (20000.0f64 * 1.12) as u16);
        let m = level_match_columns(vec![left.clone(), right]);
        let (l, r) = (f64::from(m[0].get(29, 0, 0)), f64::from(m[1].get(0, 0, 0)));
        assert!((r - l).abs() / l < 0.02);
        assert_eq!((m[1].width, m[1].height), (30, 40));
        // Gain is clipped: a 2x step only corrects to 0.85.
        let m = level_match_columns(vec![left, flat(30, 40, 40000)]);
        assert_eq!(m[1].get(0, 0, 0), 34000);
    }

    #[test]
    fn concat_crops_to_min_height() {
        let c = concat_columns(&[flat(2, 3, 1), flat(3, 2, 2)]);
        assert_eq!((c.width, c.height), (5, 2));
        assert_eq!(c.get(1, 1, 0), 1);
        assert_eq!(c.get(2, 1, 0), 2);
    }

    fn textured(w: usize, h: usize, x0: usize) -> RgbImage {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let gx = x0 + x;
                let v = (20000.0 + 8000.0 * ((gx as f64) * 0.37).sin() * ((y as f64) * 0.21).cos() + 3000.0 * ((gx * 7 + y * 13) % 17) as f64 / 17.0)
                    as u16;
                data.extend_from_slice(&[v, v / 2, v / 3]);
            }
        }
        RgbImage { width: w, height: h, data }
    }

    #[test]
    fn overlapped_stitch_recovers_the_seam_exactly() {
        // Two tiles of one continuous texture: [0,140) and [80,200): overlap 60 px.
        let a = textured(140, 50, 0);
        let b = textured(120, 50, 80);
        let ir_a = GrayImage { width: 140, height: 50, data: (0..140 * 50).map(|i| (i % 140) as u16).collect() };
        let ir_b = GrayImage { width: 120, height: 50, data: (0..120 * 50).map(|i| (i % 120 + 80) as u16).collect() };
        let (rgb, ir, seams) = stitch_overlapped(vec![a, b], vec![ir_a, ir_b], 64);
        assert_eq!(seams.len(), 1);
        assert_eq!((seams[0].dx, seams[0].dy), (60, 0));
        assert_eq!((rgb.width, rgb.height), (200, 50));
        let whole = textured(200, 50, 0);
        assert_eq!(rgb.data, whole.data);
        assert_eq!(ir.width, 200);
        assert!((0..200).all(|x| ir.get(x, 3) == x as u16));
    }

    #[test]
    fn roll_matches_numpy() {
        // np.roll([0,1,2,3,4], 2) == [3,4,0,1,2]
        let r: Vec<usize> = (0..5).map(|i| roll_index(i, 2, 5)).collect();
        assert_eq!(r, vec![3, 4, 0, 1, 2]);
        let r: Vec<usize> = (0..5).map(|i| roll_index(i, -1, 5)).collect();
        assert_eq!(r, vec![1, 2, 3, 4, 0]);
    }

    fn dusty(w: usize, h: usize, ox: i32, oy: i32) -> (RgbImage, GrayImage) {
        let speck = |x: i32, y: i32| -> f64 {
            let mut d: f64 = 0.0;
            for cy in (y / 23 - 1)..=(y / 23 + 1) {
                for cx in (x / 23 - 1)..=(x / 23 + 1) {
                    let hsh = (cx.wrapping_mul(73856093) ^ cy.wrapping_mul(19349663)) as u32;
                    if !hsh.is_multiple_of(3) {
                        continue;
                    }
                    let (sx, sy) = (cx * 23 + (hsh % 17) as i32 + 3, cy * 23 + ((hsh / 17) % 17) as i32 + 3);
                    let r2 = f64::from((x - sx).pow(2) + (y - sy).pow(2));
                    d = d.max((1.0 - r2 / 6.0).max(0.0));
                }
            }
            d
        };
        let mut rgb = Vec::new();
        let mut ir = Vec::new();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let base = 30000.0 + 5000.0 * (f64::from(x) * 0.01).sin();
                let v = (base * (1.0 - 0.7 * speck(x, y))) as u16;
                rgb.extend_from_slice(&[v, v, v]);
                ir.push((50000.0 * (1.0 - 0.8 * speck(x + ox, y + oy))) as u16);
            }
        }
        (RgbImage { width: w, height: h, data: rgb }, GrayImage { width: w, height: h, data: ir })
    }

    #[test]
    fn registration_offset_is_measured_and_zero_when_aligned() {
        let (rgb, ir) = dusty(300, 300, -2, 2);
        assert_eq!(transmission_ir_offset(&rgb, &ir, 8), (-2, 2));
        let (rgb, ir) = dusty(300, 300, 0, 0);
        assert_eq!(transmission_ir_offset(&rgb, &ir, 8), (0, 0));
        // too small / mismatched -> (0,0)
        let (rgb, ir) = dusty(30, 30, -2, 2);
        assert_eq!(transmission_ir_offset(&rgb, &ir, 8), (0, 0));
        assert_eq!(transmission_ir_offset(&flat(40, 40, 1), &GrayImage { width: 1, height: 1, data: vec![0] }, 8), (0, 0));
    }

    proptest::proptest! {
        #[test]
        fn assembly_never_panics(raw in proptest::collection::vec(proptest::num::u8::ANY, 0..400), w in 0usize..20, h in 0usize..20) {
            let _ = assemble_bil(&raw, w, h);
            let _ = collapse_ir(&raw, w, h);
        }
    }
}
