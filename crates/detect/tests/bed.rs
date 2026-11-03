//! Flatbed prescan detection on synthetic beds.

mod common;

use common::{Bed, Content, Strip};
use silvercraft_detect::{Axis, BedConfig, BedDetection, FilmType, Format35, Format120, Holder, Rgb, StripKind, detect_bed};

fn run(bed: &Bed, cfg: &BedConfig) -> BedDetection {
    let (w, h, data) = bed.render();
    detect_bed(&Rgb::new(w, h, &data).unwrap(), bed.dpi, cfg).unwrap()
}

fn images(n: usize) -> Vec<Content> {
    vec![Content::Image; n]
}

/// Every truth frame of every strip is found once, centred within `tol_mm`.
fn assert_frames(bed: &Bed, det: &BedDetection, counts: &[usize], tol_mm: f64) {
    let ppm = bed.dpi / 25.4;
    assert_eq!(det.strips.len(), counts.len(), "strips: {:#?}\nwarnings {:?}", det.strips, det.warnings);
    for (si, &count) in counts.iter().enumerate() {
        let got: Vec<_> = det.frames.iter().filter(|f| f.strip == si).collect();
        assert_eq!(got.len(), count, "strip {si}: {:#?}", got);
        let truth = bed.frame_centres_px(si);
        for (f, t) in got.iter().zip(&truth) {
            let d = ((f.rotated.cx - t.0).powi(2) + (f.rotated.cy - t.1).powi(2)).sqrt() / ppm;
            assert!(d < tol_mm, "strip {si} frame {}: centre off by {d:.2} mm ({:?} vs {t:?})", f.index, (f.rotated.cx, f.rotated.cy));
        }
        for (k, f) in got.iter().enumerate() {
            assert_eq!(f.index, k);
            assert_eq!(f.label, format!("s{}-f{:02}", si + 1, k + 1));
        }
    }
}

/// The acceptance bed: three skewed strips of negatives, thin skies next to gaps, a false gap of
/// pure base inside a frame, a gap hidden by content, and a short strip.
fn acceptance_bed() -> Bed {
    let mut s0 = images(6);
    s0[1] = Content::SkyAtStart(6.0);
    s0[3] = Content::SkyAtStart(6.0);
    s0[2] = Content::FalseGap(2.0);
    let mut s1 = images(6);
    s1[4] = Content::FalseGap(1.5);
    Bed::strip35(vec![
        Strip { centre_mm: 30.0, angle_deg: 0.6, frames: s0, hidden_gaps: vec![] },
        Strip { centre_mm: 75.0, angle_deg: -0.8, frames: s1, hidden_gaps: vec![3] },
        Strip { centre_mm: 120.0, angle_deg: 0.0, frames: images(4), hidden_gaps: vec![] },
    ])
}

#[test]
fn three_strips_auto() {
    let bed = acceptance_bed();
    let det = run(&bed, &BedConfig::default());
    assert_frames(&bed, &det, &[6, 6, 4], 1.0);
    for s in &det.strips {
        assert_eq!(s.kind, StripKind::Strip35);
        assert_eq!(s.axis, Axis::Vertical);
        assert!((s.pitch_mm.unwrap() - 38.0).abs() < 0.5, "{s:?}");
        assert!((s.across_mm - 25.0).abs() < 0.6, "{s:?}");
    }
    // Skew is measured.
    for (s, want) in det.strips.iter().zip([0.6, -0.8, 0.0]) {
        assert!((s.angle_deg - want).abs() < 0.15, "angle {} vs {want}", s.angle_deg);
    }
    // Millimetre boxes: 25 mm across (the opening) by ~36 mm along, at a 38 mm pitch.
    for f in &det.frames {
        assert!((f.mm.height - 36.0).abs() < 2.0, "{f:?}");
        assert!(!f.blank && !f.partial, "{f:?}");
        // The two frames beside the hidden gap were placed by the comb, not seen: lower confidence.
        let filled = f.strip == 1 && (f.index == 2 || f.index == 3);
        assert_eq!(f.confidence < 0.5, filled, "{f:?}");
    }
}

#[test]
fn three_strips_with_pitch_prior() {
    let bed = acceptance_bed();
    let cfg = BedConfig { holder: Holder::Strip35 { format: Format35::Full }, ..Default::default() };
    let det = run(&bed, &cfg);
    assert_frames(&bed, &det, &[6, 6, 4], 1.0);
    assert!(det.warnings.is_empty(), "{:?}", det.warnings);
}

