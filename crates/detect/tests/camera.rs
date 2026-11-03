//! Camera-scan frame detection on synthetic captures.

mod common;

use common::Capture;
use silvercraft_detect::{CameraConfig, CameraFrame, PxRect, Rgb, RotRect, detect_camera_frame};

fn detect(c: &Capture, cfg: &CameraConfig) -> Option<CameraFrame> {
    let data = c.render();
    detect_camera_frame(&Rgb::new(c.width, c.height, &data).unwrap(), cfg).unwrap()
}

/// IoU of the detected rotated frame (compared upright, in its own frame) with the truth.
fn upright_iou(c: &Capture, f: &CameraFrame) -> f64 {
    let (cx, cy, w, h, _) = c.truth();
    let truth = RotRect { cx, cy, width: w, height: h, angle_deg: 0.0 }.upright_box(c.width, c.height).unwrap();
    let got = RotRect { angle_deg: 0.0, ..f.rotated }.upright_box(c.width, c.height).unwrap();
    got.iou(&truth)
}

#[test]
fn finds_the_frame_and_its_rotation() {
    for (i, angle) in [-2.5, -1.2, 0.0, 0.7, 2.0].into_iter().enumerate() {
        let mut c = Capture::new(1200, 800, angle);
        c.seed = 1.0 + i as f64;
        c.centre = (600.0 + 20.0 * i as f64, 400.0 - 10.0 * i as f64);
        let f = detect(&c, &CameraConfig::default()).unwrap_or_else(|| panic!("angle {angle}: nothing found"));
        assert!(f.edges_found && f.confidence >= 0.9, "angle {angle}: {f:?}");
        assert!((f.angle_deg - angle).abs() < 0.25, "angle {angle}: estimated {}", f.angle_deg);
        let iou = upright_iou(&c, &f);
        assert!(iou > 0.95, "angle {angle}: IoU {iou:.3} {f:?}");
        // The axis-aligned rect covers the rotated frame.
        let bb = f.rotated.bounding_box(c.width, c.height).unwrap();
        assert_eq!(bb, f.rect);
    }
}

#[test]
fn deskew_improves_rotated_captures() {
    let c = Capture::new(1200, 800, 2.5);
    let with = detect(&c, &CameraConfig::default()).unwrap();
    let without = detect(&c, &CameraConfig { deskew: false, ..Default::default() });
    let iou_with = upright_iou(&c, &with);
    let iou_without = without.as_ref().map_or(0.0, |f| upright_iou(&c, f));
    assert!(iou_with > 0.95 && iou_with > iou_without + 0.02, "with {iou_with:.3} without {iou_without:.3} {without:?}");
}

#[test]
fn light_source_outside_without_gate() {
    let mut c = Capture::new(1200, 800, -0.8);
    c.gate = false;
    let f = detect(&c, &CameraConfig::default()).unwrap();
    assert!(f.edges_found, "{f:?}");
    assert!(upright_iou(&c, &f) > 0.93, "{f:?}");
}

#[test]
fn thin_negative_still_framed() {
    let mut c = Capture::new(1200, 800, 0.5);
    c.thin = true;
    let f = detect(&c, &CameraConfig::default()).unwrap();
    assert!(f.edges_found, "{f:?}");
    assert!(upright_iou(&c, &f) > 0.93, "{f:?}");
}

#[test]
fn invisible_edge_is_completed_from_the_aspect() {
    let mut c = Capture::new(1200, 800, 0.0);
    c.base_left_mm = 9.0;
    let f = detect(&c, &CameraConfig::default()).unwrap();
    assert!(f.edges_found && f.confidence < 0.8, "completed boxes are never confident: {f:?}");
    assert!(upright_iou(&c, &f) > 0.93, "{f:?}");
    // Without the shape prior only the visible part (or the aperture) is found.
    let free = detect(&c, &CameraConfig { aspect: 0.0, ..Default::default() }).unwrap();
    assert!(upright_iou(&c, &free) < upright_iou(&c, &f), "{free:?}");
}

#[test]
fn nothing_to_find() {
    let cfg = CameraConfig::default();
    // Bare light table.
    let white = vec![0.9f32; 300 * 200 * 3];
    assert!(detect_camera_frame(&Rgb::new(300, 200, &white).unwrap(), &cfg).unwrap().is_none());
    // Lens cap.
    let black = vec![0.0f32; 300 * 200 * 3];
    assert!(detect_camera_frame(&Rgb::new(300, 200, &black).unwrap(), &cfg).unwrap().is_none());
    // An empty gate (uniform light in a dark surround) is at most the low-confidence aperture.
    let mut gate = vec![0.01f32; 300 * 200 * 3];
    for y in 40..160 {
        for x in 50..250 {
            gate[(y * 300 + x) * 3..][..3].copy_from_slice(&[0.8, 0.8, 0.8]);
        }
    }
    if let Some(f) = detect_camera_frame(&Rgb::new(300, 200, &gate).unwrap(), &cfg).unwrap() {
        assert!(!f.edges_found && f.confidence < 0.5, "{f:?}");
        assert!(f.rect.iou(&PxRect::new(50, 40, 200, 120)) > 0.9);
    }
}

/// A thin negative with a crisp dense subject and a pale base-density border along half of each
/// side: the frame's own edges are weak, while rectangles inside the photograph are strong. Their outside
/// strips run through image content, not one flat base, so none is reported confidently.
#[test]
fn inner_content_rectangle_is_not_confident() {
    let mut c = Capture::new(1200, 800, 0.0);
    c.thin = true;
    c.inner_block = true;
    c.base_border_mm = 3.0;
    let confidently_wrong = |f: &CameraFrame| f.confidence >= 0.8 && upright_iou(&c, f) < 0.9;
    // The inside-texture check is off in both runs, to isolate the outside-strip checks.
    let base = CameraConfig { min_inside_std: 0.0, ..Default::default() };
    let f = detect(&c, &base).unwrap();
    assert!(!confidently_wrong(&f), "{f:?}");
    let lax = detect(&c, &CameraConfig { max_outside_roughness: 10.0, max_outside_spread: 10.0, ..base.clone() }).unwrap();
    assert!(confidently_wrong(&lax), "without the outside checks: {lax:?}");
}

/// Every camera knob at hostile values on a real capture: never a panic.
#[test]
fn hostile_config_values_on_a_real_capture() {
    let c = Capture::new(600, 400, 1.0);
    let data = c.render();
    let img = Rgb::new(c.width, c.height, &data).unwrap();
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 1e12] {
        for k in 0..11 {
            let mut cfg = CameraConfig::default();
            match k {
                0 => cfg.film_threshold = v,
                1 => cfg.edge_threshold = v,
                2 => cfg.min_support = v,
                3 => cfg.confident_support = v,
                4 => cfg.max_outside_spread = v,
                5 => cfg.min_inside_std = v,
                6 => cfg.max_skew_deg = v,
                7 => cfg.aspect = v,
                8 => cfg.aspect_tolerance = v,
                9 => cfg.max_outside_roughness = v,
                _ => cfg.edge_candidates = if v > 0.0 { usize::MAX } else { 0 },
            }
            let _ = detect_camera_frame(&img, &cfg).unwrap();
        }
    }
    for work_size in [0, 1, usize::MAX] {
        let _ = detect_camera_frame(&img, &CameraConfig { work_size, ..Default::default() }).unwrap();
    }
}
