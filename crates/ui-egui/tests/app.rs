//! The app headless: simulated scanner and a fake camera, driven through the same methods the
//! buttons call, rendering every tab along the way.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use silvercraft_engine::hardware;
use silvercraft_engine::tether::{Shot, TetherCamera, TetherWorker, whole_frame_measure};
use silvercraft_epson::{FrameBox, PixelRect, SimScanner};
use silvercraft_film::profiles::ProfileRegistry;
use silvercraft_ui_egui::{AppSettings, Services, SilverApp, Tab};

struct FakeCamera;

impl TetherCamera for FakeCamera {
    fn model(&self) -> String {
        "Fake camera".into()
    }
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String> {
        Err("no live view".into())
    }
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String> {
        sink.write_all(b"II*\0").map_err(|e| e.to_string())?;
        Ok(Shot { extension: "arw".into(), bytes: 4, warning: None })
    }
    fn close(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}

/// A camera with live view: a 64 × 48 picture.
struct LiveCamera;

impl TetherCamera for LiveCamera {
    fn model(&self) -> String {
        "Live camera".into()
    }
    fn live_view_jpeg(&mut self) -> Result<Vec<u8>, String> {
        let rgb: Vec<u8> = (0..64 * 48).flat_map(|i| [(i % 64 * 4) as u8, 120, (i / 64 * 5) as u8]).collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90).encode(&rgb, 64, 48, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())?;
        Ok(out)
    }
    fn capture(&mut self, sink: &mut dyn Write) -> Result<Shot, String> {
        sink.write_all(b"II*\0").map_err(|e| e.to_string())?;
        Ok(Shot { extension: "arw".into(), bytes: 4, warning: None })
    }
    fn close(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-ui-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn app(rolls: &std::path::Path) -> SilverApp {
    let services = Services {
        scanner: hardware::simulated_scanner(SimScanner::default()),
        camera: Arc::new(|m, p, w| TetherWorker::spawn(|| Ok(Box::new(FakeCamera) as Box<dyn TetherCamera>), m, p, w)),
        frame_measure: Arc::new(whole_frame_measure),
        detect: Some(Arc::new(|_p| vec![FrameBox { bounds: PixelRect { x: 40, y: 40, width: 120, height: 80 }, label: "x".into() }])),
        settings_path: None,
    };
    let settings = AppSettings { rolls_dir: rolls.to_path_buf(), scan_dpi: 400, send_to_lightcraft: false, ..Default::default() };
    SilverApp::new(services, settings, ProfileRegistry::builtin())
}

struct Driver {
    ctx: egui::Context,
    app: SilverApp,
    time: f64,
}

impl Driver {
    fn frame(&mut self) {
        self.time += 1.0 / 30.0;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            time: Some(self.time),
            ..Default::default()
        };
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        out.textures_delta.clear();
    }

    fn key(&mut self, key: egui::Key) {
        self.time += 1.0 / 30.0;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            time: Some(self.time),
            events: vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }],
            ..Default::default()
        };
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        out.textures_delta.clear();
    }

    fn until(&mut self, what: &str, mut cond: impl FnMut(&SilverApp) -> bool) {
        let end = Instant::now() + Duration::from_secs(60);
        while !cond(&self.app) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            self.frame();
            std::thread::sleep(Duration::from_millis(5));
        }
        self.frame();
    }

    fn every_tab(&mut self) {
        let keep = self.app.tab;
        for t in [Tab::Scanner, Tab::Camera, Tab::Rolls, Tab::Stocks] {
            self.app.tab = t;
            self.frame();
        }
        self.app.tab = keep;
    }
}

