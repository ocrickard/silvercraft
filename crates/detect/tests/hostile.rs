//! Hostile inputs: errors, never panics.

use proptest::prelude::*;
use silvercraft_detect::{
    BedConfig, CameraConfig, DetectError, FilmType, Format35, Format120, Holder, LiveConfig, Rgb, RollPrior, detect_bed, detect_camera_frame,
    detect_camera_frame_with_prior, detect_camera_roll, detect_live,
};

#[test]
fn malformed_buffers_are_errors() {
    assert!(matches!(Rgb::new(0, 10, &[] as &[f32]), Err(DetectError::Empty { .. })));
    assert!(matches!(Rgb::new(4, 4, &[0.0f32; 47]), Err(DetectError::BufferSize { .. })));
    assert!(matches!(Rgb::new(usize::MAX, 3, &[0.0f32; 9]), Err(DetectError::Overflow { .. })));
    assert!(detect_live(&[0u8; 10], 4, 4, &LiveConfig::default(), &CameraConfig::default()).is_err());
    assert!(detect_live(&[], 0, 0, &LiveConfig::default(), &CameraConfig::default()).is_err());
    let data = vec![0.5f32; 30 * 30 * 3];
    let img = Rgb::new(30, 30, &data).unwrap();
    for dpi in [0.0, -200.0, f64::NAN, f64::INFINITY, 1e9, 5.0] {
        assert!(matches!(detect_bed(&img, dpi, &BedConfig::default()), Err(DetectError::Param(_))), "dpi {dpi}");
    }
}

#[test]
fn degenerate_images_find_nothing() {
    let cfg = BedConfig::default();
    for (w, h) in [(1, 1), (2, 3), (3, 2), (64, 1), (1, 64)] {
        for v in [0.0f32, 1.0, f32::NAN, f32::INFINITY, -1.0, f32::MAX] {
            let data = vec![v; w * h * 3];
            let img = Rgb::new(w, h, &data).unwrap();
            let bed = detect_bed(&img, 200.0, &cfg).unwrap();
            assert!(bed.frames.is_empty(), "{w}x{h} {v}");
            assert!(detect_camera_frame(&img, &CameraConfig::default()).unwrap().is_none(), "{w}x{h} {v}");
        }
    }
    // Large flat images: nothing to find, no panic.
    for v in [0.0f32, 0.5, 1.0, f32::NAN] {
        let data = vec![v; 400 * 300 * 3];
        let img = Rgb::new(400, 300, &data).unwrap();
        assert!(detect_bed(&img, 100.0, &cfg).unwrap().frames.is_empty());
        let _ = detect_camera_frame(&img, &CameraConfig::default()).unwrap();
    }
}

#[test]
fn non_finite_pixels_inside_a_real_bed() {
    // A bright strip with NaN and inf sprinkled in.
    let (w, h) = (300, 600);
    let mut data = vec![0.0005f32; w * h * 3];
    for y in 50..550 {
        for x in 100..200 {
            let v = if (x + y) % 37 == 0 {
                f32::NAN
            } else if (x * y) % 53 == 0 {
                f32::INFINITY
            } else {
                0.4
            };
            data[(y * w + x) * 3..][..3].copy_from_slice(&[v, v * 0.6, v * 0.3]);
        }
    }
    let img = Rgb::new(w, h, &data).unwrap();
    for holder in [
        Holder::Auto,
        Holder::Strip35 { format: Format35::Full },
        Holder::Medium120 { format: Format120::SixBySix },
        Holder::Sheet4x5,
        Holder::Slides35,
    ] {
        for film in [FilmType::Negative, FilmType::Positive] {
            let det = detect_bed(&img, 100.0, &BedConfig { holder, film, ..Default::default() }).unwrap();
            for f in &det.frames {
                assert!(f.confidence.is_finite() && f.rotated.cx.is_finite() && f.mm.x.is_finite());
            }
        }
    }
    assert!(detect_camera_frame(&img, &CameraConfig::default()).is_ok());
}

fn any_f64() -> impl Strategy<Value = f64> {
    prop_oneof![Just(f64::NAN), Just(f64::INFINITY), Just(f64::NEG_INFINITY), Just(0.0), Just(-1.0), Just(1e12), -2.0f64..2.0, 0.0f64..200.0,]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn random_images_and_configs_never_panic(
        w in 1usize..90,
        h in 1usize..90,
        seed in any::<u64>(),
        dpi in 10.0f64..400.0,
        a in any_f64(), b in any_f64(), c in any_f64(), d in any_f64(), e in any_f64(),
        n in 0usize..20,
    ) {
        let mut s = seed | 1;
        let data: Vec<f32> = (0..w * h * 3).map(|_| {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            match s % 11 { 0 => f32::NAN, 1 => f32::INFINITY, 2 => -0.5, _ => (s % 1000) as f32 / 1000.0 }
        }).collect();
        let img = Rgb::new(w, h, &data).unwrap();
        let bed = BedConfig {
            open_threshold: a, bare_level: b, min_across_mm: c, min_along_mm: d, merge_gap_mm: e,
            edge_inset: a, gap_level_tolerance: b, gap_uniformity: c, pitch_tolerance: d, pitch_tolerance_wide: e,
            smaller_pitch_preference: a, min_comb_score: b,
            holder: if n % 2 == 0 { Holder::Strip35 { format: Format35::Free } } else { Holder::Auto },
            ..Default::default()
        };
        let _ = detect_bed(&img, dpi, &bed);
        let cam = CameraConfig {
            work_size: n * 37, film_threshold: a, edge_threshold: b, min_support: c, confident_support: d,
            max_outside_spread: e, min_inside_std: a, max_skew_deg: b, aspect: c, aspect_tolerance: d,
            edge_candidates: n, max_outside_roughness: e, deskew: n % 3 == 0,
            roll_prior: n % 2 == 0, prior_size_tolerance: a, prior_edge_step: b,
        };
        let _ = detect_camera_frame(&img, &cam);
        let _ = detect_camera_roll(&[img, img, img], &cam);
        let prior = RollPrior { width: a, height: b, cx: c, cy: d, angle_deg: e, frames: n };
        let _ = detect_camera_frame_with_prior(&img, &cam, &prior);
        let bytes: Vec<u8> = data.iter().map(|v| (v.abs().min(1.0) * 255.0) as u8).collect();
        let live = LiveConfig { work_size: n * 20, confident_threshold: a, edge_margin: b, centre_tolerance: c, deskew: n % 2 == 0 };
        let _ = detect_live(&bytes, w, h, &live, &cam);
    }
}
