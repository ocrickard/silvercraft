//! Decoding captures into linear transmittance (0..1 per channel) for the film core.
//!
//! - **Camera scans** (a raw of the film on a light source): LightCraft's pure-Rust raw decoder,
//!   demosaiced at unity white balance in camera-native RGB. With a camera matrix, the
//!   orange-mask-aware conversion ([`mask_aware`]) moves it to linear sRGB primaries without the
//!   clipping a plain matrix causes on an orange negative; without one, density is measured in
//!   camera-native channels and the roll's density matrix absorbs the crosstalk.
//! - **Scanner scans**: 8/16-bit or float RGB TIFFs, and RGBI TIFFs whose fourth channel is the
//!   infrared plane (returned separately, never as alpha).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod mask_aware;
pub mod tiff_scan;

use std::path::Path;

use silvercraft_film::image::RgbImage;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported capture: {0}")]
    Unsupported(String),
    #[error("raw: {0}")]
    Raw(String),
    #[error("TIFF: {0}")]
    Tiff(String),
    #[error("image: {0}")]
    Image(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DecodeError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// A digital camera photographed the film; make/model of the camera.
    Camera {
        make: Option<String>,
        model: Option<String>,
    },
    Scanner,
}

/// A decoded capture: linear transmittance, scan RGB.
#[derive(Clone, Debug)]
pub struct Capture {
    pub rgb: RgbImage,
    /// Registered infrared transmittance, one value per pixel, when the capture has one.
    pub ir: Option<Vec<f32>>,
    pub kind: CaptureKind,
    /// Whether `rgb` is in linear sRGB primaries (true) or camera-native channels (false).
    pub srgb_primaries: bool,
}

