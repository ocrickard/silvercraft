//! Live-view frame measurement for tethered autocapture.
//!
//! The autocapture state machine (settle, fire once, wait for the film to advance) lives in
//! `silvercraft-ptp` (`AutoCapture::update(rect, sig, now, confident)`). This module supplies its
//! per-frame inputs from a live-view RGB8 frame: the film-frame rectangle in live-view pixels
//! and whether the detection is *confident*: four frame edges found (not the aperture fallback),
//! confidence at or above `live.confident_threshold`, and the whole frame inside the view (a
//! frame cut by the view edge while the strip is moving is never confident). The engine converts
//! [`PxRect`] to ptp's `Rect` (same x, y, width, height).
//!
//! It runs the camera-scan detector on an area-downsampled copy (`live.work_size`, 320 px by
//! default), which is a few milliseconds for a 1024-px live frame.

use crate::camera::detect_planes;
use crate::config::{CameraConfig, LiveConfig};
use crate::geom::{PxRect, RotRect};
use crate::util::downsample_rgb;
use crate::{Result, Rgb};

/// One live-view measurement.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveFrame {
    /// Film-frame rectangle in live-view pixels (`None`: no film in view).
    pub rect: Option<PxRect>,
    pub rotated: Option<RotRect>,
    /// Detector confidence (0.9 four strong edges, 0.55 weak edges, 0.35 aperture only).
    pub confidence: f32,
    /// Feed as `confident` to the autocapture state machine.
    pub confident: bool,
    /// The whole frame is inside the view (with `live.edge_margin`).
    pub in_view: bool,
    /// The frame centre is within `live.centre_tolerance` of the view centre.
    pub centred: bool,
    /// Frame-centre offset from the view centre, as fractions of the view width and height.
    pub offset: (f64, f64),
}

impl LiveFrame {
    fn none() -> Self {
        Self { rect: None, rotated: None, confidence: 0.0, confident: false, in_view: false, centred: false, offset: (0.0, 0.0) }
    }
}

/// Measure the film frame in one live-view frame (`rgb8`: interleaved 8-bit RGB).
pub fn detect_live(rgb8: &[u8], width: usize, height: usize, live: &LiveConfig, camera: &CameraConfig) -> Result<LiveFrame> {
    let img = Rgb::new(width, height, rgb8)?;
    let factor = width.max(height).div_ceil(live.work_size.clamp(64, 4096)).max(1);
    let planes = downsample_rgb(&img, factor, |v: u8| f32::from(v) / 255.0)?;
    let Some(f) = detect_planes(&planes, factor as f64, width, height, camera, live.deskew) else {
        return Ok(LiveFrame::none());
    };
    let (w, h) = (width as f64, height as f64);
    let r = f.rect;
    let mx = live.edge_margin * w;
    let my = live.edge_margin * h;
    let in_view = f64::from(r.x) >= mx && f64::from(r.y) >= my && r.right() as f64 <= w - mx && r.bottom() as f64 <= h - my;
    let offset = ((f.rotated.cx - w / 2.0) / w, (f.rotated.cy - h / 2.0) / h);
    let centred = offset.0.abs() <= live.centre_tolerance && offset.1.abs() <= live.centre_tolerance;
    let confident = f.edges_found && f64::from(f.confidence) >= live.confident_threshold && in_view;
    Ok(LiveFrame { rect: Some(r), rotated: Some(f.rotated), confidence: f.confidence, confident, in_view, centred, offset })
}
