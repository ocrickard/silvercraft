//! Synthetic beds, camera captures and live-view sequences.

#![allow(dead_code)]

/// Deterministic hash noise in [-1, 1].
pub fn noise(x: usize, y: usize, seed: u64) -> f32 {
    let mut h =
        (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ seed.wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    (h % 20_001) as f32 / 10_000.0 - 1.0
}

/// Smooth photographic-ish texture in [0, 1] at (u, v) mm, varied by `seed`.
pub fn texture(u: f64, v: f64, seed: f64) -> f64 {
    let a = (u * 0.31 + seed).sin() * (v * 0.17 + 2.0 * seed).cos();
    let b = (u * 0.07 + v * 0.11 + 3.0 * seed).sin();
    let c = ((u - 5.0 * seed.cos()).powi(2) + (v - 8.0 * seed.sin()).powi(2)).sqrt() * 0.4;
    (0.5 + 0.25 * a + 0.2 * b + 0.15 * c.sin()).clamp(0.0, 1.0)
}

/// C-41 base (orange mask) transmittance.
pub const BASE: [f32; 3] = [0.45, 0.28, 0.16];

/// What a frame looks like.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Content {
    /// Ordinary textured image (density 0.3..1.4 above base).
    Image,
    /// Image whose first `mm` near the leading gap is thin, uniform sky (density 0.03): a
    /// near-gap that is as bright as base for a negative.
    SkyAtStart(f64),
    /// Image with a band of exactly base density, `mm` wide, across the middle: a false gap.
    FalseGap(f64),
    /// Unexposed: all base.
    Blank,
    /// Image with a band `mm` wide of very dense content (density 2.8, opaque as the holder)
    /// across the whole width: splits the opening into two pieces.
    Dense(f64),
}

/// One strip on a synthetic bed.
#[derive(Clone, Debug)]
pub struct Strip {
    /// Strip centre across the bed (mm, x for vertical strips).
    pub centre_mm: f64,
    /// Skew (degrees, clockwise on screen positive).
    pub angle_deg: f64,
    pub frames: Vec<Content>,
    /// Gaps (by index k = gap before frame k) painted over with content (invisible gaps).
    pub hidden_gaps: Vec<usize>,
}

/// A synthetic 35 mm bed: holder (opaque) with vertical openings, film strips cut in their gaps,
/// bare light where the opening runs past the film.
#[derive(Clone, Debug)]
pub struct Bed {
    pub dpi: f64,
    pub width_mm: f64,
    pub height_mm: f64,
    pub opening_top_mm: f64,
    pub opening_len_mm: f64,
    pub opening_across_mm: f64,
    pub film_top_mm: f64,
    pub pitch_mm: f64,
    pub frame_mm: f64,
    pub across_mm: f64,
    pub positive: bool,
    pub horizontal: bool,
    pub strips: Vec<Strip>,
}

impl Bed {
    pub fn strip35(strips: Vec<Strip>) -> Self {
        Self {
            dpi: 150.0,
            width_mm: 150.0,
            height_mm: 262.0,
            opening_top_mm: 12.0,
            opening_len_mm: 238.0,
            opening_across_mm: 25.0,
            film_top_mm: 5.0,
            pitch_mm: 38.0,
            frame_mm: 36.0,
            across_mm: 25.0,
            positive: false,
            horizontal: false,
            strips,
        }
    }

    fn ppm(&self) -> f64 {
        self.dpi / 25.4
    }

    /// Image size in pixels.
    pub fn size(&self) -> (usize, usize) {
        let (w, h) = ((self.width_mm * self.ppm()).round() as usize, (self.height_mm * self.ppm()).round() as usize);
        if self.horizontal { (h, w) } else { (w, h) }
    }

    /// Strip pivot (mm): centre of the opening.
    fn pivot(&self, s: &Strip) -> (f64, f64) {
        (s.centre_mm, self.opening_top_mm + self.opening_len_mm / 2.0)
    }

    /// (t along from the opening top, u across from the strip centre) in mm for a bed point (mm).
    fn local(&self, s: &Strip, x: f64, y: f64) -> (f64, f64) {
        let (cx, cy) = self.pivot(s);
        let (sn, cs) = s.angle_deg.to_radians().sin_cos();
        let (dx, dy) = (x - cx, y - cy);
        // The strip axis is rotated by -angle for a vertical strip whose "width side" is rotated
        // by +angle: axis a = (-sin, cos), normal n = (cos, sin).
        let u = dx * cs + dy * sn;
        let t = -dx * sn + dy * cs;
        (t + self.opening_len_mm / 2.0, u)
    }