/// Decode-stage configuration (`[decode]` in a SilverCraft pipeline config).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecodeConfig {
    /// Demosaic for camera-scan raws: `ahd` (default, sharpest), `ppg`, or `bilinear` (fastest).
    pub demosaic: Demosaic,
    /// Camera colour matrix for camera scans: `none` measures density in camera-native channels
    /// (the roll's density matrix absorbs the crosstalk), `file` uses the matrix a DNG camera scan
    /// carries, `custom` uses `xyz_to_camera`.
    pub camera_matrix: MatrixSource,
    /// XYZ (D65) → camera matrix, row-major, for `camera_matrix = "custom"`.
    pub xyz_to_camera: Option<[[f64; 3]; 3]>,
    /// Percentile of each channel taken as the orange-mask level in the mask-aware conversion.
    pub mask_percentile: f64,
    /// Infrared plane of RGBI scans: `use` it for dust repair, or `ignore` it.
    pub infrared: Infrared,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self { demosaic: Demosaic::Ahd, camera_matrix: MatrixSource::None, xyz_to_camera: None, mask_percentile: 95.0, infrared: Infrared::Use }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Demosaic {
    Ahd,
    Ppg,
    Bilinear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatrixSource {
    None,
    File,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Infrared {
    Use,
    Ignore,
}

impl DecodeConfig {
    /// Problems with this configuration, as `(field, message)`.
    pub fn validate(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if !(50.0..=100.0).contains(&self.mask_percentile) {
            out.push(("decode.mask_percentile".into(), format!("{} is outside 50..100", self.mask_percentile)));
        }
        match (self.camera_matrix, &self.xyz_to_camera) {
            (MatrixSource::Custom, None) => out.push(("decode.xyz_to_camera".into(), "camera_matrix = \"custom\" needs xyz_to_camera".into())),
            (MatrixSource::Custom, Some(m)) if mask_aware::camera_to_srgb(m).is_none() => {
                out.push(("decode.xyz_to_camera".into(), "matrix is singular or not finite".into()))
            }
            (MatrixSource::None | MatrixSource::File, Some(_)) => {
                out.push(("decode.xyz_to_camera".into(), "ignored unless camera_matrix = \"custom\"".into()))
            }
            _ => {}
        }
        out
    }
}

const RAW_EXTENSIONS: &[&str] = &["arw", "dng", "nef", "cr2", "cr3", "raf", "rw2", "orf", "pef", "srw", "3fr", "iiq"];

/// Largest capture file read into memory. The biggest honest input — an uncompressed
/// 6×7 cm frame at 6400 dpi in 16-bit RGBI — is about 2.6 GB; this leaves generous
/// headroom while keeping a hostile multi-GB file (sparse or otherwise) from being
/// slurped whole before any format limit gets a chance to apply.
pub const MAX_CAPTURE_FILE_BYTES: u64 = 8 << 30;

/// Largest total number of f32 samples decoding may produce (RGB + IR planes).
/// Bounds the expansion a small header can claim (an 8-bit TIFF quadruples in f32):
/// 2e9 samples is 8 GB of planes, far above any real frame.
pub const MAX_DECODED_SAMPLES: usize = 2_000_000_000;

/// Read a capture file with the size gate applied *before* the bytes are in memory:
/// the length comes from metadata, and the read itself is capped in case the file
/// grows between the two. Other hostile-file readers (separate IR planes, the frame
/// viewer) go through here too.
pub fn read_capped(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > MAX_CAPTURE_FILE_BYTES {
        return Err(DecodeError::Unsupported(format!(
            "{} is {len} bytes (limit {MAX_CAPTURE_FILE_BYTES}); refusing to read it into memory",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len.min(64 << 20)).unwrap_or(0));
    (&mut f).take(MAX_CAPTURE_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CAPTURE_FILE_BYTES {
        return Err(DecodeError::Unsupported(format!("{} grew past {MAX_CAPTURE_FILE_BYTES} bytes while reading", path.display())));
    }
    Ok(bytes)
}

/// Decode a capture file, choosing the path by extension.
pub fn decode_file(path: &Path, config: &DecodeConfig) -> Result<Capture> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    if RAW_EXTENSIONS.contains(&ext.as_str()) {
        decode_camera_raw(&read_capped(path)?, config)
    } else if matches!(ext.as_str(), "tif" | "tiff") {
        let mut cap = tiff_scan::decode(&read_capped(path)?)?;
        if config.infrared == Infrared::Ignore {
            cap.ir = None;
        }
        Ok(cap)
    } else {
        Err(DecodeError::Unsupported(format!("{} (expected a camera raw or a scanner TIFF)", path.display())))
    }
}

/// Decode a camera-scan raw to linear transmittance.
pub fn decode_camera_raw(bytes: &[u8], config: &DecodeConfig) -> Result<Capture> {
    let raw = lightcraft_raw::decode(bytes).map_err(|e| DecodeError::Raw(e.to_string()))?;
    if raw.cfa.is_none() && raw.cpp != 3 {
        return Err(DecodeError::Unsupported(format!("{}-sample linear raw", raw.cpp)));
    }
    // unity white balance, camera-native RGB, white level 1.0
    let developed = raw
        .develop(match config.demosaic {
            Demosaic::Ahd => lightcraft_raw::Method::Ahd,
            Demosaic::Ppg => lightcraft_raw::Method::Ppg,
            Demosaic::Bilinear => lightcraft_raw::Method::Bilinear,
        })
        .map_err(|e| DecodeError::Raw(e.to_string()))?;
    let (w, h) = (developed.width, developed.height);
    let samples =
        w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(|| DecodeError::Raw(format!("developed raw geometry {w}x{h} overflows")))?;
    if samples > MAX_DECODED_SAMPLES {
        return Err(DecodeError::Raw(format!("developed raw is {w}x{h} ({samples} samples, limit {MAX_DECODED_SAMPLES})")));
    }
    let mut data: Vec<f32> = Vec::with_capacity(samples);
    for p in &developed.data {
        data.extend(p.iter().map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 }));
    }
    let matrix = match config.camera_matrix {
        MatrixSource::None => None,
        MatrixSource::Custom => config.xyz_to_camera,
        MatrixSource::File => raw.color.color_matrix[0].map(|m| m.0),
    };
    let srgb_primaries = match matrix.and_then(|m| mask_aware::camera_to_srgb(&m)) {
        Some(m) => {
            mask_aware::convert(&mut data, &m, config.mask_percentile);
            true
        }
        None => false,
    };
    let rgb = RgbImage::new(w, h, data).map_err(|e| DecodeError::Image(e.to_string()))?;
    let kind = CaptureKind::Camera { make: raw.metadata.make.clone(), model: raw.metadata.model.clone() };
    Ok(Capture { rgb, ir: None, kind, srgb_primaries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_rejects_typos() {
        let c = DecodeConfig::default();
        let text = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<DecodeConfig>(&text).unwrap(), c);
        let c: DecodeConfig = toml::from_str("demosaic = \"ppg\"\ninfrared = \"ignore\"").unwrap();
        assert_eq!((c.demosaic, c.infrared, c.mask_percentile), (Demosaic::Ppg, Infrared::Ignore, 95.0));
        assert!(toml::from_str::<DecodeConfig>("demosiac = \"ppg\"").is_err());
        assert!(toml::from_str::<DecodeConfig>("demosaic = \"amaze\"").is_err());
    }

    #[test]
    fn oversized_capture_files_are_refused_from_metadata() {
        let path = std::env::temp_dir().join(format!("sc-decode-huge-{}.tif", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_CAPTURE_FILE_BYTES + 1).unwrap();
        drop(f);
        let e = decode_file(&path, &DecodeConfig::default()).unwrap_err();
        assert!(e.to_string().contains("refusing to read it into memory"), "{e}");
        // An unsupported extension is refused without reading the file at all.
        let other = std::env::temp_dir().join(format!("sc-decode-huge-{}.xyz", std::process::id()));
        std::fs::write(&other, b"x").unwrap();
        assert!(matches!(decode_file(&other, &DecodeConfig::default()), Err(DecodeError::Unsupported(_))));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&other);
    }

    #[test]
    fn validation_catches_inconsistent_matrices() {
        assert!(DecodeConfig::default().validate().is_empty());
        let custom_missing = DecodeConfig { camera_matrix: MatrixSource::Custom, ..Default::default() };
        assert_eq!(custom_missing.validate().len(), 1);
        let singular = DecodeConfig { camera_matrix: MatrixSource::Custom, xyz_to_camera: Some([[0.0; 3]; 3]), ..Default::default() };
        assert_eq!(singular.validate().len(), 1);
        let pct = DecodeConfig { mask_percentile: 120.0, ..Default::default() };
        assert_eq!(pct.validate()[0].0, "decode.mask_percentile");
    }
}
