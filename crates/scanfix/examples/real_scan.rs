//! Run the scan health report and the artifact detectors on a real 16-bit scanner TIFF.
//!
//! ```text
//! cargo run --release -p silvercraft-scanfix --example real_scan -- RGB.tiff [IR.tiff] [--rows] [--toml 'CONFIG'] [--crop X,Y,W,H] [--profile X0,X1] [--json]
//! ```
//!
//! Prints what was found and what a repair would change; it never writes an image.

use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::time::Instant;

use silvercraft_imgproc::Plane;
use silvercraft_scanfix::{ScanImage, ScanfixConfig, StreakAxis, analyze, repair};
use tiff::decoder::{Decoder, DecodingResult};

/// Read a TIFF as linear 0..1 planes (one per sample).
fn read_planes(path: &str) -> Result<Vec<Plane>, Box<dyn Error>> {
    let mut dec = Decoder::new(BufReader::new(File::open(path)?))?;
    let (w, h) = dec.dimensions()?;
    let (w, h) = (w as usize, h as usize);
    let data: Vec<f32> = match dec.read_image()? {
        DecodingResult::U16(v) => v.into_iter().map(|x| f32::from(x) / 65535.0).collect(),
        DecodingResult::U8(v) => v.into_iter().map(|x| f32::from(x) / 255.0).collect(),
        DecodingResult::F32(v) => v,
        _ => return Err("unsupported sample format".into()),
    };
    let channels = data.len() / (w * h).max(1);
    Ok(silvercraft_imgproc::split_channels(w, h, channels, &data)?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files = Vec::new();
    let mut cfg = ScanfixConfig::default();
    let mut crop: Option<[usize; 4]> = None;
    let mut json = false;
    let mut profile: Option<(usize, usize)> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rows" => cfg.axis = StreakAxis::Rows,
            "--toml" => cfg = toml::from_str(it.next().ok_or("--toml 'TOML'")?)?,
            "--json" => json = true,
            "--profile" => {
                let v: Vec<usize> = it.next().ok_or("--profile X0,X1")?.split(',').map(str::parse).collect::<Result<_, _>>()?;
                let [x0, x1] = v[..] else { return Err("--profile takes X0,X1".into()) };
                profile = Some((x0, x1));
            }
            "--crop" => {
                let v: Vec<usize> = it.next().ok_or("--crop X,Y,W,H")?.split(',').map(str::parse).collect::<Result<_, _>>()?;
                let [x, y, w, h] = v[..] else { return Err("--crop takes X,Y,W,H".into()) };
                crop = Some([x, y, w, h]);
            }
            f => files.push(f.to_owned()),
        }
    }
    let rgb_path = files.first().ok_or("usage: real_scan RGB.tiff [IR.tiff] [--rows] [--crop X,Y,W,H] [--json]")?;
    let cut = |p: Plane| -> Result<Plane, Box<dyn Error>> {
        Ok(match crop {
            Some([x, y, w, h]) => p.crop(x, y, w, h)?,
            None => p,
        })
    };
    let mut planes = read_planes(rgb_path)?.into_iter();
    let (Some(r), Some(g), Some(b)) = (planes.next(), planes.next(), planes.next()) else {
        return Err("need an RGB TIFF".into());
    };
    let ir = match files.get(1) {
        Some(p) => read_planes(p)?.into_iter().next().map(cut).transpose()?,
        None => None,
    };
    let img = ScanImage::new([cut(r)?, cut(g)?, cut(b)?], ir)?;
    println!("{rgb_path}: {}×{} {}", img.width(), img.height(), if img.ir().is_some() { "+ IR" } else { "" });

    if let Some((x0, x1)) = profile {
        // Column medians (×10⁴) over eight bands of rows, per channel: shows a streak's profile.
        let bands = 8;
        let h = img.height();
        for (name, p) in ["red", "green", "blue"].iter().zip(img.rgb()) {
            println!("profile {name} columns {x0}..={x1} (mean in 16-bit codes per row band)");
            for band in 0..bands {
                let (y0, y1) = (band * h / bands, (band + 1) * h / bands);
                let row: Vec<String> = (x0..=x1.min(img.width() - 1))
                    .map(|x| {
                        let mut v: Vec<f32> = (y0..y1).map(|y| p.get(x, y)).collect();
                        v.sort_by(f32::total_cmp);
                        let mean = v.iter().sum::<f32>() / v.len() as f32;
                        format!("{:6.0}", 65535.0 * mean)
                    })
                    .collect();
                println!("  rows {y0:5}..{y1:5}: {}", row.join(""));
            }
            println!("  second difference v[x] - (v[x-1] + v[x+1]) / 2, mean in codes ×10:");
            for band in 0..bands {
                let (y0, y1) = (band * h / bands, (band + 1) * h / bands);
                let row: Vec<String> = (x0.max(1)..=x1.min(img.width() - 2))
                    .map(|x| {
                        let s: f32 = (y0..y1).map(|y| p.get(x, y) - 0.5 * (p.get(x - 1, y) + p.get(x + 1, y))).sum();
                        format!("{:6.0}", 655350.0 * s / (y1 - y0) as f32)
                    })
                    .collect();
                println!("  rows {y0:5}..{y1:5}: {}", row.join(""));
            }
        }
    }
    let t = Instant::now();
    let report = analyze(&img, &cfg)?;
    println!("analyze: {:.2} s", t.elapsed().as_secs_f32());
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    for c in &report.channels {
        println!(
            "  {:8} mean {:.4} black {:.5} white {:.4} clip-hi {:.3}% clip-lo {:.3}% noise {:.6} DR {:.1} st headroom {:.2} st",
            c.channel.name(),
            c.mean,
            c.black_level,
            c.white_level,
            100.0 * c.clipped_high,
            100.0 * c.clipped_low,
            c.noise_floor,
            c.dynamic_range_stops,
            c.headroom_stops
        );
    }
    for b in &report.banding {
        println!(
            "  banding {:8} score {:.3}% max {:.2}% banded {:?} strong {:?}",
            b.channel.name(),
            100.0 * b.rms,
            100.0 * b.max_deviation,
            b.banded.iter().take(12).map(|s| (s.index, s.gain)).collect::<Vec<_>>(),
            b.strong.iter().map(|s| (s.index, s.gain)).collect::<Vec<_>>()
        );
    }
    for d in &report.line_defects {
        println!("  line {} [{}..{}] score {:.2} repairs {:?}", d.describe(), d.start, d.end, d.score, d.repairs);
    }
    println!("warnings:");
    for w in &report.warnings {
        println!("  - {}", w.message);
    }

    let mut fixed = img.clone();
    let t = Instant::now();
    let rep = repair(&mut fixed, &cfg)?;
    let changed: usize = img
        .rgb()
        .iter()
        .zip(fixed.rgb())
        .chain(img.ir().into_iter().zip(fixed.ir()))
        .map(|(a, b)| a.data().iter().zip(b.data()).filter(|(x, y)| x.to_bits() != y.to_bits()).count())
        .sum();
    println!("repair: {:.2} s, {} values changed, repaired = {}", t.elapsed().as_secs_f32(), changed, rep.repaired);
    Ok(())
}
