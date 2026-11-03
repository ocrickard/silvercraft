//! Frame finding: frames on a flatbed prescan, the film frame in a camera capture, and the frame
//! in live view for autocapture (all from `silvercraft-detect`).

use silvercraft_detect::{
    CameraFrame, DetectConfig, Rgb, RollFrame, RollPrior, detect_bed, detect_camera_frame, detect_camera_frame_with_prior, detect_live,
};
use silvercraft_epson::session::Preview;
use silvercraft_epson::{FrameBox, PixelRect};
use silvercraft_film::RgbImage;
use silvercraft_ptp::autocapture::Rect;

use crate::tether::{FrameMeasure, LiveImage};

/// Frames on a bed prescan (blank frames left out), in reading order, labelled 01, 02, …, and
/// the detector's warnings.
pub fn bed_frames(preview: &Preview, cfg: &DetectConfig) -> (Vec<FrameBox>, Vec<String>) {
    let img = &preview.image;
    let data: Vec<f32> = img.data().iter().map(|&v| f32::from(v) / 65535.0).collect();
    let Ok(rgb) = Rgb::new(img.width(), img.height(), &data) else { return (Vec::new(), vec!["the prescan is empty".into()]) };
    match detect_bed(&rgb, f64::from(preview.dpi.get()), &cfg.bed) {
        Ok(d) => {
            let frames = d
                .frames
                .iter()
                .filter(|f| !f.blank)
                .enumerate()
                .map(|(i, f)| FrameBox {
                    bounds: PixelRect { x: f.bounds.x, y: f.bounds.y, width: f.bounds.width, height: f.bounds.height },
                    label: format!("{:02}", i + 1),
                })
                .collect();
            (frames, d.warnings)
        }
        Err(e) => (Vec::new(), vec![format!("frame detection failed: {e}")]),
    }
}

/// Longest side the camera-frame detector works on (it finds edges, not detail).
const CAMERA_DETECT_SIDE: usize = 2048;

/// The capture area-downsampled to at most [`CAMERA_DETECT_SIDE`], and the step.
fn detect_copy(img: &RgbImage) -> Option<(Vec<f32>, usize, usize, usize)> {
    let (w, h) = (img.width(), img.height());
    let step = w.max(h).div_ceil(CAMERA_DETECT_SIDE).max(1);
    let (sw, sh) = (w / step, h / step);
    if sw == 0 || sh == 0 {
        return None;
    }
    let src = img.data();
    let mut small = Vec::with_capacity(sw * sh * 3);
    for y in 0..sh {
        for x in 0..sw {
            for c in 0..3 {
                let mut s = 0.0f32;
                for yy in 0..step {
                    let row = (y * step + yy) * w;
                    for xx in 0..step {
                        s += src[(row + x * step + xx) * 3 + c];
                    }
                }
                small.push(s / (step * step) as f32);
            }
        }
    }
    Some((small, sw, sh, step))
}

/// A frame found on the downsampled copy, in capture pixels.
fn upscale(mut f: CameraFrame, step: usize, w: usize, h: usize) -> Option<CameraFrame> {
    let k = step as f64;
    f.rotated.cx *= k;
    f.rotated.cy *= k;
    f.rotated.width *= k;
    f.rotated.height *= k;
    let r = f.rect;
    let s = |v: u32, max: usize| u32::try_from(((v as usize) * step).min(max)).ok();
    let (x0, y0, x1, y1) = (s(r.x, w)?, s(r.y, h)?, s(r.x + r.width, w)?, s(r.y + r.height, h)?);
    f.rect = silvercraft_detect::PxRect::new(x0, y0, x1.checked_sub(x0)?, y1.checked_sub(y0)?);
    Some(f)
}

/// The film frame in a camera capture (detected alone), in capture pixels.
pub fn camera_frame(img: &RgbImage, cfg: &DetectConfig) -> Option<CameraFrame> {
    let (small, sw, sh, step) = detect_copy(img)?;
    let f = detect_camera_frame(&Rgb::new(sw, sh, &small).ok()?, &cfg.camera).ok()??;
    upscale(f, step, img.width(), img.height())
}

/// The film frame in a camera capture of a roll whose frame size is known (`prior`, fitted
/// with `silvercraft_detect::fit_roll_prior` from the roll's [`camera_frame`]s), in capture
/// pixels.
pub fn camera_frame_with_prior(img: &RgbImage, cfg: &DetectConfig, prior: &RollPrior) -> Option<RollFrame> {
    let (small, sw, sh, step) = detect_copy(img)?;
    let mut r = detect_camera_frame_with_prior(&Rgb::new(sw, sh, &small).ok()?, &cfg.camera, prior).ok()?;
    r.frame = r.frame.and_then(|f| upscale(f, step, img.width(), img.height()));
    Some(r)
}

/// A frame's box `[x0, y0, x1, y1]` (clamped to a `w × h` capture) and whether all four film
/// edges were seen (otherwise the box is the holder's gate opening: good for cropping, but what
/// lies outside it is the holder, not film base).
pub fn frame_box_of(f: &CameraFrame, w: usize, h: usize) -> Option<([usize; 4], bool)> {
    let r = f.rect;
    let b = [(r.x as usize).min(w), (r.y as usize).min(h), (r.right() as usize).min(w), (r.bottom() as usize).min(h)];
    (b[2] > b[0] && b[3] > b[1]).then_some((b, f.edges_found))
}

/// The film frame `[x0, y0, x1, y1]` in a camera capture, if one is found, and whether all four
/// film edges were seen (see [`frame_box_of`]).
pub fn camera_frame_box(img: &RgbImage, cfg: &DetectConfig) -> Option<([usize; 4], bool)> {
    frame_box_of(&camera_frame(img, cfg)?, img.width(), img.height())
}

/// The live-view frame finder for the tether workflow.
pub fn live_measure(cfg: DetectConfig) -> FrameMeasure {
    Box::new(move |img: &LiveImage| match detect_live(&img.rgb, img.width, img.height, &cfg.live, &cfg.camera) {
        Ok(f) => (f.rect.map(|r| Rect::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32)), f.confident),
        Err(_) => (None, false),
    })
}
