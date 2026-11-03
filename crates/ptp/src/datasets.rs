//! Standard PTP datasets: DeviceInfo, ObjectInfo, storage and handle lists.

use crate::PtpError;
use crate::codec::Reader;

/// Object format codes.
pub mod format {
    /// JPEG (EXIF).
    pub const EXIF_JPEG: u16 = 0x3801;
    /// Sony ARW (EXT).
    pub const SONY_ARW: u16 = 0xB101;
}

/// GetDeviceInfo dataset (PIMA 15740 §5.5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Standard version (e.g. 100 = 1.00).
    pub standard_version: u16,
    /// Vendor extension id (Sony: 0x11 per libgphoto2 (EXT)).
    pub vendor_extension_id: u32,
    /// Vendor extension version.
    pub vendor_extension_version: u16,
    /// Vendor extension description.
    pub vendor_extension_desc: String,
    /// Functional mode.
    pub functional_mode: u16,
    /// Supported operation codes.
    pub operations: Vec<u16>,
    /// Supported event codes.
    pub events: Vec<u16>,
    /// Supported device property codes.
    pub device_properties: Vec<u16>,
    /// Capture formats.
    pub capture_formats: Vec<u16>,
    /// Image formats.
    pub image_formats: Vec<u16>,
    /// Manufacturer.
    pub manufacturer: String,
    /// Model.
    pub model: String,
    /// Device (firmware) version.
    pub device_version: String,
    /// Serial number.
    pub serial_number: String,
}

impl DeviceInfo {
    /// Parse the dataset. Fields after the operation list are optional: a
    /// truncated tail leaves them empty rather than failing, since only the
    /// header and op list matter for capability checks.
    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        let mut r = Reader::new(buf);
        let mut d = DeviceInfo {
            standard_version: r.u16()?,
            vendor_extension_id: r.u32()?,
            vendor_extension_version: r.u16()?,
            vendor_extension_desc: r.ptp_string()?,
            functional_mode: r.u16()?,
            operations: r.u16_array()?,
            ..DeviceInfo::default()
        };
        let mut tail = || -> Result<(), PtpError> {
            d.events = r.u16_array()?;
            d.device_properties = r.u16_array()?;
            d.capture_formats = r.u16_array()?;
            d.image_formats = r.u16_array()?;
            d.manufacturer = r.ptp_string()?;
            d.model = r.ptp_string()?;
            d.device_version = r.ptp_string()?;
            d.serial_number = r.ptp_string()?;
            Ok(())
        };
        if let Err(e) = tail() {
            log::warn!("DeviceInfo tail truncated: {e}");
        }
        Ok(d)
    }

    /// Whether the device lists `op` as supported.
    pub fn supports(&self, op: u16) -> bool {
        self.operations.contains(&op)
    }
}

/// GetObjectInfo dataset (PIMA 15740 §5.5.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Storage id.
    pub storage_id: u32,
    /// Object format code.
    pub format: u16,
    /// Protection status.
    pub protection: u16,
    /// Compressed size in bytes (0xFFFFFFFF for 4 GiB or more).
    pub compressed_size: u32,
    /// Thumbnail format.
    pub thumb_format: u16,
    /// Thumbnail size.
    pub thumb_size: u32,
    /// Thumbnail width.
    pub thumb_width: u32,
    /// Thumbnail height.
    pub thumb_height: u32,
    /// Image width.
    pub image_width: u32,
    /// Image height.
    pub image_height: u32,
    /// Bit depth.
    pub bit_depth: u32,
    /// Parent object.
    pub parent: u32,
    /// Association type.
    pub association_type: u16,
    /// Association description.
    pub association_desc: u32,
    /// Sequence number.
    pub sequence_number: u32,
    /// Original filename (e.g. `DSC01234.ARW`); empty when absent.
    pub filename: String,
    /// Capture date string (`YYYYMMDDThhmmss`), empty when absent.
    pub capture_date: String,
}

impl ObjectInfo {
    /// Parse. Only the first 12 bytes are required; everything after is best-effort.
    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        let mut r = Reader::new(buf);
        let mut o = ObjectInfo { storage_id: r.u32()?, format: r.u16()?, protection: r.u16()?, compressed_size: r.u32()?, ..ObjectInfo::default() };
        let mut tail = || -> Result<(), PtpError> {
            o.thumb_format = r.u16()?;
            o.thumb_size = r.u32()?;
            o.thumb_width = r.u32()?;
            o.thumb_height = r.u32()?;
            o.image_width = r.u32()?;
            o.image_height = r.u32()?;
            o.bit_depth = r.u32()?;
            o.parent = r.u32()?;
            o.association_type = r.u16()?;
            o.association_desc = r.u32()?;
            o.sequence_number = r.u32()?;
            o.filename = r.ptp_string()?;
            o.capture_date = r.ptp_string()?;
            Ok(())
        };
        if let Err(e) = tail() {
            log::debug!("ObjectInfo tail truncated: {e}");
        }
        Ok(o)
    }

    /// Whether this looks like a Sony RAW (format code or `.ARW` name).
    pub fn is_arw(&self) -> bool {
        self.format == format::SONY_ARW || self.filename.to_ascii_lowercase().ends_with(".arw")
    }
}

