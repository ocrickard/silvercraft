//! PIMA 15740 USB generic container codec (little-endian) and a bounds-checked
//! dataset reader.
//!
//! ```text
//! offset size field
//! 0      u32  length          total container bytes including this 12-byte header
//! 4      u16  type            1=Command 2=Data 3=Response 4=Event
//! 6      u16  code            opcode / response code / event code
//! 8      u32  transaction_id
//! 12     ...  payload         Command/Response/Event: u32 params; Data: raw bytes
//! ```

use crate::PtpError;

/// Container header length.
pub const HEADER_LEN: usize = 12;
/// Most parameters a command or response may carry.
pub const MAX_PARAMS: usize = 5;
/// A data container `length` of 0xFFFFFFFF means "4 GiB or more: read until a
/// short packet".
pub const LENGTH_UNKNOWN: u32 = 0xFFFF_FFFF;

/// Container types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ContainerKind {
    /// Host to device operation request.
    Command = 1,
    /// Data phase (either direction).
    Data = 2,
    /// Device response.
    Response = 3,
    /// Asynchronous event.
    Event = 4,
}

impl ContainerKind {
    /// Decode the wire value.
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(Self::Command),
            2 => Some(Self::Data),
            3 => Some(Self::Response),
            4 => Some(Self::Event),
            _ => None,
        }
    }
}

/// Standard operation codes used here.
pub mod op {
    /// GetDeviceInfo.
    pub const GET_DEVICE_INFO: u16 = 0x1001;
    /// OpenSession.
    pub const OPEN_SESSION: u16 = 0x1002;
    /// CloseSession.
    pub const CLOSE_SESSION: u16 = 0x1003;
    /// GetStorageIDs.
    pub const GET_STORAGE_IDS: u16 = 0x1004;
    /// GetObjectHandles.
    pub const GET_OBJECT_HANDLES: u16 = 0x1007;
    /// GetObjectInfo.
    pub const GET_OBJECT_INFO: u16 = 0x1008;
    /// GetObject.
    pub const GET_OBJECT: u16 = 0x1009;
}

/// Response codes.
pub mod rc {
    /// OK.
    pub const OK: u16 = 0x2001;
    /// General error.
    pub const GENERAL_ERROR: u16 = 0x2002;
    /// Session not open.
    pub const SESSION_NOT_OPEN: u16 = 0x2003;
    /// Operation not supported.
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    /// Invalid object handle (Sony: "nothing there yet").
    pub const INVALID_OBJECT_HANDLE: u16 = 0x2009;
    /// Device busy.
    pub const DEVICE_BUSY: u16 = 0x2019;
    /// Session already open (treated as success by OpenSession).
    pub const SESSION_ALREADY_OPEN: u16 = 0x201E;
    /// Sony SDIO_Connect's usual answer; harmless.
    pub const SONY_CONNECT_A101: u16 = 0xA101;
}

/// Event codes.
pub mod event {
    /// Standard ObjectAdded.
    pub const OBJECT_ADDED: u16 = 0x4002;
    /// Standard CaptureComplete.
    pub const CAPTURE_COMPLETE: u16 = 0x400D;
    /// Sony ObjectAdded (EXT, unverified) (param: handle, often 0xFFFFC001).
    pub const SONY_OBJECT_ADDED: u16 = 0xC201;
    /// Sony ObjectRemoved (EXT, unverified).
    pub const SONY_OBJECT_REMOVED: u16 = 0xC202;
    /// Sony DevicePropChanged (EXT, unverified) (param: property code).
    pub const SONY_PROP_CHANGED: u16 = 0xC203;
}

/// A parsed container header. `kind` is kept raw so invalid values can be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Total container length including the header.
    pub length: u32,
    /// Raw container type.
    pub kind: u16,
    /// Operation / response / event code.
    pub code: u16,
    /// Transaction id.
    pub txn: u32,
}

impl Header {
    /// Parse the first 12 bytes; `None` when `buf` is shorter.
    pub fn parse(buf: &[u8]) -> Option<Header> {
        let h = buf.get(..HEADER_LEN)?;
        Some(Header {
            length: u32::from_le_bytes([h[0], h[1], h[2], h[3]]),
            kind: u16::from_le_bytes([h[4], h[5]]),
            code: u16::from_le_bytes([h[6], h[7]]),
            txn: u32::from_le_bytes([h[8], h[9], h[10], h[11]]),
        })
    }

    /// Typed container kind.
    pub fn container_kind(&self) -> Option<ContainerKind> {
        ContainerKind::from_u16(self.kind)
    }

    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.length.to_le_bytes());
        out[4..6].copy_from_slice(&self.kind.to_le_bytes());
        out[6..8].copy_from_slice(&self.code.to_le_bytes());
        out[8..12].copy_from_slice(&self.txn.to_le_bytes());
        out
    }
}

