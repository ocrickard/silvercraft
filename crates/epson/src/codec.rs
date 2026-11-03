//! ESC/I + B8 wire codec: command bytes, the FS W parameter block, the FS G start
//! response, the ESC # handshake transform, ESC z gamma LUTs, scan geometry and the
//! identity/status parsers. Pure functions; nothing here touches a transport.
//!
//! Byte values marked *replayed* come from a vendor-driver USB trace kept
//! outside this repository. Their meaning is unknown; they are reproduced
//! exactly because they are the only hardware-qualified values.

use crate::EpsonError;

/// USB vendor id (Epson).
pub const VENDOR_ID: u16 = 0x04B8;
/// USB product id (Perfection V800/V850).
pub const PRODUCT_ID: u16 = 0x0151;
/// Interface number claimed.
pub const INTERFACE: u8 = 0;
/// Bulk OUT endpoint.
pub const EP_OUT: u8 = 0x01;
/// Bulk IN endpoint.
pub const EP_IN: u8 = 0x82;
/// Bulk packet size; IN requests are rounded up to whole packets.
pub const USB_PACKET: usize = 512;

/// Acknowledge.
pub const ACK: u8 = 0x06;
/// Negative acknowledge (e.g. FS W width > ~4096 px).
pub const NAK: u8 = 0x15;
/// Cancel; the device answers one byte (typically ACK).
pub const CAN: u8 = 0x18;

/// `ESC @` initialize → ACK.
pub const ESC_AT: [u8; 2] = [0x1B, 0x40];
/// `ESC F` status → 4 bytes.
pub const ESC_F: [u8; 2] = [0x1B, 0x46];
/// `FS F` extended status → 16 bytes.
pub const FS_F: [u8; 2] = [0x1C, 0x46];
/// `FS I` extended identity → 80 bytes.
pub const FS_I: [u8; 2] = [0x1C, 0x49];
/// `ESC I` inquiry → 4-byte header + payload.
pub const ESC_I: [u8; 2] = [0x1B, 0x49];
/// `ESC f` extended status → 4-byte header + `header[2]` bytes.
pub const ESC_LOWER_F: [u8; 2] = [0x1B, 0x66];
/// `ESC D` data format (bit-depth probe) → ACK, then 1-byte parameter → ACK.
pub const ESC_D: [u8; 2] = [0x1B, 0x44];
/// `ESC z` gamma LUT upload → ACK, then 257 bytes → ACK.
pub const ESC_LOWER_Z: [u8; 2] = [0x1B, 0x7A];
/// `FS W` set scan parameters → ACK, then 64 bytes → ACK.
pub const FS_W: [u8; 2] = [0x1C, 0x57];
/// `ESC Z` selector → ACK, then 1 byte → ACK.
pub const ESC_Z: [u8; 2] = [0x1B, 0x5A];
/// `FS G` start scan / start poll → 14 bytes.
pub const FS_G: [u8; 2] = [0x1C, 0x47];
/// `FS S` read current parameter block → 64 bytes (IR-arming handshake source).
pub const FS_S: [u8; 2] = [0x1C, 0x53];
/// `ESC #` extended handshake → ACK, then 32 bytes → ACK.
pub const ESC_HASH: [u8; 2] = [0x1B, 0x23];

/// `ESC F` reply of an idle, healthy scanner.
pub const CLEAN_IDLE: [u8; 4] = [0x02, 0x12, 0x00, 0x00];

/// Largest single bulk read ever issued (SBP2 block, ED:157; §1.4).
pub const MAX_READ: usize = 0x20000;
/// Lines per device raster chunk.
pub const CHUNK_LINES: u32 = 32;
/// Widest scan read in one pass; wider scans are tiled (FS W NAKs ~>4096 px, EP:1517-1530).
pub const MAX_SINGLE_READ_WIDTH_PX: u32 = 4096;
/// Column overlap between wide dual-pass tiles.
pub const TILE_OVERLAP_PX: u32 = 64;
/// Transparency-unit film area width in inches; beyond hard-locks the firmware.
pub const TPU_MAX_WIDTH_IN: f64 = 8.0;
/// Transparency-unit film area height in inches.
pub const TPU_MAX_HEIGHT_IN: f64 = 10.0;
/// Highest dpi offered. 6400 is not hardware-tested (§8).
pub const MAX_DPI: u32 = 6400;
/// Preview priming cycles before the first scan handle.
pub const PRIMING_CYCLES: usize = 3;

/// *Replayed*: the ESC D bit-depth probe values the vendor sends, in this order.
pub const ESC_D_PROBES: [u8; 3] = [0x08, 0x10, 0x08];
/// *Replayed*: the ESC Z selector value (sample-depth selector + 3; ED:4583-4586, ED:161-164).
pub const ESC_Z_VALUE: u8 = 0x03;

