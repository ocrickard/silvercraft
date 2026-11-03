//! Camera-roll detection (the roll's frame size as a prior) on synthetic rolls.

mod common;

use common::Capture;
use silvercraft_detect::{
    CameraConfig, CameraFrame, Rgb, RollPrior, RotRect, detect_camera_frame, detect_camera_frame_with_prior, detect_camera_roll, fit_roll_prior,
};

const W: usize = 900;
const H: usize = 600;

fn upright_iou(c: &Capture, f: &CameraFrame) -> f64 {
    let (cx, cy, w, h, _) = c.truth();
    let truth = RotRect { cx, cy, width: w, height: h, angle_deg: 0.0 }.upright_box(c.width, c.height).unwrap();
    let got = RotRect { angle_deg: 0.0, ..f.rotated }.upright_box(c.width, c.height).unwrap();
    got.iou(&truth)
}

fn confident(f: &CameraFrame) -> bool {
    f.edges_found && f.confidence >= 0.8
}

/// A copy-stand roll: `n` captures with a little jitter in position, each with its own image.
fn roll(n: usize, faint: impl Fn(usize) -> bool) -> Vec<Capture> {
    (0..n)
        .map(|i| {
            let mut c = Capture::new(W, H, 0.0);
            c.seed = 1.0 + 0.7 * i as f64;
            c.centre = (W as f64 / 2.0 + [0.0, 2.0, -1.5, 1.0, -2.0][i % 5], H as f64 / 2.0 + [0.0, -1.0, 1.5, 2.0, -0.5][i % 5]);
            c.faint = faint(i);
            c
        })
        .collect()
}

fn run(caps: &[Capture], cfg: &CameraConfig) -> (Vec<Vec<f32>>, silvercraft_detect::RollDetection) {
    let data: Vec<Vec<f32>> = caps.iter().map(Capture::render).collect();
    let imgs: Vec<Rgb<'_, f32>> = caps.iter().zip(&data).map(|(c, d)| Rgb::new(c.width, c.height, d).unwrap()).collect();
    let det = detect_camera_roll(&imgs, cfg);
    (data, det)
}

/// Thin negatives whose frame edges are faint in linear light: single-frame detection finds no
/// confident frame, the roll's frame size (from the well-exposed frames) places every one.
#[test]
fn faint_frames_are_found_with_the_roll_prior() {
    let caps = roll(8, |i| i >= 3);
    let cfg = CameraConfig::default();
    let (data, det) = run(&caps, &cfg);
    let prior = det.prior.expect("a prior from the three well-exposed frames");
    assert_eq!(prior.frames, 3, "{prior:?}");
    assert!((prior.width * W as f64 - caps[0].truth().2).abs() < 0.02 * caps[0].truth().2, "{prior:?}");
    for (i, (c, f)) in caps.iter().zip(&det.frames).enumerate() {
        let single = detect_camera_frame(&Rgb::new(W, H, &data[i]).unwrap(), &cfg).unwrap();
        let got = f.frame.as_ref().unwrap_or_else(|| panic!("frame {i}: nothing"));
        assert!(confident(got), "frame {i}: {f:?}");
        assert!(upright_iou(c, got) > 0.95, "frame {i}: IoU {:.3} {f:?}", upright_iou(c, got));
        if c.faint {
            assert!(f.used_prior && f.sides >= 3, "frame {i}: {f:?}");
            // Without the roll, the frame is not found confidently.
            assert!(!single.as_ref().is_some_and(confident), "frame {i}: single {single:?}");
        } else {
            assert!(!f.used_prior, "frame {i}: a confident frame of the roll's size is kept: {f:?}");
            assert_eq!(single.as_ref(), got.into(), "frame {i}");
        }
    }
    // A side needs a real step: with no threshold, noise would count, so nothing is placed.
    for step in [0.0, -1.0, f64::NAN] {
        let (_, det) = run(&caps, &CameraConfig { prior_edge_step: step, ..cfg.clone() });
        assert!(det.frames.iter().all(|f| !f.used_prior), "{step}: {det:?}");
    }
    // With the roll prior off, every frame is its single-frame result.
    let off = CameraConfig { roll_prior: false, ..cfg };
    let (_, det) = run(&caps, &off);
    assert!(det.prior.is_none());
    assert!(det.frames.iter().filter(|f| f.frame.as_ref().is_some_and(confident)).count() == 3, "{det:?}");
}

/// A frame that really sits elsewhere (the strip was moved) is found where it is, not at the
/// roll's typical position.
#[test]
fn a_displaced_frame_is_not_forced_to_the_typical_position() {
    let mut caps = roll(6, |i| i >= 3);
    let (dx, dy) = (70.0, -35.0);
    caps[4].centre = (caps[4].centre.0 + dx, caps[4].centre.1 + dy);
    let (_, det) = run(&caps, &CameraConfig::default());
    let f = det.frames[4].frame.as_ref().expect("the displaced frame");
    assert!(det.frames[4].used_prior, "{:?}", det.frames[4]);
    assert!(confident(f) && upright_iou(&caps[4], f) > 0.95, "IoU {:.3} {f:?}", upright_iou(&caps[4], f));
    // The typical position would have been well off.
    let mut typical = caps[4].clone();
    typical.centre = caps[0].centre;
    assert!(upright_iou(&typical, f) < 0.85, "{f:?}");
}

