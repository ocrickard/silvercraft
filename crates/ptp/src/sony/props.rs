//! Sony SDIO property datasets.
//!
//! **Unverified layouts.** the established code never parsed these structurally; it only
//! scanned the 0x9209 blob for the bytes `15 D2` and read the u16 at +8
//!. The structured layouts below come from libgphoto2
//! (`ptp_unpack_Sony_DPD`) and must be checked against a hex dump from the
//! real body during bring-up. The parser is therefore tolerant: it tries
//! several layout variants, and when none parses cleanly it falls back to
//! the original scan for ObjectInMemory.

use crate::PtpError;
use crate::codec::Reader;

/// Sony property codes (EXT, unverified except ObjectInMemory).
pub mod code {
    /// F-number, u16, f x 100.
    pub const F_NUMBER: u16 = 0x5007;
    /// Focus mode, u16.
    pub const FOCUS_MODE: u16 = 0x500A;
    /// Exposure program, u16.
    pub const EXPOSURE_PROGRAM: u16 = 0x500E;
    /// Exposure compensation, i16, EV x 1000.
    pub const EXPOSURE_COMPENSATION: u16 = 0x5010;
    /// Shutter speed, u32 `num << 16 | den`; 0 = bulb.
    pub const SHUTTER_SPEED: u16 = 0xD20D;
    /// ObjectInMemory, u16; >= 0x8000 means an object waits at 0xFFFFC001 (HW).
    pub const OBJECT_IN_MEMORY: u16 = 0xD215;
    /// ISO, u32; 0x00FFFFFF = auto.
    pub const ISO: u16 = 0xD21E;
    /// Still-image save destination, u16 (EXT, low confidence): 1 = PC, 0x10 = card, 0x11 = both.
    pub const SAVE_DESTINATION: u16 = 0xD222;
}

/// PTP datatype codes.
pub mod datatype {
    /// i8.
    pub const INT8: u16 = 0x0001;
    /// u8.
    pub const UINT8: u16 = 0x0002;
    /// i16.
    pub const INT16: u16 = 0x0003;
    /// u16.
    pub const UINT16: u16 = 0x0004;
    /// i32.
    pub const INT32: u16 = 0x0005;
    /// u32.
    pub const UINT32: u16 = 0x0006;
    /// i64.
    pub const INT64: u16 = 0x0007;
    /// u64.
    pub const UINT64: u16 = 0x0008;
    /// i128.
    pub const INT128: u16 = 0x0009;
    /// u128.
    pub const UINT128: u16 = 0x000A;
    /// Array flag (u32 count + elements).
    pub const ARRAY_FLAG: u16 = 0x4000;
    /// PTP string.
    pub const STRING: u16 = 0xFFFF;
}

/// A property value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropValue {
    /// Signed integer (i8..i64).
    Int(i64),
    /// Unsigned integer (u8..u64).
    UInt(u64),
    /// 128-bit value, raw little-endian bytes.
    Wide([u8; 16]),
    /// String.
    Str(String),
    /// Array.
    Array(Vec<PropValue>),
}

impl PropValue {
    /// As u64, when an integer that fits.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            PropValue::UInt(v) => Some(v),
            PropValue::Int(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }
}

/// Upper bound on array elements, to bound allocation on hostile input.
const MAX_ARRAY: usize = 4096;

/// Read a value of `dt`.
pub fn read_value(r: &mut Reader<'_>, dt: u16) -> Result<PropValue, PtpError> {
    Ok(match dt {
        datatype::INT8 => PropValue::Int(i64::from(r.u8()? as i8)),
        datatype::UINT8 => PropValue::UInt(u64::from(r.u8()?)),
        datatype::INT16 => PropValue::Int(i64::from(r.u16()? as i16)),
        datatype::UINT16 => PropValue::UInt(u64::from(r.u16()?)),
        datatype::INT32 => PropValue::Int(i64::from(r.u32()? as i32)),
        datatype::UINT32 => PropValue::UInt(u64::from(r.u32()?)),
        datatype::INT64 => PropValue::Int(r.u64()? as i64),
        datatype::UINT64 => PropValue::UInt(r.u64()?),
        datatype::INT128 | datatype::UINT128 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(r.bytes(16)?);
            PropValue::Wide(a)
        }
        datatype::STRING => PropValue::Str(r.ptp_string()?),
        d if d & datatype::ARRAY_FLAG != 0 && (d & !datatype::ARRAY_FLAG) != 0 && (d & !datatype::ARRAY_FLAG) <= 0x0A => {
            let n = r.u32()? as usize;
            if n > MAX_ARRAY || n > r.remaining() {
                return Err(PtpError::Dataset(format!("array count {n} too large")));
            }
            let inner = d & !datatype::ARRAY_FLAG;
            PropValue::Array((0..n).map(|_| read_value(r, inner)).collect::<Result<_, _>>()?)
        }
        other => return Err(PtpError::Dataset(format!("unknown datatype 0x{other:04x}"))),
    })
}

