//! Heal a scanner TIFF (with or without a separate IR TIFF) and write before/after previews
//! (inverted, stretched) plus the mask and attenuation map as PPM/PGM. `before_pos.pgm` and
//! `after_pos.pgm` are contrast-stretched grey positives (log density) for judging faint scratches.
//!
//! Usage: heal_scan <rgb.tiff> <ir.tiff | -> <out_dir> [dpi]
//! Extra `[heal]` keys come from the HEAL_TOML environment variable (e.g. `scratches = true`).
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use silvercraft_heal::{HealConfig, IrPlane, heal};

fn write_pnm(path: &Path, w: usize, h: usize, channels: usize, data: &[u8]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(f, "P{}\n{w} {h}\n255\n", if channels == 3 { 6 } else { 5 })?;
    f.write_all(data)
}

fn percentile(mut v: Vec<f32>, p: f32) -> f32 {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * p).round() as usize]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return Err("usage: heal_scan <rgb.tiff> <ir.tiff | -> <out_dir> [dpi]".into());
    }
    let out = Path::new(&args[3]);
    std::fs::create_dir_all(out)?;
    let cap = silvercraft_decode::decode_file(Path::new(&args[1]), &Default::default())?;
    let (w, h) = (cap.rgb.width(), cap.rgb.height());
    let ir = if args[2] == "-" {
        None
    } else {
        let ir = silvercraft_decode::tiff_scan::decode_ir(&std::fs::read(&args[2])?, w, h)?;
        Some(IrPlane::new(w, h, ir)?)
    };
    let base = HealConfig::from_toml_str(&std::env::var("HEAL_TOML").unwrap_or_default()).map_err(|d| d.to_string())?;
    let cfg = HealConfig { dpi: args.get(4).map(|s| s.parse()).transpose()?, ..base };

    let t = Instant::now();
    let res = heal(&cap.rgb, ir.as_ref(), &cfg)?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("{w}x{h} healed in {ms:.1} ms");
    println!("{:#?}", res.stats);
    for warning in &res.warnings {
        println!("warning: {warning}");
    }
    let mut kinds = std::collections::BTreeMap::new();
    for d in &res.defects {
        *kinds.entry(format!("{:?}/{:?}/deep={}", d.kind, d.tier, d.deep)).or_insert(0usize) += 1;
    }
    println!("{kinds:#?}");
    let probe: Option<Vec<usize>> = std::env::var("HEAL_PROBE").ok().map(|v| v.split(',').filter_map(|t| t.parse().ok()).collect());
    for d in res.defects.iter().filter(|d| {
        d.tier == silvercraft_heal::Tier::Large
            || probe.as_ref().is_some_and(|p| p.len() == 2 && d.bbox[0] <= p[0] && p[0] < d.bbox[2] && d.bbox[1] <= p[1] && p[1] < d.bbox[3])
    }) {
        println!(
            "{:?} {:?} bbox {:?} area {} core {} length {:.0} min_att {:.2}",
            d.tier, d.kind, d.bbox, d.area, d.core_area, d.length, d.min_attenuation
        );
    }
    for l in &res.lines {
        let (a, b) = (l.knots.first().map_or(0.0, |k| k[1]), l.knots.last().map_or(0.0, |k| k[1]));
        println!(
            "line {:?} along {}..{} across {a:.1}->{b:.1} width {:.1} contrast {:+.4} score {:.1} {:?} px {}",
            l.axis, l.start, l.end, l.width, l.contrast, l.score, l.repair, l.pixels
        );
    }

    // preview: invert the negative with a per-channel stretch, gamma 2.2
    let stretch = |img: &[f32]| -> Vec<u8> {
        let mut lo = [0.0f32; 3];
        let mut hi = [1.0f32; 3];
        for c in 0..3 {
            let v: Vec<f32> = cap.rgb.data().iter().skip(c).step_by(3).copied().collect();
            lo[c] = percentile(v.clone(), 0.05);
            hi[c] = percentile(v, 0.995);
        }
        img.iter()
            .enumerate()
            .map(|(i, &v)| {
                let c = i % 3;
                let p = 1.0 - ((v - lo[c]) / (hi[c] - lo[c])).clamp(0.0, 1.0);
                (p.powf(1.0 / 2.2) * 255.0).round() as u8
            })
            .collect()
    };
    write_pnm(&out.join("before.ppm"), w, h, 3, &stretch(cap.rgb.data()))?;
    write_pnm(&out.join("after.ppm"), w, h, 3, &stretch(res.image.data()))?;
    // grey positive: log density stretched between percentiles of the central area
    let dens = |img: &[f32]| -> Vec<f32> { img.chunks_exact(3).map(|p| -((p[0] + p[1] + p[2]) / 3.0).max(1e-6).ln()).collect() };
    let d0 = dens(cap.rgb.data());
    let central: Vec<f32> =
        (0..w * h).filter(|i| (i % w) > w / 8 && (i % w) < w * 7 / 8 && (i / w) > h / 8 && (i / w) < h * 7 / 8).map(|i| d0[i]).collect();
    let (lo, hi) = (percentile(central.clone(), 0.01), percentile(central, 0.99));
    let grey = |d: &[f32]| -> Vec<u8> { d.iter().map(|&v| (((v - lo) / (hi - lo)).clamp(0.0, 1.0) * 255.0).round() as u8).collect() };
    write_pnm(&out.join("before_pos.pgm"), w, h, 1, &grey(&d0))?;
    write_pnm(&out.join("after_pos.pgm"), w, h, 1, &grey(&dens(res.image.data())))?;
    // 16-bit density (0..4) for local stretching
    let d16 = |d: &[f32]| -> Vec<u8> { d.iter().flat_map(|&v| (((v / 4.0).clamp(0.0, 1.0) * 65535.0).round() as u16).to_be_bytes()).collect() };
    for (name, d) in [("before_d.pgm", d0.clone()), ("after_d.pgm", dens(res.image.data()))] {
        let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(name))?);
        write!(f, "P5\n{w} {h}\n65535\n")?;
        f.write_all(&d16(&d))?;
    }
    write_pnm(&out.join("mask.pgm"), w, h, 1, &res.mask.data().iter().map(|&b| if b { 255 } else { 0 }).collect::<Vec<_>>())?;
    if let Some(a) = &res.attenuation {
        write_pnm(
            &out.join("att.pgm"),
            w,
            h,
            1,
            &a.data().iter().map(|&v| ((v - 0.5) * 2.0).clamp(0.0, 1.0).mul_add(255.0, 0.0) as u8).collect::<Vec<_>>(),
        )?;
    }
    let changed_outside = (0..w * h)
        .filter(|&i| !res.mask.data()[i] && (0..3).any(|c| cap.rgb.data()[3 * i + c].to_bits() != res.image.data()[3 * i + c].to_bits()))
        .count();
    println!("pixels changed outside the mask: {changed_outside}");
    Ok(())
}
