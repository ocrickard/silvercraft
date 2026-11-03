//! Scanner TIFFs: grey, RGB, or RGBI (red, green, blue, infrared) at 8/16 bits or float.
//!
//! The fourth channel of a four-channel scan is the infrared plane. It is returned separately,
//! never treated as alpha (LightCraft's TIFF reader would divide the colour by it as premultiplied
//! alpha). Values are normalised to 0..1 transmittance by the sample range.

use std::io::Cursor;

use silvercraft_film::image::RgbImage;
use tiff::ColorType;
use tiff::decoder::{Decoder, DecodingResult, Limits};

use crate::{Capture, CaptureKind, DecodeError, MAX_DECODED_SAMPLES, Result};

/// Largest decode accepted: a 6×7 cm frame at 6400 dpi in 16-bit RGBI is about 2.6 GB.
const MAX_BYTES: usize = 4 << 30;

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.decoding_buffer_size = MAX_BYTES;
    limits.intermediate_buffer_size = MAX_BYTES;
    limits
}

/// Refuse geometry whose f32 expansion exceeds the sample budget, *before* the
/// decoder allocates anything like it. `out_per_pixel` counts every f32 plane the
/// result will hold (3 for RGB, 4 for RGBI, 3 for expanded grey).
fn check_budget(w: usize, h: usize, out_per_pixel: usize) -> Result<()> {
    let out = w.checked_mul(h).and_then(|px| px.checked_mul(out_per_pixel));
    match out {
        Some(n) if n <= MAX_DECODED_SAMPLES => Ok(()),
        _ => Err(DecodeError::Tiff(format!("{w}×{h}×{out_per_pixel} would expand past the {MAX_DECODED_SAMPLES}-sample decode limit"))),
    }
}

fn err(e: impl std::fmt::Display) -> DecodeError {
    DecodeError::Tiff(e.to_string())
}

/// Samples as f32 in 0..1 (integers by their full range; floats as stored, non-finite → 0).
fn normalised(data: DecodingResult) -> Result<Vec<f32>> {
    fn clean(v: f32) -> f32 {
        if v.is_finite() { v.max(0.0) } else { 0.0 }
    }
    Ok(match data {
        DecodingResult::U8(v) => v.into_iter().map(|x| x as f32 / 255.0).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|x| x as f32 / 65535.0).collect(),
        DecodingResult::U32(v) => v.into_iter().map(|x| (x as f64 / u32::MAX as f64) as f32).collect(),
        DecodingResult::F16(v) => v.into_iter().map(|x| clean(f32::from(x))).collect(),
        DecodingResult::F32(v) => v.into_iter().map(clean).collect(),
        DecodingResult::F64(v) => v.into_iter().map(|x| clean(x as f32)).collect(),
        other => return Err(DecodeError::Unsupported(format!("TIFF sample type {:?}", std::mem::discriminant(&other)))),
    })
}

pub fn decode(bytes: &[u8]) -> Result<Capture> {
    let mut dec = Decoder::new(Cursor::new(bytes)).map_err(err)?.with_limits(limits());
    let (w, h) = dec.dimensions().map_err(err)?;
    let (w, h) = (w as usize, h as usize);
    let channels = match dec.colortype().map_err(err)? {
        ColorType::Gray(_) => 1,
        ColorType::RGB(_) => 3,
        ColorType::RGBA(_) => 4,
        ColorType::Multiband { num_samples, .. } if (3..=4).contains(&num_samples) => num_samples as usize,
        other => return Err(DecodeError::Unsupported(format!("TIFF colour type {other:?}"))),
    };
    // Grey expands to RGB (3 per pixel); RGBI holds 4. The normalised temp holds
    // `channels` per pixel, never more than the output planes.
    check_budget(w, h, channels.max(3))?;
    let samples = normalised(dec.read_image().map_err(err)?)?;
    if w == 0 || h == 0 || samples.len() != w * h * channels {
        return Err(DecodeError::Tiff(format!("{} samples for {w}×{h}×{channels}", samples.len())));
    }
    let (rgb, ir) = match channels {
        1 => (samples.iter().flat_map(|&v| [v, v, v]).collect(), None),
        3 => (samples, None),
        _ => {
            let mut rgb = Vec::with_capacity(w * h * 3);
            let mut ir = Vec::with_capacity(w * h);
            for p in samples.chunks_exact(4) {
                rgb.extend_from_slice(&p[..3]);
                ir.push(p[3]);
            }
            (rgb, Some(ir))
        }
    };
    let rgb = RgbImage::new(w, h, rgb).map_err(|e| DecodeError::Image(e.to_string()))?;
    Ok(Capture { rgb, ir, kind: CaptureKind::Scanner, srgb_primaries: true })
}