/// Key of the ESC # payload transform.
pub const HANDSHAKE_KEY: [u8; 16] = [0xC6, 0x30, 0xB8, 0x53, 0x9C, 0x00, 0x00, 0x00, 0x81, 0x79, 0xF9, 0x44, 0x97, 0x1A, 0x5D, 0x36];

// --- FS W parameter block bytes. ---
/// Bit depth byte (offset 25): production is 16-bit only.
pub const PARAM_BIT_DEPTH: u8 = 16;
/// *Replayed*: visible colour byte (offset 24), "B8 colour byte sequence".
pub const VISIBLE_COLOR_BYTE: u8 = 0x12;
/// *Replayed*: visible source byte (offset 26), transparency unit.
pub const VISIBLE_SOURCE_BYTE: u8 = 0x01;
/// *Replayed*: visible control byte 28 (vendor trace value, ED:4281-4289).
pub const VISIBLE_BYTE_28: u8 = 0x60;
/// *Replayed*: visible mode byte (offset 38).
pub const VISIBLE_MODE_BYTE: u8 = 0x00;
/// *Replayed* (live vendor capture 2026-06-18): IR colour byte; IR arming is keyed on it.
pub const IR_COLOR_BYTE: u8 = 0x13;
/// *Replayed*: IR source byte (TPU infrared).
pub const IR_SOURCE_BYTE: u8 = 0x03;
/// *Replayed*: IR control byte 28.
pub const IR_BYTE_28: u8 = 0x20;
/// *Replayed*: IR mode byte (offset 38).
pub const IR_MODE_BYTE: u8 = 0x02;
/// *Replayed*: byte 29, both passes.
pub const PARAM_BYTE_29: u8 = 0x03;
/// *Replayed*: byte 32, both passes.
pub const PARAM_BYTE_32: u8 = 0x01;
/// *Replayed*: byte 33, both passes.
pub const PARAM_BYTE_33: u8 = 0x80;

/// A scan region in inches from the transparency-unit film-area origin.
///
/// Construction validates the hardware guard (§2.6): positive finite size, non-negative
/// origin, and the area within 8.0 × 10.0 in (+1e-6). An out-of-range area hard-locks the
/// firmware, so an invalid `Region` cannot exist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

impl Region {
    /// Validate and build a region (inches).
    pub fn new(left: f64, top: f64, width: f64, height: f64) -> Result<Region, EpsonError> {
        if ![left, top, width, height].iter().all(|v| v.is_finite()) {
            return Err(EpsonError::InvalidRequest("scan region must be finite".into()));
        }
        if width <= 0.0 || height <= 0.0 {
            return Err(EpsonError::InvalidRequest("scan region width and height must be positive".into()));
        }
        if left < 0.0 || top < 0.0 {
            return Err(EpsonError::InvalidRequest("scan region must start inside the film area".into()));
        }
        if left + width > TPU_MAX_WIDTH_IN + 1e-6 || top + height > TPU_MAX_HEIGHT_IN + 1e-6 {
            return Err(EpsonError::InvalidRequest(format!(
                "scan region {left:.3},{top:.3} {width:.3}x{height:.3} in exceeds the {TPU_MAX_WIDTH_IN}x{TPU_MAX_HEIGHT_IN} in \
                 transparency-unit film area; requesting more hard-locks the scanner"
            )));
        }
        Ok(Region { left, top, width, height })
    }

    /// Left edge, inches.
    pub fn left(&self) -> f64 {
        self.left
    }
    /// Top edge, inches.
    pub fn top(&self) -> f64 {
        self.top
    }
    /// Width, inches.
    pub fn width(&self) -> f64 {
        self.width
    }
    /// Height, inches.
    pub fn height(&self) -> f64 {
        self.height
    }
}

/// A validated resolution, 1..=6400 dpi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dpi(u32);

/// The bed-preview resolution.
pub const PREVIEW_DPI: Dpi = Dpi(200);

impl Dpi {
    /// Validate a resolution.
    pub fn new(dpi: u32) -> Result<Dpi, EpsonError> {
        if dpi == 0 || dpi > MAX_DPI {
            return Err(EpsonError::InvalidRequest(format!("dpi {dpi} outside 1..={MAX_DPI}")));
        }
        Ok(Dpi(dpi))
    }
    /// The value.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// A scan area in device pixels at the scan dpi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceArea {
    /// Origin x (pixels).
    pub x: u32,
    /// Origin y (lines).
    pub y: u32,
    /// Width (pixels, a multiple of 8).
    pub width: u32,
    /// Height (lines).
    pub height: u32,
}

