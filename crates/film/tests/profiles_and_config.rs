//! Profile files (load, override, diagnostics, round trip) and FilmConfig
//! knobs that must actually change the pipeline's behaviour.

#![allow(clippy::field_reassign_with_default)] // config tweaks read clearer as assignments in tests

mod common;

use std::path::PathBuf;

use common::{Stock, negative, patch_center, window_mean};
use silvercraft_film::config::{BaseMethod, ExposureMode, PositiveMode, WhiteSource};
use silvercraft_film::diag::Severity;
use silvercraft_film::profiles::{BUILTIN_PROFILE_FILES, FilmProfile, ProfileRegistry};
use silvercraft_film::scene::Referred;
use silvercraft_film::{
    FilmConfig, FilmKind, FrameInput, RgbImage, RollSamples, develop_frame, develop_roll, develop_single, film_profile, fit_roll,
};

const SLIDE: &str = r#"
schema = "silvercraft/profile/1"
id = "test-slide"
name = "Test Slide"
kind = "color-positive"
process = "E-6"

[positive]
mode = "as-is"
gamma = 2.4

[look]
saturation = 1.1
"#;

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("silvercraft-film-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn errors_of(text: &str) -> Vec<silvercraft_film::Diagnostic> {
    match FilmProfile::from_toml_str(text, "t.toml") {
        Ok(_) => Vec::new(),
        Err(d) => d,
    }
}

#[test]
fn every_builtin_round_trips_through_to_toml() {
    for (id, _) in BUILTIN_PROFILE_FILES {
        let p = film_profile(id).unwrap();
        let back = FilmProfile::from_toml_str(&p.to_toml(), id).unwrap();
        assert_eq!(back.to_toml(), p.to_toml(), "{id}");
        assert!(p.validate().is_empty(), "{id}: {:?}", p.validate());
    }
}

#[test]
fn diagnostics_for_bad_profiles() {
    // Bad TOML syntax: located error.
    let d = errors_of("schema = \"silvercraft/profile/1\"\nid = = 3");
    assert!(d[0].message.starts_with("line 2"), "{d:?}");
    assert_eq!(d[0].source.as_deref(), Some("t.toml"));
    // Unknown key (typo) is an error, top level and inside sections.
    assert!(errors_of(&SLIDE.replace("process", "proces"))[0].message.contains("proces"));
    assert!(errors_of(&SLIDE.replace("mode = \"as-is\"", "mode = \"as-is\"\ngama = 2.0"))[0].message.contains("gama"));
    // Wrong section for kind.
    let wrong = SLIDE.replace("[positive]", "[negative]").replace("mode = \"as-is\"\ngamma = 2.4", "scene_gamma = 0.6");
    let d = errors_of(&wrong);
    assert!(d.iter().any(|x| x.path == "negative"), "{d:?}");
    assert!(d.iter().any(|x| x.path == "positive"), "{d:?}");
    // Out of range values.
    let neg = FilmProfile::template(FilmKind::ColorNegative).to_toml();
    let d = errors_of(&neg.replace("scene_gamma = 0.6", "scene_gamma = 9.0"));
    assert!(d.iter().any(|x| x.path == "negative.scene_gamma"), "{d:?}");
    let d = errors_of(&neg.replace("low_percentile = 0.5", "low_percentile = 120.0"));
    assert!(d.iter().any(|x| x.path == "negative.low_percentile"), "{d:?}");
    let d = errors_of(&neg.replace("scene_gamma = 0.6", "scene_gamma = 0.6\ndmin = [0.5, 0.0, 1.5]"));
    assert!(d.iter().any(|x| x.path == "negative.dmin"), "{d:?}");
    let d = errors_of(&SLIDE.replace("test-slide", "Test Slide!"));
    assert!(d.iter().any(|x| x.path == "id"), "{d:?}");
    let d = errors_of(&SLIDE.replace("silvercraft/profile/1", "silvercraft/profile/9"));
    assert!(d.iter().any(|x| x.path == "schema"), "{d:?}");
    // [processing] is checked against FilmConfig.
    let d = errors_of(&format!("{SLIDE}\n[processing.scene]\nmid_grey = -1.0\n"));
    assert!(d.iter().any(|x| x.path == "processing.scene.mid_grey"), "{d:?}");
    let d = errors_of(&format!("{SLIDE}\n[processing.scene]\nmidgrey = 0.2\n"));
    assert!(!d.is_empty());
}

