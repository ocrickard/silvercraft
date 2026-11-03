//! Per-frame high-resolution region planning from the bed preview (§5.3).
//! Pure functions: frame detection itself is app-level.

use crate::codec::{Dpi, MAX_DPI, Region};
use crate::{EpsonError, Preview};

/// A rectangle in preview pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// A detected frame on the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBox {
    /// Bounds in preview pixels.
    pub bounds: PixelRect,
    /// Label (used in file names).
    pub label: String,
}

/// Planner settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramePlanOptions {
    /// Padding around each frame (1.0 mm).
    pub padding_mm: f64,
    /// Requested dpi; `None` = the scanner maximum.
    pub target_dpi: Option<u32>,
    /// Highest validated/optical dpi (6400).
    pub max_dpi: u32,
}

impl Default for FramePlanOptions {
    fn default() -> Self {
        Self { padding_mm: 1.0, target_dpi: None, max_dpi: MAX_DPI }
    }
}

/// One planned high-res frame scan.
#[derive(Debug, Clone, PartialEq)]
pub struct FramePlan {
    /// 1-based index.
    pub index: usize,
    /// Frame label.
    pub label: String,
    /// Padded bounds in preview pixels.
    pub padded: PixelRect,
    /// Region to scan (inches from the bed origin).
    pub region: Region,
    /// Target resolution.
    pub dpi: Dpi,
    /// `max(1, round(width_in × dpi))`.
    pub estimated_width_px: u32,
    /// `max(1, round(height_in × dpi))`.
    pub estimated_height_px: u32,
    /// `w × h × 3 × 2`.
    pub estimated_image_bytes: u64,
    /// `"{index:03}-{label}-{dpi}ppi"`.
    pub file_stem: String,
}

/// A label made safe for a file stem: anything that could act as a path separator or
/// traversal (`/`, `\`, `.` runs, …) collapses to `-`. The manifest keeps the original
/// label; only the on-disk name is sanitised.
fn stem_part(label: &str) -> String {
    let mut out = String::new();
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "frame".into() } else { out }
}

fn target_dpi(opts: &FramePlanOptions) -> Result<Dpi, EpsonError> {
    let max = opts.max_dpi.clamp(1, MAX_DPI);
    let t = match opts.target_dpi {
        Some(0) => return Err(EpsonError::InvalidRequest("target dpi must be positive".into())),
        Some(t) => t,
        None => max,
    };
    Dpi::new(t.min(max))
}

/// Plan frames on a bed preview of `preview_w × preview_h` px at `preview_dpi`. Frames whose
/// padded region cannot form a valid scan region are skipped.
pub fn plan_frames_px(
    preview_w: u32,
    preview_h: u32,
    preview_dpi: Dpi,
    frames: &[FrameBox],
    opts: &FramePlanOptions,
) -> Result<Vec<FramePlan>, EpsonError> {
    if !(opts.padding_mm >= 0.0 && opts.padding_mm.is_finite()) {
        return Err(EpsonError::InvalidRequest("padding must be non-negative".into()));
    }
    let dpi = target_dpi(opts)?;
    let pdpi = f64::from(preview_dpi.get());
    let pad = (opts.padding_mm / 25.4 * pdpi).ceil().min(f64::from(u32::MAX)) as u32;
    let mut out: Vec<FramePlan> = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let b = f.bounds;
        let x0 = b.x.saturating_sub(pad);
        let y0 = b.y.saturating_sub(pad);
        let x1 = b.x.saturating_add(b.width).saturating_add(pad).min(preview_w);
        let y1 = b.y.saturating_add(b.height).saturating_add(pad).min(preview_h);
        let padded = PixelRect { x: x0, y: y0, width: x1.saturating_sub(x0).max(1), height: y1.saturating_sub(y0).max(1) };
        let left = f64::from(padded.x) / pdpi;
        let top = f64::from(padded.y) / pdpi;
        let right = f64::from(padded.x + padded.width) / pdpi;
        let bottom = f64::from(padded.y + padded.height) / pdpi;
        let Ok(region) = Region::new(left, top, right - left, bottom - top) else { continue };
        let d = f64::from(dpi.get());
        let w = ((region.width() * d).round_ties_even() as u32).max(1);
        let h = ((region.height() * d).round_ties_even() as u32).max(1);
        let index = i + 1;
        // Sanitised and unique: two labels that sanitise alike ("1/2", "1-2") must not
        // name the same capture file and silently overwrite each other.
        let base_stem = format!("{index:03}-{}-{}ppi", stem_part(&f.label), dpi.get());
        let mut file_stem = base_stem.clone();
        let mut n = 2u32;
        while out.iter().any(|p: &FramePlan| p.file_stem == file_stem) {
            file_stem = format!("{base_stem}-{n}");
            n += 1;
        }
        out.push(FramePlan {
            index,
            label: f.label.clone(),
            padded,
            region,
            dpi,
            estimated_width_px: w,
            estimated_height_px: h,
            estimated_image_bytes: u64::from(w) * u64::from(h) * 6,
            file_stem,
        });
    }
    Ok(out)
}