    fn bed_point(&self, s: &Strip, t: f64, u: f64) -> (f64, f64) {
        let (cx, cy) = self.pivot(s);
        let (sn, cs) = s.angle_deg.to_radians().sin_cos();
        let tt = t - self.opening_len_mm / 2.0;
        (cx + u * cs - tt * sn, cy + u * sn + tt * cs)
    }

    /// Truth: per strip, the frame centres (px, image coordinates) of the non-blank-end frames.
    pub fn frame_centres_px(&self, strip: usize) -> Vec<(f64, f64)> {
        let s = &self.strips[strip];
        let k = self.ppm();
        (0..s.frames.len())
            .map(|i| {
                let t = self.film_top_mm + (i as f64 + 0.5) * self.pitch_mm;
                let (x, y) = self.bed_point(s, t, 0.0);
                if self.horizontal { (y * k, x * k) } else { (x * k, y * k) }
            })
            .collect()
    }

    fn sample(&self, xm: f64, ym: f64, px: usize, py: usize) -> [f32; 3] {
        let n = 1.0 + 0.004 * noise(px, py, 7);
        for (si, s) in self.strips.iter().enumerate() {
            let (t, u) = self.local(s, xm, ym);
            if u.abs() > self.opening_across_mm / 2.0 || t < 0.0 || t > self.opening_len_mm {
                continue;
            }
            let film_len = s.frames.len() as f64 * self.pitch_mm;
            let ft = t - self.film_top_mm;
            if ft < 0.0 || ft >= film_len {
                return [n, n, n]; // bare light
            }
            let k = (ft / self.pitch_mm).floor() as usize;
            let local = ft - k as f64 * self.pitch_mm; // 0..pitch, gap halves at both ends
            let half_gap = (self.pitch_mm - self.frame_mm) / 2.0;
            let in_gap = local < half_gap || local > self.pitch_mm - half_gap;
            let hidden = (local < half_gap && s.hidden_gaps.contains(&k)) || (local > self.pitch_mm - half_gap && s.hidden_gaps.contains(&(k + 1)));
            let fl = local - half_gap; // position inside the frame
            let density = if in_gap && !hidden {
                0.0
            } else {
                match s.frames.get(k).copied().unwrap_or(Content::Image) {
                    Content::Blank => 0.0,
                    Content::SkyAtStart(mm) if fl < mm => 0.03,
                    Content::FalseGap(mm) if (fl - self.frame_mm / 2.0).abs() < mm / 2.0 => 0.0,
                    Content::Dense(mm) if (fl - self.frame_mm / 2.0).abs() < mm / 2.0 => 2.8,
                    _ => 0.3 + 1.1 * texture(u, fl, (si * 17 + k * 5) as f64 + 0.3),
                }
            };
            if self.positive {
                // Slide: unexposed is black; "density" here is image brightness inverted.
                let tr = if in_gap && !hidden { 0.003 } else { (0.85 * 10f64.powf(-density)) as f32 };
                return [tr * n, tr * 0.95 * n, tr * 0.9 * n];
            }
            let att = 10f64.powf(-density) as f32;
            return [BASE[0] * att * n, BASE[1] * att * n * 0.97, BASE[2] * att * n * 0.94];
        }
        let hold = 0.0008 * (1.0 + 0.5 * noise(px, py, 3));
        [hold, hold, hold]
    }

    /// Render as interleaved linear RGB.
    pub fn render(&self) -> (usize, usize, Vec<f32>) {
        let (w, h) = self.size();
        let k = 1.0 / self.ppm();
        let mut out = vec![0f32; w * h * 3];
        for py in 0..h {
            for px in 0..w {
                let (bx, by) = if self.horizontal { (py, px) } else { (px, py) };
                let v = self.sample((bx as f64 + 0.5) * k, (by as f64 + 0.5) * k, px, py);
                out[(py * w + px) * 3..][..3].copy_from_slice(&v);
            }
        }
        (w, h, out)
    }
}

/// A synthetic camera capture of one 35 mm frame over a light table: dark gate (optional),
/// rebate with sprocket holes, neighbouring frames cut by the view, rotated by `angle_deg`.
#[derive(Clone, Debug)]
pub struct Capture {
    pub width: usize,
    pub height: usize,
    /// Pixels per mm of film.
    pub scale: f64,
    pub angle_deg: f64,
    /// Frame centre in pixels.
    pub centre: (f64, f64),
    /// Dark gate around the film (else bare light outside the film).
    pub gate: bool,
    pub seed: f64,
    /// Thin negative: content density low (near base).
    pub thin: bool,
    /// The first `mm` of the frame (from its left edge) is exactly base: an invisible edge.
    pub base_left_mm: f64,
    /// A dense 3:2 block with crisp edges inside the frame (a dark subject on a pale ground).
    pub inner_block: bool,
    /// A border this wide (mm) along half of each frame side is exactly base: the frame edges
    /// are visible along only half their length.
    pub base_border_mm: f64,
    /// A very thin negative: the image is a pale wash, a little denser than base in blue only
    /// (density 0.04), with a textured subject in the middle. Its frame edges are faint in
    /// linear light (the orange mask makes blue the darkest channel) but clear in density.
    pub faint: bool,
    /// Unexposed frame: all base.
    pub blank: bool,
}