/// Encode `v` as datatype `dt` (for SetExtDeviceProp's data phase).
pub fn encode_value(dt: u16, v: &PropValue) -> Result<Vec<u8>, PtpError> {
    let bad = || PtpError::InvalidArgument(format!("value {v:?} does not fit datatype 0x{dt:04x}"));
    let int = |v: &PropValue| -> Option<i128> {
        match *v {
            PropValue::Int(i) => Some(i128::from(i)),
            PropValue::UInt(u) => Some(i128::from(u)),
            _ => None,
        }
    };
    Ok(match dt {
        datatype::INT8 => (i8::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())? as u8).to_le_bytes().to_vec(),
        datatype::UINT8 => u8::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::INT16 => i16::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::UINT16 => u16::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::INT32 => i32::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::UINT32 => u32::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::INT64 => i64::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::UINT64 => u64::try_from(int(v).ok_or_else(bad)?).map_err(|_| bad())?.to_le_bytes().to_vec(),
        datatype::STRING => match v {
            PropValue::Str(s) => crate::codec::encode_ptp_string(s),
            _ => return Err(bad()),
        },
        _ => return Err(bad()),
    })
}

/// Allowed values of a property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropForm {
    /// No form.
    None,
    /// Range.
    Range {
        /// Minimum.
        min: PropValue,
        /// Maximum.
        max: PropValue,
        /// Step.
        step: PropValue,
    },
    /// Enumeration. Newer Sony bodies send a "set" list and a "get" list.
    Enum {
        /// Settable values (the only list on single-list layouts).
        values: Vec<PropValue>,
        /// Second ("get") list, when the layout has one.
        get_values: Option<Vec<PropValue>>,
    },
}

/// One Sony extended property descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonyProp {
    /// Property code.
    pub code: u16,
    /// Datatype.
    pub datatype: u16,
    /// GetSet flag.
    pub get_set: u8,
    /// Sony's extra byte after GetSet (IsEnabled).
    pub is_enabled: u8,
    /// Factory default.
    pub default: PropValue,
    /// Current value.
    pub current: PropValue,
    /// Allowed values.
    pub form: PropForm,
}

/// Variant of the 0x9209 blob layout that parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropLayout {
    /// Width of the leading descriptor count (8 per libgphoto2, 4 tried as fallback).
    pub count_width: u8,
    /// Enumeration lists per descriptor (1 or 2).
    pub enum_lists: u8,
}

/// How a property snapshot was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMethod {
    /// Structured parse with this layout.
    Structured(PropLayout),
    /// the original byte-pattern scan (only ObjectInMemory is known).
    Scan,
    /// Nothing usable.
    Failed,
}

/// Result of [`parse_all_props`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropSnapshot {
    /// Parsed descriptors (empty unless structured).
    pub props: Vec<SonyProp>,
    /// How it was parsed.
    pub method: ParseMethod,
    /// ObjectInMemory (0xD215) current value, if found.
    pub object_in_memory: Option<u16>,
}

impl PropSnapshot {
    /// Find a descriptor.
    pub fn get(&self, code: u16) -> Option<&SonyProp> {
        self.props.iter().find(|p| p.code == code)
    }
}

fn read_enum(r: &mut Reader<'_>, dt: u16) -> Result<Vec<PropValue>, PtpError> {
    let n = r.u16()? as usize;
    if n > r.remaining() {
        return Err(PtpError::Dataset(format!("enum count {n} exceeds remaining bytes")));
    }
    (0..n).map(|_| read_value(r, dt)).collect()
}

fn parse_descriptor(r: &mut Reader<'_>, enum_lists: u8) -> Result<SonyProp, PtpError> {
    let code = r.u16()?;
    let dt = r.u16()?;
    let get_set = r.u8()?;
    let is_enabled = r.u8()?;
    let default = read_value(r, dt)?;
    let current = read_value(r, dt)?;
    let form = match r.u8()? {
        0 => PropForm::None,
        1 => PropForm::Range { min: read_value(r, dt)?, max: read_value(r, dt)?, step: read_value(r, dt)? },
        2 => {
            let values = read_enum(r, dt)?;
            let get_values = if enum_lists >= 2 { Some(read_enum(r, dt)?) } else { None };
            PropForm::Enum { values, get_values }
        }
        f => return Err(PtpError::Dataset(format!("unknown form flag {f} for prop 0x{code:04x}"))),
    };
    Ok(SonyProp { code, datatype: dt, get_set, is_enabled, default, current, form })
}

