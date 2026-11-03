//! SilverCraft infrared dust and scratch repair.
//!
//! [`heal`] takes a capture's **linear transmittance** RGB (the negative as scanned, before
//! inversion) and, when the scanner made one, its infrared plane, and returns the repaired image,
//! the defect mask and statistics.
//!
//! # Why on the negative
//!
//! Repair runs before film-base estimation, roll calibration and inversion. Dust and hairs are
//! dark specks on the negative; left in, they skew the film-base and roll statistics (a hair
//! across the rebate reads as base density) before anyone sees them. The IR attenuation is also
//! physically a transmittance factor, so dividing it out (partial correction) or replacing
//! occluded pixels is only meaningful in linear transmittance. Every later stage then sees clean
//! film.
//!
//! # Pipeline (IR)
//!
//! 1. **Registration** ([`register`]): the dual-pass IR is offset from the visible pass (about
//!    one pixel on the Epson V800/V850). Defects are dark in both, so their relative-darkness
//!    maps are correlated over integer offsets and the peak refined to sub-pixel; the IR is
//!    resampled onto the RGB grid. Low confidence falls back to `ir_shift`.
//! 2. **Normalisation** ([`normalize`]): lamp/vignette falloff is removed with a masked
//!    large-scale background, and the image that leaks into IR on chromogenic film is removed by
//!    regressing log IR on the log visible channels. The result is an attenuation map
//!    (1 = clean, < 1 = blocked).
//! 3. **Silver check**: silver-image B&W film (HP5, Tri-X) is opaque in IR, so its IR *is* the
//!    picture. A leakage slope above `silver_slope` refuses IR repair with a warning instead of
//!    painting over the image.
//! 4. **Mask** ([`detect`]): seeds above `max(seed_sigma·σ_MAD, seed_depth)` grow (bounded
//!    hysteresis) through pixels above the grow threshold; small components are dropped and the
//!    rest dilated by the halo. The absolute depth floors keep Newton rings (a few percent,
//!    smooth) and grain out. Each component carries area, elongation, depth and its fill tier.
//! 5. **Fill** ([`fill`]): ring average (specks), multi-scale membrane that continues the edges
//!    running through a defect (medium, and specks on an edge), membrane tone plus best-match
//!    patch texture (large blobs, hairs and scratches), optional matched grain; or, for partial
//!    occluders with `partial_correction`, division by the attenuation (Digital ICE style).
//!
//! Without IR (camera scans) an optional, conservative detector ([`detect::no_ir_core`]) finds
//! only small, sharp, neutral, isolated dark specks; it is off by default.
//!
//! # Long scratches
//!
//! Scratches on the film itself ("rain" along the strip from winding and processing) do not
//! show in the IR pass. With `scratches = true`, [`scratches`] finds long, thin, faint lines along
//! the strip in the visible image (after the dust repair, with or without IR) and divides out or
//! interpolates them. They are reported in `lines`, in `defects` as [`Kind::Scratch`] and in
//! [`HealStats::line_scratches`].
//!
//! Pixels outside the final (dilated) mask are never written: the output is bit-identical to the
//! input there.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod config;
pub mod detect;
pub mod fill;
pub mod normalize;
pub mod register;
pub mod scratches;
mod util;

use rayon::prelude::*;
use silvercraft_film::RgbImage;
use silvercraft_film::diag::Diagnostic;
use silvercraft_imgproc::{Mask, Plane, filter};

pub use config::{HealConfig, ScratchAxis, ScratchRepair};
pub use detect::{Defect, Kind, Tier};
pub use silvercraft_imgproc::{Mask as DefectMask, Plane as IrPlane};

/// Errors: only inconsistent inputs or an invalid config. Unusable data (no IR signal, silver
/// film, a runaway mask) is not an error: the image comes back unchanged with a warning.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HealError {
    #[error("infrared plane is {ir_width}×{ir_height} ({ir_len} values) but the image is {width}×{height}")]
    SizeMismatch { width: usize, height: usize, ir_width: usize, ir_height: usize, ir_len: usize },
    #[error("invalid heal config: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Config(Vec<Diagnostic>),
    #[error("image: {0}")]
    Image(String),
}

