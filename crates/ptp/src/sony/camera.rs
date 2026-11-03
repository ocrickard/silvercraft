//! The Sony camera driver: SDIO handshake, properties, shutter, capture
//! (card and save-to-PC buffer paths) and live view.

use std::collections::BTreeSet;
use std::io::Write;
use std::time::Duration;

use silvercraft_usb::Transport;

use super::liveview::{JpegSource, locate_jpeg};
use super::props::{ExtDeviceInfo, PropSnapshot, PropValue, encode_value, parse_all_props};
use crate::clock::Clock;
use crate::codec::{event, rc};
use crate::datasets::{DeviceInfo, ObjectInfo};
use crate::session::Session;
use crate::{Event, PtpError};

/// Sony SDIO operation codes.
pub mod sdio {
    /// SDIO_Connect (data-IN).
    pub const CONNECT: u16 = 0x9201;
    /// SDIO_GetExtDeviceInfo (data-IN; param = protocol version).
    pub const GET_EXT_DEVICE_INFO: u16 = 0x9202;
    /// SDIO_SetExtDeviceProp (data-OUT).
    pub const SET_EXT_DEVICE_PROP: u16 = 0x9205;
    /// SDIO_ControlDevice (data-OUT u16 button state).
    pub const CONTROL_DEVICE: u16 = 0x9207;
    /// SDIO_GetAllExtDevicePropInfo (data-IN).
    pub const GET_ALL_EXT_DEVICE_PROP_INFO: u16 = 0x9209;
    /// Protocol version 200, validated on the ILCE-1 (HW).
    pub const PROTOCOL_200: u16 = 0x00C8;
    /// Protocol version 300 (newer bodies) (EXT, unverified).
    pub const PROTOCOL_300: u16 = 0x012C;
    /// S1 (half press) control code.
    pub const CTRL_S1: u16 = 0xD2C1;
    /// S2 (full press) control code.
    pub const CTRL_S2: u16 = 0xD2C2;
    /// Button down.
    pub const PRESS: u16 = 2;
    /// Button up.
    pub const RELEASE: u16 = 1;
}

/// Pseudo-handle of the captured object in camera RAM.
pub const CAPTURED_OBJECT: u32 = 0xFFFF_C001;
/// Pseudo-handle of the live-view frame.
pub const LIVEVIEW_OBJECT: u32 = 0xFFFF_C002;
/// ObjectInMemory threshold: at or above, an object waits at [`CAPTURED_OBJECT`].
pub const OBJECT_READY: u16 = 0x8000;
/// Smallest payload accepted as a real capture (1024).
pub const MIN_OBJECT_BYTES: u64 = 1024;
/// Cap on a live-view payload held in memory.
pub const MAX_LIVEVIEW_BYTES: usize = 32 * 1024 * 1024;

/// Shutter button timings (ILCE-1 values).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShutterTimings {
    /// Hold S1 before pressing S2. Releasing too soon means the a1 never exposes (HW).
    pub af_settle: Duration,
    /// Hold S2 before releasing.
    pub s2_hold: Duration,
}

impl Default for ShutterTimings {
    fn default() -> Self {
        Self { af_settle: Duration::from_millis(1000), s2_hold: Duration::from_millis(800) }
    }
}

/// Driver options. Defaults are the original.
#[derive(Debug, Clone, PartialEq)]
pub struct SonyOptions {
    /// SDIO protocol version sent to 0x9202.
    pub protocol_version: u16,
    /// Shutter timings.
    pub timings: ShutterTimings,
    /// Overall capture deadline (20 s).
    pub capture_timeout: Duration,
    /// Card path: handle poll interval (0.25 s).
    pub card_poll: Duration,
    /// Object download retry interval (0.3 s).
    pub object_poll: Duration,
    /// Buffer path: ObjectInMemory poll interval (0.2 s).
    pub status_poll: Duration,
    /// Stale RAM objects drained before firing (16).
    pub max_ram_drain: u32,
    /// Live view: keep retrying for this long (2 s).
    pub liveview_retry: Duration,
    /// Live view: retry interval (50 ms).
    pub liveview_interval: Duration,
    /// Use the interrupt endpoint to wake polling loops early. Polling stays
    /// the source of truth.
    pub use_events: bool,
    /// Retry the handshake once (after 500 ms) when GetExtDeviceInfo does not
    /// answer OK with a parseable payload. An addition.
    pub handshake_retry: bool,
}