#[test]
fn strips_running_horizontally() {
    let mut bed = acceptance_bed();
    bed.horizontal = true;
    let det = run(&bed, &BedConfig::default());
    assert_frames(&bed, &det, &[6, 6, 4], 1.0);
    for f in &det.frames {
        assert_eq!(f.axis, Axis::Horizontal);
        assert!(f.bounds.width > f.bounds.height, "{f:?}");
        assert!((f.mm.width - 36.0).abs() < 2.0 && (f.mm.height - 25.0).abs() < 1.5, "{f:?}");
    }
}

#[test]
fn positive_film_has_black_gaps() {
    let mut s0 = images(5);
    s0[2] = Content::SkyAtStart(5.0); // for a slide: thin = bright, not gap-like
    let mut bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.3, frames: s0, hidden_gaps: vec![] },
        Strip { centre_mm: 100.0, angle_deg: 0.0, frames: images(6), hidden_gaps: vec![] },
    ]);
    bed.positive = true;
    let cfg = BedConfig { film: FilmType::Positive, holder: Holder::Strip35 { format: Format35::Full }, ..Default::default() };
    let det = run(&bed, &cfg);
    assert_frames(&bed, &det, &[5, 6], 1.0);
    // With negative polarity the gaps are invisible: nominal placement, weak or no comb.
    let neg = run(&bed, &BedConfig { film: FilmType::Negative, ..cfg });
    assert!(neg.strips.iter().any(|s| s.fallback || s.comb_score.unwrap_or(0.0) < 0.45), "{:#?}", neg.strips);
}

#[test]
fn half_frame_free_pitch_finds_the_fundamental() {
    let mut bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.0, frames: images(11), hidden_gaps: vec![] },
        Strip { centre_mm: 100.0, angle_deg: 0.0, frames: images(10), hidden_gaps: vec![] },
    ]);
    bed.pitch_mm = 19.0;
    bed.frame_mm = 17.6;
    let det = run(&bed, &BedConfig { holder: Holder::Strip35 { format: Format35::Free }, ..Default::default() });
    assert_frames(&bed, &det, &[11, 10], 1.0);
    assert!(det.strips.iter().all(|s| (s.pitch_mm.unwrap() - 19.0).abs() < 0.4), "{:#?}", det.strips);
}

#[test]
fn blank_frames_are_flagged_and_leader_dropped() {
    let mut s0 = images(6);
    s0[2] = Content::Blank;
    s0[5] = Content::Blank; // trailing unexposed frame: leader
    let bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.0, frames: s0, hidden_gaps: vec![] },
        Strip { centre_mm: 100.0, angle_deg: 0.0, frames: images(6), hidden_gaps: vec![] },
    ]);
    let det = run(&bed, &BedConfig { holder: Holder::Strip35 { format: Format35::Full }, ..Default::default() });
    let s0: Vec<_> = det.frames.iter().filter(|f| f.strip == 0).collect();
    assert_eq!(s0.len(), 5, "{s0:#?}");
    assert!(s0[2].blank && s0[2].confidence < 0.5, "{:?}", s0[2]);
    assert!(s0.iter().enumerate().all(|(i, f)| f.blank == (i == 2)));
    let keep = run(&bed, &BedConfig { holder: Holder::Strip35 { format: Format35::Full }, drop_blank_end_frames: false, ..Default::default() });
    assert_eq!(keep.frames.iter().filter(|f| f.strip == 0).count(), 6);
}

#[test]
fn dense_band_does_not_split_a_strip() {
    let mut s0 = images(6);
    s0[2] = Content::Dense(8.0);
    let bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.0, frames: s0, hidden_gaps: vec![] },
        Strip { centre_mm: 100.0, angle_deg: 0.0, frames: images(6), hidden_gaps: vec![] },
    ]);
    let det = run(&bed, &BedConfig::default());
    assert_frames(&bed, &det, &[6, 6], 1.0);
    let split = run(&bed, &BedConfig { merge_gap_mm: 0.0, ..Default::default() });
    assert!(split.strips.len() > 2, "without merging the dense band splits the strip");
}

#[test]
fn medium_format_strips() {
    let mut bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.4, frames: images(3), hidden_gaps: vec![] },
        Strip { centre_mm: 110.0, angle_deg: 0.0, frames: images(3), hidden_gaps: vec![] },
    ]);
    bed.opening_across_mm = 57.0;
    bed.across_mm = 57.0;
    bed.pitch_mm = 58.5;
    bed.frame_mm = 56.0;
    bed.opening_len_mm = 230.0;
    let det = run(&bed, &BedConfig::default());
    assert_frames(&bed, &det, &[3, 3], 1.2);
    assert!(det.strips.iter().all(|s| s.kind == StripKind::Medium120));
    let prior = run(&bed, &BedConfig { holder: Holder::Medium120 { format: Format120::SixBySix }, ..Default::default() });
    assert_frames(&bed, &prior, &[3, 3], 1.2);
    // A 35 mm hint rejects 120-wide openings.
    let wrong = run(&bed, &BedConfig { holder: Holder::Strip35 { format: Format35::Full }, ..Default::default() });
    assert!(wrong.frames.is_empty());
}

