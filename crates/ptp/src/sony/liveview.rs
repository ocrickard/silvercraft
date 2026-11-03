//! Live-view payload decoding.

use crate::PtpError;

/// Where the JPEG was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegSource {
    /// The `u32 offset, u32 size` dataset header (EXT, unverified).
    Header,
    /// the original marker scan (first SOI to last EOI).
    MarkerScan,
}

/// Locate the JPEG in a Sony live-view (or object) payload; returns the byte
/// range and how it was found.
///
/// First tries the libgphoto2 header (`u32 offset_to_jpeg`, `u32 jpeg_size`):
/// used only when it is in bounds and points at an SOI marker. Otherwise
/// falls back to the original `extract_jpeg`: from the first `FF D8` to the
/// last `FF D9` inclusive, or to the end when there is no EOI after the SOI.
pub fn locate_jpeg(payload: &[u8]) -> Result<(std::ops::Range<usize>, JpegSource), PtpError> {
    if let (Some(o), Some(s)) = (payload.get(0..4), payload.get(4..8)) {
        let offset = u32::from_le_bytes([o[0], o[1], o[2], o[3]]) as usize;
        let size = u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize;
        if offset >= 8
            && size >= 4
            && offset.checked_add(size).is_some_and(|end| end <= payload.len())
            && payload.get(offset..offset + 2) == Some(&[0xFF, 0xD8])
        {
            return Ok((offset..offset + size, JpegSource::Header));
        }
    }
    let start = payload.windows(2).position(|w| w == [0xFF, 0xD8]).ok_or_else(|| PtpError::Camera("no JPEG SOI marker in object payload".into()))?;
    let end = payload.windows(2).rposition(|w| w == [0xFF, 0xD9]);
    match end {
        Some(e) if e > start => Ok((start..e + 2, JpegSource::MarkerScan)),
        _ => Ok((start..payload.len(), JpegSource::MarkerScan)),
    }
}

/// Copy the JPEG out of a payload.
pub fn extract_jpeg(payload: &[u8]) -> Result<Vec<u8>, PtpError> {
    let (r, _) = locate_jpeg(payload)?;
    Ok(payload.get(r).unwrap_or_default().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_scan_extract_case() {
        let jpeg = b"\xff\xd8\xff\xe0payload\xff\xd9".to_vec();
        let mut blob = vec![0x00, 0x11, 0x22];
        blob.extend_from_slice(&jpeg);
        blob.extend_from_slice(&[0x99, 0x99]);
        assert_eq!(extract_jpeg(&blob).unwrap(), jpeg);
        assert!(extract_jpeg(b"no jpeg here").is_err());
        assert!(extract_jpeg(&[]).is_err());
    }

    #[test]
    fn no_eoi_returns_tail() {
        let blob = b"xx\xff\xd8abc";
        assert_eq!(extract_jpeg(blob).unwrap(), b"\xff\xd8abc");
        // EOI before SOI only.
        let blob = b"\xff\xd9x\xff\xd8abc";
        assert_eq!(extract_jpeg(blob).unwrap(), b"\xff\xd8abc");
    }

    #[test]
    fn header_preferred_over_trailing_metadata_with_eoi() {
        let jpeg = b"\xff\xd8\xff\xe0data\xff\xd9";
        let mut blob = Vec::new();
        blob.extend_from_slice(&16u32.to_le_bytes());
        blob.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        blob.extend_from_slice(&[0; 8]);
        blob.extend_from_slice(jpeg);
        // Focus-frame metadata that happens to contain FF D9.
        blob.extend_from_slice(b"meta\xff\xd9meta");
        let (r, src) = locate_jpeg(&blob).unwrap();
        assert_eq!(src, JpegSource::Header);
        assert_eq!(&blob[r], jpeg);
    }

    #[test]
    fn bogus_header_falls_back() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&u32::MAX.to_le_bytes());
        blob.extend_from_slice(&u32::MAX.to_le_bytes());
        blob.extend_from_slice(b"\xff\xd8x\xff\xd9");
        let (_, src) = locate_jpeg(&blob).unwrap();
        assert_eq!(src, JpegSource::MarkerScan);
    }
}
