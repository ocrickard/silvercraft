//! Roll develop : base -> layer gamma (optional) -> roll
//! calibration -> per-frame scene-linear outputs, every knob from
//! [`FilmConfig`]. A single frame is just an N = 1 roll (decision D2).
//!
//! Two-pass streaming API for real rolls (36-77 frames of 50 MP scans that
//! cannot all be held in memory):
//!
//! 1. Push each frame into [`RollSamples`] (it keeps only a strided subsample,
//!    the gap base, the full-frame content base and the label), then drop
//!    the full image.
//! 2. [`fit_roll`] computes everything roll-level into a serializable
//!    [`RollModel`] (base, layer gamma, calibration, G_ref, k, strip gains,
//!    and the effective config it used).
//! 3. [`develop_frame`] re-reads one full frame and applies the model.
//!
//! [`develop_roll`] is exactly that composition over in-memory frames.
//!
//! `fit_roll` uses the config AS GIVEN: compose the stock's `[processing]`
//! table and user overrides first with
//! [`crate::profiles::FilmProfile::effective_config`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::base::{aggregate_roll_base, base_is_usable};
use crate::calibrate::{FitParams, RollCalibrationDocument, fit_roll_calibration_with};
use crate::config::{BaseMethod, FilmConfig, PositiveMode};
use crate::diag::has_errors;
use crate::error::FilmError;
use crate::image::RgbImage;
use crate::invert::density_clamped;
use crate::layer_gamma::{LayerGammaDetail, corrected_density_scale, roll_layer_gamma_detail_with, strip_key_with, strip_residual_gains_with};
use crate::profiles::{FilmKind, FilmProfile};
use crate::scene::{RollScale, SceneFrame, bw_e_mid, bw_negative_to_scene_linear, k_from_e_mid_with, negative_to_scene_linear, positive_to_linear};
use crate::stats::channel_percentiles;

/// One frame of an in-memory roll.
#[derive(Debug, Clone, Copy)]
pub struct FrameInput<'a> {
    /// The frame (crop, rebate-free) as linear transmittance 0..1.
    pub image: &'a RgbImage,
    /// Inter-frame gap base sampled from this frame's capture margins
    /// ([`crate::base::sample_film_base`]), when available.
    pub gap_base: Option<[f64; 3]>,
    /// Frame label; `strip-NN` groups frames for per-strip balance.
    pub label: Option<&'a str>,
}

impl<'a> FrameInput<'a> {
    pub fn new(image: &'a RgbImage) -> Self {
        Self { image, gap_base: None, label: None }
    }
}

/// What the roll fit keeps of one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSamples {
    /// Strided interleaved-RGB subsample.
    pub samples: Vec<f32>,
    pub gap_base: Option<[f64; 3]>,
    pub label: Option<String>,
    /// Content-percentile base of the FULL frame (method "content" and the
    /// gap fallback); non-finite values read as 0.
    pub content_base: [f64; 3],
}

fn content_base(rgb: &[f32], percentile: f64) -> [f64; 3] {
    let finite: Vec<f32> = rgb.iter().map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
    channel_percentiles(&finite, percentile as f32).map(f64::from)
}

/// Accumulates per-frame samples for [`fit_roll`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollSamples {
    pub max_samples_per_frame: usize,
    /// Percentile used for each frame's content base.
    pub content_percentile: f64,
    pub frames: Vec<FrameSamples>,
}

impl RollSamples {
    /// Default content percentile (95).
    pub fn new(max_samples_per_frame: usize) -> Self {
        Self { max_samples_per_frame, content_percentile: 95.0, frames: Vec::new() }
    }

    /// Sampling as `config` asks (`calibration.max_samples_per_frame`,
    /// `base.content_percentile`).
    pub fn for_config(config: &FilmConfig) -> Self {
        Self {
            max_samples_per_frame: config.calibration.max_samples_per_frame,
            content_percentile: config.base.content_percentile,
            frames: Vec::new(),
        }
    }

    /// Subsample and record a full frame (drop the image afterwards).
    pub fn push_image(&mut self, image: &RgbImage, gap_base: Option<[f64; 3]>, label: Option<&str>) {
        self.frames.push(FrameSamples {
            samples: image.subsample(self.max_samples_per_frame),
            gap_base,
            label: label.map(str::to_owned),
            content_base: content_base(image.data(), self.content_percentile),
        });
    }