fn parse_with(buf: &[u8], layout: PropLayout) -> Result<(Vec<SonyProp>, usize), PtpError> {
    let mut r = Reader::new(buf);
    let count = if layout.count_width == 8 { r.u64()? } else { u64::from(r.u32()?) };
    // Smallest descriptor: code+dt+getset+enabled+2x u8 + form = 9 bytes.
    if count > (r.remaining() / 9) as u64 {
        return Err(PtpError::Dataset(format!("descriptor count {count} exceeds blob size")));
    }
    let mut props = Vec::with_capacity(count as usize);
    for _ in 0..count {
        props.push(parse_descriptor(&mut r, layout.enum_lists)?);
    }
    Ok((props, r.remaining()))
}

/// the original heuristic: the u16 at offset +8 from the first
/// `15 D2` byte pair. Note the original dead helper `_objects_in_memory` read
/// +7, which is wrong; only +8 is ported.
pub fn scan_object_in_memory(buf: &[u8]) -> Option<u16> {
    let idx = buf.windows(2).position(|w| w == [0x15, 0xD2])?;
    let b = buf.get(idx + 8..idx + 10)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Parse an SDIO_GetAllExtDevicePropInfo (0x9209) blob. Never fails: when no
/// structured layout parses, falls back to the byte-pattern scan.
pub fn parse_all_props(buf: &[u8]) -> PropSnapshot {
    let layouts = [
        PropLayout { count_width: 8, enum_lists: 1 },
        PropLayout { count_width: 8, enum_lists: 2 },
        PropLayout { count_width: 4, enum_lists: 1 },
        PropLayout { count_width: 4, enum_lists: 2 },
    ];
    let mut best: Option<(Vec<SonyProp>, usize, PropLayout)> = None;
    for layout in layouts {
        if let Ok((props, rest)) = parse_with(buf, layout) {
            let better = match &best {
                None => true,
                Some((_, best_rest, _)) => rest < *best_rest,
            };
            if better {
                best = Some((props, rest, layout));
            }
            if rest == 0 {
                break;
            }
        }
    }
    if let Some((props, rest, layout)) = best
        && !props.is_empty()
    {
        if rest != 0 {
            log::debug!("0x9209 parsed with {layout:?} leaving {rest} trailing bytes");
        }
        let oim = props.iter().find(|p| p.code == code::OBJECT_IN_MEMORY).and_then(|p| p.current.as_u64()).and_then(|v| u16::try_from(v).ok());
        let oim = oim.or_else(|| scan_object_in_memory(buf));
        return PropSnapshot { props, method: ParseMethod::Structured(layout), object_in_memory: oim };
    }
    match scan_object_in_memory(buf) {
        Some(v) => PropSnapshot { props: Vec::new(), method: ParseMethod::Scan, object_in_memory: Some(v) },
        None => PropSnapshot { props: Vec::new(), method: ParseMethod::Failed, object_in_memory: None },
    }
}

/// SDIO_GetExtDeviceInfo (0x9202) payload (EXT, unverified): u16 version,
/// u32-counted u16 property codes, u32-counted u16 control codes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtDeviceInfo {
    /// SDIO protocol version reported.
    pub version: u16,
    /// Supported property codes.
    pub props: Vec<u16>,
    /// Supported control codes.
    pub controls: Vec<u16>,
}

