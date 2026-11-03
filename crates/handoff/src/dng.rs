//! Scene-linear positives as LinearRaw DNGs that LightCraft develops as raws.
//!
//! The pixels are scan RGB in linear sRGB/Rec.709 primaries, D65 white (what the scanner and the
//! mask-aware camera decode deliver), with mid-grey at 0.18 and highlights above 1 kept. LightCraft
//! converts to its Rec.2020 working space through the DNG colour model, applies its filmic tone map
//! (only DNGs get it; a float TIFF opens display-referred and flat) and starts white balance at
//! `AsShotNeutral`.
//!
//! LightCraft rebuilds any channel at or above 0.99 of white as clipped, so the data is scaled down
//! until its peak is 0.95 and the scale goes into `BaselineExposure` (never brightened). The
//! frame's own auto-exposure is added on top, so zeroing it in LightCraft restores roll exposure.

use lightcraft_color::{D50, D65, SRGB, bradford};
use lightcraft_raw::{
    BlackLevel, ColorData, DngCompression, DngWriteOptions, Metadata, Method, OpcodeLists, Orientation, RawData, RawFormat, RawImage, Rect,
};

use crate::{HandoffError, Result};

/// The peak sample value written; LightCraft treats ≥ 0.99 of white as clipped.
pub const PEAK_TARGET: f32 = 0.95;

/// Largest frame accepted, in samples: LightCraft's raw decoder refuses more than 2^30.
pub const MAX_SAMPLES: usize = 1 << 30;

/// Sample storage. Float32 keeps dense-negative highlights exactly; half is about half the size
/// with 11 bits of relative precision at every exponent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Samples {
    #[default]
    Float,
    Half,
}

/// A scene-linear positive: interleaved RGB, `width * height * 3` values.
#[derive(Clone, Copy, Debug)]
pub struct LinearFrame<'a> {
    pub width: usize,
    pub height: usize,
    pub rgb: &'a [f32],
}

/// What goes into the DNG besides the pixels.
#[derive(Clone, Debug, Default)]
pub struct DngFrameInfo {
    /// White balance the frame was measured at, as camera-neutral multipliers' inverse (green = 1);
    /// `None` = neutral (1, 1, 1).
    pub as_shot_neutral: Option<[f64; 3]>,
    /// Frame auto-exposure in EV, added to the peak-scaling compensation.
    pub exposure_ev: f64,
    pub orientation: Orientation,
    /// Film camera make/model when known (never "SilverCraft": LightCraft keys camera presets on it).
    pub make: Option<String>,
    pub model: Option<String>,
    /// Complete XMP packet embedded as tag 700 (see [`crate::xmp`]).
    pub xmp: Option<String>,
    pub samples: Samples,
}

/// Colour tags for linear sRGB primaries, D65 white.
pub fn linear_srgb_color(as_shot_neutral: [f64; 3], baseline_exposure: f64) -> ColorData {
    let to_xyz_d50 = bradford(D65, D50).mul(&SRGB.to_xyz());
    let to_xyz_d65 = bradford(D50, D65).mul(&to_xyz_d50);
    let cm = to_xyz_d65.inverse().unwrap_or(lightcraft_raw::Mat3::IDENTITY);
    ColorData {
        illuminant: [21, 0],
        color_matrix: [Some(cm), None],
        forward_matrix: [Some(to_xyz_d50), None],
        camera_calibration: [None, None],
        analog_balance: None,
        as_shot_neutral: Some(as_shot_neutral),
        as_shot_white_xy: None,
        baseline_exposure,
        baseline_sharpness: None,
        profile: Default::default(),
    }
}

/// Peak-scaling factor (≤ 1) and the cleaned sample for a value.
fn peak_scale(rgb: &[f32]) -> f32 {
    let peak = rgb.iter().copied().filter(|v| v.is_finite()).fold(0.0f32, f32::max);
    if peak > PEAK_TARGET { PEAK_TARGET / peak } else { 1.0 }
}

fn validate(frame: &LinearFrame<'_>) -> Result<()> {
    let n = frame.width.checked_mul(frame.height).and_then(|p| p.checked_mul(3));
    match n {
        _ if frame.width == 0 || frame.height == 0 => Err(HandoffError::Invalid("empty frame".into())),
        Some(n) if n == frame.rgb.len() && n <= MAX_SAMPLES => Ok(()),
        Some(n) if n > MAX_SAMPLES => {
            Err(HandoffError::Invalid(format!("{}×{} frame exceeds LightCraft's {MAX_SAMPLES}-sample limit", frame.width, frame.height)))
        }
        _ => Err(HandoffError::Invalid(format!("{} samples for a {}×{} RGB frame", frame.rgb.len(), frame.width, frame.height))),
    }
}

