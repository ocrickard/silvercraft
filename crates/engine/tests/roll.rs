//! End-to-end roll development on synthetic scanner TIFFs.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use silvercraft_engine::{ConfigSources, EngineError, Event, FrameSource, RollRequest, develop_roll};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 16-bit RGB "negative": orange-ish base with a darker (denser) gradient scene.
fn write_negative(path: &Path, w: u32, h: u32, seed: u32) {
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let scene = 0.15 + 0.7 * (x as f32 / w as f32) * (0.5 + 0.5 * ((y + seed * 7) as f32 / h as f32));
            let base = [0.80f32, 0.55, 0.35];
            for b in base {
                // transmittance falls with scene exposure (density rises)
                let t = b * 10f32.powf(-1.2 * scene);
                data.push((t * 65535.0) as u16);
            }
        }
    }
    let f = std::fs::File::create(path).unwrap();
    let mut enc = tiff::encoder::TiffEncoder::new(f).unwrap();
    enc.write_image::<tiff::encoder::colortype::RGB16>(w, h, &data).unwrap();
}

fn request(dir: &Path, frames: Vec<FrameSource>) -> RollRequest {
    RollRequest {
        roll: "Test Roll".into(),
        stock: "generic-c41".into(),
        frames,
        sources: ConfigSources { no_user: true, sets: vec![format!("output.dir={:?}", dir.join("out").to_string_lossy())], ..Default::default() },
    }
}