/// Where the defect mask came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSource {
    /// No dust mask (disabled, no IR and no-IR detection off, or refused). Long scratches are
    /// reported separately.
    None,
    Infrared,
    /// The conservative visible-light speck detector.
    Visible,
}

/// The IR → RGB offset that was applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Registration {
    /// The IR feature of RGB pixel (x, y) is at (x + dx, y + dy) in the raw IR plane.
    pub dx: f32,
    pub dy: f32,
    /// Confidence of the estimate (0..1); 0 when registration was off.
    pub confidence: f32,
    /// True when the estimate was applied; false when `ir_shift` was used instead.
    pub estimated: bool,
}

/// Summary numbers of one heal.
#[derive(Debug, Clone, PartialEq)]
pub struct HealStats {
    pub source: MaskSource,
    /// Percent of the frame inside the final mask.
    pub coverage_pct: f64,
    pub defects: usize,
    pub small: usize,
    pub medium: usize,
    pub large: usize,
    pub corrected: usize,
    /// Defects of kind [`Kind::Scratch`]: IR hairs and scratches plus the long visible scratches.
    pub scratches: usize,
    pub deep: usize,
    /// Long visible scratches along the strip ("rain") repaired by [`scratches`].
    pub line_scratches: usize,
    /// Percent of the frame rewritten by the long-scratch repair.
    pub line_coverage_pct: f64,
    pub registration: Option<Registration>,
    /// Fitted log slopes of IR on visible R, G, B (image leakage); zero without IR.
    pub leakage: [f32; 3],
    /// Robust noise sigma of the log attenuation.
    pub noise: f32,
}

/// The outcome of [`heal`].
#[derive(Debug, Clone)]
pub struct HealResult {
    /// The repaired linear transmittance image.
    pub image: RgbImage,
    /// Every pixel that was (or would have been) rewritten: the dilated defect mask.
    pub mask: Mask,
    /// IR attenuation map (1 = clear) on the RGB grid, when IR was used.
    pub attenuation: Option<Plane>,
    pub defects: Vec<Defect>,
    /// The long visible scratches (also listed in `defects` as [`Kind::Scratch`]).
    pub lines: Vec<scratches::LineScratch>,
    pub stats: HealStats,
    /// Human-readable reasons something was skipped or degraded.
    pub warnings: Vec<String>,
}

fn unchanged(rgb: &RgbImage, warnings: Vec<String>) -> Result<HealResult, HealError> {
    let mask = Mask::empty(rgb.width(), rgb.height()).map_err(|e| HealError::Image(e.to_string()))?;
    Ok(HealResult {
        image: rgb.clone(),
        mask,
        attenuation: None,
        defects: Vec::new(),
        lines: Vec::new(),
        stats: HealStats {
            source: MaskSource::None,
            coverage_pct: 0.0,
            defects: 0,
            small: 0,
            medium: 0,
            large: 0,
            corrected: 0,
            scratches: 0,
            deep: 0,
            line_scratches: 0,
            line_coverage_pct: 0.0,
            registration: None,
            leakage: [0.0; 3],
            noise: 0.0,
        },
        warnings,
    })
}

/// Heal a capture whose infrared plane is a raw `Vec<f32>` (as `decode::Capture.ir`).
pub fn heal_capture(rgb: &RgbImage, ir: Option<&[f32]>, cfg: &HealConfig) -> Result<HealResult, HealError> {
    let plane = match ir {
        Some(v) => Some(Plane::new(rgb.width(), rgb.height(), v.to_vec()).map_err(|_| HealError::SizeMismatch {
            width: rgb.width(),
            height: rgb.height(),
            ir_width: 0,
            ir_height: 0,
            ir_len: v.len(),
        })?),
        None => None,
    };
    heal(rgb, plane.as_ref(), cfg)
}