/// The `RawImage` LightCraft's writer serialises.
pub fn raw_image(frame: &LinearFrame<'_>, info: &DngFrameInfo) -> Result<RawImage> {
    validate(frame)?;
    let neutral = info.as_shot_neutral.unwrap_or([1.0; 3]);
    if !neutral.iter().all(|v| v.is_finite() && *v > 0.0) {
        return Err(HandoffError::Invalid(format!("as-shot neutral {neutral:?}")));
    }
    if !info.exposure_ev.is_finite() {
        return Err(HandoffError::Invalid("exposure is not finite".into()));
    }
    let scale = peak_scale(frame.rgb);
    let clean = |v: f32| if v.is_finite() { (v * scale).max(0.0) } else { 0.0 };
    let baseline = info.exposure_ev + (1.0 / scale as f64).log2();
    let (w, h) = (frame.width, frame.height);
    let metadata = Metadata {
        make: info.make.clone(),
        model: info.model.clone(),
        width: Some(w as u32),
        height: Some(h as u32),
        orientation: Some(info.orientation),
        ..Metadata::default()
    };
    Ok(RawImage {
        format: RawFormat::Dng,
        width: w,
        height: h,
        cpp: 3,
        data: RawData::F32(frame.rgb.iter().map(|&v| clean(v)).collect()),
        cfa: None,
        bits: match info.samples {
            Samples::Float => 32,
            Samples::Half => 16,
        },
        black: BlackLevel::uniform(0.0),
        white: vec![1.0],
        active_area: Rect::new(0, 0, w, h),
        crop: Rect::new(0, 0, w, h),
        orientation: info.orientation,
        color: linear_srgb_color(neutral, baseline),
        wb_multipliers: None,
        linearized: false,
        opcodes: OpcodeLists::default(),
        metadata,
    })
}

/// Encode a scene-linear positive as a Deflate-compressed float LinearRaw DNG.
pub fn encode(frame: &LinearFrame<'_>, info: &DngFrameInfo) -> Result<Vec<u8>> {
    let raw = raw_image(frame, info)?;
    let compression = DngCompression::Deflate { tile: 256, half: info.samples == Samples::Half };
    let opts = DngWriteOptions { compression, xmp: info.xmp.clone(), ..Default::default() };
    lightcraft_raw::write_dng(&raw, &opts).map_err(|e| HandoffError::Dng(e.to_string()))
}

/// A delivered DNG read back: scene-linear RGB at roll scale (`BaselineExposure` applied, so
/// mid-grey is 0.18 again) and the frame's white balance.
#[derive(Clone, Debug)]
pub struct Scene {
    pub width: usize,
    pub height: usize,
    /// Interleaved RGB, `width * height * 3` values.
    pub rgb: Vec<f32>,
    pub as_shot_neutral: [f64; 3],
    /// How the frame is to be shown (not applied to `rgb`).
    pub orientation: Orientation,
}

