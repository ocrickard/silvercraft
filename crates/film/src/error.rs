//! Crate error type.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum FilmError {
    #[error("image is empty ({width}x{height})")]
    EmptyImage { width: usize, height: usize },
    #[error("image buffer has {got} values, expected {expected} ({width}x{height}x3)")]
    BufferSize { width: usize, height: usize, expected: usize, got: usize },
    #[error("image dimensions {width}x{height} overflow")]
    DimensionOverflow { width: usize, height: usize },
    #[error("no frames supplied")]
    NoFrames,
    #[error("film base {0:?} is unusable (non-finite or at the sensor floor); supply an explicit dmin")]
    InvalidBase([f64; 3]),
    #[error("film_bases length ({bases}) must match frames length ({frames})")]
    MismatchedBases { frames: usize, bases: usize },
    #[error("frame length must be a multiple of 3 (interleaved RGB)")]
    BadFrameLength,
    #[error("film type '{0}' is not handled by the film core (digital originals belong to LightCraft)")]
    UnsupportedFilmType(String),
    #[error("invalid film config ({} diagnostics; first: {})", .0.len(), .0.first().map(ToString::to_string).unwrap_or_default())]
    InvalidConfig(Vec<crate::diag::Diagnostic>),
    #[error("invalid parameter: {0}")]
    InvalidParameter(&'static str),
}