fn encode_params(kind: ContainerKind, code: u16, txn: u32, params: &[u32]) -> Result<Vec<u8>, PtpError> {
    if params.len() > MAX_PARAMS {
        return Err(PtpError::InvalidArgument(format!("PTP containers take at most {MAX_PARAMS} parameters, got {}", params.len())));
    }
    // len <= 12 + 20, cannot overflow u32.
    let length = (HEADER_LEN + 4 * params.len()) as u32;
    let mut out = Vec::with_capacity(length as usize);
    out.extend_from_slice(&Header { length, kind: kind as u16, code, txn }.encode());
    for p in params {
        out.extend_from_slice(&p.to_le_bytes());
    }
    Ok(out)
}

/// Encode a command container (at most 5 params).
pub fn encode_command(code: u16, txn: u32, params: &[u32]) -> Result<Vec<u8>, PtpError> {
    encode_params(ContainerKind::Command, code, txn, params)
}

/// Encode a response container (at most 5 params). Used by fakes and tests.
pub fn encode_response(code: u16, txn: u32, params: &[u32]) -> Result<Vec<u8>, PtpError> {
    encode_params(ContainerKind::Response, code, txn, params)
}

/// Encode an event container (at most 5 params). Used by fakes and tests.
pub fn encode_event(code: u16, txn: u32, params: &[u32]) -> Result<Vec<u8>, PtpError> {
    encode_params(ContainerKind::Event, code, txn, params)
}

/// Encode a data container carrying `payload` with the operation's code and
/// transaction id. Payloads of 4 GiB or more get the 0xFFFFFFFF length marker.
pub fn encode_data(code: u16, txn: u32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len().saturating_add(HEADER_LEN)).unwrap_or(LENGTH_UNKNOWN);
    let mut out = Vec::with_capacity(payload.len() + HEADER_LEN);
    out.extend_from_slice(&Header { length, kind: ContainerKind::Data as u16, code, txn }.encode());
    out.extend_from_slice(payload);
    out
}

/// A response (or event) container: code, transaction id and parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Response code.
    pub code: u16,
    /// Transaction id.
    pub txn: u32,
    /// Parameters (at most 5 are read).
    pub params: Vec<u32>,
}

impl Response {
    /// `code == OK`.
    pub fn is_ok(&self) -> bool {
        self.code == rc::OK
    }
}

/// An event container.
pub type Event = Response;

fn parse_params_container(buf: &[u8], want: ContainerKind) -> Result<Response, PtpError> {
    let h = Header::parse(buf).ok_or_else(|| PtpError::InvalidContainer(format!("{} bytes is shorter than a header", buf.len())))?;
    if h.kind != want as u16 {
        return Err(PtpError::InvalidContainer(format!("expected {want:?} container, got type 0x{:04x}", h.kind)));
    }
    if (h.length as usize) < HEADER_LEN {
        return Err(PtpError::InvalidContainer(format!("container length {} below header size", h.length)));
    }
    let end = (h.length as usize).min(buf.len());
    let n = ((end - HEADER_LEN) / 4).min(MAX_PARAMS);
    let params = (0..n)
        .filter_map(|i| {
            let o = HEADER_LEN + 4 * i;
            buf.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        })
        .collect();
    Ok(Response { code: h.code, txn: h.txn, params })
}

/// Parse a response container)`).
pub fn parse_response(buf: &[u8]) -> Result<Response, PtpError> {
    parse_params_container(buf, ContainerKind::Response)
}

/// Parse an event container.
pub fn parse_event(buf: &[u8]) -> Result<Event, PtpError> {
    parse_params_container(buf, ContainerKind::Event)
}

/// Bounds-checked little-endian reader for PTP datasets. Every read returns
/// an error instead of panicking on truncation.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Read from the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Take `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], PtpError> {
        let end = self.pos.checked_add(n).ok_or_else(|| PtpError::Dataset("length overflow".into()))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| PtpError::Dataset(format!("need {n} bytes at offset {}, only {} left", self.pos, self.remaining())))?;
        self.pos = end;
        Ok(s)
    }

    /// Skip `n` bytes.
    pub fn skip(&mut self, n: usize) -> Result<(), PtpError> {
        self.bytes(n).map(|_| ())
    }

    /// u8.
    pub fn u8(&mut self) -> Result<u8, PtpError> {
        Ok(self.bytes(1)?[0])
    }

    /// u16 LE.
    pub fn u16(&mut self) -> Result<u16, PtpError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// u32 LE.
    pub fn u32(&mut self) -> Result<u32, PtpError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// u64 LE.
    pub fn u64(&mut self) -> Result<u64, PtpError> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A u32-counted array of u16 (the standard PTP array encoding). The count
    /// is checked against the remaining bytes before allocating.
    pub fn u16_array(&mut self) -> Result<Vec<u16>, PtpError> {
        let n = self.u32()? as usize;
        if n > self.remaining() / 2 {
            return Err(PtpError::Dataset(format!("u16 array count {n} exceeds remaining {} bytes", self.remaining())));
        }
        (0..n).map(|_| self.u16()).collect()
    }

    /// A u32-counted array of u32.
    pub fn u32_array(&mut self) -> Result<Vec<u32>, PtpError> {
        let n = self.u32()? as usize;
        if n > self.remaining() / 4 {
            return Err(PtpError::Dataset(format!("u32 array count {n} exceeds remaining {} bytes", self.remaining())));
        }
        (0..n).map(|_| self.u32()).collect()
    }

    /// PTP string: u8 char count (including the NUL), then UTF-16LE. Lone
    /// surrogates are replaced, a missing NUL is tolerated.
    pub fn ptp_string(&mut self) -> Result<String, PtpError> {
        let n = self.u8()? as usize;
        let raw = self.bytes(n * 2)?;
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Ok(char::decode_utf16(units[..end].iter().copied()).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)).collect())
    }
}

