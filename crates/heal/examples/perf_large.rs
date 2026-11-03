//! Timing on a synthetic 4800 dpi frame (6000×9000) with a variable number of defects.
//!
//! Usage: perf_large [defects ...]   (default: 500 2000 8000)
use std::time::Instant;

use silvercraft_film::RgbImage;
use silvercraft_heal::{HealConfig, IrPlane, heal};

fn hash(k: u64) -> u64 {
    let mut z = k.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(k: u64) -> f32 {
    (hash(k) >> 40) as f32 / (1u64 << 24) as f32
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let counts: Vec<usize> = std::env::args().skip(1).map(|a| a.parse()).collect::<Result<_, _>>()?;
    let counts = if counts.is_empty() { vec![500, 2000, 8000] } else { counts };
    let (w, h) = (6000usize, 9000usize);
    let scene = |x: usize, y: usize| 0.3 + 0.15 * ((x as f32 / 400.0).sin() * (y as f32 / 530.0).cos()) + 0.1 * (x as f32 / w as f32);
    for &n in &counts {
        let mut occ = vec![0.0f32; w * h];
        for k in 0..n as u64 {
            let (cx, cy) = (unit(3 * k) * w as f32, unit(3 * k + 1) * h as f32);
            if k % 25 == 0 {
                // a hair: 600 px sinusoid, ~10 px wide
                for t in 0..600 {
                    let x = cx + t as f32;
                    let y = cy + 40.0 * (t as f32 / 70.0).sin();
                    for dy in -5..=5 {
                        for dx in -1..=1 {
                            let (px, py) = (x as isize + dx, y as isize + dy);
                            if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                                occ[py as usize * w + px as usize] = 0.9;
                            }
                        }
                    }
                }
            } else {
                let r = 3.0 + 12.0 * unit(3 * k + 2);
                let ri = r.ceil() as isize;
                for dy in -ri..=ri {
                    for dx in -ri..=ri {
                        let (px, py) = (cx as isize + dx, cy as isize + dy);
                        if (dx * dx + dy * dy) as f32 <= r * r && px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                            occ[py as usize * w + px as usize] = 0.95;
                        }
                    }
                }
            }
        }
        let mut rgb = vec![0.0f32; w * h * 3];
        let mut ir = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let g = 1.0 + 0.05 * (unit(i as u64 ^ 0xABCD) - 0.5);
                let t = scene(x, y) * g * (1.0 - occ[i]);
                rgb[3 * i..3 * i + 3].copy_from_slice(&[t * 1.1, t, t * 0.85]);
                ir[i] = 0.7 * (scene(x, y) / 0.3).powf(0.2) * (1.0 - occ[i]);
            }
        }
        let rgb = RgbImage::new(w, h, rgb)?;
        let ir = IrPlane::new(w, h, ir)?;
        let cfg = HealConfig { dpi: Some(4800.0), ..HealConfig::default() };
        let t = Instant::now();
        let res = heal(&rgb, Some(&ir), &cfg)?;
        println!(
            "{n:>6} defects requested: {:>6} found, {:.2}% coverage, small/medium/large {}/{}/{} in {:.2} s",
            res.stats.defects,
            res.stats.coverage_pct,
            res.stats.small,
            res.stats.medium,
            res.stats.large,
            t.elapsed().as_secs_f64()
        );
        for warning in &res.warnings {
            println!("  warning: {warning}");
        }
    }
    Ok(())
}