impl Capture {
    pub fn new(width: usize, height: usize, angle_deg: f64) -> Self {
        Self {
            width,
            height,
            scale: width as f64 / 60.0,
            angle_deg,
            centre: (width as f64 / 2.0, height as f64 / 2.0),
            gate: true,
            seed: 1.0,
            thin: false,
            base_left_mm: 0.0,
            inner_block: false,
            base_border_mm: 0.0,
            faint: false,
            blank: false,
        }
    }

    /// The truth frame (36 × 24 mm) as (cx, cy, w, h, angle) in pixels.
    pub fn truth(&self) -> (f64, f64, f64, f64, f64) {
        (self.centre.0, self.centre.1, 36.0 * self.scale, 24.0 * self.scale, self.angle_deg)
    }

    fn sample(&self, x: f64, y: f64, px: usize, py: usize) -> [f32; 3] {
        let (sn, cs) = self.angle_deg.to_radians().sin_cos();
        let (dx, dy) = (x - self.centre.0, y - self.centre.1);
        // Film coordinates (mm) with the frame upright.
        let u = (dx * cs + dy * sn) / self.scale;
        let v = (-dx * sn + dy * cs) / self.scale;
        let n = 1.0 + 0.004 * noise(px, py, 11);
        // Film is 35 mm wide; the gate exposes 31 mm of it.
        if v.abs() > 17.5 || (self.gate && v.abs() > 15.5) {
            return if self.gate { [0.01 * n; 3] } else { [n, n, n] };
        }
        if self.gate && u.abs() > 22.0 {
            return [0.01 * n; 3];
        }
        // Sprocket holes: 2 mm × 2.8 mm at 4.75 mm pitch, centred 2.0 mm inside each film edge.
        let rebate_hole = (v.abs() - 15.5).abs() < 1.0 && ((u + 100.0).rem_euclid(4.75) - 2.4).abs() < 1.4;
        if rebate_hole {
            return [n, n, n];
        }
        let in_frame_v = v.abs() < 12.0;
        let k = ((u + 19.0) / 38.0).floor(); // frame index, 0 = this frame
        let local = u + 19.0 - k * 38.0; // 0..38, frame at 1..37
        let base_left = (1.0..1.0 + self.base_left_mm).contains(&local);
        let base_border = ((local < 1.0 + self.base_border_mm || local > 37.0 - self.base_border_mm) && v < 0.0)
            || (v.abs() > 12.0 - self.base_border_mm && local < 19.0);
        let density = if k == 0.0 && in_frame_v && (base_left || base_border || self.blank) {
            [0.0; 3]
        } else if k == 0.0 && self.inner_block && (local - 19.0).abs() < 10.5 && v.abs() < 7.0 {
            [0.7 + 0.9 * texture(local * 3.0, v * 3.0, self.seed); 3]
        } else if in_frame_v && (1.0..37.0).contains(&local) {
            let t = texture(local, v, self.seed + 3.0 * k);
            if self.faint {
                if (local - 19.0).abs() < 8.0 && v.abs() < 6.0 {
                    [0.1 + 0.5 * t, 0.1 + 0.5 * t, 0.15 + 0.5 * t]
                } else {
                    [0.0, 0.004, 0.04 + 0.01 * t]
                }
            } else if self.thin {
                [0.02 + 0.35 * t * t; 3]
            } else {
                [0.3 + 1.1 * t; 3]
            }
        } else {
            [0.0; 3]
        };
        let att = |c: usize| 10f64.powf(-density[c]) as f32;
        [BASE[0] * att(0) * n, BASE[1] * att(1) * n, BASE[2] * att(2) * n]
    }

    pub fn render(&self) -> Vec<f32> {
        let mut out = vec![0f32; self.width * self.height * 3];
        for py in 0..self.height {
            for px in 0..self.width {
                let v = self.sample(px as f64 + 0.5, py as f64 + 0.5, px, py);
                out[(py * self.width + px) * 3..][..3].copy_from_slice(&v);
            }
        }
        out
    }

    /// Render as 8-bit, gamma-encoded like a live-view JPEG.
    pub fn render_u8(&self) -> Vec<u8> {
        self.render().iter().map(|v| (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8).collect()
    }
}