/// Repair dust and scratches on a linear transmittance capture. See the crate docs.
pub fn heal(rgb: &RgbImage, ir: Option<&Plane>, cfg: &HealConfig) -> Result<HealResult, HealError> {
    let errors: Vec<Diagnostic> = cfg.validate().into_iter().filter(Diagnostic::is_error).collect();
    if !errors.is_empty() {
        return Err(HealError::Config(errors));
    }
    let (w, h) = (rgb.width(), rgb.height());
    if let Some(p) = ir
        && (p.width() != w || p.height() != h)
    {
        return Err(HealError::SizeMismatch { width: w, height: h, ir_width: p.width(), ir_height: p.height(), ir_len: p.len() });
    }
    let mut warnings = Vec::new();
    if !cfg.enabled {
        return unchanged(rgb, warnings);
    }
    let Ok(planes) = silvercraft_imgproc::split_channels(w, h, 3, rgb.data()) else {
        return unchanged(rgb, vec!["image buffer is inconsistent: repair skipped".into()]);
    };
    let planes: [Plane; 3] = match <[Plane; 3]>::try_from(planes) {
        Ok(p) => p,
        Err(_) => return unchanged(rgb, vec!["image buffer is inconsistent: repair skipped".into()]),
    };
    let Some(luma) = util::luma(w, h, rgb.data()) else {
        return unchanged(rgb, vec!["image buffer is inconsistent: repair skipped".into()]);
    };
    let scale = cfg.scale() as f32;
    if w < 8 || h < 8 {
        return unchanged(rgb, vec![format!("{w}×{h} is too small to repair")]);
    }

    let mut source = MaskSource::None;
    let mut registration = None;
    let mut leakage = [0.0f32; 3];
    let mut noise = 0.0f32;
    let mut attenuation: Option<Plane> = None;
    let mut core: Option<Mask> = None;
    let mut holder = Mask::from_plane(&luma, |_| false);

    match ir {
        Some(ir) if cfg.use_ir => {
            // 1. registration
            let (mut dx, mut dy) = (cfg.ir_shift[0] as f32, cfg.ir_shift[1] as f32);
            let mut reg = Registration { dx, dy, confidence: 0.0, estimated: false };
            if cfg.register {
                let est = register::estimate(&luma, ir, (cfg.max_shift as f32 * scale).max(0.0), scale);
                reg.confidence = est.confidence;
                if est.confidence >= cfg.min_registration_confidence as f32 {
                    (dx, dy) = (est.dx, est.dy);
                    reg = Registration { dx, dy, confidence: est.confidence, estimated: true };
                } else {
                    warnings.push(format!(
                        "IR registration not confident ({:.2} < {:.2}): using ir_shift ({dx}, {dy})",
                        est.confidence, cfg.min_registration_confidence
                    ));
                }
            }
            registration = Some(reg);
            let ir_reg = register::apply(ir, dx, dy);
            // 2. normalisation
            match normalize::normalize(&planes, &ir_reg, cfg, scale) {
                Err(normalize::Unusable::Empty) => warnings.push("IR plane is empty (all zero or not finite): IR repair skipped".into()),
                Err(normalize::Unusable::NoFilm) => warnings.push("IR plane shows no film area: IR repair skipped".into()),
                Ok(n) => {
                    leakage = n.leakage;
                    noise = n.noise;
                    holder = n.holder.clone();
                    let slope: f32 = n.leakage.iter().sum();
                    // 3. silver check
                    if slope > cfg.silver_slope as f32 {
                        warnings.push(format!(
                            "IR looks like image (silver B&W film? IR/visible slope {slope:.2} > {:.2}): IR repair skipped",
                            cfg.silver_slope
                        ));
                    } else {
                        // 4. detection
                        let ir_core = detect::ir_core(&n.s, &n.detail, &n.valid, &n.local_noise, cfg, scale);
                        let steps = (cfg.visible_grow * f64::from(scale)).round() as usize;
                        core = Some(if steps > 0 {
                            // darkness over twice the growth range: the growth check looks past it
                            let region = silvercraft_imgproc::morph::dilate(&ir_core, 2 * steps);
                            let dark = detect::visible_darkness(&luma, &region, ((8.0 * scale).round() as usize).max(2));
                            let t = (3.0 * detect::relative_grain(&luma)).max(cfg.visible_grow_depth as f32);
                            detect::grow_visible_bounded(&ir_core, &dark, &n.valid, t, steps)
                        } else {
                            ir_core
                        });
                        source = MaskSource::Infrared;
                        attenuation = Some(n.s.map(|v| (-v).exp().min(1.5)));
                    }
                }
            }
        }
        Some(_) => {}
        None => {}
    }
    if core.is_none() && cfg.no_ir_detect {
        let surround = util::surround(&luma, scale);
        let margin = silvercraft_imgproc::morph::dilate(&surround, (cfg.border_margin as f32 * scale).round() as usize);
        let valid = Mask::from_plane(&luma, |v| v.is_finite()).and(&margin.not()).unwrap_or_else(|_| margin.not());
        core = Some(detect::no_ir_core(&planes, &valid, cfg, scale));
        holder = holder.or(&surround).unwrap_or(surround);
        source = MaskSource::Visible;
    }

    let mut data = rgb.data().to_vec();
    let empty = Mask::from_plane(&luma, |_| false);
    let (mut mask, mut defects) = match core {
        Some(core) => match dust(rgb, &luma, &core, &holder, attenuation.as_ref(), cfg, scale, &mut data) {
            Ok((m, d, warning)) => {
                warnings.extend(warning);
                (m, d)
            }
            Err(w) => {
                warnings.push(w);
                source = MaskSource::None;
                (empty.clone(), Vec::new())
            }
        },
        None => (empty.clone(), Vec::new()),
    };

    // long scratches along the strip (visible only), on the dust-repaired image
    let mut lines = Vec::new();
    let mut line_coverage_pct = 0.0;
    if cfg.scratches {
        let surround = util::surround(&luma, scale);
        let all = holder.or(&surround).unwrap_or(surround);
        let mut never = silvercraft_imgproc::morph::dilate(&all, (cfg.border_margin as f32 * scale).round() as usize);
        for (i, n) in never.data_mut().iter_mut().enumerate() {
            *n |= !(data[3 * i].is_finite() && data[3 * i + 1].is_finite() && data[3 * i + 2].is_finite());
        }
        let out = scratches::repair(&mut data, &never, cfg, scale, 0x5C2A_7C4E);
        warnings.extend(out.warnings);
        line_coverage_pct = 100.0 * out.mask.fraction();
        mask = mask.or(&out.mask).unwrap_or(mask);
        defects.extend(out.lines.iter().map(|l| Defect {
            bbox: l.bbox,
            area: l.pixels,
            core_area: l.core_pixels,
            length: (l.end - l.start) as f32,
            elongation: ((l.end - l.start) as f32 / l.width.max(1.0)).max(1.0),
            kind: Kind::Scratch,
            min_attenuation: (-l.contrast).exp().min(1.0),
            deep: false,
            tier: match l.repair {
                scratches::LineRepair::Corrected => Tier::Corrected,
                scratches::LineRepair::Interpolated => Tier::Interpolated,
            },
        }));
        lines = out.lines;
    }

    let image = RgbImage::new(w, h, data).unwrap_or_else(|_| rgb.clone());
    let count = |t: Tier| defects.iter().filter(|d| d.tier == t).count();
    let stats = HealStats {
        source,
        coverage_pct: 100.0 * mask.fraction(),
        defects: defects.len(),
        small: count(Tier::Small),
        medium: count(Tier::Medium),
        large: count(Tier::Large),
        corrected: count(Tier::Corrected),
        scratches: defects.iter().filter(|d| d.kind == Kind::Scratch).count(),
        deep: defects.iter().filter(|d| d.deep).count(),
        line_scratches: lines.len(),
        line_coverage_pct,
        registration,
        leakage,
        noise,
    };
    Ok(HealResult { image, mask, attenuation, defects, lines, stats, warnings })
}