/// Read a scene-linear DNG (ours, or any 3-channel LinearRaw DNG) with LightCraft's decoder.
pub fn decode(bytes: &[u8]) -> Result<Scene> {
    let raw = lightcraft_raw::decode(bytes).map_err(|e| HandoffError::Dng(e.to_string()))?;
    if raw.cpp != 3 || raw.cfa.is_some() {
        return Err(HandoffError::Dng(format!("not an RGB LinearRaw DNG ({} samples per pixel)", raw.cpp)));
    }
    let gain = 2f32.powf(raw.color.baseline_exposure as f32);
    let as_shot_neutral = raw.color.as_shot_neutral.unwrap_or([1.0; 3]);
    let img = raw.develop(Method::Bilinear).map_err(|e| HandoffError::Dng(e.to_string()))?;
    let rgb = img.data.into_iter().flat_map(|p| p.map(|v| v * gain)).collect();
    Ok(Scene { width: img.width, height: img.height, rgb, as_shot_neutral, orientation: raw.orientation })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: usize, h: usize, peak: f32) -> Vec<f32> {
        (0..w * h).flat_map(|i| [i as f32 / (w * h) as f32 * peak, 0.18, 0.05]).collect()
    }

    fn decode_scene(bytes: &[u8]) -> (lightcraft_raw::RawImage, Vec<[f32; 3]>) {
        let raw = lightcraft_raw::decode(bytes).unwrap();
        let gain = 2f32.powf(raw.color.baseline_exposure as f32);
        let img = raw.develop(lightcraft_raw::Method::Bilinear).unwrap();
        let px = img.data.iter().map(|p| p.map(|v| v * gain)).collect();
        (raw, px)
    }

    #[test]
    fn scene_values_survive_with_highlights_above_one() {
        let (w, h) = (64, 32);
        let rgb = ramp(w, h, 40.0);
        let info = DngFrameInfo::default();
        let bytes = encode(&LinearFrame { width: w, height: h, rgb: &rgb }, &info).unwrap();
        let (raw, px) = decode_scene(&bytes);
        assert_eq!((raw.width, raw.height, raw.cpp), (w, h, 3));
        // the stored peak stays below LightCraft's clip threshold
        let stored = match &raw.data {
            RawData::F32(d) => d.iter().copied().fold(0.0, f32::max),
            _ => panic!("float data expected"),
        };
        assert!(stored <= PEAK_TARGET + 1e-6 && stored > 0.9, "{stored}");
        // and BaselineExposure restores the scene scale, including values far above 1
        for (i, p) in px.iter().enumerate() {
            let want = &rgb[i * 3..i * 3 + 3];
            for c in 0..3 {
                assert!((p[c] - want[c]).abs() <= want[c] * 1e-4 + 1e-6, "{i} {p:?} {want:?}");
            }
        }
    }

    #[test]
    fn decode_reads_back_the_scene_and_its_white_balance() {
        let (w, h) = (24, 8);
        let rgb = ramp(w, h, 6.0);
        let info =
            DngFrameInfo { exposure_ev: 0.5, as_shot_neutral: Some([0.8, 1.0, 1.25]), orientation: Orientation::Rotate270, ..Default::default() };
        let scene = decode(&encode(&LinearFrame { width: w, height: h, rgb: &rgb }, &info).unwrap()).unwrap();
        assert_eq!((scene.width, scene.height, scene.rgb.len()), (w, h, rgb.len()), "stored as captured");
        assert_eq!(scene.orientation, Orientation::Rotate270, "shown turned");
        assert_eq!(scene.as_shot_neutral, [0.8, 1.0, 1.25]);
        // the frame's own exposure is part of the baseline: values come back at 2^0.5 of the input
        let k = 2f32.powf(0.5);
        for (got, want) in scene.rgb.iter().zip(&rgb) {
            assert!((got - want * k).abs() <= want * k * 1e-4 + 1e-6, "{got} {want}");
        }
        assert!(decode(b"not a dng").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn exposure_and_neutral_are_carried_not_baked() {
        let rgb = ramp(16, 16, 0.5);
        let info = DngFrameInfo { exposure_ev: 1.5, as_shot_neutral: Some([0.8, 1.0, 1.25]), ..Default::default() };
        let raw = raw_image(&LinearFrame { width: 16, height: 16, rgb: &rgb }, &info).unwrap();
        // nothing to scale down: the frame's own exposure is the whole baseline
        assert!((raw.color.baseline_exposure - 1.5).abs() < 1e-12);
        assert_eq!(raw.color.as_shot_neutral, Some([0.8, 1.0, 1.25]));
        match &raw.data {
            RawData::F32(d) => assert_eq!(&d[..], &rgb[..]),
            _ => panic!("float data expected"),
        }
    }

    #[test]
    fn neutral_scan_white_stays_neutral_in_lightcraft() {
        let rgb = vec![0.5f32; 8 * 8 * 3];
        let bytes = encode(&LinearFrame { width: 8, height: 8, rgb: &rgb }, &DngFrameInfo { samples: Samples::Half, ..Default::default() }).unwrap();
        let raw = lightcraft_raw::decode(&bytes).unwrap();
        let xy = lightcraft_raw::color::as_shot_white_xy(&raw);
        assert!((xy.x - D65.x).abs() < 2e-3 && (xy.y - D65.y).abs() < 2e-3, "{xy:?}");
        let t = lightcraft_raw::color::camera_transform(&raw, xy);
        let white = t.matrix.apply([t.wb[0] as f64, t.wb[1] as f64, t.wb[2] as f64]);
        assert!(white.iter().all(|v| (v - white[0]).abs() < 1e-3), "{white:?}");
    }

    #[test]
    fn hostile_frames_are_errors() {
        let ok = vec![0.1f32; 12];
        let bad = [
            LinearFrame { width: 0, height: 4, rgb: &[] },
            LinearFrame { width: 2, height: 2, rgb: &ok[..11] },
            LinearFrame { width: usize::MAX, height: 2, rgb: &ok },
        ];
        for f in bad {
            assert!(encode(&f, &DngFrameInfo::default()).is_err());
        }
        let f = LinearFrame { width: 2, height: 2, rgb: &ok };
        assert!(encode(&f, &DngFrameInfo { as_shot_neutral: Some([0.0, 1.0, 1.0]), ..Default::default() }).is_err());
        assert!(encode(&f, &DngFrameInfo { exposure_ev: f64::NAN, ..Default::default() }).is_err());
        // non-finite pixels are written as black, not refused
        let nan = vec![f32::NAN, f32::INFINITY, -1.0, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2];
        let raw = raw_image(&LinearFrame { width: 2, height: 2, rgb: &nan }, &DngFrameInfo::default()).unwrap();
        match &raw.data {
            RawData::F32(d) => assert_eq!(&d[..3], &[0.0, 0.0, 0.0]),
            _ => panic!("float data expected"),
        }
    }
}
