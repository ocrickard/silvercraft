//! The tether worker with a fake camera: live view, manual capture, autocapture once per frame.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use silvercraft_engine::manifest::RollManifest;
use silvercraft_engine::rotation::Rotation;
use silvercraft_engine::tether::{
    ExposureControl, ExposureSetting, Shot, TetherCamera, TetherCommand, TetherEvent, TetherWorker, decode_jpeg, exposure_label, whole_frame_measure,
};
use silvercraft_ptp::autocapture::AutoCaptureParams;
use silvercraft_ptp::autocapture::Rect;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-tether-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Shows "scene" `n` (a pattern that differs strongly between scenes) in live view.
struct FakeCamera {
    scene: Arc<AtomicU32>,
    shots: Arc<Mutex<u32>>,
}

/// A camera whose exposure compensation can be read and set.
struct ExposureCamera(Arc<Mutex<i64>>);

const COMP_STEPS: [i64; 5] = [-700, -300, 0, 300, 700];

impl TetherCamera for ExposureCamera {
    fn model(&self) -> String {
        "Fake".into()
    }
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String> {
        Ok(jpeg_of(0))
    }
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String> {
        sink.write_all(b"raw").map_err(|e| e.to_string())?;
        Ok(Shot { extension: "arw".into(), bytes: 3, warning: None })
    }
    fn close(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
    fn exposure(&mut self) -> Result<Vec<ExposureSetting>, String> {
        let v = *self.0.lock().unwrap();
        let c = ExposureControl::Compensation;
        Ok(vec![ExposureSetting { control: c, value: v, label: exposure_label(c, v), choices: COMP_STEPS.to_vec(), settable: true }])
    }
    fn set_exposure(&mut self, control: ExposureControl, value: i64) -> Result<(), String> {
        if control != ExposureControl::Compensation || !COMP_STEPS.contains(&value) {
            return Err("no".into());
        }
        *self.0.lock().unwrap() = value;
        Ok(())
    }
}

fn jpeg_of(scene: u32) -> Vec<u8> {
    let (w, h) = (64u16, 48u16);
    let mut rgb = Vec::new();
    for y in 0..h as u32 {
        for x in 0..w as u32 {
            let v = (((x / 8 + y / 8 + scene) % 2) * 200 + 20) as u8;
            let v = if (x + scene * 13) % 64 < 32 { v } else { 255 - v };
            rgb.extend_from_slice(&[v, v, v]);
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 95).encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

impl TetherCamera for FakeCamera {
    fn model(&self) -> String {
        "Fake".into()
    }
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String> {
        Ok(jpeg_of(self.scene.load(Ordering::SeqCst)))
    }
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String> {
        *self.shots.lock().unwrap() += 1;
        sink.write_all(b"II*\0fake raw").map_err(|e| e.to_string())?;
        Ok(Shot { extension: "arw".into(), bytes: 12, warning: None })
    }
    fn close(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}

fn spawn(scene: &Arc<AtomicU32>, shots: &Arc<Mutex<u32>>, params: AutoCaptureParams) -> TetherWorker {
    let (scene, shots) = (scene.clone(), shots.clone());
    TetherWorker::spawn(move || Ok(Box::new(FakeCamera { scene, shots }) as Box<dyn TetherCamera>), whole_frame_measure(), params, Arc::new(|| {}))
}

fn fast_params() -> AutoCaptureParams {
    AutoCaptureParams { settle_frames: 3, settle_seconds: 0.25, max_sample_gap: 1.0, advance_frames: 2, ..AutoCaptureParams::app() }
}

/// Collect events for `d`.
fn drain_for(w: &TetherWorker, d: Duration) -> Vec<TetherEvent> {
    let end = Instant::now() + d;
    let mut out = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if let Some(e) = w.recv_timeout(left) {
            out.push(e);
        }
    }
    out
}

#[test]
fn decodes_live_view_jpeg() {
    let img = decode_jpeg(&jpeg_of(0)).unwrap();
    assert_eq!((img.width, img.height, img.rgb.len()), (64, 48, 64 * 48 * 3));
    assert!(decode_jpeg(b"not a jpeg").is_err());
}

#[test]
fn manual_capture_needs_a_roll_then_records_it() {
    let (scene, shots) = (Arc::new(AtomicU32::new(0)), Arc::new(Mutex::new(0)));
    let w = spawn(&scene, &shots, fast_params());
    assert!(matches!(w.recv(), Some(TetherEvent::Connected { .. })));
    w.send(TetherCommand::Capture);
    assert!(matches!(w.recv_timeout(Duration::from_secs(2)), Some(TetherEvent::Warning(_))));

    let dir = tmp("manual");
    w.send(TetherCommand::SetRoll { dir: dir.clone(), roll: "R".into(), stock: "kodak-portra-400".into() });
    w.send(TetherCommand::Capture);
    let ev = w.recv_timeout(Duration::from_secs(2));
    let Some(TetherEvent::Captured { frame, path, .. }) = ev else { panic!("{ev:?}") };
    assert_eq!(frame.label, "001");
    assert_eq!(std::fs::read(&path).unwrap(), b"II*\0fake raw");
    let m = RollManifest::load(&dir).unwrap();
    assert_eq!(m.stock, "kodak-portra-400");
    assert_eq!(m.frames.len(), 1);

    // a crop set on frame 001 meanwhile (frame viewer) survives the next capture
    let mut m = m;
    m.frames[0].crop = Some([0.1, 0.1, 0.9, 0.9]);
    m.save(&dir).unwrap();
    w.send(TetherCommand::Capture);
    let ev = w.recv_timeout(Duration::from_secs(2));
    assert!(matches!(&ev, Some(TetherEvent::Captured { frame, .. }) if frame.label == "002"), "{ev:?}");
    let m = RollManifest::load(&dir).unwrap();
    assert_eq!(m.frames.len(), 2);
    assert_eq!(m.frames[0].crop, Some([0.1, 0.1, 0.9, 0.9]));
    drop(w);
    assert_eq!(*shots.lock().unwrap(), 2);
}

#[test]
fn autocapture_fires_once_per_settled_frame() {
    let (scene, shots) = (Arc::new(AtomicU32::new(0)), Arc::new(Mutex::new(0)));
    let w = spawn(&scene, &shots, fast_params());
    let dir = tmp("auto");
    w.send(TetherCommand::SetRoll { dir: dir.clone(), roll: "R".into(), stock: "generic-c41".into() });
    w.send(TetherCommand::LiveView(true));
    w.send(TetherCommand::AutoCapture(true));
    let ev = drain_for(&w, Duration::from_millis(1500));
    assert!(ev.iter().any(|e| matches!(e, TetherEvent::Live { auto: Some(_), .. })));
    assert_eq!(*shots.lock().unwrap(), 1, "one capture for the first frame, none while it holds still");

    scene.store(5, Ordering::SeqCst); // advance the film
    drain_for(&w, Duration::from_millis(1500));
    assert_eq!(*shots.lock().unwrap(), 2, "the advanced frame is captured once");
    w.send(TetherCommand::LiveView(false));
    drop(w);
    assert_eq!(RollManifest::load(&dir).unwrap().frames.len(), 2);
}

#[test]
fn open_failure_is_reported() {
    let w = TetherWorker::spawn(|| Err("no camera".into()), whole_frame_measure(), AutoCaptureParams::app(), Arc::new(|| {}));
    assert!(matches!(w.recv(), Some(TetherEvent::Failed(_))));
}

#[test]
fn a_frame_set_by_hand_and_the_rotation_go_with_the_capture() {
    let (scene, shots) = (Arc::new(AtomicU32::new(0)), Arc::new(Mutex::new(0)));
    let w = spawn(&scene, &shots, fast_params());
    let dir = tmp("by-hand");
    w.send(TetherCommand::SetRoll { dir: dir.clone(), roll: "R".into(), stock: "generic-c41".into() });
    // past the image on the right: kept within it
    w.send(TetherCommand::SetFrame(Some(Rect::new(10, 8, 100, 30))));
    w.send(TetherCommand::SetRotation(Rotation::from_degrees(90)));
    w.send(TetherCommand::LiveView(true));
    let ev = drain_for(&w, Duration::from_millis(300));
    assert!(ev.iter().any(|e| matches!(e, TetherEvent::Live { rect: Some(r), .. } if *r == Rect::new(10, 8, 54, 30))), "{ev:?}");
    w.send(TetherCommand::Capture);
    let ev = drain_for(&w, Duration::from_millis(500));
    let frame = ev.iter().find_map(|e| if let TetherEvent::Captured { frame, .. } = e { Some(frame.clone()) } else { None }).unwrap();
    assert_eq!(frame.frame_box, Some([10, 8, 64, 38]));
    assert_eq!(frame.capture_size, Some((64, 48)));
    assert_eq!(frame.rotation.degrees(), 90);
    // back to detection (here: the whole image)
    w.send(TetherCommand::SetFrame(None));
    drain_for(&w, Duration::from_millis(200));
    let ev = drain_for(&w, Duration::from_millis(300));
    assert!(ev.iter().any(|e| matches!(e, TetherEvent::Live { rect: Some(r), .. } if *r == Rect::new(0, 0, 64, 48))));
    w.send(TetherCommand::LiveView(false));
    drop(w);
    assert_eq!(RollManifest::load(&dir).unwrap().frames[0].rotation.degrees(), 90);
}

#[test]
fn exposure_is_read_after_connecting_and_after_a_change() {
    let comp = Arc::new(Mutex::new(0));
    let c = comp.clone();
    let w = TetherWorker::spawn(
        move || Ok(Box::new(ExposureCamera(c)) as Box<dyn TetherCamera>),
        whole_frame_measure(),
        AutoCaptureParams::app(),
        Arc::new(|| {}),
    );
    let settings = |ev: &[TetherEvent]| ev.iter().rev().find_map(|e| if let TetherEvent::Exposure(s) = e { Some(s.clone()) } else { None });
    let s = settings(&drain_for(&w, Duration::from_millis(400))).expect("read after connecting");
    assert_eq!((s[0].value, s[0].label.as_str(), s[0].step(1)), (0, "0 EV", Some(300)));
    w.send(TetherCommand::SetExposure(ExposureControl::Compensation, 300));
    let s = settings(&drain_for(&w, Duration::from_millis(400))).expect("read back after the change");
    assert_eq!((s[0].value, s[0].label.as_str()), (300, "+0.3 EV"));
    assert_eq!(*comp.lock().unwrap(), 300);
    // the camera refuses: a warning, and the setting stays
    w.send(TetherCommand::SetExposure(ExposureControl::Compensation, 123));
    let ev = drain_for(&w, Duration::from_millis(400));
    assert!(ev.iter().any(|e| matches!(e, TetherEvent::Warning(_))));
    assert_eq!(settings(&ev).unwrap()[0].value, 300);
}

#[test]
fn exposure_labels() {
    use ExposureControl::*;
    assert_eq!(exposure_label(Shutter, (1 << 16) | 125), "1/125");
    assert_eq!(exposure_label(Shutter, (4 << 16) | 10), "0.4\"");
    assert_eq!(exposure_label(Shutter, (2 << 16) | 1), "2\"");
    assert_eq!(exposure_label(Shutter, 0), "Bulb");
    assert_eq!(exposure_label(Aperture, 280), "f/2.8");
    assert_eq!(exposure_label(Aperture, 800), "f/8");
    assert_eq!(exposure_label(Iso, 100), "100");
    assert_eq!(exposure_label(Iso, 0x00ff_ffff), "Auto");
    assert_eq!(exposure_label(Compensation, -1700), "-1.7 EV");
    assert_eq!(exposure_label(Mode, 1), "Manual");
}