impl Default for SonyOptions {
    fn default() -> Self {
        Self {
            protocol_version: sdio::PROTOCOL_200,
            timings: ShutterTimings::default(),
            capture_timeout: Duration::from_secs(20),
            card_poll: Duration::from_millis(250),
            object_poll: Duration::from_millis(300),
            status_poll: Duration::from_millis(200),
            max_ram_drain: 16,
            liveview_retry: Duration::from_secs(2),
            liveview_interval: Duration::from_millis(50),
            use_events: true,
            handshake_retry: true,
        }
    }
}

/// What the handshake saw (for bring-up logs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandshakeReport {
    /// Response codes of Connect(1), Connect(2), GetExtDeviceInfo, Connect(3).
    pub response_codes: [u16; 4],
    /// Parsed 0x9202 payload, when it parsed.
    pub ext_info: Option<ExtDeviceInfo>,
    /// Raw 0x9202 payload.
    pub ext_info_raw: Vec<u8>,
    /// Whether the retry was used.
    pub retried: bool,
}

/// Which capture path ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePath {
    /// The shot was saved to a card and fetched by its handle.
    Card,
    /// Save-to-PC: fetched from camera RAM at 0xFFFFC001.
    Buffer,
}

/// What kind of file the download is, sniffed from its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// TIFF-based RAW (ARW): starts with `II` or `MM`.
    Raw,
    /// JPEG (a postview: set the camera's PC save format to RAW).
    Jpeg,
    /// Anything else.
    Other,
}

impl ObjectKind {
    /// Sniff a capture's kind from its first bytes (raw TIFF header or JPEG).
    pub fn sniff(head: &[u8]) -> Self {
        if head.starts_with(&[0xFF, 0xD8]) {
            ObjectKind::Jpeg
        } else if head.starts_with(b"II") || head.starts_with(b"MM") {
            ObjectKind::Raw
        } else {
            ObjectKind::Other
        }
    }
}

/// A finished capture; the bytes went to the caller's sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    /// Bytes written to the sink.
    pub bytes: u64,
    /// Sniffed kind.
    pub kind: ObjectKind,
    /// Path used.
    pub path: CapturePath,
    /// Handle downloaded.
    pub handle: u32,
    /// ObjectInfo, when the camera provided one (filename = original name).
    pub info: Option<ObjectInfo>,
    /// User-facing warning (e.g. JPEG instead of RAW).
    pub warning: Option<String>,
}

/// One live-view frame (still JPEG-encoded; decoding is the caller's job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveViewFrame {
    /// The JPEG bytes.
    pub jpeg: Vec<u8>,
    /// Per-connection frame counter, starting at 1.
    pub sequence: u64,
    /// Clock time when it arrived.
    pub timestamp: Duration,
    /// How the JPEG was located.
    pub source: JpegSource,
    /// Size of the whole payload.
    pub payload_len: usize,
}

/// Writer adapter that counts bytes and keeps the first few for sniffing.
struct Tap<'a> {
    inner: &'a mut dyn Write,
    count: u64,
    head: Vec<u8>,
}

impl<'a> Tap<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner, count: 0, head: Vec::new() }
    }
}

impl Write for Tap<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        if self.head.len() < 16 {
            let take = (16 - self.head.len()).min(n);
            self.head.extend_from_slice(&buf[..take]);
        }
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A connected Sony camera.
pub struct SonyCamera<T: Transport, C: Clock> {
    session: Session<T>,
    clock: C,
    opts: SonyOptions,
    ext_info: Option<ExtDeviceInfo>,
    live_sequence: u64,
    events_ok: bool,
    object_event_seen: bool,
}

impl<T: Transport, C: Clock> std::fmt::Debug for SonyCamera<T, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SonyCamera").field("opts", &self.opts).field("ext_info", &self.ext_info).field("live_sequence", &self.live_sequence).finish()
    }
}

