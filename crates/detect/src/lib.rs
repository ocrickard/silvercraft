//! SilverCraft frame detection.
//!
//! - [`bed`]: flatbed prescan detection. Finds the holder openings and the film strips in them,
//!   then the frames along each strip. The inter-frame gaps are scored and regularised by the
//!   frame pitch (a comb fit). Output is per-frame boxes in prescan pixels and millimetres.
//! - [`camera`]: one frame per camera capture (film over a light table). Finds the frame
//!   rectangle from four supported image/rebate edges and estimates a small deskew angle.
//! - [`camera_roll`]: a whole camera roll. The frames share one size (a copy stand), so weak
//!   frames are re-detected with the roll's consensus size, searching only for position.
//! - [`live`]: the per-frame measurement that drives tethered autocapture: the film-frame
//!   rectangle in live-view pixels and whether it is confident (four edges found, wholly in
//!   view). The autocapture state machine itself lives in `silvercraft-ptp`.
//!
//! The bed path is DPI-aware, skew-aware and polarity-aware, and the camera
//! path deskews.
//!
//! All inputs are untrusted: dimensions are checked, non-finite samples are treated as zero, and
//! nothing panics.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bed;
pub mod camera;
pub mod camera_roll;
pub mod config;
pub mod geom;
pub mod live;
mod util;

pub use bed::{Axis, BedDetection, BedFrame, Orientation, StripInfo, StripKind, detect_bed};
pub use camera::{CameraFrame, detect_camera_frame};
pub use camera_roll::{
    RollDetection, RollFrame, RollPrior, detect_camera_frame_with_prior, detect_camera_roll, fit_roll_prior, is_confident, needs_prior,
};
pub use config::{BedConfig, CameraConfig, DetectConfig, FieldDoc, FilmType, Format35, Format120, Holder, LiveConfig};
pub use geom::{MmRect, PxRect, RotRect};
pub use live::{LiveFrame, detect_live};

/// Detection errors. Only malformed calls are errors; an image with nothing to find is a
/// successful empty result.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DetectError {
    #[error("empty image ({width}×{height})")]
    Empty { width: usize, height: usize },
    #[error("buffer of {got} values does not match {width}×{height}×{channels}")]
    BufferSize { width: usize, height: usize, channels: usize, got: usize },
    #[error("image dimensions overflow ({width}×{height})")]
    Overflow { width: usize, height: usize },
    #[error("invalid parameter: {0}")]
    Param(String),
    #[error(transparent)]
    Img(#[from] silvercraft_imgproc::ImgError),
}

pub type Result<T> = std::result::Result<T, DetectError>;

/// A borrowed interleaved RGB image (linear transmittance for scans; any monotone encoding
/// for live view).
#[derive(Clone, Copy, Debug)]
pub struct Rgb<'a, T> {
    width: usize,
    height: usize,
    data: &'a [T],
}

impl<'a, T> Rgb<'a, T> {
    /// Wrap `data` (`width × height × 3`, row-major interleaved), checking the length.
    pub fn new(width: usize, height: usize, data: &'a [T]) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(DetectError::Empty { width, height });
        }
        let n = width.checked_mul(height).and_then(|p| p.checked_mul(3)).ok_or(DetectError::Overflow { width, height })?;
        if data.len() != n {
            return Err(DetectError::BufferSize { width, height, channels: 3, got: data.len() });
        }
        Ok(Self { width, height, data })
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn data(&self) -> &'a [T] {
        self.data
    }
}