#[test]
fn registry_loads_dirs_overrides_and_survives_bad_files() {
    let dir = tmpdir("registry");
    std::fs::write(dir.join("a-slide.toml"), SLIDE).unwrap();
    std::fs::write(dir.join("b-broken.toml"), "this is not toml = = =").unwrap();
    // Overrides a builtin by id.
    let portra = film_profile("kodak-portra-400").unwrap().to_toml().replace("name = \"Kodak Portra 400\"", "name = \"My Portra\"");
    std::fs::write(dir.join("c-portra.toml"), portra).unwrap();
    std::fs::write(dir.join("ignored.txt"), "not a profile").unwrap();
    let mut registry = ProfileRegistry::builtin();
    let builtins = registry.profiles().len();
    registry.load_dir(&dir);
    assert_eq!(registry.profiles().len(), builtins + 1, "slide added, portra replaced, broken skipped");
    assert_eq!(registry.get("test-slide").unwrap().positive_gamma(), Some(2.4));
    assert_eq!(registry.get("kodak-portra-400").unwrap().name, "My Portra");
    let diags = registry.diagnostics();
    assert!(diags.iter().any(|d| d.severity == Severity::Error && d.source.as_deref().is_some_and(|s| s.ends_with("b-broken.toml"))), "{diags:?}");
    assert!(diags.iter().any(|d| d.severity == Severity::Note && d.message.contains("overrides")), "{diags:?}");
    let index = registry.profiles().iter().position(|p| p.id == "kodak-portra-400").unwrap();
    assert!(registry.source_of(index).unwrap().ends_with("c-portra.toml"));
    // Missing dir / file: diagnostics, not panics.
    registry.load_dir(&dir.join("nope"));
    assert!(!registry.load_file(&dir.join("nope.toml")));
    // The look table is preserved verbatim.
    assert_eq!(registry.get("test-slide").unwrap().look["saturation"].as_float(), Some(1.1));
    let _ = std::fs::remove_dir_all(&dir);
    // user_dirs ends with the per-user profiles dir when HOME is known.
    if std::env::var_os("HOME").is_some() {
        assert!(ProfileRegistry::user_dirs().last().is_some_and(|p| p.ends_with("profiles")));
    }
}

fn slide_frame(light: [f32; 3]) -> RgbImage {
    let data: Vec<f32> = (0..4000).flat_map(|i| light.map(|w| w * (0.02 + 0.98 * i as f32 / 3999.0))).collect();
    RgbImage::new(100, 40, data).unwrap()
}

#[test]
fn positives_develop_from_loaded_files() {
    let colour = FilmProfile::from_toml_str(SLIDE, "slide").unwrap();
    let bw_text = SLIDE.replace("color-positive", "bw-positive").replace("test-slide", "test-bw-slide");
    let bw = FilmProfile::from_toml_str(&bw_text, "bw").unwrap();
    let frame = slide_frame([0.9, 0.8, 0.6]);
    let out = develop_single(&frame, None, &colour, &FilmConfig::default()).unwrap();
    assert_eq!(out.meta.referred, Referred::Display);
    let px = out.image.pixel(50, 20).unwrap();
    assert!((px[0] - px[1]).abs() < 1e-3 && (px[1] - px[2]).abs() < 1e-3, "backlight removed: {px:?}");
    let out = develop_single(&frame, None, &bw, &FilmConfig::default()).unwrap();
    assert!(out.image.data().chunks_exact(3).all(|p| p[0] == p[1] && p[1] == p[2]));
    // Unrender (from the profile's [positive] via a config override): scene-referred,
    // median luminance at mid grey, steeper slide gamma flattened by 1/gamma.
    let mut cfg = FilmConfig::default();
    cfg.positive.mode = Some(PositiveMode::Unrender);
    let out = develop_single(&frame, None, &colour, &cfg).unwrap();
    assert_eq!(out.meta.referred, Referred::Scene);
    assert!((out.meta.median_neutral_l - 0.18).abs() < 0.01, "{}", out.meta.median_neutral_l);
    let a = out.image.pixel(10, 0).unwrap()[1];
    let b = out.image.pixel(10, 39).unwrap()[1];
    let t = |y: usize| 0.02 + 0.98 * (y * 100 + 10) as f32 / 3999.0;
    assert!(((b / a).ln() / (t(39) / t(0)).ln() - 1.0 / 2.4).abs() < 0.01, "slope must be 1/gamma");
    // Explicit white.
    let mut cfg = FilmConfig::default();
    cfg.positive.white_source = WhiteSource::Explicit;
    cfg.positive.white = Some([1.0; 3]);
    let out = develop_single(&frame, None, &colour, &cfg).unwrap();
    assert!(out.image.data().iter().all(|v| *v <= 0.9 + 1e-6));
    let px = out.image.pixel(99, 39).unwrap();
    assert!((px[0] - 0.9).abs() < 1e-5 && (px[2] - 0.6).abs() < 1e-5, "explicit white keeps the light's colour: {px:?}");
}

