//! SilverCraft image primitives shared by the repair and detection crates: single-channel float
//! [`Plane`]s, binary [`Mask`]s, separable filters, morphology, resampling, statistics and
//! connected components. Everything is plain safe Rust; borders are clamped (edge replicate).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod components;
pub mod filter;
pub mod mask;
pub mod morph;
pub mod plane;
pub mod resample;
pub mod stats;

pub use components::{Component, label};
pub use mask::Mask;
pub use plane::{Plane, interleave, split_channels};

/// Errors from image primitives (all come from inconsistent dimensions or bad parameters).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImgError {
    #[error("empty image ({width}×{height})")]
    Empty { width: usize, height: usize },
    #[error("buffer of {got} values does not match {width}×{height}×{channels}")]
    BufferSize { width: usize, height: usize, channels: usize, got: usize },
    #[error("image dimensions overflow ({width}×{height})")]
    Overflow { width: usize, height: usize },
    #[error("size mismatch: {a_w}×{a_h} vs {b_w}×{b_h}")]
    SizeMismatch { a_w: usize, a_h: usize, b_w: usize, b_h: usize },
    #[error("invalid parameter: {0}")]
    Param(&'static str),
}

pub type Result<T> = std::result::Result<T, ImgError>;