impl<T: Transport, C: Clock> SonyCamera<T, C> {
    /// Wrap an open session. Does no I/O; call [`SonyCamera::handshake`].
    pub fn new(session: Session<T>, clock: C, opts: SonyOptions) -> Self {
        Self { session, clock, opts, ext_info: None, live_sequence: 0, events_ok: true, object_event_seen: false }
    }

    /// Wrap and handshake.
    pub fn connect(session: Session<T>, clock: C, opts: SonyOptions) -> Result<(Self, HandshakeReport), PtpError> {
        let mut cam = Self::new(session, clock, opts);
        let report = cam.handshake()?;
        Ok((cam, report))
    }

    /// The session.
    pub fn session(&mut self) -> &mut Session<T> {
        &mut self.session
    }

    /// The clock.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Options.
    pub fn options(&self) -> &SonyOptions {
        &self.opts
    }

    /// The 0x9202 payload from the last handshake, when it parsed.
    pub fn ext_info(&self) -> Option<&ExtDeviceInfo> {
        self.ext_info.as_ref()
    }

    /// Close the session (CloseSession). Dropping the camera does the same.
    pub fn close(self) -> Result<(), PtpError> {
        self.session.close()
    }

    fn handshake_once(&mut self) -> Result<HandshakeReport, PtpError> {
        let mut report = HandshakeReport::default();
        let (r, _) = self.session.transaction_vec(sdio::CONNECT, &[1, 0, 0], None)?;
        report.response_codes[0] = r.code;
        let (r, _) = self.session.transaction_vec(sdio::CONNECT, &[2, 0, 0], None)?;
        report.response_codes[1] = r.code;
        let (r, data) = self.session.transaction_vec(sdio::GET_EXT_DEVICE_INFO, &[u32::from(self.opts.protocol_version)], None)?;
        report.response_codes[2] = r.code;
        report.ext_info = if r.is_ok() { ExtDeviceInfo::parse(&data).ok() } else { None };
        report.ext_info_raw = data;
        let (r, _) = self.session.transaction_vec(sdio::CONNECT, &[3, 0, 0], None)?;
        report.response_codes[3] = r.code;
        Ok(report)
    }

    /// The Sony SDIO handshake:
    /// Connect(1,0,0), Connect(2,0,0), GetExtDeviceInfo(version), Connect(3,0,0),
    /// all data-IN. Response codes are not checked (0xA101 is normal on Connect).
    pub fn handshake(&mut self) -> Result<HandshakeReport, PtpError> {
        let mut report = self.handshake_once()?;
        if self.opts.handshake_retry && report.ext_info.is_none() {
            log::warn!("SDIO handshake: GetExtDeviceInfo answered 0x{:04x} without a parseable payload; retrying once", report.response_codes[2]);
            self.clock.sleep(Duration::from_millis(500));
            report = self.handshake_once()?;
            report.retried = true;
        }
        log::info!(
            "SDIO handshake codes {:04x?}, ext info {:?}",
            report.response_codes,
            report.ext_info.as_ref().map(|e| (e.version, e.props.len(), e.controls.len()))
        );
        self.ext_info = report.ext_info.clone();
        Ok(report)
    }

    /// GetDeviceInfo.
    pub fn device_info(&mut self) -> Result<DeviceInfo, PtpError> {
        self.session.device_info()
    }

    /// Raw SDIO_GetAllExtDevicePropInfo blob (for bring-up fixtures).
    pub fn all_props_raw(&mut self) -> Result<Vec<u8>, PtpError> {
        let (_r, data) = self.session.transaction_vec(sdio::GET_ALL_EXT_DEVICE_PROP_INFO, &[u32::from(self.opts.protocol_version)], None)?;
        Ok(data)
    }

    /// All extended properties (structured parse with scan fallback).
    pub fn all_props(&mut self) -> Result<PropSnapshot, PtpError> {
        Ok(parse_all_props(&self.all_props_raw()?))
    }

    /// ObjectInMemory (0xD215). Unreadable values read as 0, as before;
    /// only fatal (device gone) errors propagate.
    pub fn object_in_memory(&mut self) -> Result<u16, PtpError> {
        match self.all_props() {
            Ok(s) => Ok(s.object_in_memory.unwrap_or(0)),
            Err(e) if e.is_fatal() => Err(e),
            Err(e) => {
                log::debug!("ObjectInMemory unreadable: {e}");
                Ok(0)
            }
        }
    }

