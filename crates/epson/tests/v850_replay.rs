//! Replays the USB traffic recorded from a real Epson Perfection V850 during live bring-up
//! (2026-10-09, `examples/v800_bringup.rs`) through the production `Session` code path.
//!
//! Each fixture is one bring-up process run: a fresh process, so carriage calibration started
//! Unknown and a targeted scan auto-ran its own origin prescan first (that prescan is in the
//! log). Each `# --- open handle k ---` segment becomes its own scripted transport, handed out
//! in order by [`ReplayOpener`]; every host write must equal the recorded one
//! (`ScriptedTransport::verify`), every recorded step must be consumed, and every recorded
//! handle must be opened. That pins the driver's wire behaviour to what worked on hardware.
//!
//! Raster payloads were scrubbed to `z:LEN` zero runs (the images are private photos; see
//! `examples/scrub_traffic.rs`). The driver refuses an all-zero visible raster, so
//! before replay each scrubbed transfer is filled with a synthetic ramp whose
//! last byte stays 0x00: a chunk's trailer is always the last byte of a transfer (the device
//! ends each chunk with a short packet), so trailers remain the recorded zeros. Device bytes
//! never influence what the host writes during the raster phase (it ACKs chunk by chunk), so
//! this cannot mask a wire difference.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use silvercraft_epson::{
    Calibration, CancelToken, Clock, Dpi, EpsonError, HandleOpener, Pass, PassMeta, Phase, PrescanOptions, RecoveryClass, Region, ScanRequest,
    Session, SessionConfig, VirtualClock,
};
use silvercraft_usb::{MockEvent, ScriptedTransport, Step, parse_recording};

const IDLE: [u8; 4] = [0x02, 0x12, 0x00, 0x00];
/// Device replies longer than this are raster (the scrubber's threshold).
const RASTER_MIN: usize = 513;
const HANDLE_MARK: &str = "# --- open handle ";

/// Hands out one scripted transport per recorded handle, in order.
struct ReplayOpener {
    queue: VecDeque<ScriptedTransport>,
    clock: Arc<VirtualClock>,
    id: String,
}

impl HandleOpener for ReplayOpener {
    type Handle = ScriptedTransport;
    fn open(&mut self) -> Result<ScriptedTransport, EpsonError> {
        self.queue.pop_front().ok_or_else(|| EpsonError::Protocol("replay: the session opened more handles than were recorded".into()))
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn enumeration_id(&self) -> Option<String> {
        // The calibration registry is process-global: a unique id per replay keeps tests
        // independent and starts each one uncalibrated, as the recorded process did.
        Some(self.id.clone())
    }
}

/// One fixture, split into per-handle scripts.
struct Replay {
    handles: Vec<ScriptedTransport>,
    /// Truncated (prefix-checked) writes in the fixture.
    prefix_writes: usize,
    /// Scrubbed raster transfers.
    raster_transfers: usize,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

impl Replay {
    fn load(text: &str) -> Replay {
        let mut segments: Vec<String> = Vec::new();
        let mut preamble = String::new();
        for line in text.lines() {
            if line.starts_with(HANDLE_MARK) {
                segments.push(String::new());
            }
            let seg = segments.last_mut().unwrap_or(&mut preamble);
            seg.push_str(line);
            seg.push('\n');
        }
        assert!(parse_recording(&preamble).unwrap().is_empty(), "traffic before the first open-handle marker");
        let (mut prefix_writes, mut raster_transfers) = (0, 0);
        let handles = segments
            .iter()
            .enumerate()
            .map(|(k, seg)| {
                let mut steps = parse_recording(seg).unwrap_or_else(|e| panic!("handle {}: {e}", k + 1));
                for step in &mut steps {
                    match step {
                        Step::OutPrefix { .. } => prefix_writes += 1,
                        Step::In(d) if d.len() >= RASTER_MIN => {
                            assert!(d.iter().all(|&b| b == 0), "handle {}: unscrubbed raster in a committed fixture", k + 1);
                            synthesize(d);
                            raster_transfers += 1;
                        }
                        _ => {}
                    }
                }
                ScriptedTransport::new(steps)
            })
            .collect();
        Replay { handles, prefix_writes, raster_transfers }
    }

