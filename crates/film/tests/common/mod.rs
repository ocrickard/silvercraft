//! Deterministic synthetic C-41 negatives for the integration tests.
//!
//! Scene (linear, mid-grey 0.18) -> per-layer density via the
//! Cineon-style characteristic `D_c = gamma_c * log10(1 + E_c / E0)` (the exact
//! inverse of the scene-linear transform, so a perfect develop recovers `L ∝ E`)
//! -> transmittance `T_c = base_c * 10^-D_c` with seeded multiplicative noise.
#![allow(dead_code)]

use silvercraft_film::RgbImage;

/// xorshift64* PRNG.
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform(1e-12, 1.0);
        let u2 = self.uniform(0.0, 1.0);
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Orange C-41 base (R, G, B transmittance of clear film).
pub const ORANGE_BASE: [f64; 3] = [0.82, 0.55, 0.33];
/// Exposure at which density reaches gamma*log10(2) (the toe scale).
pub const E0: f64 = 0.18 / 16.0;

#[derive(Debug, Clone, Copy)]
pub struct Stock {
    pub base: [f64; 3],
    /// Layer gammas R, G, B (density per log10 exposure).
    pub gamma: [f64; 3],
    /// Multiplicative noise sigma on transmittance.
    pub noise: f64,
}

impl Stock {
    pub fn even(gamma: f64) -> Self {
        Self { base: ORANGE_BASE, gamma: [gamma; 3], noise: 0.003 }
    }

    pub fn density(&self, e: [f64; 3]) -> [f64; 3] {
        [0, 1, 2].map(|c| self.gamma[c] * (1.0 + e[c].max(0.0) / E0).log10())
    }

    pub fn transmittance(&self, e: [f64; 3], rng: &mut Rng) -> [f32; 3] {
        let d = self.density(e);
        [0, 1, 2].map(|c| (self.base[c] * 10f64.powf(-d[c]) * (1.0 + self.noise * rng.normal())) as f32)
    }
}

/// Named patches of the synthetic scene (scene-linear RGB).
pub const PATCHES: [(&str, [f64; 3]); 9] = [
    ("grey05", [0.05, 0.05, 0.05]),
    ("grey18", [0.18, 0.18, 0.18]),
    ("grey36", [0.36, 0.36, 0.36]),
    ("grey72", [0.72, 0.72, 0.72]),
    ("red", [0.40, 0.06, 0.05]),
    ("green", [0.08, 0.30, 0.06]),
    ("blue", [0.05, 0.08, 0.35]),
    ("skin", [0.45, 0.30, 0.22]),
    ("sky", [0.20, 0.30, 0.55]),
];

/// The scene layout: `width x height`; left half a horizontal grey ramp from
/// -4 EV to +4 EV around 0.18 (with `exposure` EV applied to everything),
/// right half a 3x3 grid of [`PATCHES`].
pub fn scene(width: usize, height: usize, exposure_ev: f64) -> Vec<[f64; 3]> {
    let gain = 2f64.powf(exposure_ev);
    let half = width / 2;
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let rgb = if x < half {
                let ev = -4.0 + 8.0 * x as f64 / (half.max(2) - 1) as f64;
                [0.18 * 2f64.powf(ev); 3]
            } else {
                let px = ((x - half) * 3 / (width - half)).min(2);
                let py = (y * 3 / height).min(2);
                PATCHES[py * 3 + px].1
            };
            out.push(rgb.map(|v| v * gain));
        }
    }
    out
}

/// Centre of patch `index` in the scene layout.
pub fn patch_center(width: usize, height: usize, index: usize) -> (usize, usize) {
    let half = width / 2;
    let cell_w = (width - half) / 3;
    let cell_h = height / 3;
    (half + cell_w * (index % 3) + cell_w / 2, cell_h * (index / 3) + cell_h / 2)
}

/// Mean of a `(2r+1)^2` window.
pub fn window_mean(image: &RgbImage, cx: usize, cy: usize, r: usize) -> [f64; 3] {
    let mut sum = [0.0; 3];
    let mut n = 0.0;
    for y in cy - r..=cy + r {
        for x in cx - r..=cx + r {
            let p = image.pixel(x, y).unwrap();
            for c in 0..3 {
                sum[c] += f64::from(p[c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

/// A synthetic negative of [`scene`].
pub fn negative(width: usize, height: usize, stock: &Stock, exposure_ev: f64, seed: u64) -> RgbImage {
    let mut rng = Rng(seed);
    let data: Vec<f32> = scene(width, height, exposure_ev).into_iter().flat_map(|e| stock.transmittance(e, &mut rng)).collect();
    RgbImage::new(width, height, data).unwrap()
}

/// A capture strip: `rebate` rows of clear base above and below the frame
/// (for gap-base sampling). Returns (capture, y0, y1).
pub fn capture_with_rebate(frame: &RgbImage, stock: &Stock, rebate: usize, seed: u64) -> (RgbImage, usize, usize) {
    let mut rng = Rng(seed);
    let w = frame.width();
    let mut data = Vec::new();
    let mut rebate_rows = |data: &mut Vec<f32>| {
        for _ in 0..rebate * w {
            data.extend(stock.base.map(|b| (b * (1.0 + stock.noise * rng.normal())) as f32));
        }
    };
    rebate_rows(&mut data);
    data.extend_from_slice(frame.data());
    rebate_rows(&mut data);
    (RgbImage::new(w, frame.height() + 2 * rebate, data).unwrap(), rebate, rebate + frame.height())
}