    /// SDIO_ControlDevice(code) with a u16 data phase. Returns the response code.
    pub fn control(&mut self, code: u16, value: u16) -> Result<u16, PtpError> {
        let o = self.session.transaction(sdio::CONTROL_DEVICE, &[u32::from(code)], Some(&value.to_le_bytes()), &mut std::io::sink())?;
        Ok(o.response.code)
    }

    /// SDIO_SetExtDeviceProp(code) with `value` encoded as `datatype`
    /// (EXT, unverified: no validated reference sets properties).
    pub fn set_ext_prop(&mut self, code: u16, datatype: u16, value: &PropValue) -> Result<(), PtpError> {
        let data = encode_value(datatype, value)?;
        let o = self.session.transaction(sdio::SET_EXT_DEVICE_PROP, &[u32::from(code)], Some(&data), &mut std::io::sink())?;
        if o.response.is_ok() { Ok(()) } else { Err(PtpError::Response { op: sdio::SET_EXT_DEVICE_PROP, code: o.response.code }) }
    }

    /// S1 press, settle, S2 press, hold, S2 release, S1 release.
    /// Response codes are not checked. Releases are attempted even when a
    /// press fails, so a button is never left held down.
    pub fn trigger_shutter(&mut self) -> Result<(), PtpError> {
        let t = self.opts.timings;
        let pressed = (|| {
            self.control(sdio::CTRL_S1, sdio::PRESS)?;
            self.clock.sleep(t.af_settle);
            self.control(sdio::CTRL_S2, sdio::PRESS)?;
            self.clock.sleep(t.s2_hold);
            Ok::<(), PtpError>(())
        })();
        let r2 = self.control(sdio::CTRL_S2, sdio::RELEASE);
        let r1 = self.control(sdio::CTRL_S1, sdio::RELEASE);
        pressed?;
        r2?;
        r1?;
        Ok(())
    }

