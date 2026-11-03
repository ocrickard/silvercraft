//! The flatbed worker against the simulated V800: prescan → scan frames (dual IR) → manifest →
//! develop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use silvercraft_engine::manifest::RollManifest;
use silvercraft_engine::scanner::{ScanJob, ScannerCommand, ScannerEvent, ScannerWorker};
use silvercraft_engine::{ConfigSources, develop_roll};
use silvercraft_epson::session::{PrescanOptions, Session, SessionConfig};
use silvercraft_epson::{Dpi, FrameBox, PixelRect, SimScanner};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-scanner-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn worker(sim: &SimScanner, dir: &std::path::Path) -> ScannerWorker {
    let opener = sim.opener();
    ScannerWorker::spawn(move || Ok(Session::with_opener(opener, SessionConfig::default())), dir.to_path_buf(), Arc::new(|| {}))
}

/// Events up to and including the next `Idle`.
fn until_idle(w: &ScannerWorker) -> Vec<ScannerEvent> {
    let mut out = Vec::new();
    while let Some(e) = w.recv() {
        let idle = matches!(e, ScannerEvent::Idle);
        out.push(e);
        if idle {
            break;
        }
    }
    out
}

fn small_prescan() -> PrescanOptions {
    PrescanOptions { dpi: Dpi::new(50).unwrap(), width_in: 2.0, height_in: 2.0 }
}

#[test]
fn prescan_then_dual_scan_writes_captures_and_manifest_then_develops() {
    let sim = SimScanner::default();
    let dir = tmp("flow");
    let w = worker(&sim, &dir);
    assert!(matches!(w.recv(), Some(ScannerEvent::Connected)));

    w.send(ScannerCommand::Prescan(small_prescan()));
    let ev = until_idle(&w);
    let (preview, path) = ev
        .iter()
        .find_map(|e| match e {
            ScannerEvent::Preview { preview, path } => Some((preview.clone(), path.clone())),
            _ => None,
        })
        .expect("a preview");
    assert!(path.exists(), "prescan saved");
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::Phase(_))), "progress is reported");

    let roll_dir = dir.join("R-001");
    let frames = vec![
        FrameBox { bounds: PixelRect { x: 10, y: 10, width: 30, height: 20 }, label: "01".into() },
        FrameBox { bounds: PixelRect { x: 10, y: 50, width: 30, height: 20 }, label: "02".into() },
    ];
    w.send(ScannerCommand::Scan(ScanJob {
        frames,
        preview_size: (preview.image.width() as u32, preview.image.height() as u32),
        preview_dpi: 50,
        dpi: 200,
        infrared: true,
        padding_mm: 1.0,
        dir: roll_dir.clone(),
        roll: "R-001".into(),
        stock: "generic-c41".into(),
    }));
    let ev = until_idle(&w);
    let failures: Vec<_> = ev.iter().filter(|e| matches!(e, ScannerEvent::Failed { .. })).collect();
    assert!(failures.is_empty(), "{failures:?}");
    let scanned = ev.iter().filter(|e| matches!(e, ScannerEvent::FrameScanned { .. })).count();
    assert_eq!(scanned, 2);
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::ScanFinished { frames: 2, .. })));

    let m = RollManifest::load(&roll_dir).unwrap();
    assert_eq!(m.frames.len(), 2);
    for f in &m.frames {
        assert!(roll_dir.join(&f.capture).exists());
        assert!(roll_dir.join(f.infrared.as_ref().unwrap()).exists());
        let [x0, y0, x1, y1] = f.frame_box.unwrap();
        let (cw, ch) = f.capture_size.unwrap();
        // the frame sits inside the padded capture, about 1 mm (8 px at 200 dpi) in
        assert!(x0 >= 4 && y0 >= 4 && x1 < cw && y1 < ch, "{:?} in {cw}×{ch}", f.frame_box);
    }

    // the manifest develops as a roll
    let mut req = m.to_request(&roll_dir, ConfigSources { no_user: true, ..Default::default() });
    req.sources.sets.push(format!("output.dir={:?}", roll_dir.join("developed").to_string_lossy()));
    let out = develop_roll(&req, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!(out.written.len(), 2);

    // the viewer finds every developed frame where the develop put it, and shows it and its scan
    let paths = silvercraft_engine::developed_paths(&req).unwrap();
    assert_eq!(paths.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(), out.written);
    let shown = silvercraft_engine::view::render_developed(&paths[0].1).unwrap();
    assert_eq!(shown.rgb.len(), shown.width * shown.height * 3);
    assert!(shown.width > 64 && shown.rgb.iter().any(|&v| v > 0), "full size, not black");
    let scan = silvercraft_engine::view::load_scan(&req.frames[0].path).unwrap();
    assert_eq!(scan.rgb.len(), scan.width * scan.height * 3);
    assert!(silvercraft_engine::view::render_developed(&req.frames[0].path).is_err(), "a scan TIFF is not a developed frame");
}