/// Decode a separate single-channel infrared file registered to a scan of `width × height`.
pub fn decode_ir(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let mut dec = Decoder::new(Cursor::new(bytes)).map_err(err)?.with_limits(limits());
    let (w, h) = dec.dimensions().map_err(err)?;
    if (w as usize, h as usize) != (width, height) {
        return Err(DecodeError::Tiff(format!("infrared plane is {w}×{h}, scan is {width}×{height}")));
    }
    let channels = match dec.colortype().map_err(err)? {
        ColorType::Gray(_) => 1,
        ColorType::RGB(_) => 3,
        other => return Err(DecodeError::Unsupported(format!("infrared TIFF colour type {other:?}"))),
    };
    check_budget(width, height, channels)?;
    let samples = normalised(dec.read_image().map_err(err)?)?;
    if samples.len() != width * height * channels {
        return Err(DecodeError::Tiff("truncated infrared plane".into()));
    }
    // an RGB-encoded IR file carries the same value in every channel; keep the first
    Ok(samples.into_iter().step_by(channels).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::encoder::{TiffEncoder, colortype};

    fn encode_rgb16(w: u32, h: u32, data: &[u16]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        TiffEncoder::new(&mut out).unwrap().write_image::<colortype::RGB16>(w, h, data).unwrap();
        out.into_inner()
    }

    fn encode_rgba16(w: u32, h: u32, data: &[u16]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        TiffEncoder::new(&mut out).unwrap().write_image::<colortype::RGBA16>(w, h, data).unwrap();
        out.into_inner()
    }

    #[test]
    fn rgb16_scan_is_normalised_transmittance() {
        let data: Vec<u16> = (0..4 * 2 * 3).map(|i| (i * 2000) as u16).collect();
        let cap = decode(&encode_rgb16(4, 2, &data)).unwrap();
        assert_eq!((cap.rgb.width(), cap.rgb.height()), (4, 2));
        assert!(cap.ir.is_none());
        assert!((cap.rgb.data()[5] - 10000.0 / 65535.0).abs() < 1e-6);
        assert_eq!(cap.kind, CaptureKind::Scanner);
    }

    #[test]
    fn fourth_channel_is_infrared_not_alpha() {
        // the colour must come through untouched even where the IR plane is dark (dust)
        let data: Vec<u16> = vec![40000, 30000, 20000, 0, 40000, 30000, 20000, 65535];
        let cap = decode(&encode_rgba16(2, 1, &data)).unwrap();
        assert_eq!(cap.ir.as_deref(), Some(&[0.0, 1.0][..]));
        let rgb = cap.rgb.data();
        assert_eq!(&rgb[..3], &rgb[3..]);
        assert!((rgb[0] - 40000.0 / 65535.0).abs() < 1e-6);
    }

    #[test]
    fn separate_ir_file_must_match_the_scan() {
        let mut out = Cursor::new(Vec::new());
        TiffEncoder::new(&mut out).unwrap().write_image::<colortype::Gray16>(3, 2, &[0, 100, 200, 300, 400, 65535]).unwrap();
        let bytes = out.into_inner();
        let ir = decode_ir(&bytes, 3, 2).unwrap();
        assert_eq!(ir.len(), 6);
        assert_eq!(ir[5], 1.0);
        assert!(decode_ir(&bytes, 2, 3).is_err());
    }

    #[test]
    fn decode_budget_refuses_absurd_geometry_before_allocating() {
        assert!(check_budget(4, 2, 3).is_ok());
        assert!(check_budget(15_118, 17_638, 4).is_ok(), "a 6×7 cm frame at 6400 dpi still fits");
        assert!(check_budget(100_000, 100_000, 3).is_err());
        assert!(check_budget(usize::MAX, 2, 3).is_err(), "overflow is an error, not a wrap");
        assert!(check_budget(usize::MAX / 2, usize::MAX / 2, 4).is_err());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"not a tiff").is_err());
        assert!(decode(&[]).is_err());
        let mut t = encode_rgb16(4, 4, &[1000; 48]);
        t.truncate(t.len() / 2);
        assert!(decode(&t).is_err());
    }
}