/// Dust repair from a detected core: final mask, components, tiered fill into `data`. Returns
/// the mask and defects, or a warning when the mask is implausible (nothing is written).
fn dust(
    rgb: &RgbImage,
    luma: &Plane,
    core: &Mask,
    holder: &Mask,
    attenuation: Option<&Plane>,
    cfg: &HealConfig,
    scale: f32,
    data: &mut [f32],
) -> Result<(Mask, Vec<Defect>, Option<String>), String> {
    let (w, h) = (rgb.width(), rgb.height());
    // final mask: core + halo, never into the holder or non-finite pixels
    let mut never = silvercraft_imgproc::morph::dilate(holder, (cfg.border_margin as f32 * scale).round() as usize);
    for (i, n) in never.data_mut().iter_mut().enumerate() {
        *n |= !(rgb.data()[3 * i].is_finite() && rgb.data()[3 * i + 1].is_finite() && rgb.data()[3 * i + 2].is_finite());
    }
    let halo = (cfg.halo as f32 * scale).round().max(0.0) as usize;
    let mask = detect::final_mask(core, halo, &never);
    let coverage_pct = 100.0 * mask.fraction();
    if coverage_pct > cfg.max_coverage {
        return Err(format!(
            "defect mask covers {coverage_pct:.1}% of the frame (> {:.1}%): likely a misdetection, repair skipped",
            cfg.max_coverage
        ));
    }

    // components and their statistics
    let (labels, comps) = silvercraft_imgproc::label(&mask);
    let lists = detect::pixel_lists(&labels, comps.len());
    let s2 = scale * scale;
    let defects: Vec<Defect> = comps
        .iter()
        .zip(&lists)
        .map(|(c, px)| {
            let (length, elongation) = detect::shape(px, w);
            let core_px: Vec<usize> = px.iter().copied().filter(|&i| core.data()[i]).collect();
            let min_attenuation = match attenuation {
                Some(a) => core_px.iter().map(|&i| a.data()[i]).fold(1.0f32, f32::min),
                None => 0.0,
            };
            let kind = if length >= cfg.scratch_min_length as f32 * scale && elongation >= cfg.scratch_min_aspect as f32 {
                Kind::Scratch
            } else {
                Kind::Speck
            };
            let deep = min_attenuation < cfg.deep_attenuation as f32;
            let area = px.len();
            let tier = if cfg.partial_correction && attenuation.is_some() && min_attenuation >= cfg.partial_floor as f32 {
                Tier::Corrected
            } else if kind == Kind::Scratch || area as f64 >= cfg.large_min_area * f64::from(s2) {
                Tier::Large
            } else if area as f64 <= cfg.small_max_area * f64::from(s2) {
                Tier::Small
            } else {
                Tier::Medium
            };
            Defect { bbox: c.bbox, area, core_area: core_px.len(), length, elongation, kind, min_attenuation, deep, tier }
        })
        .collect();

    // fill
    let forbidden = mask.or(&never).unwrap_or_else(|_| mask.clone());
    let ctx = fill::FillContext { width: w, height: h, rgb: rgb.data(), forbidden: &forbidden, luma, cfg, scale };
    let smooth_att =
        if defects.iter().any(|d| d.tier == Tier::Corrected) { attenuation.map(|a| filter::gaussian(a, 0.7 * scale.max(1.0))) } else { None };
    let writes: Vec<Vec<(usize, [f32; 3])>> = defects
        .par_iter()
        .zip(&lists)
        .enumerate()
        .map(|(k, (d, px))| match (d.tier, &smooth_att) {
            (Tier::Corrected, Some(a)) => {
                let floor = cfg.partial_floor as f32;
                px.iter()
                    .map(|&i| {
                        let t = a.data()[i].clamp(floor, 1.0);
                        let v = &rgb.data()[3 * i..3 * i + 3];
                        (i, [v[0] / t, v[1] / t, v[2] / t])
                    })
                    .collect()
            }
            _ => fill::fill_component(&ctx, px, d.bbox, d.tier, util::splitmix(k as u64 + 1)),
        })
        .collect();
    let mut unfilled = 0usize;
    for (wr, px) in writes.iter().zip(&lists) {
        unfilled += px.len().saturating_sub(wr.len());
        for &(i, v) in wr {
            if let Some(dst) = data.get_mut(3 * i..3 * i + 3) {
                dst.copy_from_slice(&v);
            }
        }
    }
    let warning = (unfilled > 0).then(|| format!("{unfilled} defect pixels had no clean surroundings and were left as scanned"));
    Ok((mask, defects, warning))
}