#[test]
fn scanner_workflow_from_prescan_to_developed_roll() {
    let rolls = tmp("scan");
    let ctx = egui::Context::default();
    silvercraft_ui_egui::theme::apply(&ctx);
    let mut d = Driver { ctx: ctx.clone(), app: app(&rolls), time: 0.0 };
    d.every_tab();

    d.app.scanner.connect(&d.app.services.clone(), &d.app.shared, &ctx);
    d.until("connect", |a| a.scanner.connected);
    d.app.scanner.prescan();
    d.until("prescan", |a| a.scanner.preview.is_some() && a.scanner.busy.is_none());
    assert_eq!(d.app.scanner.editor.frames.len(), 1, "detection ran on the preview");
    assert_eq!(d.app.scanner.editor.frames[0].label, "01", "and was renumbered");
    d.every_tab();

    d.app.shared.settings.develop_after_capture = true;
    d.app.scanner.scan(&d.app.shared);
    d.until("scan + develop", |a| a.tab == Tab::Rolls && !a.rolls.running() && a.rolls.outcome.is_some());
    let outcome = d.app.rolls.outcome.clone().unwrap();
    let o = outcome.unwrap_or_else(|e| panic!("develop failed: {e}\n{}", d.app.rolls.log.join("\n")));
    assert_eq!(o.written.len(), 1);
    assert_eq!(d.app.rolls.thumbnails.len(), 1, "a thumbnail per developed frame");
    assert!(o.dir.starts_with(rolls.join("R-001")), "{}", o.dir.display());
    d.every_tab();
    assert_eq!(d.app.rolls.rolls.len(), 1, "the roll is listed");

    // a frame at full size: developed first, then the scan; Esc goes back to the grid
    d.app.tab = Tab::Rolls;
    d.app.rolls.open_viewer(0, &d.app.shared);
    assert_eq!(d.app.rolls.viewer.as_ref().map(|v| v.source), Some(silvercraft_ui_egui::viewer::Source::Developed));
    d.until("developed frame shown", |a| a.rolls.viewer.as_ref().is_some_and(|v| v.shown()));
    d.app.rolls.viewer.as_mut().unwrap().source = silvercraft_ui_egui::viewer::Source::Scan;
    d.until("scan shown", |a| a.rolls.viewer.as_ref().is_some_and(|v| v.shown()));
    assert!(d.app.rolls.viewer.as_ref().unwrap().error().is_none());

    // crop it (C), keep the left half (Enter): saved in the roll, and the frame develops again
    let dng = o.written[0].clone();
    let full = silvercraft_engine::view::render_developed(&dng).unwrap();
    d.key(egui::Key::C);
    let start = d.app.rolls.viewer.as_ref().unwrap().cropping.expect("C starts a crop").crop;
    d.until("whole scan shown to crop", |a| a.rolls.viewer.as_ref().is_some_and(|v| v.shown()));
    let half = [start[0], start[1], (start[0] + start[2]) / 2.0, start[3]];
    d.app.rolls.viewer.as_mut().unwrap().cropping.as_mut().unwrap().crop = half;
    d.key(egui::Key::Enter);
    let m = silvercraft_engine::manifest::RollManifest::load(&rolls.join("R-001")).unwrap();
    assert_eq!(m.frames[0].crop, Some(half));
    d.until("frame developed again", |a| a.rolls.redevelop.is_empty() && !a.rolls.running());
    let cropped = silvercraft_engine::view::render_developed(&dng).unwrap();
    let (fw, cw) = (full.width, cropped.width);
    assert!((cw as f64 - fw as f64 / 2.0).abs() <= 2.0, "half as wide: {cw} of {fw}\n{}", d.app.rolls.log.join("\n"));
    d.until("developed again shown", |a| {
        a.rolls.viewer.as_ref().is_some_and(|v| v.shown() && v.source == silvercraft_ui_egui::viewer::Source::Developed)
    });
    assert_eq!(d.app.rolls.thumbnails.len(), 1, "its thumbnail replaced, not added");

    d.key(egui::Key::Escape);
    assert!(d.app.rolls.viewer.is_none(), "Esc closes the viewer");
    d.app.scanner.disconnect();
}

#[test]
fn a_crashed_scanner_worker_unwedges_the_scanner_tab() {
    let rolls = tmp("worker-crash");
    let ctx = egui::Context::default();
    silvercraft_ui_egui::theme::apply(&ctx);
    let mut a = app(&rolls);
    a.services.scanner = Arc::new(|dir, wake| {
        silvercraft_engine::scanner::ScannerWorker::spawn::<silvercraft_epson::sim::SimOpener, _>(|| panic!("simulated open crash"), dir, wake)
    });
    let mut d = Driver { ctx: ctx.clone(), app: a, time: 0.0 };
    d.app.scanner.connect(&d.app.services.clone(), &d.app.shared, &ctx);
    // The panic is caught in the worker, reported, and the dead thread detected:
    // the tab returns to the connect state instead of claiming a live scanner.
    d.until("worker death", |a| a.scanner.worker_absent() && a.scanner.error.is_some());
    assert!(!d.app.scanner.connected);
    assert!(d.app.scanner.busy.is_none());
}

#[test]
fn camera_capture_records_a_roll() {
    let rolls = tmp("cam");
    let ctx = egui::Context::default();
    let mut d = Driver { ctx: ctx.clone(), app: app(&rolls), time: 0.0 };
    d.app.tab = Tab::Camera;
    d.app.camera.connect(&d.app.services.clone(), &ctx);
    d.until("camera", |a| a.camera.model.is_some());
    d.frame(); // the roll folder is sent to the worker
    d.app.camera.capture();
    d.until("capture", |a| !a.camera.captured.is_empty());
    assert!(rolls.join("R-001").join("001.arw").exists());
    d.app.camera.set_live(true);
    d.until("live-view warning", |a| a.camera.notes.iter().any(|n| n.contains("live view")));
    d.app.camera.disconnect();
    d.every_tab();
}

