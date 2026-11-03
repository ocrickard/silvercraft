//! Run bed detection on a prescan TIFF and draw the boxes.
//!
//! `cargo run --release -p silvercraft-detect --example bed_prescan -- <prescan.tiff> <dpi> <out.ppm> [holder]`
//! where holder is `auto` (default), `35`, `35-half`, `35-pano`, `35-free`, `120`, `sheet`, `slides`.
//! The PPM is the prescan downsampled 2× (gamma 1/2.2) with strips in blue, frames in green
//! (yellow when partial or blank, red under 0.5 confidence).

use std::io::Write;

use silvercraft_detect::{BedConfig, Format35, Format120, Holder, Rgb, detect_bed};

fn read_tiff(path: &str) -> Result<(usize, usize, Vec<f32>), Box<dyn std::error::Error>> {
    let mut dec = tiff::decoder::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?))?;
    let (w, h) = dec.dimensions()?;
    let ch = match dec.colortype()? {
        tiff::ColorType::RGB(_) => 3,
        tiff::ColorType::RGBA(_) => 4,
        other => return Err(format!("unsupported colour type {other:?}").into()),
    };
    let data: Vec<f32> = match dec.read_image()? {
        tiff::decoder::DecodingResult::U16(v) => v.iter().map(|&x| f32::from(x) / 65535.0).collect(),
        tiff::decoder::DecodingResult::U8(v) => v.iter().map(|&x| f32::from(x) / 255.0).collect(),
        tiff::decoder::DecodingResult::F32(v) => v,
        _ => return Err("unsupported sample format".into()),
    };
    let rgb = data.chunks_exact(ch).flat_map(|p| [p[0], p[1], p[2]]).collect();
    Ok((w as usize, h as usize, rgb))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return Err("usage: bed_prescan <prescan.tiff> <dpi> <out.ppm> [holder]".into());
    }
    let (w, h, rgb) = read_tiff(&args[1])?;
    let dpi: f64 = args[2].parse()?;
    let holder = match args.get(4).map(String::as_str).unwrap_or("auto") {
        "35" => Holder::Strip35 { format: Format35::Full },
        "35-half" => Holder::Strip35 { format: Format35::Half },
        "35-pano" => Holder::Strip35 { format: Format35::Panoramic },
        "35-free" => Holder::Strip35 { format: Format35::Free },
        "120" => Holder::Medium120 { format: Format120::Free },
        "sheet" => Holder::Sheet4x5,
        "slides" => Holder::Slides35,
        _ => Holder::Auto,
    };
    let cfg = BedConfig { holder, ..Default::default() };
    let t = std::time::Instant::now();
    let det = detect_bed(&Rgb::new(w, h, &rgb)?, dpi, &cfg)?;
    eprintln!("{w}x{h} @ {dpi} dpi: {} strips, {} frames in {:?}; light {:.3}", det.strips.len(), det.frames.len(), t.elapsed(), det.light_level);
    for s in &det.strips {
        eprintln!(
            "strip {} {:?} {:?} angle {:.2}° across {:.1} mm along {:.1} mm pitch {:?} mm comb {:?} fallback {}",
            s.index,
            s.kind,
            s.bounds,
            s.angle_deg,
            s.across_mm,
            s.along_mm,
            s.pitch_mm.map(|p| (p * 10.0).round() / 10.0),
            s.comb_score,
            s.fallback
        );
    }
    for f in &det.frames {
        eprintln!(
            "  {} {:?} {:.1}x{:.1} mm conf {:.2}{}{}",
            f.label,
            f.bounds,
            f.mm.width,
            f.mm.height,
            f.confidence,
            if f.partial { " partial" } else { "" },
            if f.blank { " blank" } else { "" }
        );
    }
    for warn in &det.warnings {
        eprintln!("warning: {warn}");
    }
    // Draw.
    let (ow, oh) = (w / 2, h / 2);
    let peak = det.light_level.max(1e-6);
    let mut img = vec![0u8; ow * oh * 3];
    for y in 0..oh {
        for x in 0..ow {
            for c in 0..3 {
                let v = rgb[((2 * y) * w + 2 * x) * 3 + c] / peak;
                img[(y * ow + x) * 3 + c] = (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            }
        }
    }
    let mut rect = |r: silvercraft_detect::PxRect, col: [u8; 3], inset: u32| {
        let (x0, y0) = ((r.x / 2 + inset) as usize, (r.y / 2 + inset) as usize);
        let (x1, y1) = (((r.x + r.width) / 2).saturating_sub(inset + 1) as usize, ((r.y + r.height) / 2).saturating_sub(inset + 1) as usize);
        for t in 0..2 {
            for x in x0..=x1.min(ow - 1) {
                for y in [y0 + t, y1.saturating_sub(t)] {
                    if y < oh {
                        img[(y * ow + x) * 3..][..3].copy_from_slice(&col);
                    }
                }
            }
            for y in y0..=y1.min(oh - 1) {
                for x in [x0 + t, x1.saturating_sub(t)] {
                    if x < ow {
                        img[(y * ow + x) * 3..][..3].copy_from_slice(&col);
                    }
                }
            }
        }
    };
    for s in &det.strips {
        rect(s.bounds, [40, 120, 255], 0);
    }
    for f in &det.frames {
        let col = if f.confidence < 0.5 {
            [255, 40, 40]
        } else if f.partial || f.blank {
            [255, 220, 0]
        } else {
            [0, 255, 80]
        };
        rect(f.bounds, col, 3);
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[3])?);
    write!(out, "P6\n{ow} {oh}\n255\n")?;
    out.write_all(&img)?;
    Ok(())
}
