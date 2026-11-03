//! SilverCraft → LightCraft handoff.
//!
//! A developed frame leaves SilverCraft as a scene-linear positive in a LinearRaw DNG ([`dng`]),
//! with its metadata in an XMP packet embedded and beside it ([`xmp`], [`write`]), and is then
//! imported into LightCraft's library ([`deliver`]): over the control channel of a running
//! LightCraft ([`lightcraft`]), through its watched folder, or left for a manual import.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod config;
pub mod deliver;
pub mod dng;
pub mod lightcraft;
pub mod write;
pub mod xmp;

pub use config::{HandoffConfig, OutputConfig, Route, TemplateVars};
pub use deliver::{Delivered, DeliveryOptions, deliver};
pub use dng::{DngFrameInfo, LinearFrame, Samples};
pub use lightcraft::LookPreset;
pub use lightcraft_raw::Orientation;
pub use write::write_frame;
pub use xmp::XmpPacket;

#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("DNG: {0}")]
    Dng(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("LightCraft: {0}")]
    Rejected(String),
    #[error("control protocol: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, HandoffError>;
