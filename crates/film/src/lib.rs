//! SilverCraft film core: film base estimation, negative inversion (live
//! preview), roll calibration, layer gamma, and the normative scene-linear
//! output transform.
//!
//! Everything display-referred (paper, shoulder, sRGB, looks) is left to
//! LightCraft.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod base;
pub mod calibrate;
pub mod config;
pub mod diag;
pub mod error;
pub mod image;
pub mod invert;
pub mod layer_gamma;
pub mod pipeline;
pub mod profiles;
pub mod scene;
pub mod schema;
pub mod stats;

pub use error::FilmError;
pub use image::RgbImage;

pub use calibrate::{FitParams, RollCalibrationDocument, fit_roll_calibration};
pub use config::FilmConfig;
pub use diag::{Diagnostic, Severity};
pub use pipeline::{FrameInput, FrameSamples, RollModel, RollResult, RollSamples, develop_frame, develop_roll, develop_single, fit_roll};
pub use profiles::{FilmKind, FilmProfile, FilmProfileSpec, ProfileRegistry, builtin_profiles, film_profile, profile_for_capture};
pub use scene::{Referred, RollScale, SceneFrame, SceneMetadata};