/// The platen size in device pixels at `dpi` (8.0 × 10.0 in).
pub fn platen_pixels(dpi: Dpi) -> (u32, u32) {
    let d = f64::from(dpi.get());
    ((TPU_MAX_WIDTH_IN * d) as u32, (TPU_MAX_HEIGHT_IN * d) as u32)
}

impl DeviceArea {
    /// True when the origin is (0,0): the only scan an uncalibrated carriage arms.
    pub fn at_origin(&self) -> bool {
        self.x == 0 && self.y == 0
    }
    /// True when this area is safe to put on the wire at `dpi`: a positive multiple-of-8
    /// width and every pixel inside the 8×10 in film area.
    pub fn fits_platen(&self, dpi: Dpi) -> bool {
        let (max_w, max_h) = platen_pixels(dpi);
        self.width >= 8
            && self.width.is_multiple_of(8)
            && self.height >= 1
            && self.x.checked_add(self.width).is_some_and(|r| r <= max_w)
            && self.y.checked_add(self.height).is_some_and(|b| b <= max_h)
    }
    /// This area pulled inside the platen at `dpi`: the size shrinks (width stays a
    /// multiple of 8), and an origin at or past the far edge moves back so a minimum
    /// 8×1 strip remains. The result always [`fits_platen`](Self::fits_platen).
    pub fn clamped_to_platen(mut self, dpi: Dpi) -> DeviceArea {
        let (max_w, max_h) = platen_pixels(dpi);
        self.width = (self.width / 8 * 8).clamp(8, max_w);
        if self.x >= max_w {
            self.x = max_w - 8;
        }
        if self.x + self.width > max_w {
            self.width = (max_w - self.x) / 8 * 8;
            if self.width < 8 {
                self.x = max_w - 8;
                self.width = 8;
            }
        }
        self.height = self.height.clamp(1, max_h);
        if self.y >= max_h {
            self.y = max_h - 1;
        }
        if self.y + self.height > max_h {
            self.height = max_h - self.y;
        }
        self
    }
    /// Bytes of a full 3-sample 16-bit raster of this area.
    pub fn raster_bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 6
    }
}

fn round_px(v: f64) -> u32 {
    // Region bounds keep v <= 10 in * 6400 dpi; clamp anyway so a float edge case can't wrap.
    let r = v.round_ties_even();
    if r <= 0.0 {
        0
    } else if r >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        r as u32
    }
}

/// Inches → device pixels, bit-exact with the golden vectors: Python `round` is
/// round-half-to-even; the width is floored to a multiple of **8** px.
///
/// Ambiguity: code comments say the scanner quantises widths to a 64-px grid
/// while `area_pixels` floors to 8. We follow the code (8), which is what the hardware
/// qualified (1176 × 1940 bed preview, 1176 = 8·147, not a multiple of 64).
pub fn area_pixels(region: &Region, dpi: Dpi) -> DeviceArea {
    let d = f64::from(dpi.get());
    let x = round_px(region.left * d);
    let y = round_px(region.top * d);
    let w = round_px(region.width * d).max(8);
    let h = round_px(region.height * d).max(1);
    // Quantising can push a region that ends exactly at the film-area edge past it
    // (7.999 in + 0.001 in at 200 dpi rounds to x=1600, w=8 = 8.04 in). Inches were
    // validated in `Region::new`; device pixels are revalidated here, so the area
    // that reaches FS W is inside the platen at every dpi.
    DeviceArea { x, y, width: 8 * (w / 8), height: h }.clamped_to_platen(dpi)
}

/// Which pass a parameter block configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    /// Visible RGB; `high_speed` sets byte 27 (the origin prescan uses it).
    Visible {
        /// High-speed flag (byte 27).
        high_speed: bool,
    },
    /// Infrared (dual pass second handle).
    Infrared,
}

/// Build the 64-byte FS W parameter block (§2.3). All other bytes are zero.
///
/// `DeviceArea` fields are public, so this is the last line of defence: the area is
/// clamped to the platen here too, and the bytes on the wire can never describe a
/// scan outside the 8×10 in film area however the area was constructed.
pub fn param_block(area: &DeviceArea, dpi: Dpi, pass: PassKind) -> [u8; 64] {
    let area = &area.clamped_to_platen(dpi);
    let mut p = [0u8; 64];
    let mut put = |off: usize, v: u32| {
        if let Some(s) = p.get_mut(off..off + 4) {
            s.copy_from_slice(&v.to_le_bytes());
        }
    };
    put(0, dpi.get());
    put(4, dpi.get());
    put(8, area.x);
    put(12, area.y);
    put(16, area.width);
    put(20, area.height);
    let (color, source, high_speed, b28, mode) = match pass {
        PassKind::Visible { high_speed } => (VISIBLE_COLOR_BYTE, VISIBLE_SOURCE_BYTE, u8::from(high_speed), VISIBLE_BYTE_28, VISIBLE_MODE_BYTE),
        PassKind::Infrared => (IR_COLOR_BYTE, IR_SOURCE_BYTE, 0, IR_BYTE_28, IR_MODE_BYTE),
    };
    p[24] = color;
    p[25] = PARAM_BIT_DEPTH;
    p[26] = source;
    p[27] = high_speed;
    p[28] = b28;
    p[29] = PARAM_BYTE_29;
    p[32] = PARAM_BYTE_32;
    p[33] = PARAM_BYTE_33;
    p[38] = mode;
    p
}