    /// Record a caller-made subsample (interleaved RGB; a trailing partial
    /// pixel is ignored). The content base is estimated from the subsample.
    pub fn push_samples(&mut self, mut samples: Vec<f32>, gap_base: Option<[f64; 3]>, label: Option<&str>) {
        samples.truncate(samples.len() / 3 * 3);
        let content_base = content_base(&samples, self.content_percentile);
        self.frames.push(FrameSamples { samples, gap_base, label: label.map(str::to_owned), content_base });
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Everything roll-level, serializable into the roll sidecar and reusable
/// later (e.g. to develop one rescanned frame).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollModel {
    /// Profile id.
    pub profile: String,
    pub kind: FilmKind,
    /// Roll film base (unused for positives).
    pub film_base: [f64; 3],
    pub layer_gamma: Option<LayerGammaDetail>,
    /// Density multiplier used as the calibration anchor: profile layer gain /
    /// measured layer gamma.
    pub density_anchor: [f64; 3],
    /// Colour negatives only.
    pub calibration: Option<RollCalibrationDocument>,
    pub e_mid: Option<f64>,
    pub g_ref: f64,
    pub k: f64,
    /// Per-strip residual gains keyed by strip label ("" = unlabelled).
    pub strip_gains: BTreeMap<String, [f64; 3]>,
    /// Positives: resolved mode and gamma.
    pub positive_mode: PositiveMode,
    pub positive_gamma: Option<f64>,
    pub frames_fitted: usize,
    /// The effective config this roll was fitted with (develop_frame uses it).
    pub config: FilmConfig,
}

/// Roll develop result (in-memory API).
#[derive(Debug, Clone, PartialEq)]
pub struct RollResult {
    pub model: RollModel,
    pub frames: Vec<SceneFrame>,
}

/// Roll film base per `[base]`: `base.dmin` > profile dmin (method explicit,
/// or `prefer_profile_dmin`) > method (gap-family: median of gap bases, with
/// the content fallback; content: median of content bases).
fn roll_base(frames: &[FrameSamples], profile: &FilmProfile, config: &FilmConfig) -> Result<[f64; 3], FilmError> {
    let check = |base: [f64; 3]| if base_is_usable(base) { Ok(base) } else { Err(FilmError::InvalidBase(base)) };
    let b = &config.base;
    if let Some(dmin) = b.dmin {
        return check(dmin);
    }
    if (b.method == BaseMethod::Explicit || b.prefer_profile_dmin)
        && let Some(dmin) = profile.dmin_f64()
    {
        return check(dmin);
    }
    let content = || aggregate_roll_base(&frames.iter().map(|f| f.content_base).collect::<Vec<_>>()).unwrap_or([0.0; 3]);
    match b.method {
        BaseMethod::Explicit => Err(FilmError::InvalidParameter("base method \"explicit\" needs base.dmin or a profile dmin")),
        BaseMethod::Content => check(content()),
        BaseMethod::Gap | BaseMethod::AutoBand | BaseMethod::Rows => {
            let negative = profile.kind().is_negative();
            let roll_content = negative.then(content);
            let gaps: Vec<[f64; 3]> = frames
                .iter()
                .filter_map(|f| {
                    f.gap_base.filter(|g| !negative || (gap_can_be_base(*g, f.content_base) && roll_content.is_none_or(|c| gap_can_be_base(*g, c))))
                })
                .collect();
            // a few frames' gaps (the rest were holder or picture) don't speak for the roll
            let gaps = if negative && gaps.len() * 2 < frames.iter().filter(|f| f.gap_base.is_some()).count() { Vec::new() } else { gaps };
            match aggregate_roll_base(&gaps) {
                Some(base) => check(base),
                None if b.fallback_to_content => check(content()),
                None => Err(FilmError::InvalidParameter("no frame has a gap base and base.fallback_to_content is off")),
            }
        }
    }
}

/// A negative's base is the clearest film on it: a "gap" much darker than the frame's own
/// clearest content is something else (a camera holder's mask around a frame box, the scanner
/// lid) and must not set the base, or every frame inverts to black.
fn gap_can_be_base(gap: [f64; 3], content: [f64; 3]) -> bool {
    (0..3).all(|c| content[c].is_nan() || content[c] <= 0.0 || gap[c] >= GAP_MIN_OF_CONTENT * content[c])
}

/// How much darker than the content base (its high percentile) a gap base may read and still be
/// film base: generous, since dense highlights keep the content percentile below the base.
const GAP_MIN_OF_CONTENT: f64 = 0.5;

/// Pass 1: fit everything roll-level from the samples with `config` as given.
pub fn fit_roll(samples: &RollSamples, profile: &FilmProfile, config: &FilmConfig) -> Result<RollModel, FilmError> {
    let diagnostics = config.validate();
    if has_errors(&diagnostics) {
        return Err(FilmError::InvalidConfig(diagnostics));
    }
    let frames = &samples.frames;
    if frames.is_empty() {
        return Err(FilmError::NoFrames);
    }
    let kind = profile.kind();
    let g_ref = config.scene.g_ref.unwrap_or_else(|| profile.scene_gamma());
    let mut model = RollModel {
        profile: profile.id.clone(),
        kind,
        film_base: [1.0; 3],
        layer_gamma: None,
        density_anchor: [1.0; 3],
        calibration: None,
        e_mid: None,
        g_ref,
        k: 1.0,
        strip_gains: BTreeMap::new(),
        positive_mode: config.positive.mode.unwrap_or_else(|| profile.positive_mode()),
        positive_gamma: config.positive.gamma.or_else(|| profile.positive_gamma()),
        frames_fitted: frames.len(),
        config: config.clone(),
    };
    let refs: Vec<&[f32]> = frames.iter().map(|f| f.samples.as_slice()).collect();
    match kind {
        FilmKind::ColorPositive | FilmKind::BlackAndWhitePositive => Ok(model),
        FilmKind::BlackAndWhiteNegative => {
            let base = roll_base(frames, profile, config)?;
            model.film_base = base;
            model.e_mid = bw_e_mid(&refs, base, &config.scene);
            model.k = k_from_e_mid_with(model.e_mid, g_ref, &config.scene);
            Ok(model)
        }
        FilmKind::ColorNegative => {
            let base = roll_base(frames, profile, config)?;
            let neg = profile.negative_params();
            let eps = config.calibration.epsilon.max(1e-6);
            let mut anchor = profile.layer_gain();
            let lg = &config.layer_gamma;
            if lg.measure && frames.len() >= lg.min_frames.max(1) {
                let densities: Vec<Vec<[f64; 3]>> = refs
                    .iter()
                    .map(|s| s.chunks_exact(3).map(|p| [0, 1, 2].map(|c| density_clamped(f64::from(p[c]), base[c], eps) * anchor[c])).collect())
                    .collect();
                let detail = roll_layer_gamma_detail_with(&densities, lg);
                anchor = corrected_density_scale(anchor, detail.gamma);
                model.layer_gamma = Some(detail);
            }
            let params = FitParams {
                density_scale: anchor,
                low_percentile: neg.low_percentile,
                high_percentile: neg.high_percentile,
                epsilon: config.calibration.epsilon,
                neutralize: config.calibration.neutralize.unwrap_or(neg.neutralize),
            };
            let calibration = fit_roll_calibration_with(&refs, &vec![base; frames.len()], &params, &config.calibration, &config.scene, g_ref)?;
            if config.strip_balance.enabled {
                let labelled: Vec<(&str, &[f32])> = frames.iter().zip(&refs).map(|(f, s)| (f.label.as_deref().unwrap_or(""), *s)).collect();
                model.strip_gains = strip_residual_gains_with(&labelled, &calibration, &config.strip_balance.label_prefix, &config.calibration);
            }
            model.film_base = base;
            model.density_anchor = anchor;
            model.e_mid = calibration.e_mid;
            model.k = k_from_e_mid_with(calibration.e_mid, g_ref, &config.scene);
            model.calibration = Some(calibration);
            Ok(model)
        }
    }
}

/// Pass 2: develop one full frame with a fitted roll model.
pub fn develop_frame(model: &RollModel, image: &RgbImage, label: Option<&str>) -> Result<SceneFrame, FilmError> {
    let config = &model.config;
    let scale = RollScale { g_ref: model.g_ref, k: model.k };
    match model.kind {
        FilmKind::ColorPositive | FilmKind::BlackAndWhitePositive => {
            positive_to_linear(image, model.positive_mode, model.positive_gamma, model.kind == FilmKind::BlackAndWhitePositive, config)
        }
        FilmKind::BlackAndWhiteNegative => bw_negative_to_scene_linear(image, model.film_base, scale, config),
        FilmKind::ColorNegative => {
            let calibration = model.calibration.as_ref().ok_or(FilmError::InvalidParameter("colour-negative model lacks a calibration"))?;
            let gain = if model.strip_gains.is_empty() {
                [1.0; 3]
            } else {
                let key = strip_key_with(label.unwrap_or(""), &config.strip_balance.label_prefix);
                model.strip_gains.get(&key).copied().unwrap_or([1.0; 3])
            };
            negative_to_scene_linear(image, calibration, scale, gain, config)
        }
    }
}

/// Develop an in-memory roll: [`fit_roll`] then [`develop_frame`] per frame.
pub fn develop_roll(frames: &[FrameInput<'_>], profile: &FilmProfile, config: &FilmConfig) -> Result<RollResult, FilmError> {
    let mut samples = RollSamples::for_config(config);
    for frame in frames {
        samples.push_image(frame.image, frame.gap_base, frame.label);
    }
    let model = fit_roll(&samples, profile, config)?;
    let developed = frames.iter().map(|f| develop_frame(&model, f.image, f.label)).collect::<Result<Vec<_>, _>>()?;
    Ok(RollResult { model, frames: developed })
}

/// Develop one frame as an N = 1 roll.
pub fn develop_single(image: &RgbImage, gap_base: Option<[f64; 3]>, profile: &FilmProfile, config: &FilmConfig) -> Result<SceneFrame, FilmError> {
    let frame = FrameInput { image, gap_base, label: None };
    let mut result = develop_roll(&[frame], profile, config)?;
    result.frames.pop().ok_or(FilmError::NoFrames)
}