#[test]
fn cancel_stops_a_scan_and_the_scanner_stays_usable() {
    let sim = SimScanner::default();
    let dir = tmp("cancel");
    let w = worker(&sim, &dir);
    assert!(matches!(w.recv(), Some(ScannerEvent::Connected)));
    w.cancel(); // a cancel before a command is cleared when the command starts
    w.send(ScannerCommand::Prescan(small_prescan()));
    let ev = until_idle(&w);
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::Preview { .. })), "stale cancel ignored: {ev:?}");

    // cancel mid-prescan from another thread once progress starts: a large prescan, so a busy
    // machine cannot finish it before the cancel lands
    w.send(ScannerCommand::Prescan(PrescanOptions { dpi: Dpi::new(300).unwrap(), width_in: 6.0, height_in: 8.0 }));
    let mut cancelled = false;
    let mut sent = false;
    while let Some(e) = w.recv() {
        if matches!(e, ScannerEvent::Phase(_)) && !sent {
            w.cancel();
            sent = true;
        }
        if matches!(e, ScannerEvent::Cancelled) {
            cancelled = true;
        }
        if matches!(e, ScannerEvent::Idle) {
            break;
        }
    }
    assert!(cancelled, "the prescan was cancelled");
    w.send(ScannerCommand::Status);
    let ev = until_idle(&w);
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::Status { clean_idle: true, .. })), "{ev:?}");
}

#[test]
fn cancel_drops_queued_commands_instead_of_running_them() {
    let sim = SimScanner::default();
    let dir = tmp("queued-cancel");
    let w = worker(&sim, &dir);
    assert!(matches!(w.recv(), Some(ScannerEvent::Connected)));
    // A large prescan keeps the worker busy while the scan queues behind it.
    w.send(ScannerCommand::Prescan(PrescanOptions { dpi: Dpi::new(300).unwrap(), width_in: 6.0, height_in: 8.0 }));
    let roll_dir = dir.join("R-queued");
    let mut queued = false;
    let mut events = Vec::new();
    let mut idles = 0;
    while let Some(e) = w.recv() {
        if matches!(e, ScannerEvent::Phase(_)) && !queued {
            w.send(ScannerCommand::Scan(ScanJob {
                frames: vec![FrameBox { bounds: PixelRect { x: 10, y: 10, width: 30, height: 20 }, label: "01".into() }],
                preview_size: (100, 100),
                preview_dpi: 50,
                dpi: 200,
                infrared: false,
                padding_mm: 1.0,
                dir: roll_dir.clone(),
                roll: "R-queued".into(),
                stock: "generic-c41".into(),
            }));
            w.cancel();
            queued = true;
        }
        if matches!(e, ScannerEvent::Idle) {
            idles += 1;
            if idles == 2 {
                events.push(e);
                break;
            }
        }
        events.push(e);
    }
    assert!(queued, "the scan was queued while the prescan ran");
    assert!(!events.iter().any(|e| matches!(e, ScannerEvent::FrameScanned { .. } | ScannerEvent::ScanFinished { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, ScannerEvent::Busy { what } if what == "scan")), "the queued scan never started: {events:?}");
    assert!(!roll_dir.exists(), "the queued scan never ran");
    // The worker is still usable for commands sent after the cancel.
    w.send(ScannerCommand::Status);
    let ev = until_idle(&w);
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::Status { clean_idle: true, .. })), "{ev:?}");
}

#[test]
fn scan_into_a_roll_with_a_corrupt_manifest_fails_closed() {
    let sim = SimScanner::default();
    let dir = tmp("corrupt-manifest");
    let roll_dir = dir.join("R-corrupt");
    std::fs::create_dir_all(&roll_dir).unwrap();
    let torn = r#"{"schema":"silvercraft/roll/1","roll":"R-corrupt","frames":[{"label":"0"#;
    std::fs::write(roll_dir.join("roll.json"), torn).unwrap();
    let w = worker(&sim, &dir);
    assert!(matches!(w.recv(), Some(ScannerEvent::Connected)));
    w.send(ScannerCommand::Scan(ScanJob {
        frames: vec![FrameBox { bounds: PixelRect { x: 10, y: 10, width: 30, height: 20 }, label: "01".into() }],
        preview_size: (100, 100),
        preview_dpi: 50,
        dpi: 200,
        infrared: false,
        padding_mm: 1.0,
        dir: roll_dir.clone(),
        roll: "R-corrupt".into(),
        stock: "generic-c41".into(),
    }));
    let ev = until_idle(&w);
    assert!(ev.iter().any(|e| matches!(e, ScannerEvent::Failed { message, .. } if message.contains("refusing to overwrite"))), "{ev:?}");
    assert!(!ev.iter().any(|e| matches!(e, ScannerEvent::FrameScanned { .. })));
    assert_eq!(std::fs::read_to_string(roll_dir.join("roll.json")).unwrap(), torn, "the corrupt manifest is preserved");
}

#[test]
fn open_failure_is_reported() {
    let w = ScannerWorker::spawn::<silvercraft_epson::sim::SimOpener, _>(
        || Err(silvercraft_epson::EpsonError::InvalidRequest("no scanner".into())),
        tmp("open"),
        Arc::new(|| {}),
    );
    assert!(matches!(w.recv(), Some(ScannerEvent::Failed { .. })));
}

#[test]
fn dropping_the_worker_mid_scan_joins_cleanly() {
    let sim = SimScanner::default();
    let dir = tmp("drop");
    let w = worker(&sim, &dir);
    w.send(ScannerCommand::Prescan(small_prescan()));
    drop(w); // cancels and waits
    let mut s = Session::with_opener(sim.opener(), SessionConfig::default());
    assert!(s.status().unwrap().clean_idle, "left idle");
}