fn colour_frame() -> (RgbImage, Stock) {
    let stock = Stock::even(0.6);
    (negative(160, 90, &stock, 0.0, 77), stock)
}

#[test]
fn config_knobs_change_behaviour() {
    let (frame, stock) = colour_frame();
    let profile = film_profile("generic-c41").unwrap();
    let base = stock.base.map(|v| f64::from(v as f32));
    let develop = |cfg: &FilmConfig| develop_single(&frame, Some(stock.base), profile, cfg).unwrap();
    let default = develop(&FilmConfig::default());

    // exposure: fixed / roll.
    let mut cfg = FilmConfig::default();
    cfg.exposure.mode = ExposureMode::Fixed;
    cfg.exposure.fixed_ev = 1.25;
    assert_eq!(develop(&cfg).meta.baseline_exposure_ev, 1.25);
    cfg.exposure.mode = ExposureMode::Roll;
    assert_eq!(develop(&cfg).meta.baseline_exposure_ev, 0.0);

    // base: explicit dmin wins over the gap base; content differs from gap.
    let mut cfg = FilmConfig::default();
    cfg.base.method = BaseMethod::Explicit;
    cfg.base.dmin = Some([0.7, 0.5, 0.3]);
    assert_eq!(develop(&cfg).meta.film_base, [0.7, 0.5, 0.3]);
    cfg.base.dmin = None;
    assert!(develop_single(&frame, Some(stock.base), profile, &cfg).is_err(), "explicit without any dmin");
    let mut cfg = FilmConfig::default();
    cfg.base.method = BaseMethod::Content;
    assert_ne!(develop(&cfg).meta.film_base, default.meta.film_base);
    let mut cfg = FilmConfig::default();
    cfg.base.fallback_to_content = false;
    assert!(develop_single(&frame, None, profile, &cfg).is_err(), "no gap base and no fallback");

    // scene: mid_grey and g_ref scale the output.
    let mut cfg = FilmConfig::default();
    cfg.scene.mid_grey = 0.36;
    let (x, y) = patch_center(160, 90, 1);
    let g0 = window_mean(&default.image, x, y, 3)[1];
    let g1 = window_mean(&develop(&cfg).image, x, y, 3)[1];
    assert!((g1 / g0 - 2.0).abs() < 1e-3, "{g1} vs {g0}");
    let mut cfg = FilmConfig::default();
    cfg.scene.g_ref = Some(0.5);
    assert_eq!(develop(&cfg).meta.g_ref, Some(0.5));

    // white balance off -> unit AsShotNeutral.
    let mut cfg = FilmConfig::default();
    cfg.white_balance.residual = false;
    assert_eq!(develop(&cfg).meta.as_shot_neutral, [1.0; 3]);

    // calibration disabled -> diagonal anchor + identity curve.
    let mut cfg = FilmConfig::default();
    cfg.calibration.enabled = false;
    let c = develop(&cfg).meta.calibration.unwrap();
    assert_eq!(c.curve_gamma, [1.0; 3]);
    assert_eq!(c.curve_alpha, [0.0; 3]);

    // highlight cap off vs on, and the single-channel blend, on a frame with
    // one blown (floored) blue record in a bright area.
    let mut data = frame.data().to_vec();
    for p in data.chunks_exact_mut(3).take(800) {
        p[2] = 0.01;
    }
    let blown = RgbImage::new(160, 90, data).unwrap();
    let run = |cfg: &FilmConfig| {
        let mut cfg = cfg.clone();
        cfg.base.dmin = Some(base);
        // A capture whose blue noise floor sits at 0.033 (Dcap_B = 1.0, below
        // the ~1.38 D of the bright ramp end): below it nothing is resolved.
        cfg.scene.noise_floor = Some([1e-3, 1e-3, 0.033]);
        develop_single(&blown, None, profile, &cfg).unwrap()
    };
    let capped = run(&FilmConfig::default());
    assert!(capped.meta.clipped_fraction > 0.05);
    let mut cfg = FilmConfig::default();
    cfg.scene.highlight_cap = false;
    let uncapped = run(&cfg);
    let blue = |f: &silvercraft_film::SceneFrame| f.image.pixel(75, 0).unwrap()[2];
    assert!(blue(&uncapped) > blue(&capped), "without the cap the sub-floor pixel reads noise as extra density");
    let mut cfg = FilmConfig::default();
    cfg.scene.single_channel_highlight_blend = true;
    let blended = run(&cfg);
    let p = blended.image.pixel(75, 0).unwrap();
    let p_capped = capped.image.pixel(75, 0).unwrap();
    // Blend raises the capped blue to the neutral-axis estimate of R,G.
    assert!(p[2] > p_capped[2] * 1.5, "{p:?} vs {p_capped:?}");
    assert!((p[2] / p[1] - 1.0).abs() < 0.05, "blended highlight is neutral again: {p:?}");
    assert_eq!([p[0], p[1]], [p_capped[0], p_capped[1]]);
}