/// Plan frames on a [`Preview`] returned by the prescan.
pub fn plan_frames(preview: &Preview, frames: &[FrameBox], opts: &FramePlanOptions) -> Result<Vec<FramePlan>, EpsonError> {
    let w = u32::try_from(preview.image.width()).unwrap_or(u32::MAX);
    let h = u32::try_from(preview.image.height()).unwrap_or(u32::MAX);
    plan_frames_px(w, h, preview.dpi, frames, opts)
}

/// Tight exposed-frame crop `(x0, y0, x1, y1)` inside a high-res capture of `region`
/// (`exposed_box_in_capture`, highres.py:12-48): centre of the loose detector bounds ± the
/// physical frame size.
pub fn exposed_box_in_capture(
    frame: PixelRect,
    region: &Region,
    preview_dpi: f64,
    target_dpi: f64,
    along_mm: f64,
    across_mm: f64,
    capture_width: u32,
    capture_height: u32,
    inset_px: u32,
) -> Result<(u32, u32, u32, u32), EpsonError> {
    if !(along_mm > 0.0 && across_mm > 0.0 && preview_dpi > 0.0 && target_dpi > 0.0) || capture_width == 0 || capture_height == 0 {
        return Err(EpsonError::InvalidRequest("frame size, dpi and capture size must be positive".into()));
    }
    let cx_in = (f64::from(frame.x) + f64::from(frame.width) / 2.0) / preview_dpi;
    let cy_in = (f64::from(frame.y) + f64::from(frame.height) / 2.0) / preview_dpi;
    let cx = (cx_in - region.left()) * target_dpi;
    let cy = (cy_in - region.top()) * target_dpi;
    let half_w = across_mm / 2.0 * target_dpi / 25.4 - f64::from(inset_px);
    let half_h = along_mm / 2.0 * target_dpi / 25.4 - f64::from(inset_px);
    if half_w <= 0.0 || half_h <= 0.0 {
        return Err(EpsonError::InvalidRequest("inset is larger than half the exposed frame".into()));
    }
    let clampi = |v: f64, lo: f64, hi: f64| v.round_ties_even().clamp(lo, hi.max(lo));
    let (cw, ch) = (f64::from(capture_width), f64::from(capture_height));
    let x0 = clampi(cx - half_w, 0.0, cw - 1.0);
    let y0 = clampi(cy - half_h, 0.0, ch - 1.0);
    let x1 = clampi(cx + half_w, x0 + 1.0, cw).max(x0 + 1.0);
    let y1 = clampi(cy + half_h, y0 + 1.0, ch).max(y0 + 1.0);
    Ok((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_conversion_and_naming() {
        let frames = vec![FrameBox { bounds: PixelRect { x: 100, y: 200, width: 283, height: 189 }, label: "35mm".into() }];
        let p =
            plan_frames_px(1176, 1940, Dpi::new(200).unwrap(), &frames, &FramePlanOptions { target_dpi: Some(3200), ..Default::default() }).unwrap();
        assert_eq!(p.len(), 1);
        // pad = ceil(1/25.4*200) = 8 px
        assert_eq!(p[0].padded, PixelRect { x: 92, y: 192, width: 299, height: 205 });
        assert!((p[0].region.left() - 0.46).abs() < 1e-12);
        assert!((p[0].region.top() - 0.96).abs() < 1e-12);
        assert!((p[0].region.width() - 1.495).abs() < 1e-9);
        assert_eq!(p[0].dpi.get(), 3200);
        assert_eq!(p[0].estimated_width_px, 4784);
        assert_eq!(p[0].estimated_image_bytes, 4784 * 3280 * 6);
        assert_eq!(p[0].file_stem, "001-35mm-3200ppi");
    }

    #[test]
    fn dpi_defaults_and_clamps_and_edges_clamp() {
        let frames = vec![
            FrameBox { bounds: PixelRect { x: 0, y: 0, width: 50, height: 50 }, label: "a".into() },
            FrameBox { bounds: PixelRect { x: 1170, y: 1930, width: 50, height: 50 }, label: "b".into() },
        ];
        let p = plan_frames_px(1176, 1940, Dpi::new(200).unwrap(), &frames, &FramePlanOptions::default()).unwrap();
        assert_eq!(p[0].dpi.get(), 6400);
        assert_eq!((p[0].padded.x, p[0].padded.y), (0, 0));
        assert_eq!(p[1].padded.x + p[1].padded.width, 1176);
        let p =
            plan_frames_px(1176, 1940, Dpi::new(200).unwrap(), &frames, &FramePlanOptions { target_dpi: Some(9600), ..Default::default() }).unwrap();
        assert_eq!(p[0].dpi.get(), 6400);
        assert!(plan_frames_px(1, 1, Dpi::new(200).unwrap(), &frames, &FramePlanOptions { target_dpi: Some(0), ..Default::default() }).is_err());
    }

    #[test]
    fn file_stems_cannot_escape_the_roll_folder() {
        let frames = vec![
            FrameBox { bounds: PixelRect { x: 10, y: 10, width: 30, height: 20 }, label: "../../evil".into() },
            FrameBox { bounds: PixelRect { x: 10, y: 50, width: 30, height: 20 }, label: "a/b\\c".into() },
        ];
        let p = plan_frames_px(1176, 1940, Dpi::new(200).unwrap(), &frames, &FramePlanOptions::default()).unwrap();
        assert_eq!(p.len(), 2);
        for plan in &p {
            assert!(!plan.file_stem.contains(['/', '\\']), "{}", plan.file_stem);
            assert!(!plan.file_stem.split('-').any(|c| c == ".."), "{}", plan.file_stem);
        }
        assert_eq!(p[0].label, "../../evil", "the manifest label is untouched");
        assert_ne!(p[0].file_stem, p[1].file_stem);
    }

    #[test]
    fn exposed_box() {
        let region = Region::new(0.5, 1.0, 1.6, 1.1).unwrap();
        let b =
            exposed_box_in_capture(PixelRect { x: 100, y: 200, width: 300, height: 200 }, &region, 200.0, 1000.0, 36.0, 24.0, 1600, 1100, 0).unwrap();
        // centre (1.25, 1.5) in -> (750, 500) px; half 472.4 x 708.7
        assert_eq!(b, (278, 0, 1222, 1100));
        assert!(exposed_box_in_capture(PixelRect { x: 0, y: 0, width: 1, height: 1 }, &region, 200.0, 1000.0, 1.0, 1.0, 10, 10, 1000).is_err());
    }
}