/// The fields of an FS W block, as the simulator decodes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedParams {
    /// Main dpi.
    pub dpi: u32,
    /// Area.
    pub area: DeviceArea,
    /// Colour byte (0x12 visible, 0x13 IR).
    pub color: u8,
    /// Bit depth byte.
    pub bit_depth: u8,
    /// Source byte.
    pub source: u8,
    /// Mode byte (38).
    pub mode: u8,
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Decode an FS W block (None unless exactly 64 bytes).
pub fn parse_param_block(b: &[u8]) -> Option<ParsedParams> {
    if b.len() != 64 {
        return None;
    }
    Some(ParsedParams {
        dpi: u32_at(b, 0)?,
        area: DeviceArea { x: u32_at(b, 8)?, y: u32_at(b, 12)?, width: u32_at(b, 16)?, height: u32_at(b, 20)? },
        color: *b.get(24)?,
        bit_depth: *b.get(25)?,
        source: *b.get(26)?,
        mode: *b.get(38)?,
    })
}

/// The 14-byte FS G start response (§2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartResponse {
    /// Raw bytes.
    pub raw: [u8; 14],
}

impl StartResponse {
    /// Parse; anything but exactly 14 bytes is a protocol error.
    pub fn parse(b: &[u8]) -> Result<StartResponse, EpsonError> {
        let raw: [u8; 14] = b.try_into().map_err(|_| EpsonError::Protocol(format!("FS G start response has {} bytes, expected 14", b.len())))?;
        Ok(StartResponse { raw })
    }
    /// Status flags (byte 1).
    pub fn status(&self) -> u8 {
        self.raw[1]
    }
    /// Production busy rule: `byte1 & 0x80`.
    pub fn busy(&self) -> bool {
        self.raw[1] & 0x80 != 0
    }
    /// Image bytes per full chunk.
    pub fn unit_word(&self) -> u32 {
        u32::from_le_bytes([self.raw[2], self.raw[3], self.raw[4], self.raw[5]])
    }
    /// Number of full chunks.
    pub fn full_chunk_count(&self) -> u32 {
        u32::from_le_bytes([self.raw[6], self.raw[7], self.raw[8], self.raw[9]])
    }
    /// Image bytes of the final remainder chunk (0 = none).
    pub fn remainder(&self) -> u32 {
        u32::from_le_bytes([self.raw[10], self.raw[11], self.raw[12], self.raw[13]])
    }
    /// The transfer plan (§3.5). Errors when the device gave no geometry.
    pub fn plan(&self) -> Result<TransferPlan, EpsonError> {
        let unit = self.unit_word();
        if unit == 0 {
            return Err(EpsonError::Protocol(format!("start returned no transfer geometry: {}", hex(&self.raw))));
        }
        let (full, remainder) = match (self.full_chunk_count(), self.remainder()) {
            (0, 0) => (1, 0),
            other => other,
        };
        Ok(TransferPlan { unit, full, remainder })
    }
}

/// Planned raster chunks: `full` chunks of `unit` bytes then one of `remainder` (if > 0).
/// Each chunk arrives followed by one trailing status byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferPlan {
    /// Bytes per full chunk.
    pub unit: u32,
    /// Full chunk count.
    pub full: u32,
    /// Remainder chunk bytes (0 = none).
    pub remainder: u32,
}

impl TransferPlan {
    /// Number of chunks.
    pub fn chunk_count(&self) -> u64 {
        u64::from(self.full) + u64::from(self.remainder > 0)
    }
    /// Image bytes in chunk `i`.
    pub fn chunk_len(&self, i: u64) -> u32 {
        if i < u64::from(self.full) { self.unit } else { self.remainder }
    }
    /// Total image bytes.
    pub fn total_bytes(&self) -> u64 {
        u64::from(self.unit) * u64::from(self.full) + u64::from(self.remainder)
    }
}

/// A 4-byte `ESC F` status reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Raw reply (any length the device sent).
    pub raw: Vec<u8>,
}

