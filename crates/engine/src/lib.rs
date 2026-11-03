//! SilverCraft engine: everything between a capture on disk and a frame in LightCraft, shared by
//! the CLI and the app.
//!
//! - [`roll`]: develop a roll (decode → scanner fixes → dust repair → roll calibration →
//!   scene-linear DNG → LightCraft), reporting progress as [`roll::Event`]s.
//! - [`context`]: stock registry and layered pipeline config resolution.
//! - [`manifest`]: `roll.json`, the record of a capture session.
//! - [`scanner`]: the flatbed workflow on a worker thread.
//! - [`tether`]: the camera-scanning workflow (live view, autocapture) on a worker thread.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod context;
pub mod frames;
pub mod hardware;
pub mod looks;
pub mod manifest;
pub mod roll;
pub mod rotation;
pub mod scanner;
pub mod tether;
pub mod tiffio;
pub mod view;

pub use context::{ConfigSources, is_camera_scan};
pub use roll::{Event, FrameSource, RollOutcome, RollRequest, Thumbnail, develop_roll, develop_roll_frames, developed_paths};

/// Engine errors carry a human-readable message (they end up in a status line or on stderr).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Message(String),
    #[error("the pipeline configuration is invalid")]
    InvalidConfig,
    #[error("cancelled")]
    Cancelled,
}

impl From<String> for EngineError {
    fn from(s: String) -> Self {
        Self::Message(s)
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