    /// Wait up to `d`, returning early when an object event arrives on the
    /// interrupt endpoint. Interrupt failures disable events for the session.
    fn wait(&mut self, d: Duration) -> Result<(), PtpError> {
        let start = self.clock.now();
        if self.opts.use_events && self.events_ok {
            match self.session.poll_event(d) {
                Ok(Some(ev)) => {
                    if self.note_event(&ev) {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => {
                    log::warn!("interrupt endpoint failed ({e}); falling back to polling only");
                    self.events_ok = false;
                }
            }
        }
        let spent = self.clock.now().saturating_sub(start);
        if spent < d {
            self.clock.sleep(d - spent);
        }
        Ok(())
    }

    /// Record an event; true when it signals a new object.
    fn note_event(&mut self, ev: &Event) -> bool {
        log::debug!("event 0x{:04x} params {:x?}", ev.code, ev.params);
        let object = matches!(ev.code, event::SONY_OBJECT_ADDED | event::OBJECT_ADDED | event::CAPTURE_COMPLETE);
        self.object_event_seen |= object;
        object
    }

    /// Read one event (interrupt endpoint), if any arrives within `timeout`.
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, PtpError> {
        let ev = self.session.poll_event(timeout)?;
        if let Some(e) = &ev {
            self.note_event(e);
        }
        Ok(ev)
    }

    fn all_handles(&mut self, storages: &[u32]) -> Result<BTreeSet<u32>, PtpError> {
        let mut out = BTreeSet::new();
        for &s in storages {
            match self.session.object_handles(s) {
                Ok(h) => out.extend(h),
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => log::debug!("GetObjectHandles(0x{s:08x}) failed: {e}"),
            }
        }
        Ok(out)
    }

    /// GetObject streamed into `sink`. `Ok(None)` when the camera sent no
    /// data (not ready yet; safe to retry). A data phase that then fails or
    /// is too small is [`PtpError::PartialDownload`]: the sink is dirty.
    fn download(&mut self, handle: u32, sink: &mut dyn Write) -> Result<Option<(u64, ObjectKind)>, PtpError> {
        let mut tap = Tap::new(sink);
        let r = self.session.get_object(handle, &mut tap);
        let (count, kind) = (tap.count, ObjectKind::sniff(&tap.head));
        match r {
            Ok(o) if o.response.is_ok() && count >= MIN_OBJECT_BYTES => Ok(Some((count, kind))),
            Ok(_) if count == 0 => Ok(None),
            Ok(o) => Err(PtpError::PartialDownload { bytes: count, code: o.response.code }),
            Err(e) if count == 0 => Err(e),
            Err(e) => {
                log::warn!("GetObject(0x{handle:08x}) failed after {count} bytes: {e}");
                Err(PtpError::PartialDownload { bytes: count, code: 0 })
            }
        }
    }

    /// Fetch the object at 0xFFFFC001: GetObjectInfo primes it, GetObject
    /// consumes it. `Ok(None)` when not fetchable yet.
    fn drain_one(&mut self, sink: &mut dyn Write) -> Result<Option<(u64, ObjectKind, Option<ObjectInfo>)>, PtpError> {
        let (r, info) = match self.session.object_info(CAPTURED_OBJECT) {
            Ok(v) => v,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                log::debug!("GetObjectInfo(0xFFFFC001) failed: {e}");
                return Ok(None);
            }
        };
        let size = info.as_ref().map_or(0, |i| i.compressed_size);
        if !r.is_ok() || u64::from(size) <= MIN_OBJECT_BYTES {
            return Ok(None);
        }
        Ok(self.download(CAPTURED_OBJECT, sink)?.map(|(n, k)| (n, k, info)))
    }

    fn deadline_passed(&self, deadline: Duration) -> bool {
        self.clock.now() >= deadline
    }

    /// Shoot one frame and stream the file into `sink`.
    ///
    /// With a card in the camera (GetStorageIDs lists a usable storage) the
    /// card path runs; otherwise the save-to-PC buffer path. On
    /// [`PtpError::PartialDownload`] the sink holds a partial file and must
    /// be discarded; on any other error nothing was written to it.
    pub fn capture(&mut self, sink: &mut dyn Write) -> Result<CaptureResult, PtpError> {
        let storages = match self.session.storage_ids() {
            Ok(s) => s,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                log::debug!("GetStorageIDs failed ({e}); using the buffer path");
                Vec::new()
            }
        };
        let mut result = if storages.is_empty() { self.capture_buffer(sink)? } else { self.capture_card(&storages, sink)? };
        if result.kind == ObjectKind::Jpeg {
            result.warning = Some(
                "camera returned a JPEG, not RAW: set the camera's PC Remote save format to RAW \
                 (Menu > Network > Transfer/Remote > PC Remote Function > RAW+J PC Save Img > RAW)"
                    .into(),
            );
        }
        Ok(result)
    }

    fn capture_card(&mut self, storages: &[u32], sink: &mut dyn Write) -> Result<CaptureResult, PtpError> {
        let before = self.all_handles(storages)?;
        self.object_event_seen = false;
        self.trigger_shutter()?;
        let deadline = self.clock.now() + self.opts.capture_timeout;

        let added = loop {
            let now = self.all_handles(storages)?;
            let added: Vec<u32> = now.difference(&before).copied().collect();
            if !added.is_empty() {
                break added;
            }
            if self.deadline_passed(deadline) {
                return Err(PtpError::Camera(
                    "no new image appeared on the card -- did the shutter fire? \
                     (camera in PC Remote, lens on MF or release-priority)"
                        .into(),
                ));
            }
            self.wait(self.opts.card_poll)?;
        };

        // Prefer the RAW of a RAW+JPEG pair; highest handle among candidates.
        let mut infos = Vec::new();
        for &h in &added {
            let info = match self.session.object_info(h) {
                Ok((_, info)) => info,
                Err(e) if e.is_fatal() => return Err(e),
                Err(_) => None,
            };
            infos.push((h, info));
        }
        let pick = infos
            .iter()
            .filter(|(_, i)| i.as_ref().is_some_and(ObjectInfo::is_arw))
            .max_by_key(|(h, _)| *h)
            .or_else(|| infos.iter().max_by_key(|(h, _)| *h))
            .cloned();
        let Some((handle, info)) = pick else {
            return Err(PtpError::Camera("no new image appeared on the card".into()));
        };

        loop {
            if let Some((bytes, kind)) = self.download(handle, sink)? {
                return Ok(CaptureResult { bytes, kind, path: CapturePath::Card, handle, info, warning: None });
            }
            if self.deadline_passed(deadline) {
                return Err(PtpError::Camera(format!("captured object 0x{handle:08x} never became readable")));
            }
            self.clock.sleep(self.opts.object_poll);
        }
    }