#[test]
fn profile_processing_and_model_embeds_config() {
    let (frame, stock) = colour_frame();
    let mut profile = film_profile("generic-c41").unwrap().clone();
    profile.processing = toml::from_str("[exposure]\nmode = \"fixed\"\nfixed_ev = 0.5\n[scene]\nhighlight_cap = false").unwrap();
    assert!(profile.validate().is_empty());
    let user: toml::Table = toml::from_str("[exposure]\nfixed_ev = -0.5").unwrap();
    let cfg = profile.effective_config(&FilmConfig::default(), Some(&user)).unwrap();
    assert_eq!(cfg.exposure.mode, ExposureMode::Fixed, "from the profile");
    assert_eq!(cfg.exposure.fixed_ev, -0.5, "user wins over the profile");
    assert!(!cfg.scene.highlight_cap);
    let frames = [FrameInput { image: &frame, gap_base: Some(stock.base), label: None }];
    let roll = develop_roll(&frames, &profile, &cfg).unwrap();
    assert_eq!(roll.model.config, cfg);
    assert_eq!(roll.frames[0].meta.baseline_exposure_ev, -0.5);
    // The model alone (after a JSON round trip) reproduces the frame.
    let json = serde_json::to_string(&roll.model).unwrap();
    let model: silvercraft_film::RollModel = serde_json::from_str(&json).unwrap();
    assert_eq!(develop_frame(&model, &frame, None).unwrap(), roll.frames[0]);
    // RollSamples::for_config follows the sampling knobs.
    let mut small = cfg.clone();
    small.calibration.max_samples_per_frame = 100;
    let mut samples = RollSamples::for_config(&small);
    samples.push_image(&frame, Some(stock.base), None);
    assert!(samples.frames[0].samples.len() < 3 * 200);
    assert!(fit_roll(&samples, &profile, &small).is_ok());
}

#[test]
fn bw_negative_profile_from_file() {
    let text = FilmProfile::template(FilmKind::BlackAndWhiteNegative).to_toml().replace("my-stock", "test-bw");
    let profile = FilmProfile::from_toml_str(&text, "bw").unwrap();
    let (frame, stock) = colour_frame();
    let out = develop_single(&frame, Some(stock.base), &profile, &FilmConfig::default()).unwrap();
    assert!(out.image.data().chunks_exact(3).all(|p| p[0] == p[1] && p[1] == p[2]));
    assert_eq!(out.meta.g_ref, Some(0.6));
}
