//! Live-view measurement for autocapture on synthetic sequences.

mod common;

use common::Capture;
use silvercraft_detect::{CameraConfig, LiveConfig, LiveFrame, PxRect, RotRect, detect_live};

fn live(c: &Capture) -> LiveFrame {
    detect_live(&c.render_u8(), c.width, c.height, &LiveConfig::default(), &CameraConfig::default()).unwrap()
}

fn truth(c: &Capture) -> PxRect {
    let (cx, cy, w, h, a) = c.truth();
    RotRect { cx, cy, width: w, height: h, angle_deg: a }.bounding_box(c.width, c.height).unwrap()
}

/// The strip is advanced (the frame slides in from the right), then settles centred. While the
/// frame is cut by the view edge it is never confident; once settled every sample is confident,
/// centred and stable, so the ptp state machine can settle and fire once.
#[test]
fn advance_then_settle() {
    let (w, h) = (640, 427);
    let mut seq: Vec<f64> = vec![1.05, 0.95, 0.85, 0.75, 0.65, 0.57];
    seq.extend([0.5; 6]);
    let mut settled = Vec::new();
    for (i, fx) in seq.iter().enumerate() {
        let mut c = Capture::new(w, h, 0.3);
        c.scale = w as f64 / 64.0;
        c.centre = (fx * w as f64, h as f64 / 2.0);
        let m = live(&c);
        let cut = (c.centre.0 + 18.0 * c.scale) > w as f64;
        if cut {
            assert!(!m.confident, "sample {i}: frame cut by the view edge must not be confident: {m:?}");
        }
        if *fx == 0.5 {
            assert!(m.confident && m.in_view && m.centred, "sample {i}: {m:?}");
            let r = m.rect.unwrap();
            assert!(r.iou(&truth(&c)) > 0.9, "sample {i}: {r:?} vs {:?}", truth(&c));
            settled.push(r);
        }
    }
    // Stable rect across the settled samples (identical input → identical output).
    assert!(settled.windows(2).all(|p| p[0] == p[1]), "{settled:?}");
}

#[test]
fn off_centre_frame_is_reported() {
    let mut c = Capture::new(640, 427, 0.0);
    c.scale = 640.0 / 70.0;
    c.centre = (0.33 * 640.0, 0.5 * 427.0);
    let m = live(&c);
    assert!(m.rect.is_some() && m.in_view && m.confident && !m.centred, "{m:?}");
    assert!((m.offset.0 + 0.17).abs() < 0.02, "{m:?}");
}

#[test]
fn empty_gate_is_not_confident() {
    let (w, h) = (320, 214);
    let mut img = vec![3u8; w * h * 3];
    for y in 40..170 {
        for x in 40..280 {
            img[(y * w + x) * 3..][..3].copy_from_slice(&[200, 150, 110]);
        }
    }
    let m = detect_live(&img, w, h, &LiveConfig::default(), &CameraConfig::default()).unwrap();
    assert!(!m.confident, "{m:?}");
    let dark = vec![0u8; w * h * 3];
    let m = detect_live(&dark, w, h, &LiveConfig::default(), &CameraConfig::default()).unwrap();
    assert!(m.rect.is_none() && !m.confident);
}

#[test]
fn full_size_live_frame() {
    let c = Capture::new(1024, 683, -0.6);
    let m = live(&c);
    assert!(m.confident, "{m:?}");
    assert!(m.rect.unwrap().iou(&truth(&c)) > 0.9, "{m:?}");
}

/// Throughput check for the 30 fps budget (run with `--release -- --ignored --nocapture`).
#[test]
#[ignore]
fn live_timing() {
    let c = Capture::new(1024, 683, -0.6);
    let img = c.render_u8();
    let (live, cam) = (LiveConfig::default(), CameraConfig::default());
    let _ = detect_live(&img, 1024, 683, &live, &cam).unwrap();
    let n = 60;
    let t = std::time::Instant::now();
    for _ in 0..n {
        assert!(detect_live(&img, 1024, 683, &live, &cam).unwrap().confident);
    }
    let per = t.elapsed() / n;
    let deskew = LiveConfig { deskew: true, ..live };
    let t2 = std::time::Instant::now();
    for _ in 0..n {
        let _ = detect_live(&img, 1024, 683, &deskew, &cam).unwrap();
    }
    eprintln!("detect_live 1024x683: {per:?} per frame; with deskew {:?}", t2.elapsed() / n);
}