    fn capture_buffer(&mut self, sink: &mut dyn Write) -> Result<CaptureResult, PtpError> {
        let deadline = self.clock.now() + self.opts.capture_timeout;

        // 1. Clean RAM so we return the shot we take, not one from history.
        let mut cleaned = 0;
        while cleaned < self.opts.max_ram_drain && self.object_in_memory()? >= OBJECT_READY {
            if self.drain_one(&mut std::io::sink())?.is_none() {
                return Err(PtpError::Camera(
                    "the camera's image RAM is stuck (an object is flagged but won't download). \
                     Unplug and replug the USB cable -- a power cycle does NOT clear it -- then try again."
                        .into(),
                ));
            }
            cleaned += 1;
        }
        if cleaned > 0 {
            log::info!("discarded {cleaned} stale object(s) from camera RAM");
        }

        // 2. Fire.
        self.object_event_seen = false;
        self.trigger_shutter()?;

        // 3. Wait for the fresh object, then fetch it.
        while !self.deadline_passed(deadline) {
            if self.object_in_memory()? >= OBJECT_READY {
                break;
            }
            self.wait(self.opts.status_poll)?;
        }
        while !self.deadline_passed(deadline) {
            if let Some((bytes, kind, info)) = self.drain_one(sink)? {
                return Ok(CaptureResult { bytes, kind, path: CapturePath::Buffer, handle: CAPTURED_OBJECT, info, warning: None });
            }
            self.clock.sleep(self.opts.object_poll);
        }
        Err(PtpError::Camera("no new frame arrived -- did the shutter fire? (camera in PC Remote, lens on MF or release-priority)".into()))
    }

    /// Fetch one live-view frame: GetObject(0xFFFFC002) and locate the JPEG,
    /// retrying every 50 ms for up to 2 s (Sony serves empty or stale objects
    /// while live view starts or resumes). No start command is needed after
    /// the handshake.
    pub fn live_view_frame(&mut self) -> Result<LiveViewFrame, PtpError> {
        let deadline = self.clock.now() + self.opts.liveview_retry;
        loop {
            let err = match self.session.transaction_vec_limited(crate::codec::op::GET_OBJECT, &[LIVEVIEW_OBJECT], None, MAX_LIVEVIEW_BYTES) {
                Ok((_r, payload)) => match locate_jpeg(&payload) {
                    Ok((range, source)) => {
                        let jpeg = payload.get(range).unwrap_or_default().to_vec();
                        self.live_sequence += 1;
                        return Ok(LiveViewFrame {
                            jpeg,
                            sequence: self.live_sequence,
                            timestamp: self.clock.now(),
                            source,
                            payload_len: payload.len(),
                        });
                    }
                    Err(e) => e,
                },
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => e,
            };
            if self.deadline_passed(deadline) {
                return Err(PtpError::Camera(format!("live view did not produce a JPEG: {err}")));
            }
            self.clock.sleep(self.opts.liveview_interval);
        }
    }

    /// Whether the interrupt endpoint is still in use.
    pub fn events_enabled(&self) -> bool {
        self.opts.use_events && self.events_ok
    }

    /// Accessor used by tests to check the code path, and by callers that
    /// want to know whether an object event arrived since the last trigger.
    pub fn object_event_seen(&self) -> bool {
        self.object_event_seen
    }

    /// The raw response code check used by Connect (exposed for callers that
    /// want to warn on unusual handshake codes).
    pub fn is_expected_connect_code(code: u16) -> bool {
        code == rc::OK || code == rc::SONY_CONNECT_A101
    }
}