#[test]
fn camera_turns_the_view_captures_with_space_keeps_a_frame_set_by_hand_and_develops_by_itself() {
    let rolls = tmp("cam-live");
    let ctx = egui::Context::default();
    silvercraft_ui_egui::theme::apply(&ctx);
    let mut a = app(&rolls);
    a.services.camera = Arc::new(|m, p, w| TetherWorker::spawn(|| Ok(Box::new(LiveCamera) as Box<dyn TetherCamera>), m, p, w));
    let mut d = Driver { ctx: ctx.clone(), app: a, time: 0.0 };
    d.app.tab = Tab::Camera;
    d.app.camera.connect(&d.app.services.clone(), &ctx);
    d.until("camera", |a| a.camera.model.is_some());
    d.app.camera.set_live(true);
    d.until("live view", |a| a.camera.live_view_size() == Some([64, 48]));

    // R turns it a quarter turn clockwise; the live view arrives turned
    d.key(egui::Key::R);
    assert_eq!(d.app.shared.settings.camera_rotation.degrees(), 90);
    d.until("turned live view", |a| a.camera.live_view_size() == Some([48, 64]));

    // the frame, set by hand (as a drag on its edges does), replaces detection
    d.app.camera.set_frame(Some(silvercraft_ptp::autocapture::Rect::new(4, 6, 50, 30)));
    d.frame();
    std::thread::sleep(Duration::from_millis(200));

    // Space captures
    d.key(egui::Key::Space);
    d.until("capture", |a| !a.camera.captured.is_empty());
    let f = &d.app.camera.captured[0];
    assert_eq!((f.rotation.degrees(), f.frame_box), (90, Some([4, 6, 54, 36])));
    let m = silvercraft_engine::manifest::RollManifest::load(&rolls.join("R-001")).unwrap();
    assert_eq!(m.frames[0].rotation.degrees(), 90);

    // a while after the last capture the roll develops, without leaving the camera
    assert!(!d.app.rolls.running());
    d.time += 20.0;
    d.until("background develop", |a| a.rolls.running() || a.rolls.outcome.is_some());
    assert_eq!(d.app.tab, Tab::Camera);
    d.until("develop finished", |a| !a.rolls.running());

    d.app.camera.set_live(false);
    d.app.camera.disconnect();
    d.every_tab();
}

#[test]
fn settings_are_saved_when_changed() {
    let rolls = tmp("settings");
    let ctx = egui::Context::default();
    let mut a = app(&rolls);
    let path = rolls.join("app.json");
    a.services.settings_path = Some(path.clone());
    let mut d = Driver { ctx, app: a, time: 0.0 };
    d.frame();
    assert!(!path.exists(), "nothing changed yet");
    d.app.shared.settings.scan_dpi = 6400;
    d.frame();
    assert_eq!(AppSettings::load(&path).scan_dpi, 6400);
}

#[test]
fn asks_the_window_manager_for_a_dark_title_bar() {
    let ctx = egui::Context::default();
    let mut app = app(&tmp("titlebar"));
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..Default::default() };
    let mut out = ctx.run_ui(raw, |ui| app.ui(ui));
    out.textures_delta.clear();
    let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
    assert!(cmds.contains(&egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark)), "{cmds:?}");
}

#[test]
fn dragging_the_top_bar_moves_the_window_every_time_though_the_window_manager_keeps_the_release() {
    let ctx = egui::Context::default();
    let mut app = app(&tmp("drag"));
    app.integrated_titlebar = true;
    app.window_controls = Some(silvercraft_ui_egui::titlebar::Controls::parse("close,minimize,maximize:"));
    let time = std::cell::Cell::new(0.0);
    let frame = |app: &mut SilverApp, events: Vec<egui::Event>| {
        time.set(time.get() + 0.05);
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            time: Some(time.get()),
            events,
            ..Default::default()
        };
        // as eframe's raw_input_hook does
        silvercraft_ui_egui::titlebar::release_after_handover(&ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.ui(ui));
        out.textures_delta.clear();
        out.viewport_output[&egui::ViewportId::ROOT].commands.clone()
    };
    let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
    for _ in 0..3 {
        frame(&mut app, vec![]);
    }
    let mut misses = vec![];
    for x in [300.0f32, 420.0, 1000.0, 1100.0, 300.0, 1000.0, 450.0] {
        for y in [8.0f32, 21.0, 34.0] {
            let p = egui::pos2(x, y);
            let mut cmds = frame(&mut app, vec![egui::Event::PointerMoved(p)]);
            cmds.extend(frame(&mut app, vec![button(p, true)]));
            for dx in [3.0, 8.0, 14.0, 20.0] {
                cmds.extend(frame(&mut app, vec![egui::Event::PointerMoved(p + egui::vec2(dx, 0.0))]));
            }
            // the window manager owns the pointer from StartDrag on: the release never reaches us
            frame(&mut app, vec![egui::Event::PointerGone]);
            time.set(time.get() + 1.0); // never a double-click
            if !cmds.contains(&egui::ViewportCommand::StartDrag) {
                misses.push((x, y, cmds));
            }
        }
    }
    assert!(misses.is_empty(), "{misses:?}");
}