#[test]
fn develops_a_roll_and_reports_progress_in_order() {
    let dir = tmp("roll");
    let frames: Vec<FrameSource> = (0..3)
        .map(|i| {
            let p = dir.join(format!("f{i}.tif"));
            write_negative(&p, 96, 64, i);
            FrameSource::whole(p)
        })
        .collect();
    let mut events = Vec::new();
    let out = develop_roll(&request(&dir, frames), &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
    assert_eq!(out.written.len(), 3);
    for p in &out.written {
        assert!(p.exists(), "{} missing", p.display());
        assert_eq!(p.extension().unwrap(), "dng");
    }
    assert!(out.dir.join("roll-model.json").exists());
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Config { .. } => Some("config"),
            Event::Measured { .. } => Some("measured"),
            Event::Fitted { .. } => Some("fitted"),
            Event::Developed { .. } => Some("developed"),
            Event::Finished { .. } => Some("finished"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["config", "measured", "measured", "measured", "fitted", "developed", "developed", "developed", "finished"]);
    assert!(out.delivered.is_none(), "handoff.send defaults off");
}

#[test]
fn cancel_stops_before_writing() {
    let dir = tmp("cancel");
    let p = dir.join("f.tif");
    write_negative(&p, 32, 32, 0);
    let r = develop_roll(&request(&dir, vec![FrameSource::whole(p)]), &AtomicBool::new(true), &mut |_| {});
    assert_eq!(r.unwrap_err(), EngineError::Cancelled);
    assert!(!dir.join("out").exists());
}

#[test]
fn labels_that_slug_to_the_same_stem_are_refused_up_front() {
    let dir = tmp("stem-collision");
    let frames: Vec<FrameSource> = ["1/2", "1-2"]
        .iter()
        .map(|label| {
            let mut f = FrameSource::whole(dir.join(format!("{}.tif", label.replace('/', "_"))));
            f.label = (*label).into();
            f
        })
        .collect();
    let e = develop_roll(&request(&dir, frames), &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("both develop to"), "{e}");
    assert!(e.to_string().contains("1/2") && e.to_string().contains("1-2"), "both labels named: {e}");
    assert!(!dir.join("out").exists(), "nothing was written");
}

#[test]
fn unknown_stock_and_bad_config_are_errors() {
    let dir = tmp("errors");
    let p = dir.join("f.tif");
    write_negative(&p, 32, 32, 0);
    let mut req = request(&dir, vec![FrameSource::whole(&p)]);
    req.stock = "no-such-film".into();
    let e = develop_roll(&req, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("no-such-film"), "{e}");

    let mut req = request(&dir, vec![FrameSource::whole(&p)]);
    req.sources.sets.push("film.no_such_key=1".into());
    let mut diags = Vec::new();
    let e = develop_roll(&req, &AtomicBool::new(false), &mut |e| {
        if let Event::Diagnostic(d) = e {
            diags.push(d)
        }
    })
    .unwrap_err();
    assert_eq!(e, EngineError::InvalidConfig);
    assert!(diags.iter().any(|d| d.is_error()), "the cause is reported");
}

#[test]
fn missing_capture_is_an_error_not_a_panic() {
    let dir = tmp("missing");
    let e = develop_roll(&request(&dir, vec![FrameSource::whole(dir.join("nope.tif"))]), &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("nope.tif"), "{e}");
}

#[test]
fn a_developed_roll_can_be_sent_again() {
    let dir = tmp("resend");
    let p = dir.join("f.tif");
    write_negative(&p, 32, 32, 0);
    let mut req = request(&dir, vec![FrameSource::whole(&p)]);
    assert!(silvercraft_engine::roll::send_developed(&req).is_err(), "nothing developed yet");
    develop_roll(&req, &AtomicBool::new(false), &mut |_| {}).unwrap();
    req.sources.sets.push("handoff.route=\"none\"".into());
    let msg = silvercraft_engine::roll::send_developed(&req).unwrap();
    assert!(msg.contains("route is `none`"), "{msg}");
}

#[test]
fn a_crop_set_by_hand_cuts_the_frame_and_one_frame_develops_again_alone() {
    let dir = tmp("crop");
    let frames: Vec<FrameSource> = (0..3)
        .map(|i| {
            let p = dir.join(format!("f{i}.tif"));
            write_negative(&p, 96, 64, i);
            FrameSource::whole(p)
        })
        .collect();
    let out = develop_roll(&request(&dir, frames.clone()), &AtomicBool::new(false), &mut |_| {}).unwrap();
    let before: Vec<std::time::SystemTime> = out.written.iter().map(|p| std::fs::metadata(p).unwrap().modified().unwrap()).collect();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut frames = frames;
    frames[1].crop = Some([0.25, 0.0, 0.75, 0.5]);
    let only = vec!["f1".to_string()];
    let mut req = request(&dir, frames);
    req.sources.sets.push("handoff.send=true".into());
    let mut measured = Vec::new();
    let mut size = None;
    let again = silvercraft_engine::develop_roll_frames(&req, Some(&only), &AtomicBool::new(false), &mut |e| match e {
        Event::Measured { label, .. } => measured.push(label),
        Event::Developed { label, preview, .. } => {
            assert_eq!(label, "f1", "only the cropped frame is written");
            size = preview.map(|t| (t.width, t.height));
        }
        _ => {}
    })
    .unwrap();
    assert!(measured.is_empty(), "the roll's saved model is used, not measured again: {measured:?}");
    let (w, h) = size.unwrap();
    assert!((w as f64 / h as f64 - 1.5).abs() < 0.1, "48 × 32 cut, not the 96 × 64 capture: {w} × {h}");
    assert_eq!(again.written.len(), 1);
    assert!(again.delivered.is_none(), "one frame again is never sent");
    let after: Vec<std::time::SystemTime> = out.written.iter().map(|p| std::fs::metadata(p).unwrap().modified().unwrap()).collect();
    assert_eq!((after[0], after[2]), (before[0], before[2]), "the others are left alone");
    assert!(after[1] > before[1]);

    // without a saved model (or for another stock) the whole roll is measured first
    std::fs::remove_file(dir.join("out").join("roll-model.json")).unwrap();
    let mut sizes = Vec::new();
    silvercraft_engine::develop_roll_frames(&req, Some(&only), &AtomicBool::new(false), &mut |e| {
        if let Event::Measured { label, frame, .. } = e {
            sizes.push((label, frame))
        }
    })
    .unwrap();
    assert_eq!(sizes, [("f0".into(), (96, 64)), ("f1".into(), (48, 32)), ("f2".into(), (96, 64))]);
}