/// An unexposed frame has no frame edges: the prior does not invent one.
#[test]
fn a_blank_frame_is_never_confident() {
    let mut caps = roll(5, |_| false);
    caps[3].blank = true;
    let (_, det) = run(&caps, &CameraConfig::default());
    assert!(det.prior.is_some());
    let f = &det.frames[3];
    assert!(!f.frame.as_ref().is_some_and(confident), "{f:?}");
}

/// Too few confident frames, or confident frames that disagree on the size: no prior, and every
/// frame is its single-frame result.
#[test]
fn no_prior_without_agreement() {
    let caps = roll(5, |i| i >= 2);
    let (_, det) = run(&caps, &CameraConfig::default());
    assert!(det.prior.is_none() && det.frames.iter().all(|f| !f.used_prior), "{det:?}");

    let cfg = CameraConfig::default();
    let frame = |w: f64| CameraFrame {
        rect: silvercraft_detect::PxRect::new(0, 0, 10, 10),
        rotated: RotRect { cx: 450.0, cy: 300.0, width: w, height: w / 1.5, angle_deg: 0.0 },
        angle_deg: 0.0,
        confidence: 0.9,
        edges_found: true,
        support: 0.9,
    };
    let spread = [frame(500.0), frame(560.0), frame(620.0), frame(680.0)];
    assert!(fit_roll_prior(spread.iter().map(|f| (f, (W, H))), &cfg).is_none(), "sizes disagree");
    let agree = [frame(600.0), frame(601.0), frame(599.0), frame(450.0)];
    let p = fit_roll_prior(agree.iter().map(|f| (f, (W, H))), &cfg).unwrap();
    assert_eq!(p.frames, 3, "the outlier is left out: {p:?}");
    assert!((p.width * W as f64 - 600.0).abs() < 1.0, "{p:?}");
    let weak = [CameraFrame { confidence: 0.55, ..frame(600.0) }, frame(600.0), frame(600.0)];
    assert!(fit_roll_prior(weak.iter().map(|f| (f, (W, H))), &cfg).is_none(), "only confident frames count");
}

/// Hostile priors, images and configs: never a panic, never a confident frame from nothing.
#[test]
fn hostile_rolls_and_priors() {
    let cfg = CameraConfig::default();
    assert!(detect_camera_roll(&[], &cfg).frames.is_empty());
    let good = Capture::new(W, H, 0.0);
    let data = good.render();
    let img = Rgb::new(W, H, &data).unwrap();
    let ok = RollPrior { width: 0.6, height: 0.6, cx: 0.5, cy: 0.5, angle_deg: 0.0, frames: 3 };
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 1e12, 0.999] {
        for k in 0..5 {
            let mut p = ok;
            match k {
                0 => p.width = v,
                1 => p.height = v,
                2 => p.cx = v,
                3 => p.cy = v,
                _ => p.angle_deg = v,
            }
            let _ = detect_camera_frame_with_prior(&img, &cfg, &p).unwrap();
        }
    }
    // Degenerate and non-finite images, alone and with a prior.
    for (w, h) in [(1, 1), (3, 2), (64, 1), (40, 40), (300, 200)] {
        for v in [0.0f32, 0.5, f32::NAN, f32::INFINITY, -1.0, f32::MAX] {
            let d = vec![v; w * h * 3];
            let im = Rgb::new(w, h, &d).unwrap();
            let r = detect_camera_frame_with_prior(&im, &cfg, &ok).unwrap();
            assert!(!r.frame.as_ref().is_some_and(confident), "{w}x{h} {v}: {r:?}");
            let roll = detect_camera_roll(&[im, img, img, img], &cfg);
            assert_eq!(roll.frames.len(), 4);
        }
    }
    // Captures of different sizes in one roll.
    let small = Capture::new(450, 300, 0.0);
    let sd = small.render();
    let roll = detect_camera_roll(&[img, img, img, Rgb::new(450, 300, &sd).unwrap()], &cfg);
    assert_eq!(roll.frames.len(), 4);
    // Hostile values of the roll knobs: no panic, and a blank frame never becomes confident.
    let mut blank = Capture::new(W, H, 0.0);
    blank.blank = true;
    let bd = blank.render();
    let blank = Rgb::new(W, H, &bd).unwrap();
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 1e12] {
        for k in 0..2 {
            let mut c = cfg.clone();
            match k {
                0 => c.prior_size_tolerance = v,
                _ => c.prior_edge_step = v,
            }
            let roll = detect_camera_roll(&[img, img, img, blank], &c);
            assert!(!roll.frames[3].frame.as_ref().is_some_and(confident), "{k} {v}: {:?}", roll.frames[3]);
            let _ = detect_camera_frame_with_prior(&img, &c, &ok).unwrap();
        }
    }
}
