//! A camera roll developed end to end: faint frames are framed with the roll's frame size.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use silvercraft_engine::{ConfigSources, Event, FrameSource, RollRequest, develop_roll};
use silvercraft_handoff::dng::{DngFrameInfo, LinearFrame};
use silvercraft_handoff::write::{XmpPlacement, write_frame_with};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sc-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const W: usize = 900;
const H: usize = 600;
/// Pixels per mm of film.
const S: f64 = 15.0;
/// The frame (36 × 24 mm) in the capture: `[x0, y0, x1, y1]`.
const FRAME: [usize; 4] = [180, 120, 720, 480];

fn noise(x: usize, y: usize) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    (h % 2001) as f64 / 1000.0 - 1.0
}

/// A camera capture of one 35 mm frame in a dark holder: orange base, a 2 mm gap to the next
/// frames, rebate above and below. A `faint` frame is a thin negative: a pale wash denser than
/// base in blue only, with a subject in the middle.
fn capture(seed: f64, faint: bool) -> Vec<f32> {
    let base = [0.45f64, 0.28, 0.16];
    let mut out = Vec::with_capacity(W * H * 3);
    for py in 0..H {
        for px in 0..W {
            let (u, v) = ((px as f64 + 0.5 - 450.0) / S, (py as f64 + 0.5 - 300.0) / S);
            let n = 1.0 + 0.004 * noise(px, py);
            if v.abs() > 15.5 || u.abs() > 22.0 {
                out.extend([0.01 * n as f32; 3]);
                continue;
            }
            let local = (u + 19.0).rem_euclid(38.0);
            let t = 0.5 + 0.25 * ((local * 0.31 + seed).sin() * (v * 0.17 + 2.0 * seed).cos()) + 0.2 * (local * 0.07 + v * 0.11 + 3.0 * seed).sin();
            let d = if v.abs() >= 12.0 || !(1.0..37.0).contains(&local) {
                [0.0; 3]
            } else if faint && !((local - 19.0).abs() < 8.0 && v.abs() < 6.0) {
                [0.0, 0.004, 0.04 + 0.01 * t]
            } else if faint {
                [0.1 + 0.5 * t, 0.1 + 0.5 * t, 0.15 + 0.5 * t]
            } else {
                [0.3 + 1.1 * t; 3]
            };
            for c in 0..3 {
                out.push((base[c] * 10f64.powf(-d[c]) * n) as f32);
            }
        }
    }
    out
}

fn write_roll(dir: &Path, faint: &[bool]) -> Vec<FrameSource> {
    faint
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            let rgb = capture(1.0 + 0.7 * i as f64, f);
            let frame = LinearFrame { width: W, height: H, rgb: &rgb };
            let placement = XmpPlacement { sidecar: false, embed: false };
            let p =
                write_frame_with(&dir.join("captures"), &format!("c{i}"), &frame, &DngFrameInfo::default(), &Default::default(), placement).unwrap();
            FrameSource::whole(p)
        })
        .collect()
}

fn develop(dir: &Path, frames: Vec<FrameSource>, sets: &[&str]) -> (Vec<Event>, Vec<PathBuf>) {
    let mut s = vec![format!("output.dir={:?}", dir.join("out").to_string_lossy())];
    s.extend(sets.iter().map(|x| x.to_string()));
    let req = RollRequest {
        roll: "Camera Roll".into(),
        stock: "generic-c41".into(),
        frames,
        sources: ConfigSources { no_user: true, sets: s, ..Default::default() },
    };
    let mut events = Vec::new();
    let out = develop_roll(&req, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
    (events, out.written)
}

fn dng_size(p: &Path) -> (usize, usize) {
    let c = silvercraft_decode::decode_file(p, &Default::default()).unwrap();
    (c.rgb.width(), c.rgb.height())
}

#[test]
fn faint_frames_are_framed_with_the_roll_size() {
    let dir = tmp("camroll");
    let faint = [false, false, false, true, true, false];
    let frames = write_roll(&dir, &faint);
    let (events, written) = develop(&dir, frames.clone(), &[]);
    let (fw, fh) = (FRAME[2] - FRAME[0], FRAME[3] - FRAME[1]);
    let near = |(w, h): (usize, usize)| w.abs_diff(fw) <= 12 && h.abs_diff(fh) <= 12;
    let reframed: Vec<(usize, (usize, usize), bool, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Reframed { index, frame, gap_base, confident, .. } => Some((*index, *frame, gap_base.is_some(), *confident)),
            _ => None,
        })
        .collect();
    // The faint frames (and any well-exposed one that was not confident alone).
    assert!([3, 4].iter().all(|i| reframed.iter().any(|r| r.0 == *i)), "{reframed:?}");
    for (i, frame, gap, confident) in &reframed {
        assert!(near(*frame) && *gap && *confident, "frame {i}: {frame:?} gap base {gap} confident {confident}");
    }
    // Pass 1 measured the faint frames alone first: the gate opening, no film base.
    for e in &events {
        if let Event::Measured { index: 3 | 4, frame, gap_base, .. } = e {
            assert!(!near(*frame) && gap_base.is_none(), "{e:?}");
        }
    }
    // Every reframed event comes after every measurement, before the model is fitted.
    let pos = |f: &dyn Fn(&Event) -> bool| events.iter().position(f).unwrap();
    let last_measured = events.iter().rposition(|e| matches!(e, Event::Measured { .. })).unwrap();
    assert!(last_measured < pos(&|e| matches!(e, Event::Reframed { .. })));
    assert!(events.iter().rposition(|e| matches!(e, Event::Reframed { .. })).unwrap() < pos(&|e| matches!(e, Event::Fitted { .. })));
    // Pass 2 develops the frame pass 1 settled on.
    for (i, p) in written.iter().enumerate() {
        assert!(near(dng_size(p)), "frame {i}: {:?}", dng_size(p));
    }

    // With the roll prior off, the faint frames stay at the gate opening.
    let dir = tmp("camroll-off");
    let (events, written) = develop(&dir, write_roll(&dir, &faint), &["detect.camera.roll_prior=false"]);
    assert!(!events.iter().any(|e| matches!(e, Event::Reframed { .. })));
    assert!(!near(dng_size(&written[3])), "{:?}", dng_size(&written[3]));
    assert!(near(dng_size(&written[0])));
}