impl ExtDeviceInfo {
    /// Parse; a missing control list is tolerated.
    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        let mut r = Reader::new(buf);
        let version = r.u16()?;
        let props = r.u16_array()?;
        let controls = r.u16_array().unwrap_or_default();
        Ok(Self { version, props, controls })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One u16 descriptor in libgphoto2 layout, enum form.
    pub(crate) fn u16_desc(code: u16, current: u16, enum_vals: &[u16], lists: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&code.to_le_bytes());
        b.extend_from_slice(&datatype::UINT16.to_le_bytes());
        b.push(1); // get/set
        b.push(1); // enabled
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&current.to_le_bytes());
        b.push(2);
        for _ in 0..lists {
            b.extend_from_slice(&(enum_vals.len() as u16).to_le_bytes());
            for v in enum_vals {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b
    }

    pub(crate) fn blob(descs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = (descs.len() as u64).to_le_bytes().to_vec();
        for d in descs {
            b.extend_from_slice(d);
        }
        b
    }

    fn u32_range_desc(code: u16, current: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&code.to_le_bytes());
        b.extend_from_slice(&datatype::UINT32.to_le_bytes());
        b.extend_from_slice(&[1, 1]);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&current.to_le_bytes());
        b.push(1);
        for v in [50u32, 102_400, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn structured_parse_single_enum_list() {
        // A value that contains the bytes 15 D2 *before* the real 0xD215
        // descriptor: the naive scan would read the wrong offset.
        let decoy = u16_desc(code::FOCUS_MODE, 0xD215, &[0xD215, 2], 1);
        let b = blob(&[decoy, u32_range_desc(code::ISO, 400), u16_desc(code::OBJECT_IN_MEMORY, 0x8001, &[], 1)]);
        let s = parse_all_props(&b);
        assert_eq!(s.method, ParseMethod::Structured(PropLayout { count_width: 8, enum_lists: 1 }));
        assert_eq!(s.object_in_memory, Some(0x8001));
        assert_ne!(scan_object_in_memory(&b), Some(0x8001), "scan is fooled by the decoy");
        assert_eq!(s.get(code::ISO).unwrap().current, PropValue::UInt(400));
        assert!(matches!(s.get(code::ISO).unwrap().form, PropForm::Range { .. }));
    }

    #[test]
    fn structured_parse_two_enum_lists() {
        let b = blob(&[u16_desc(code::FOCUS_MODE, 2, &[1, 2, 3], 2), u16_desc(code::OBJECT_IN_MEMORY, 0x8002, &[0], 2)]);
        let s = parse_all_props(&b);
        assert_eq!(s.method, ParseMethod::Structured(PropLayout { count_width: 8, enum_lists: 2 }));
        assert_eq!(s.object_in_memory, Some(0x8002));
        match &s.get(code::FOCUS_MODE).unwrap().form {
            PropForm::Enum { values, get_values } => {
                assert_eq!(values.len(), 3);
                assert_eq!(get_values.as_ref().unwrap().len(), 3);
            }
            f => panic!("{f:?}"),
        }
    }

    #[test]
    fn fake_blob_falls_back_to_scan_at_plus_8() {
        // tests/test_cameras.py _QueueFake: 15 d2 + 6 zero bytes + status.
        let mut b = vec![0x15, 0xD2, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&0x8003u16.to_le_bytes());
        let s = parse_all_props(&b);
        assert_eq!(s.method, ParseMethod::Scan);
        assert_eq!(s.object_in_memory, Some(0x8003));
        // The +7 bug would read 0x0380.
        assert_ne!(s.object_in_memory, Some(0x0380));
    }

    #[test]
    fn garbage_never_panics() {
        assert_eq!(parse_all_props(&[]).method, ParseMethod::Failed);
        assert_eq!(parse_all_props(&[0x15, 0xD2, 0]).object_in_memory, None);
        let b = blob(&[u16_desc(code::OBJECT_IN_MEMORY, 1, &[1, 2], 1)]);
        for n in 0..b.len() {
            let _ = parse_all_props(&b[..n]);
        }
        let mut huge = u64::MAX.to_le_bytes().to_vec();
        huge.extend_from_slice(&[0xFF; 64]);
        let _ = parse_all_props(&huge);
    }

    #[test]
    fn values_roundtrip() {
        for (dt, v) in [
            (datatype::UINT16, PropValue::UInt(0x8000)),
            (datatype::INT16, PropValue::Int(-1)),
            (datatype::UINT32, PropValue::UInt(0x00FF_FFFF)),
            (datatype::INT8, PropValue::Int(-5)),
            (datatype::UINT64, PropValue::UInt(u64::MAX)),
            (datatype::STRING, PropValue::Str("PC".into())),
        ] {
            let enc = encode_value(dt, &v).unwrap();
            assert_eq!(read_value(&mut Reader::new(&enc), dt).unwrap(), v);
        }
        assert!(encode_value(datatype::UINT8, &PropValue::UInt(300)).is_err());
        assert!(encode_value(datatype::UINT16, &PropValue::Str("x".into())).is_err());
        let arr = [2u8, 0, 0, 0, 1, 0, 2, 0];
        assert_eq!(
            read_value(&mut Reader::new(&arr), datatype::ARRAY_FLAG | datatype::UINT16).unwrap(),
            PropValue::Array(vec![PropValue::UInt(1), PropValue::UInt(2)])
        );
    }

    #[test]
    fn ext_device_info() {
        let mut b = 200u16.to_le_bytes().to_vec();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&0xD215u16.to_le_bytes());
        b.extend_from_slice(&0x5007u16.to_le_bytes());
        let e = ExtDeviceInfo::parse(&b).unwrap();
        assert_eq!(e.version, 200);
        assert_eq!(e.props, vec![0xD215, 0x5007]);
        assert!(e.controls.is_empty());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0xD2C1u16.to_le_bytes());
        assert_eq!(ExtDeviceInfo::parse(&b).unwrap().controls, vec![0xD2C1]);
        assert!(ExtDeviceInfo::parse(&[1]).is_err());
    }
}