impl Status {
    /// `02 12 00 00`.
    pub fn is_clean_idle(&self) -> bool {
        self.raw == CLEAN_IDLE
    }
    /// Vendor pre-start gate: byte1 has no 0xC0 bits and has 0x02.
    pub fn pre_start_ready(&self) -> bool {
        self.raw.len() == 4 && self.raw[1] & 0xC0 == 0 && self.raw[1] & 0x02 != 0
    }
    /// A lone `02`: a partial native tail (postflight gate, ED:2130-2179).
    pub fn is_partial_native_tail(&self) -> bool {
        self.raw == [0x02]
    }
    /// Hex text.
    pub fn hex(&self) -> String {
        hex(&self.raw)
    }
}

fn rol8(v: u8, n: u32) -> u8 {
    v.rotate_left(n)
}
fn ror8(v: u8, n: u32) -> u8 {
    v.rotate_right(n)
}

/// ESC # payload transform:
/// `out[i] = rol8(ror8(src[i] ^ KEY[i % 5], 3) ^ KEY[(i & 7) + 8], 3)`.
pub fn handshake_transform(src: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, (o, s)) in out.iter_mut().zip(src.iter()).enumerate() {
        *o = rol8(ror8(s ^ HANDSHAKE_KEY[i % 5], 3) ^ HANDSHAKE_KEY[(i & 7) + 8], 3);
    }
    out
}

/// ESC # payload from a 64-byte FS S reply (uses bytes 0..32).
pub fn handshake_payload_from_source(fs_s: &[u8]) -> Result<[u8; 32], EpsonError> {
    if fs_s.len() != 64 {
        return Err(EpsonError::Protocol(format!("FS S reply has {} bytes, expected 64", fs_s.len())));
    }
    let mut src = [0u8; 32];
    src.copy_from_slice(&fs_s[..32]);
    Ok(handshake_transform(&src))
}

/// ESC z LUT channel selectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LutChannel {
    /// `'m'` master (IR pass).
    Master,
    /// `'r'`.
    Red,
    /// `'g'`.
    Green,
    /// `'b'`.
    Blue,
}

impl LutChannel {
    /// Selector byte.
    pub fn selector(self) -> u8 {
        match self {
            LutChannel::Master => 0x6D,
            LutChannel::Red => 0x72,
            LutChannel::Green => 0x67,
            LutChannel::Blue => 0x62,
        }
    }
}

/// ESC z payload: selector + 256 entries `clamp(0,255,trunc(i*slope+offset))`.
pub fn lut_payload(channel: LutChannel, slope: f64, offset: f64) -> [u8; 257] {
    let mut p = [0u8; 257];
    p[0] = channel.selector();
    for (i, e) in p[1..].iter_mut().enumerate() {
        let v = (i as f64 * slope + offset).trunc();
        *e = if v.is_nan() || v <= 0.0 {
            0
        } else if v >= 255.0 {
            255
        } else {
            v as u8
        };
    }
    p
}

/// The production identity LUT (slope 1, offset 0).
pub fn identity_lut(channel: LutChannel) -> [u8; 257] {
    lut_payload(channel, 1.0, 0.0)
}

/// Fields decoded from an `ESC I` inquiry (header + payload).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inquiry {
    /// Leading level text, e.g. "B8".
    pub level: String,
    /// `'R' u16le` resolutions.
    pub resolutions: Vec<u16>,
    /// `'A' u16le w, u16le h` areas.
    pub areas: Vec<(u16, u16)>,
}