/// Bed of separate openings (slides, sheets): holder black, each opening filled with an image.
fn openings_bed(dpi: f64, size_mm: (f64, f64), rects_mm: &[(f64, f64, f64, f64)]) -> (usize, usize, Vec<f32>) {
    let k = dpi / 25.4;
    let (w, h) = ((size_mm.0 * k) as usize, (size_mm.1 * k) as usize);
    let mut out = vec![0.0008f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let (xm, ym) = ((x as f64 + 0.5) / k, (y as f64 + 0.5) / k);
            if let Some((i, r)) = rects_mm.iter().enumerate().find(|(_, r)| xm >= r.0 && xm < r.0 + r.2 && ym >= r.1 && ym < r.1 + r.3) {
                let t = (0.05 + 0.8 * common::texture(xm - r.0, ym - r.1, i as f64)) as f32;
                out[(y * w + x) * 3..][..3].copy_from_slice(&[t, t * 0.9, t * 0.8]);
            }
        }
    }
    (w, h, out)
}

#[test]
fn slide_mounts_are_one_frame_each() {
    let rects = [(20.0, 20.0, 36.0, 24.0), (80.0, 20.0, 36.0, 24.0), (20.0, 80.0, 24.0, 36.0), (80.0, 80.0, 24.0, 36.0)];
    let (w, h, data) = openings_bed(200.0, (140.0, 140.0), &rects);
    for holder in [Holder::Auto, Holder::Slides35] {
        let det = detect_bed(&Rgb::new(w, h, &data).unwrap(), 200.0, &BedConfig { holder, film: FilmType::Positive, ..Default::default() }).unwrap();
        assert_eq!(det.frames.len(), 4, "{holder:?}: {:#?}", det.frames);
        for (f, r) in det.frames.iter().zip(rects) {
            assert!((f.mm.x - r.0).abs() < 0.6 && (f.mm.y - r.1).abs() < 0.6, "{f:?} vs {r:?}");
            assert!((f.mm.width - r.2).abs() < 1.0 && (f.mm.height - r.3).abs() < 1.0, "{f:?} vs {r:?}");
            assert!(det.strips[f.strip].kind == StripKind::Slide);
        }
    }
}

#[test]
fn sheet_film_is_one_frame() {
    let rects = [(15.0, 15.0, 96.0, 120.0)];
    let (w, h, data) = openings_bed(100.0, (130.0, 150.0), &rects);
    for holder in [Holder::Auto, Holder::Sheet4x5] {
        let det = detect_bed(&Rgb::new(w, h, &data).unwrap(), 100.0, &BedConfig { holder, ..Default::default() }).unwrap();
        assert_eq!(det.frames.len(), 1, "{holder:?}");
        assert_eq!(det.strips[0].kind, StripKind::Sheet);
        let f = &det.frames[0];
        assert!((f.mm.width - 96.0).abs() < 1.5 && (f.mm.height - 120.0).abs() < 1.5, "{f:?}");
    }
}

/// Every bed knob at hostile values on a real bed: results or empty, never a panic or a hang.
#[test]
fn hostile_config_values_on_a_real_bed() {
    let bed = Bed::strip35(vec![
        Strip { centre_mm: 40.0, angle_deg: 0.5, frames: images(5), hidden_gaps: vec![] },
        Strip { centre_mm: 100.0, angle_deg: 0.0, frames: images(3), hidden_gaps: vec![] },
    ]);
    let (w, h, data) = bed.render();
    let img = Rgb::new(w, h, &data).unwrap();
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 1e9];
    for v in bad {
        for k in 0..17 {
            let mut c =
                BedConfig { holder: if k % 2 == 0 { Holder::Auto } else { Holder::Strip35 { format: Format35::Full } }, ..Default::default() };
            match k {
                0 => c.open_threshold = v,
                1 => c.bare_level = v,
                2 => c.bare_max_chroma = v,
                3 => c.min_across_mm = v,
                4 => c.min_along_mm = v,
                5 => c.merge_gap_mm = v,
                6 => c.edge_inset = v,
                7 => c.gap_level_tolerance = v,
                8 => c.gap_level_tolerance_positive = v,
                9 => c.gap_uniformity = v,
                10 => c.pitch_tolerance = v,
                11 => c.pitch_tolerance_wide = v,
                12 => c.smaller_pitch_preference = v,
                13 => c.min_comb_score = v,
                14 => c.weak_comb_score = v,
                15 => c.film = FilmType::Positive,
                _ => c.consensus_pitch = false,
            }
            let _ = detect_bed(&img, bed.dpi, &c).unwrap();
        }
    }
}