    fn opener(&self) -> ReplayOpener {
        let id = format!("v850-replay-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        ReplayOpener { queue: self.handles.iter().cloned().collect(), clock: Arc::new(VirtualClock::new()), id }
    }

    /// Every handle was opened, wrote exactly the recorded bytes and consumed its whole script.
    fn verify(&self) {
        for (k, t) in self.handles.iter().enumerate() {
            assert!(!t.events().is_empty(), "recorded handle {} was never used", k + 1);
            t.verify().unwrap_or_else(|e| panic!("handle {} of {}: {e}", k + 1, self.handles.len()));
        }
    }

    /// The last thing the session read on the last handle was clean idle (`02 12 00 00`).
    fn ends_clean_idle(&self) -> bool {
        let last = self.handles.last().map(|t| t.events()).unwrap_or_default();
        let read = last.iter().rev().find_map(|e| match e {
            MockEvent::In { data, .. } => Some(data.clone()),
            _ => None,
        });
        let wrote = last.iter().rev().find_map(|e| match e {
            MockEvent::Out(b) => Some(b.clone()),
            _ => None,
        });
        wrote.as_deref() == Some(&[0x1B, 0x46][..]) && read.as_deref() == Some(&IDLE[..])
    }
}

/// A non-zero ramp in place of scrubbed raster, keeping the final (trailer) byte 0x00.
fn synthesize(d: &mut [u8]) {
    for (i, b) in d.iter_mut().enumerate() {
        *b = (i % 251) as u8 + 1;
    }
    if let Some(last) = d.last_mut() {
        *last = 0;
    }
}

fn phases() -> (impl FnMut(Phase), Arc<std::sync::Mutex<Vec<Phase>>>) {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let l = log.clone();
    (move |p| l.lock().unwrap().push(p), log)
}

fn region_600() -> ScanRequest {
    ScanRequest { region: Region::new(4.80, 3.50, 1.02, 2.68).unwrap(), dpi: Dpi::new(600).unwrap(), auto_calibrate: true }
}

/// What the bring-up run printed for each pass (`*.out`), checked against the replay.
fn assert_pass(p: &PassMeta, pass: Pass, wh: (u32, u32), polls: u32, chunks: usize, start: &[u8]) {
    assert_eq!(p.pass, pass);
    assert_eq!((p.area.width, p.area.height), wh);
    assert_eq!(p.poll_attempts, polls, "{pass:?} FS G polls");
    assert_eq!(p.chunks, chunks, "{pass:?} chunks");
    assert_eq!(p.nonzero_trailers, 0);
    assert_eq!(p.raster_bytes, p.expected_bytes);
    assert_eq!(p.start_response, start);
    assert_eq!(p.pre_start, IDLE);
}

const PRESCAN_START: [u8; 14] = [0x02, 0x12, 0x00, 0x72, 0x03, 0x00, 0x3c, 0x00, 0x00, 0x00, 0x40, 0x27, 0x02, 0x00];
const SCAN600_START: [u8; 14] = [0x02, 0x12, 0x00, 0xc8, 0x01, 0x00, 0x32, 0x00, 0x00, 0x00, 0x00, 0x72, 0x00, 0x00];

#[test]
fn v850_status_replays() {
    let r = Replay::load(include_str!("fixtures/v850-status.log"));
    assert_eq!(r.handles.len(), 1);
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    let st = s.status().unwrap();
    assert!(st.clean_idle);
    assert_eq!(st.status.raw, IDLE);
    r.verify();
}

#[test]
fn v850_recover_replays() {
    let r = Replay::load(include_str!("fixtures/v850-recover.log"));
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    let rep = s.recover().unwrap();
    assert!(rep.clean_idle);
    assert_eq!(rep.class, RecoveryClass::CleanIdle);
    assert_eq!(rep.final_status, IDLE);
    r.verify();
}

#[test]
fn v850_prescan_replays() {
    let r = Replay::load(include_str!("fixtures/v850-prescan.log"));
    // preflight, 3 priming cycles, the scan handle.
    assert_eq!(r.handles.len(), 5);
    assert_eq!(r.prefix_writes, 3, "R, G, B LUTs recorded cut at 256 of 257 bytes");
    assert_eq!(r.raster_transfers, 122);
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    assert_eq!(s.calibration(), Calibration::Unknown);
    let (mut progress, log) = phases();
    let p = s.prescan(&PrescanOptions::default(), &CancelToken::new(), &mut progress).unwrap();
    r.verify();
    assert_eq!((p.image.width(), p.image.height()), (1176, 1940));
    assert_eq!(p.image.data().len(), 1176 * 1940 * 3);
    assert_eq!(p.passes.len(), 1);
    assert_pass(&p.passes[0], Pass::Visible, (1176, 1940), 5, 61, &PRESCAN_START);
    assert_eq!(s.calibration(), Calibration::Calibrated);
    assert!(r.ends_clean_idle());
    let warm = log.lock().unwrap().iter().filter(|p| matches!(p, Phase::WarmingUp { .. })).count();
    assert_eq!(warm, 4, "the live cold start answered FS G busy four times");
}

#[test]
fn v850_scan600_replays() {
    let r = Replay::load(include_str!("fixtures/v850-scan600.log"));
    // auto prescan (5 handles), then preflight, 3 priming cycles, the scan handle.
    assert_eq!(r.handles.len(), 10);
    assert_eq!(r.prefix_writes, 6);
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    assert_eq!(s.calibration(), Calibration::Unknown, "recorded in a fresh process");
    let scan = s.scan_visible(&region_600(), &CancelToken::new(), &mut |_| {}).unwrap();
    r.verify();
    assert_eq!((scan.image.width(), scan.image.height()), (608, 1608));
    assert_eq!(scan.image.data().len(), 608 * 1608 * 3);
    assert_eq!(scan.tiles, 1);
    assert_eq!(scan.passes.len(), 1);
    let p = &scan.passes[0];
    assert_eq!((p.area.x, p.area.y), (2880, 2100));
    assert_pass(p, Pass::Visible, (608, 1608), 1, 51, &SCAN600_START);
    assert!(scan.postflight.safe_to_continue, "{}", scan.postflight.reason);
    assert_eq!(scan.postflight.reason, "postflight-clean-idle");
    assert!(r.ends_clean_idle());
    assert_eq!(s.calibration(), Calibration::Calibrated);
}

#[test]
fn v850_dual600_replays() {
    let r = Replay::load(include_str!("fixtures/v850-dual600.log"));
    // auto prescan (5), preflight, 3 priming, visible + handshake, IR + postflight.
    assert_eq!(r.handles.len(), 11);
    assert_eq!(r.prefix_writes, 7, "3 prescan + 3 visible + 1 IR master LUT");
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    let (mut progress, log) = phases();
    let d = s.scan_dual_ir(&region_600(), &CancelToken::new(), &mut progress).unwrap();
    r.verify();
    assert_eq!((d.rgb.width(), d.rgb.height()), (608, 1608));
    assert_eq!(d.rgb.data().len(), 608 * 1608 * 3);
    assert_eq!((d.ir.width(), d.ir.height()), (608, 1608));
    assert_eq!(d.ir.data().len(), 608 * 1608);
    assert_eq!(d.tiles, 1);
    assert!(d.registration.same_geometry);
    assert!(d.registration.seams.is_empty());
    assert_eq!(d.passes.len(), 2);
    assert_pass(&d.passes[0], Pass::Visible, (608, 1608), 1, 51, &SCAN600_START);
    assert_pass(&d.passes[1], Pass::Infrared, (608, 1608), 4, 51, &SCAN600_START);
    assert_eq!(d.passes[0].area, d.passes[1].area);
    assert!(d.postflight.safe_to_continue, "{}", d.postflight.reason);
    assert!(r.ends_clean_idle());
    assert_eq!(log.lock().unwrap().iter().filter(|p| matches!(p, Phase::Handshake { .. })).count(), 1);
}

#[test]
fn replay_catches_a_wire_difference() {
    // The harness must fail loudly when the driver's bytes differ from the recording: corrupt
    // one recorded FS W parameter byte of the prescan and the replay must not pass.
    let text = include_str!("fixtures/v850-prescan.log").replacen("out c8000000c8000000", "out c9000000c8000000", 1);
    let r = Replay::load(&text);
    let mut s = Session::with_opener(r.opener(), SessionConfig::default());
    let res = s.prescan(&PrescanOptions::default(), &CancelToken::new(), &mut |_| {});
    assert!(res.is_err());
    let last = r.handles.last().unwrap();
    assert!(last.verify().unwrap_err().contains("write mismatch"));
}

#[test]
fn committed_fixtures_hold_no_image_data() {
    for (name, text) in [
        ("status", include_str!("fixtures/v850-status.log")),
        ("recover", include_str!("fixtures/v850-recover.log")),
        ("prescan", include_str!("fixtures/v850-prescan.log")),
        ("scan600", include_str!("fixtures/v850-scan600.log")),
        ("dual600", include_str!("fixtures/v850-dual600.log")),
    ] {
        assert!(text.len() < 32 * 1024, "{name}: fixtures stay KB-sized");
        assert!(text.lines().next().unwrap_or("").starts_with("# Epson V850 replay fixture"), "{name}: header");
        for (i, line) in text.lines().enumerate() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let ["in", _, payload] | ["int", payload] = parts[..] {
                assert!(payload.starts_with("z:") || payload.len() <= 2 * 512, "{name}:{}: device payload over 512 bytes", i + 1);
                assert!(!payload.contains(".."), "{name}:{}: truncated device payload", i + 1);
            }
        }
    }
}
