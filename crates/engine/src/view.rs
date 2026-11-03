//! Full-size renderings for the frame viewer: a developed frame as LightCraft receives it, or a
//! scan as captured. Display only; developing uses the film pipeline.

use std::path::Path;

use crate::Result;
use crate::roll::{Thumbnail, display_rendering};
use crate::rotation::Rotation;

/// The developed DNG at `path` rendered for the screen at full size (exposure and white balance
/// applied, as in the roll's thumbnails).
pub fn render_developed(path: &Path) -> Result<Thumbnail> {
    let bytes = silvercraft_decode::read_capped(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let scene = silvercraft_handoff::dng::decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let t = display_rendering(scene.width, scene.height, &scene.rgb, 0.0, scene.as_shot_neutral, usize::MAX)
        .ok_or_else(|| format!("{}: empty image", path.display()))?;
    Ok(t.rotated(Rotation::of_orientation(scene.orientation)))
}

/// A scan as captured: linear RGB (`width * height * 3`, 0..1), infrared left out.
pub struct Scan {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<f32>,
}

/// Read the capture at `path`: a scanner TIFF, or a camera raw (demosaiced quickly: this is for
/// looking at, not developing).
pub fn load_scan(path: &Path) -> Result<Scan> {
    let capture = if crate::is_camera_scan(path) {
        let config = silvercraft_decode::DecodeConfig { demosaic: silvercraft_decode::Demosaic::Bilinear, ..Default::default() };
        silvercraft_decode::decode_file(path, &config).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        let bytes = silvercraft_decode::read_capped(path).map_err(|e| format!("{}: {e}", path.display()))?;
        silvercraft_decode::tiff_scan::decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))?
    };
    let img = capture.rgb;
    Ok(Scan { width: img.width(), height: img.height(), rgb: img.into_data() })
}