/// Encode a PTP string (u8 count including NUL + UTF-16LE). Strings longer
/// than 254 UTF-16 units are truncated.
pub fn encode_ptp_string(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return vec![0];
    }
    let mut units: Vec<u16> = s.encode_utf16().take(254).collect();
    units.push(0);
    let mut out = Vec::with_capacity(1 + units.len() * 2);
    out.push(units.len() as u8);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_layout_matches_spec() {
        let p = encode_command(op::OPEN_SESSION, 1, &[0xABCD]).unwrap();
        assert_eq!(p.len(), 16);
        let h = Header::parse(&p).unwrap();
        assert_eq!(h, Header { length: 16, kind: 1, code: 0x1002, txn: 1 });
        assert_eq!(&p[12..], &0xABCDu32.to_le_bytes());
    }

    #[test]
    fn too_many_params_rejected() {
        assert!(encode_command(0x1001, 1, &[1, 2, 3, 4, 5, 6]).is_err());
        assert!(encode_command(0x1001, 1, &[1, 2, 3, 4, 5]).is_ok());
    }

    #[test]
    fn data_roundtrip_header() {
        let p = encode_data(op::GET_OBJECT, 9, &[1, 2, 3, 4]);
        let h = Header::parse(&p).unwrap();
        assert_eq!(h.length as usize, p.len());
        assert_eq!(h.container_kind(), Some(ContainerKind::Data));
        assert_eq!(h.code, op::GET_OBJECT);
        assert_eq!(h.txn, 9);
        assert_eq!(&p[12..], &[1, 2, 3, 4]);
    }

    #[test]
    fn response_with_params() {
        let r = encode_response(rc::OK, 3, &[7, 42]).unwrap();
        let p = parse_response(&r).unwrap();
        assert_eq!(p, Response { code: rc::OK, txn: 3, params: vec![7, 42] });
        assert!(parse_response(&encode_command(0x1001, 1, &[]).unwrap()).is_err());
        assert!(parse_event(&encode_event(0xC201, 0, &[0xFFFF_C001]).unwrap()).unwrap().params == vec![0xFFFF_C001]);
    }

    #[test]
    fn malformed_responses_do_not_panic() {
        assert!(parse_response(&[]).is_err());
        assert!(parse_response(&[3, 0, 0]).is_err());
        // length below header
        let mut r = encode_response(rc::OK, 1, &[]).unwrap();
        r[0] = 4;
        assert!(parse_response(&r).is_err());
        // length claims more than present: params counted from the bytes we have
        let mut r = encode_response(rc::OK, 1, &[5]).unwrap();
        r[0..4].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse_response(&r).unwrap().params, vec![5]);
        // trailing partial param ignored
        let mut r = encode_response(rc::OK, 1, &[5]).unwrap();
        r.extend_from_slice(&[1, 2]);
        let n = r.len() as u32;
        r[0..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(parse_response(&r).unwrap().params, vec![5]);
    }

    #[test]
    fn reader_bounds() {
        let mut r = Reader::new(&[1, 0, 0]);
        assert_eq!(r.u16().unwrap(), 1);
        assert!(r.u16().is_err());
        assert!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF]).u16_array().is_err());
        assert!(Reader::new(&[0xFF, 0xFF, 0xFF, 0x7F, 1]).u32_array().is_err());
        assert!(Reader::new(&[]).ptp_string().is_err());
        assert!(Reader::new(&[5, b'a', 0]).ptp_string().is_err());
    }

    #[test]
    fn ptp_string_roundtrip() {
        for s in ["", "DSC01234.ARW", "Grüße ✓"] {
            let enc = encode_ptp_string(s);
            assert_eq!(Reader::new(&enc).ptp_string().unwrap(), s);
        }
        // missing NUL and lone surrogate tolerated
        let raw = [2u8, 0x00, 0xD8, b'a', 0];
        assert_eq!(Reader::new(&raw).ptp_string().unwrap(), "\u{FFFD}a");
    }
}