/// Parse an `ESC I` reply (with or without its 4-byte `02 ..` header). Never panics.
pub fn parse_inquiry(reply: &[u8]) -> Inquiry {
    let payload = if reply.first() == Some(&0x02) && reply.len() >= 4 { &reply[4..] } else { reply };
    let level = String::from_utf8_lossy(payload.get(..2).unwrap_or(&[])).into_owned();
    let mut resolutions = Vec::new();
    let mut i = 0;
    while i + 2 < payload.len() {
        if payload[i] == b'R' {
            resolutions.push(u16::from_le_bytes([payload[i + 1], payload[i + 2]]));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut areas = Vec::new();
    let mut i = 0;
    while i + 4 < payload.len() {
        if payload[i] == b'A' {
            areas.push((u16::from_le_bytes([payload[i + 1], payload[i + 2]]), u16::from_le_bytes([payload[i + 3], payload[i + 4]])));
            i += 5;
        } else {
            i += 1;
        }
    }
    Inquiry { level, resolutions, areas }
}

/// Fields decoded from an `FS I` extended identity reply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtendedIdentity {
    /// Starts with "B8" and has ≥ 68 bytes.
    pub valid: bool,
    /// Model text at 46..62 (e.g. "GT-X980").
    pub model: String,
    /// Firmware text at 62..66 (e.g. "1.11").
    pub firmware: String,
    /// The u32le word at `offset` (4..44 step 4), if present.
    pub words: Vec<(usize, u32)>,
}

fn ascii_field(b: &[u8], range: std::ops::Range<usize>) -> String {
    b.get(range).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

/// Parse an `FS I` reply. Never panics.
pub fn parse_extended_identity(b: &[u8]) -> ExtendedIdentity {
    let mut words = Vec::new();
    if b.len() >= 46 {
        for off in (4..44).step_by(4) {
            if let Some(w) = u32_at(b, off) {
                words.push((off, w));
            }
        }
    }
    ExtendedIdentity { valid: b.len() >= 68 && b.starts_with(b"B8"), model: ascii_field(b, 46..62), firmware: ascii_field(b, 62..66), words }
}

/// Model text of an `ESC f` reply (header + payload; text at payload offset 26). Never panics.
pub fn extended_status_model(reply: &[u8]) -> String {
    let payload = reply.get(4..).unwrap_or(&[]);
    payload.get(26..).map(|s| String::from_utf8_lossy(s).trim_matches(|c: char| c == ' ' || c == '\0').to_string()).unwrap_or_default()
}

/// Lowercase space-separated hex.
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|t| u8::from_str_radix(t, 16).unwrap()).collect()
    }

    #[test]
    fn prescan_param_block_matches_spec_golden() {
        let r = Region::new(0.0, 0.0, 5.9, 9.7).unwrap();
        let dpi = Dpi::new(200).unwrap();
        let a = area_pixels(&r, dpi);
        assert_eq!(a, DeviceArea { x: 0, y: 0, width: 1176, height: 1940 });
        let p = param_block(&a, dpi, PassKind::Visible { high_speed: true });
        let mut expected =
            unhex("c8 00 00 00 c8 00 00 00 00 00 00 00 00 00 00 00 98 04 00 00 94 07 00 00 12 10 01 01 60 03 00 00 01 80 00 00 00 00 00");
        expected.resize(64, 0);
        assert_eq!(p.to_vec(), expected);
    }

    #[test]
    fn ir_param_block_shape() {
        let a = DeviceArea { x: 300, y: 300, width: 120, height: 120 };
        let p = param_block(&a, Dpi::new(300).unwrap(), PassKind::Infrared);
        assert_eq!((p[24], p[25], p[26], p[27], p[28], p[29], p[32], p[33], p[38]), (0x13, 0x10, 0x03, 0, 0x20, 0x03, 0x01, 0x80, 0x02));
        assert!(p[39..].iter().all(|&b| b == 0) && p[30] == 0 && p[31] == 0 && p[34..38].iter().all(|&b| b == 0));
        let parsed = parse_param_block(&p).unwrap();
        assert_eq!(parsed.area, a);
        assert_eq!(parsed.dpi, 300);
        assert_eq!(parse_param_block(&p[..63]), None);
    }

    #[test]
    fn area_pixels_rounds_half_even_and_floors_width_to_8() {
        // 0.16 x 0.06 in @ 50 dpi -> 8 x 3 (golden fixture geometry).
        let a = area_pixels(&Region::new(0.0, 0.0, 0.16, 0.06).unwrap(), Dpi::new(50).unwrap());
        assert_eq!((a.width, a.height), (8, 3));
        // 0.4 in @ 300 -> 120 x 120 (sim fixture).
        let a = area_pixels(&Region::new(1.0, 1.0, 0.4, 0.4).unwrap(), Dpi::new(300).unwrap());
        assert_eq!(a, DeviceArea { x: 300, y: 300, width: 120, height: 120 });
        // round-half-even: 2.5 -> 2, 3.5 -> 4.
        let a = area_pixels(&Region::new(0.025, 0.035, 0.1, 0.005).unwrap(), Dpi::new(100).unwrap());
        assert_eq!((a.x, a.y), (2, 4));
        // Tiny width -> 8 minimum; height min 1.
        let a = area_pixels(&Region::new(0.0, 0.0, 0.001, 0.001).unwrap(), Dpi::new(100).unwrap());
        assert_eq!((a.width, a.height), (8, 1));
        // 1180 -> 1176 (floor to 8).
        let a = area_pixels(&Region::new(0.0, 0.0, 5.9, 1.0).unwrap(), Dpi::new(200).unwrap());
        assert_eq!(a.width, 1176);
    }

    #[test]
    fn quantised_area_never_exceeds_the_platen() {
        // Ends exactly at the film-area corner: rounding the origin up plus the
        // 8 px minimum width would describe 8.04 in at 200 dpi without the clamp.
        let a = area_pixels(&Region::new(7.999, 9.999, 0.001, 0.001).unwrap(), Dpi::new(200).unwrap());
        assert!(a.fits_platen(Dpi::new(200).unwrap()), "{a:?}");
        assert_eq!((a.x + a.width, a.y + a.height), (1600, 2000));
        // The full platen at the extremes of the dpi range.
        for dpi in [1, 50, 200, 6400] {
            let d = Dpi::new(dpi).unwrap();
            let a = area_pixels(&Region::new(0.0, 0.0, 8.0, 10.0).unwrap(), d);
            assert!(a.fits_platen(d), "dpi {dpi}: {a:?}");
            let a = area_pixels(&Region::new(7.5, 9.5, 0.5, 0.5).unwrap(), d);
            assert!(a.fits_platen(d), "dpi {dpi}: {a:?}");
        }
        // A hand-built out-of-platen area is clamped on the wire as well.
        let bad = DeviceArea { x: 1590, y: 1990, width: 800, height: 800 };
        let p = param_block(&bad, Dpi::new(200).unwrap(), PassKind::Visible { high_speed: false });
        let wire = parse_param_block(&p).unwrap().area;
        assert!(wire.fits_platen(Dpi::new(200).unwrap()), "{wire:?}");
        assert_eq!((wire.x + wire.width, wire.y + wire.height), (1598, 2000));
    }

    #[test]
    fn region_guard() {
        assert!(Region::new(0.0, 0.0, 8.0, 10.0).is_ok());
        assert!(Region::new(0.0, 0.0, 8.5, 11.6).is_err());
        assert!(Region::new(0.5, 0.0, 7.6, 1.0).is_err());
        assert!(Region::new(0.0, 9.0, 1.0, 1.1).is_err());
        assert!(Region::new(-0.1, 0.0, 1.0, 1.0).is_err());
        assert!(Region::new(0.0, 0.0, 0.0, 1.0).is_err());
        assert!(Region::new(f64::NAN, 0.0, 1.0, 1.0).is_err());
        assert!(Region::new(0.0, 0.0, f64::INFINITY, 1.0).is_err());
        assert!(Dpi::new(0).is_err() && Dpi::new(6401).is_err() && Dpi::new(6400).is_ok());
    }

    #[test]
    fn start_response_fields_and_plan() {
        let busy = StartResponse::parse(&unhex("02 92 00 00 00 00 00 00 00 00 00 00 00 00")).unwrap();
        assert!(busy.busy());
        let ready = StartResponse::parse(&unhex("02 12 04 00 00 00 02 00 00 00 02 00 00 00")).unwrap();
        assert!(!ready.busy());
        let plan = ready.plan().unwrap();
        assert_eq!((plan.unit, plan.full, plan.remainder), (4, 2, 2));
        assert_eq!(plan.chunk_count(), 3);
        assert_eq!((plan.chunk_len(0), plan.chunk_len(2)), (4, 2));
        assert_eq!(plan.total_bytes(), 10);
        let only_rem = StartResponse::parse(&unhex("02 12 04 00 00 00 00 00 00 00 04 00 00 00")).unwrap().plan().unwrap();
        assert_eq!(only_rem.chunk_count(), 1);
        let none = StartResponse::parse(&unhex("02 12 04 00 00 00 00 00 00 00 00 00 00 00")).unwrap().plan().unwrap();
        assert_eq!((none.chunk_count(), none.chunk_len(0)), (1, 4));
        assert!(busy.plan().is_err()); // unit 0
        assert!(StartResponse::parse(&[0x02, 0x12]).is_err());
    }

    #[test]
    fn handshake_transform_matches_live_vectors() {
        let zero = handshake_transform(&[0u8; 32]);
        assert_eq!(hex(&zero), "ca fb 77 71 20 16 da 09 5f 57 09 12 04 83 76 77 3c 73 9c be 7a e0 52 e2 90 0d ff 9a ef 4c 2c 81");
        let mut ramp = [0u8; 32];
        for (i, b) in ramp.iter_mut().enumerate() {
            *b = i as u8;
        }
        assert_eq!(
            hex(&handshake_transform(&ramp)),
            "ca fa 75 72 24 13 dc 0e 57 5e 03 19 08 8e 78 78 2c 62 8e ad 6e f5 44 f5 88 14 e5 81 f3 51 32 9e"
        );
        assert_eq!(
            hex(&handshake_transform(&[0xFF; 32])),
            "35 04 88 8e df e9 25 f6 a0 a8 f6 ed fb 7c 89 88 c3 8c 63 41 85 1f ad 1d 6f f2 00 65 10 b3 d3 7e"
        );
        let src: Vec<u8> = (0..64).collect();
        assert_eq!(handshake_payload_from_source(&src).unwrap(), handshake_transform(&ramp));
        assert!(handshake_payload_from_source(&src[..32]).is_err());
    }

    #[test]
    fn lut_payloads() {
        let p = identity_lut(LutChannel::Red);
        assert_eq!(p[0], 0x72);
        assert!(p[1..].iter().enumerate().all(|(i, &v)| v as usize == i));
        let p = lut_payload(LutChannel::Master, 2.0, 0.0);
        assert_eq!((p[0], p[1], p[11], p[201], p[256]), (0x6D, 0, 20, 255, 255));
        assert_eq!([LutChannel::Green.selector(), LutChannel::Blue.selector()], [0x67, 0x62]);
    }

    // Live V800 packets (captured vectors).
    const LIVE_EXTENDED_STATUS: &str =
        "02 12 2a 00 05 00 00 00 00 00 80 c0 49 40 79 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 47 54 2d 58 39 38 30 20 20 20 20 20 20 20 20 20";
    const LIVE_IDENTITY: &str = "02 12 6a 00 42 38 52 32 00 52 3c 00 52 48 00 52 4b 00 52 50 00 52 5a 00 52 64 00 52 78 00 52 85 00 52 90 00 52 96 00 52 a0 00 52 af 00 52 b4 00 52 c8 00 52 d8 00 52 f0 00 52 0a 01 52 2c 01 52 40 01 52 5e 01 52 68 01 52 90 01 52 e0 01 52 58 02 52 d0 02 52 20 03 52 84 03 52 b0 04 52 40 06 52 08 07 52 60 09 52 80 0c 41 40 6a 40 92";
    const LIVE_EXTENDED_IDENTITY: &str = "42 38 00 00 c0 12 00 00 19 00 00 00 00 32 00 00 60 9f 00 00 60 9f 00 00 60 db 00 00 00 00 00 00 00 00 00 00 a0 6e 00 00 e0 b5 00 00 87 10 47 54 2d 58 39 38 30 20 20 20 20 20 20 20 20 20 31 2e 31 31 10 10";

    #[test]
    fn live_identity_packets_parse() {
        let inq = parse_inquiry(&unhex(LIVE_IDENTITY));
        assert_eq!(inq.level, "B8");
        assert_eq!(inq.resolutions.first(), Some(&50));
        assert_eq!(inq.resolutions.last(), Some(&3200));
        assert_eq!(inq.areas, vec![(27200, 37440)]);
        let ext = parse_extended_identity(&unhex(LIVE_EXTENDED_IDENTITY));
        assert!(ext.valid);
        assert_eq!(ext.model, "GT-X980");
        assert_eq!(ext.firmware, "1.11");
        assert!(ext.words.contains(&(4, 4800)));
        assert!(ext.words.contains(&(16, 40800)));
        assert!(ext.words.contains(&(24, 56160)));
        assert_eq!(extended_status_model(&unhex(LIVE_EXTENDED_STATUS)), "GT-X980");
    }

    #[test]
    fn status_classification() {
        assert!(Status { raw: CLEAN_IDLE.to_vec() }.is_clean_idle());
        assert!(Status { raw: CLEAN_IDLE.to_vec() }.pre_start_ready());
        assert!(!Status { raw: vec![0x02, 0x92, 0x01, 0x00] }.pre_start_ready());
        assert!(!Status { raw: vec![0x02, 0x12, 0x01, 0x00] }.is_clean_idle());
        assert!(Status { raw: vec![0x02] }.is_partial_native_tail());
    }

    proptest::proptest! {
        #[test]
        fn parsers_never_panic(b in proptest::collection::vec(proptest::num::u8::ANY, 0..200)) {
            let _ = StartResponse::parse(&b).and_then(|s| s.plan());
            let _ = parse_param_block(&b);
            let _ = parse_inquiry(&b);
            let _ = parse_extended_identity(&b);
            let _ = extended_status_model(&b);
            let _ = handshake_payload_from_source(&b);
            let s = Status { raw: b.clone() };
            let _ = (s.is_clean_idle(), s.pre_start_ready(), s.is_partial_native_tail());
        }

        #[test]
        fn geometry_never_panics(l in 0.0f64..8.0, t in 0.0f64..10.0, w in 0.0001f64..8.0, h in 0.0001f64..10.0, d in 1u32..=6400) {
            if let Ok(r) = Region::new(l, t, w, h) {
                let a = area_pixels(&r, Dpi::new(d).unwrap());
                proptest::prop_assert!(a.width.is_multiple_of(8) && a.width >= 8 && a.height >= 1);
                let _ = param_block(&a, Dpi::new(d).unwrap(), PassKind::Infrared);
            }
        }
    }
}