/// Parse a u32-counted u32 list (storage ids, object handles). The established reading treats
/// short or inconsistent lists as empty; this returns an error instead and
/// callers decide.
pub fn parse_u32_list(buf: &[u8]) -> Result<Vec<u32>, PtpError> {
    Reader::new(buf).u32_array()
}

/// GetStorageIDs filtered as before (drop 0 and 0xFFFFFFFF).
pub fn usable_storage_ids(buf: &[u8]) -> Vec<u32> {
    parse_u32_list(buf).unwrap_or_default().into_iter().filter(|&s| s != 0 && s != 0xFFFF_FFFF).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::encode_ptp_string;

    pub(crate) fn device_info_bytes(model: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&100u16.to_le_bytes());
        b.extend_from_slice(&0x11u32.to_le_bytes());
        b.extend_from_slice(&100u16.to_le_bytes());
        b.extend_from_slice(&encode_ptp_string("Sony PTP Extensions"));
        b.extend_from_slice(&0u16.to_le_bytes());
        let ops: [u16; 4] = [0x1001, 0x1002, 0x9201, 0x9207];
        b.extend_from_slice(&(ops.len() as u32).to_le_bytes());
        for o in ops {
            b.extend_from_slice(&o.to_le_bytes());
        }
        for list in [&[0xC201u16][..], &[0x5007, 0xD215], &[0x3801], &[0x3801, 0xB101]] {
            b.extend_from_slice(&(list.len() as u32).to_le_bytes());
            for v in list {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        for s in ["Sony Corporation", model, "1.00", "D053E0339897"] {
            b.extend_from_slice(&encode_ptp_string(s));
        }
        b
    }

    pub(crate) fn object_info_bytes(format: u16, size: u32, name: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x0001_0001u32.to_le_bytes());
        b.extend_from_slice(&format.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        for v in [0u32, 160, 120, 8640, 5760, 14, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&encode_ptp_string(name));
        b.extend_from_slice(&encode_ptp_string("20261009T120000"));
        b.extend_from_slice(&encode_ptp_string(""));
        b.extend_from_slice(&encode_ptp_string(""));
        b
    }

    #[test]
    fn device_info_parses() {
        let d = DeviceInfo::parse(&device_info_bytes("ILCE-1")).unwrap();
        assert_eq!(d.model, "ILCE-1");
        assert_eq!(d.manufacturer, "Sony Corporation");
        assert_eq!(d.serial_number, "D053E0339897");
        assert!(d.supports(0x9201));
        assert_eq!(d.image_formats, vec![0x3801, 0xB101]);
    }

    #[test]
    fn device_info_truncations_never_panic() {
        let full = device_info_bytes("ILCE-1");
        for n in 0..full.len() {
            let _ = DeviceInfo::parse(&full[..n]);
        }
    }

    #[test]
    fn object_info_parses_and_tolerates_truncation() {
        let b = object_info_bytes(format::SONY_ARW, 53_927_936, "DSC01234.ARW");
        let o = ObjectInfo::parse(&b).unwrap();
        assert_eq!(o.compressed_size, 53_927_936);
        assert_eq!(o.filename, "DSC01234.ARW");
        assert_eq!(o.image_width, 8640);
        assert!(o.is_arw());
        let short = ObjectInfo::parse(&b[..12]).unwrap();
        assert_eq!(short.compressed_size, 53_927_936);
        assert!(ObjectInfo::parse(&b[..11]).is_err());
        for n in 0..b.len() {
            let _ = ObjectInfo::parse(&b[..n]);
        }
    }

    #[test]
    fn storage_ids_filtered() {
        let mut b = 3u32.to_le_bytes().to_vec();
        for v in [0u32, 0x0001_0001, 0xFFFF_FFFF] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(usable_storage_ids(&b), vec![0x0001_0001]);
        assert!(usable_storage_ids(&[1, 0, 0, 0]).is_empty());
        assert!(usable_storage_ids(&[]).is_empty());
    }
}
