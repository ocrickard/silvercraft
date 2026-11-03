//! SilverCraft scanner-artifact detection and repair.
//!
//! Flatbed CCD scanners (Epson V800/V850) leave three kinds of trouble that are not on the film:
//!
//! - **Line defects** ([`lines`]): narrow streaks along the scan direction from a weak CCD element
//!   or dust on the sensor, mirror or lens, full length or partial. Mild ones are corrected with a
//!   per-line gain/offset fit to their neighbours, dead ones interpolated.
//! - **Banding** ([`banding`]): row-to-row gain flicker from the lamp or the stepper, corrected
//!   with a per-row gain.
//! - **Health** ([`health`]): clipping, black level, noise floor and dynamic-range usage per
//!   channel, with human-readable warnings.
//!
//! Input is linear transmittance (0..1) as three RGB [`Plane`]s and an optional infrared plane,
//! which is processed too because it shows the same streaks. Defaults are conservative: a clean
//! scan comes back bit-identical. Ported and extended from the original `row_defects.py`,
//! `line_defects.py` and `scanhealth.py` (MIT, same owner); the original periodic tile-boundary
//! line repair is not ported because it fixed an artifact of its Windows USB transport.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod banding;
pub mod config;
pub mod health;
pub mod lines;
mod util;

use serde::{Deserialize, Serialize};
use silvercraft_imgproc::{ImgError, Plane, interleave, split_channels};

pub use banding::{Banding, LineGain, detect_banding, repair_banding};
pub use config::{BandingConfig, FieldDoc, HealthConfig, LineConfig, ScanfixConfig, StreakAxis};
pub use health::{ChannelHealth, ScanReport, Warning, WarningKind, channel_health};
pub use lines::{LineDefect, LineRepair, Polarity, detect_lines, repair_lines};

/// Errors from scanfix (inconsistent dimensions or a bad config; never from image content).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScanfixError {
    #[error(transparent)]
    Image(#[from] ImgError),
    #[error("invalid config: {0}")]
    Config(&'static str),
    #[error("infrared plane is {ir_width}×{ir_height} but RGB is {width}×{height}")]
    IrSize { width: usize, height: usize, ir_width: usize, ir_height: usize },
}

pub type Result<T> = std::result::Result<T, ScanfixError>;

/// A scanner channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    Red,
    Green,
    Blue,
    Ir,
}

impl Channel {
    /// Lower-case name for messages ("red", "green", "blue", "infrared").
    pub fn name(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Ir => "infrared",
        }
    }
}

const RGB: [Channel; 3] = [Channel::Red, Channel::Green, Channel::Blue];

/// A scan: three linear-transmittance RGB planes and an optional infrared plane, all one size.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanImage {
    rgb: [Plane; 3],
    ir: Option<Plane>,
}

impl ScanImage {
    /// From planes; every plane must have the same size.
    pub fn new(rgb: [Plane; 3], ir: Option<Plane>) -> Result<Self> {
        rgb[0].same_size(&rgb[1])?;
        rgb[0].same_size(&rgb[2])?;
        if let Some(i) = &ir
            && i.same_size(&rgb[0]).is_err()
        {
            return Err(ScanfixError::IrSize { width: rgb[0].width(), height: rgb[0].height(), ir_width: i.width(), ir_height: i.height() });
        }
        Ok(Self { rgb, ir })
    }

    /// From interleaved RGB (`width·height·3` values) and an optional `width·height` IR plane.
    pub fn from_interleaved(width: usize, height: usize, rgb: &[f32], ir: Option<&[f32]>) -> Result<Self> {
        let mut planes = split_channels(width, height, 3, rgb)?.into_iter();
        let (Some(r), Some(g), Some(b)) = (planes.next(), planes.next(), planes.next()) else {
            return Err(ImgError::BufferSize { width, height, channels: 3, got: rgb.len() }.into());
        };
        let ir = match ir {
            Some(d) => Some(Plane::new(width, height, d.to_vec())?),
            None => None,
        };
        Self::new([r, g, b], ir)
    }

