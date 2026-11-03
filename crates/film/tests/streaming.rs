//! The two-pass streaming API (`RollSamples` -> `fit_roll` -> `develop_frame`)
//! is exactly `develop_roll`, and a serialized `RollModel` reproduces it.

#![allow(clippy::field_reassign_with_default)] // option tweaks read clearer as assignments in tests

mod common;

use common::{Stock, negative};
use silvercraft_film::{FilmConfig, FrameInput, RgbImage, RollModel, RollSamples, develop_frame, develop_roll, film_profile, fit_roll};

fn roll(stock: &Stock) -> Vec<RgbImage> {
    (0..4).map(|i| negative(180, 90, stock, f64::from(i as u8) * 0.5 - 1.0, 900 + i)).collect()
}

fn check(profile: &str, stock: Stock, per_strip: bool) {
    let frames = roll(&stock);
    let labels = ["r-strip-1-a", "r-strip-1-b", "r-strip-2-a", "r-strip-2-b"];
    let inputs: Vec<FrameInput> =
        frames.iter().zip(labels).map(|(f, l)| FrameInput { image: f, gap_base: Some(stock.base), label: Some(l) }).collect();
    let mut opts = FilmConfig::default();
    opts.strip_balance.enabled = per_strip;
    opts.calibration.max_samples_per_frame = 5000;
    let profile = film_profile(profile).unwrap();
    let whole = develop_roll(&inputs, profile, &opts).unwrap();

    // Streaming: subsample each frame, "drop" it, fit, then develop one by one.
    let mut samples = RollSamples::new(opts.calibration.max_samples_per_frame);
    for (frame, label) in frames.iter().zip(labels) {
        samples.push_image(frame, Some(stock.base), Some(label));
    }
    // numpy `flat[::n // max]` stride: fewer than 2x max samples are kept.
    assert!(samples.frames.iter().all(|f| f.samples.len() < 3 * 2 * 5000));
    let model = fit_roll(&samples, profile, &opts).unwrap();
    assert_eq!(model, whole.model);
    // The model survives the roll sidecar (JSON) bit-exactly.
    let json = serde_json::to_string(&model).unwrap();
    let reloaded: RollModel = serde_json::from_str(&json).unwrap();
    assert_eq!(reloaded, model);
    for ((frame, label), expected) in frames.iter().zip(labels).zip(&whole.frames) {
        let got = develop_frame(&reloaded, frame, Some(label)).unwrap();
        assert_eq!(&got, expected);
    }
    if per_strip {
        assert_eq!(model.strip_gains.len(), 2);
    }
}

#[test]
fn streaming_equals_develop_roll_colour_negative() {
    check("kodak-portra-400", Stock { base: common::ORANGE_BASE, gamma: [0.57, 0.55, 0.62], noise: 0.003 }, true);
    check("generic-c41", Stock::even(0.6), false);
}

#[test]
fn streaming_equals_develop_roll_bw_and_slide() {
    check("ilford-hp5-plus", Stock::even(0.7), false);
    check("fuji-velvia-50", Stock::even(0.6), false);
}

#[test]
fn caller_subsamples_and_empty_rolls() {
    let mut samples = RollSamples::new(100);
    assert!(fit_roll(&samples, film_profile("generic-c41").unwrap(), &FilmConfig::default()).is_err());
    samples.push_samples(vec![0.5, 0.4, 0.3, 0.2], None, None);
    assert_eq!(samples.frames[0].samples.len(), 3);
    let model = fit_roll(&samples, film_profile("generic-c41").unwrap(), &FilmConfig::default()).unwrap();
    let img = RgbImage::filled(3, 3, [0.25, 0.2, 0.15]).unwrap();
    assert!(develop_frame(&model, &img, None).is_ok());
    let mut broken = model.clone();
    broken.calibration = None;
    assert!(develop_frame(&broken, &img, None).is_err());
}

#[test]
fn a_dark_holder_around_the_frame_is_not_taken_for_film_base() {
    let stock = Stock { base: common::ORANGE_BASE, gamma: [0.57, 0.55, 0.62], noise: 0.003 };
    let frames = roll(&stock);
    let profile = film_profile("generic-c41").unwrap();
    let mut opts = FilmConfig::default();
    opts.base.prefer_profile_dmin = false;
    let fit = |gap: [f64; 3]| {
        let mut samples = RollSamples::new(5000);
        for f in &frames {
            samples.push_image(f, Some(gap), None);
        }
        fit_roll(&samples, profile, &opts).unwrap().film_base
    };
    let real = fit(stock.base);
    assert_eq!(real, stock.base, "a real gap base is used as it is");
    let near_base = |b: [f64; 3]| (0..3).all(|c| b[c] > 0.5 * stock.base[c] && b[c] < 1.5 * stock.base[c]);
    // a camera holder's mask, read as "gap": 1/300 of the base
    let holder = fit(stock.base.map(|v| v / 300.0));
    assert!(near_base(holder), "{holder:?} vs {:?}", stock.base);

    // one frame whose box sat inside the picture: its "gap" is picture (passable for that frame
    // alone), the others' are holder; that one frame must not set the roll's base
    let mut samples = RollSamples::new(5000);
    for (i, f) in frames.iter().enumerate() {
        let gap = if i == 0 { stock.base.map(|v| v * 0.45) } else { stock.base.map(|v| v / 300.0) };
        samples.push_image(f, Some(gap), None);
    }
    samples.frames[0].content_base = stock.base.map(|v| v * 0.6);
    let base = fit_roll(&samples, profile, &opts).unwrap().film_base;
    assert!(near_base(base), "{base:?} vs {:?}", stock.base);
}