    pub fn width(&self) -> usize {
        self.rgb[0].width()
    }
    pub fn height(&self) -> usize {
        self.rgb[0].height()
    }
    pub fn rgb(&self) -> &[Plane; 3] {
        &self.rgb
    }
    pub fn ir(&self) -> Option<&Plane> {
        self.ir.as_ref()
    }
    /// Take the planes back.
    pub fn into_parts(self) -> ([Plane; 3], Option<Plane>) {
        (self.rgb, self.ir)
    }
    /// Interleaved RGB.
    pub fn rgb_interleaved(&self) -> Result<Vec<f32>> {
        Ok(interleave(&self.rgb)?)
    }

    fn planes(&self) -> impl Iterator<Item = (Channel, &Plane)> {
        RGB.into_iter().zip(self.rgb.iter()).chain(self.ir.iter().map(|p| (Channel::Ir, p)))
    }
}

/// Measure the scan and detect its artifacts without changing it.
pub fn analyze(image: &ScanImage, cfg: &ScanfixConfig) -> Result<ScanReport> {
    run(image, None, cfg)
}

/// Detect and repair the scan's artifacts in place. The report's health section describes the
/// input; its defect and banding records say what was repaired. A clean scan is left untouched.
pub fn repair(image: &mut ScanImage, cfg: &ScanfixConfig) -> Result<ScanReport> {
    let mut out = image.clone();
    let report = run(image, Some(&mut out), cfg)?;
    if report.repaired {
        *image = out;
    }
    Ok(report)
}

/// [`repair`] on interleaved RGB and an optional IR plane; buffers are only written when
/// something was repaired.
pub fn repair_interleaved(width: usize, height: usize, rgb: &mut [f32], ir: Option<&mut [f32]>, cfg: &ScanfixConfig) -> Result<ScanReport> {
    let mut image = ScanImage::from_interleaved(width, height, rgb, ir.as_deref())?;
    let report = repair(&mut image, cfg)?;
    if report.repaired {
        rgb.copy_from_slice(&image.rgb_interleaved()?);
        if let (Some(dst), Some(src)) = (ir, image.ir()) {
            dst.copy_from_slice(src.data());
        }
    }
    Ok(report)
}

fn run(image: &ScanImage, mut out: Option<&mut ScanImage>, cfg: &ScanfixConfig) -> Result<ScanReport> {
    cfg.validate()?;
    let mut report = ScanReport {
        width: image.width(),
        height: image.height(),
        axis: cfg.axis,
        channels: image.planes().map(|(c, p)| channel_health(p, c, &cfg.health)).collect(),
        line_defects: Vec::new(),
        banding: Vec::new(),
        warnings: Vec::new(),
        repaired: false,
    };
    if cfg.enabled {
        for (i, (channel, plane)) in image.planes().enumerate() {
            if channel == Channel::Ir && !cfg.process_ir {
                continue;
            }
            let mut defects = detect_lines(plane, cfg.axis, channel, &cfg.lines)?;
            let target = match out.as_deref_mut() {
                Some(o) if i < 3 => o.rgb.get_mut(i),
                Some(o) => o.ir.as_mut(),
                None => None,
            };
            let banding = match target {
                Some(t) => {
                    report.repaired |= repair_lines(t, cfg.axis, &mut defects, &cfg.lines)? > 0;
                    let mut b = detect_banding(t, cfg.axis, channel, &cfg.banding)?;
                    report.repaired |= repair_banding(t, &mut b, &cfg.banding)? > 0;
                    b
                }
                None => detect_banding(plane, cfg.axis, channel, &cfg.banding)?,
            };
            report.line_defects.extend(defects);
            report.banding.push(banding);
        }
    }
    report.warnings = health::warnings(&report, &cfg.health);
    Ok(report)
}
