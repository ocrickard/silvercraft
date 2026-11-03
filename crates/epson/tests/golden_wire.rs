//! Byte-exact wire goldens for every production command sequence, against a scripted
//! transport, including the session preflight, priming and cleanup paths. `ScriptedTransport` fails on any unexpected or
//! differing write and `verify()` checks every scripted step was consumed.

mod common;

use common::*;
use silvercraft_epson::codec::{self, LutChannel, PassKind, Region};
use silvercraft_epson::{CancelToken, Dpi, EpsonError, PrescanOptions, ScanRequest, SessionConfig, area_pixels};
use silvercraft_usb::Step;

fn req_8x3() -> ScanRequest {
    // region 0.16 x 0.06 in @ 50 dpi -> 8 x 3 px.
    ScanRequest { region: Region::new(0.0, 0.0, 0.16, 0.06).unwrap(), dpi: Dpi::new(50).unwrap(), auto_calibrate: false }
}

fn params_8x3(pass: PassKind) -> [u8; 64] {
    let r = req_8x3();
    codec::param_block(&area_pixels(&r.region, r.dpi), r.dpi, pass)
}

const VIS: [LutChannel; 3] = [LutChannel::Red, LutChannel::Green, LutChannel::Blue];

fn visible_handle_tokens() -> Vec<&'static str> {
    vec![
        "1b 44", "08", "1b 44", "10", "1b 44", "08", "1c 46", "1b 66", "1b 40", "1b 46", "1b 7a", "lut:72", "1b 7a", "lut:67", "1b 7a", "lut:62",
        "1c 57", "param64", "1b 5a", "03", "1c 47", "1c 47", "06", "06",
    ]
}

fn ir_handle_tokens() -> Vec<&'static str> {
    vec![
        "1b 44", "08", "1b 44", "10", "1b 44", "08", "1c 46", "1b 66", "1b 40", "1b 46", "1b 7a", "lut:6d", "1c 57", "param64", "1b 5a", "03",
        "1c 47", "1c 47", "06", "06",
    ]
}

fn preamble_tokens() -> Vec<String> {
    let mut v = vec!["1b 46".to_string()]; // preflight status probe
    for _ in 0..3 {
        v.extend(["1b 40", "1c 46", "1c 49", "1b 49"].map(String::from));
    }
    v
}

#[test]
fn status_sends_exactly_one_esc_f_and_reads_four_bytes() {
    let t = Wire::new().status_probe().transport();
    let mut s = session(&t, SessionConfig::default());
    let r = s.status().unwrap();
    assert!(r.clean_idle);
    assert_eq!(t.writes(), vec![vec![0x1B, 0x46]]);
    t.verify().unwrap();
}

#[test]
fn golden_visible_capture_wire_bytes() {
    // T:5458 test_golden_visible_capture_wire_and_result: uw=4, fcc=2, rem=2, trailers 0x99.
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &[0x02, 0x12, 0x02, 0x00, 0xAA, 0xBB]);
    w.fs_g(&BUSY).fs_g(&ready(4, 2, 2));
    w.chunks(&[vec![1, 2, 3, 4, 0x99], vec![5, 6, 7, 8, 0x99], vec![9, 10, 0x99]]);
    w.postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    // The 10-byte fixture raster is too short for an 8x3 image: the wire is exact, the
    // assembly (which the original raw capture did not do) refuses it.
    let r = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop());
    assert!(matches!(r, Err(EpsonError::Raster(_))), "{r:?}");
    t.verify().unwrap();
    let all = toks(&t.writes());
    let mut expected = preamble_tokens();
    expected.extend(visible_handle_tokens().into_iter().map(String::from));
    expected.push("1b 46".into()); // postflight
    assert_eq!(all, expected);
    // no FS S / ESC # and no CAN on a complete visible read
    assert!(!t.writes().contains(&vec![0x1C, 0x53]) && !t.writes().contains(&vec![0x1B, 0x23]) && !t.writes().contains(&vec![0x18]));
}

#[test]
fn visible_scan_assembles_bil_and_drains_trailer() {
    // T:5539: one remainder chunk of 144 bytes + trailer; BIL -> pixel(0,0) = (1,9,17).
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &IDLE);
    w.fs_g(&ready(144, 0, 144));
    w.chunks(&[with_trailer(bil_8x3(), 0x99)]);
    w.postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let scan = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap();
    t.verify().unwrap();
    assert_eq!((scan.image.width(), scan.image.height()), (8, 3));
    assert_eq!([scan.image.get(0, 0, 0), scan.image.get(0, 0, 1), scan.image.get(0, 0, 2)], [1, 9, 17]);
    assert_eq!([scan.image.get(7, 2, 0), scan.image.get(7, 2, 1), scan.image.get(7, 2, 2)], [56, 64, 72]);
    assert_eq!(scan.passes[0].chunks, 1);
    assert_eq!(scan.passes[0].nonzero_trailers, 1);
    assert_eq!(scan.passes[0].poll_attempts, 1);
    assert!(scan.postflight.safe_to_continue);
    assert!(!t.writes().contains(&vec![0x06])); // single (final) chunk is never ACKed
}

#[test]
fn prescan_sets_high_speed_byte_27() {
    let opts = PrescanOptions { dpi: Dpi::new(50).unwrap(), width_in: 0.16, height_in: 0.06 };
    let params = params_8x3(PassKind::Visible { high_speed: true });
    assert_eq!((params[27], params[24], params[28]), (0x01, 0x12, 0x60));
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params, &IDLE);
    w.fs_g(&ready(144, 0, 144)).chunks(&[with_trailer(bil_8x3(), 0)]).postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let p = s.prescan(&opts, &CancelToken::new(), &mut noop()).unwrap();
    t.verify().unwrap();
    assert_eq!(p.image.width(), 8);
    assert_eq!(s.calibration(), silvercraft_epson::Calibration::Calibrated);
}

#[test]
fn start_failure_cancels_then_postflight() {
    // T:5300: FS G never ready -> drain + CAN + ESC F postflight, never a wedge.
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &IDLE);
    w.fs_g(&BUSY).fs_g(&BUSY);
    w.out(&[0x18]).inn(&[0x06]);
    w.postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig { poll_attempts: 2, ..SessionConfig::default() });
    let e = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err();
    assert!(e.to_string().contains("did not become ready"), "{e}");
    t.verify().unwrap();
    assert_eq!(t.writes().last().unwrap(), &vec![0x1B, 0x46]);
}

#[test]
fn fs_w_nak_aborts_cleanly() {
    let mut w = Wire::new();
    w.status_probe().priming3();
    for d in [0x08, 0x10, 0x08] {
        w.ack(&[0x1B, 0x44]).ack(&[d]);
    }
    w.out(&[0x1C, 0x46]).inn(&[0u8; 16]);
    w.out(&[0x1B, 0x66]).inn(&IDLE);
    w.ack(&[0x1B, 0x40]);
    w.out(&[0x1B, 0x46]).inn(&IDLE);
    for l in VIS {
        w.ack(&[0x1B, 0x7A]).ack(&codec::identity_lut(l));
    }
    w.ack(&[0x1C, 0x57]).out(&params_8x3(PassKind::Visible { high_speed: false })).inn(&[0x15]);
    w.out(&[0x18]).inn(&[0x06]).postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    assert!(matches!(s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()), Err(EpsonError::Nak(_))));
    t.verify().unwrap();
}

#[test]
fn ack_timeout_mid_command_is_cleaned_up_with_can() {
    // T:94 analogue: the ESC Z value ACK never arrives -> timeout -> drain, CAN, postflight.
    let mut w = Wire::new();
    w.status_probe().priming3();
    for d in [0x08, 0x10, 0x08] {
        w.ack(&[0x1B, 0x44]).ack(&[d]);
    }
    w.out(&[0x1C, 0x46]).inn(&[0u8; 16]);
    w.out(&[0x1B, 0x66]).inn(&IDLE);
    w.ack(&[0x1B, 0x40]);
    w.out(&[0x1B, 0x46]).inn(&IDLE);
    for l in VIS {
        w.ack(&[0x1B, 0x7A]).ack(&codec::identity_lut(l));
    }
    w.ack(&[0x1C, 0x57]).ack(&params_8x3(PassKind::Visible { high_speed: false }));
    w.ack(&[0x1B, 0x5A]).out(&[0x03]); // no ACK
    w.out(&[0x18]).inn(&[0x06]).postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let e = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err();
    assert!(matches!(e, EpsonError::Timeout { .. }), "{e}");
    t.verify().unwrap();
}

#[test]
fn golden_dual_pass_wire_bytes() {
    // T:5530 test_golden_dual_pass_wire_and_result, with a full 8x3 raster.
    let vis_params = params_8x3(PassKind::Visible { high_speed: false });
    let ir_params = params_8x3(PassKind::Infrared);
    assert_eq!((ir_params[24], ir_params[26], ir_params[28], ir_params[38]), (0x13, 0x03, 0x20, 0x02));
    let fs_s: Vec<u8> = (0..64).collect();
    let hs = codec::handshake_payload_from_source(&fs_s).unwrap();
    let raster = bil_8x3();
    let (a, b, c) = (raster[..48].to_vec(), raster[48..96].to_vec(), raster[96..].to_vec());
    let ir_raw: Vec<u8> = (0..72u16).flat_map(|i| (1000 + i * 10).to_le_bytes()).collect();

    let mut w = Wire::new();
    w.status_probe().priming3();
    // handle 1: visible + handshake, no postflight
    w.setup(&VIS, &vis_params, &IDLE);
    w.fs_g(&BUSY).fs_g(&ready(48, 2, 48));
    w.chunks(&[with_trailer(a, 0), with_trailer(b, 0), with_trailer(c, 0)]);
    w.out(&[0x1C, 0x53]).inn(&fs_s);
    w.ack(&[0x1B, 0x23]).ack(&hs);
    // handle 2: IR, master LUT, postflight
    w.setup(&[LutChannel::Master], &ir_params, &IDLE);
    w.fs_g(&BUSY).fs_g(&ready(48, 2, 48));
    w.chunks(&[with_trailer(ir_raw[..48].to_vec(), 0), with_trailer(ir_raw[48..96].to_vec(), 0), with_trailer(ir_raw[96..].to_vec(), 0)]);
    w.postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let d = s.scan_dual_ir(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap();
    t.verify().unwrap();

    let all = toks(&t.writes());
    let pre = preamble_tokens();
    assert_eq!(&all[..pre.len()], &pre[..]);
    let rest = &all[pre.len()..];
    let mut h1: Vec<String> = visible_handle_tokens().into_iter().map(String::from).collect();
    h1.extend(["1c 53", "1b 23", "hs32"].map(String::from));
    let mut h2: Vec<String> = ir_handle_tokens().into_iter().map(String::from).collect();
    h2.push("1b 46".into());
    assert_eq!(&rest[..h1.len()], &h1[..]);
    assert_eq!(&rest[h1.len()..], &h2[..]);

    assert_eq!((d.rgb.width(), d.rgb.height(), d.ir.width(), d.ir.height()), (8, 3, 8, 3));
    assert_eq!(d.rgb.get(0, 0, 1), 9);
    // IR pixel 0 = rint(mean(1000, 1010, 1020)) = 1010
    assert_eq!(d.ir.get(0, 0), 1010);
    assert!(d.registration.same_geometry);
    assert_eq!(d.registration.ir_offset, (0, 0));
    let vis_meta = &d.passes[0];
    let ir_meta = &d.passes[1];
    assert_eq!((vis_meta.poll_attempts, ir_meta.poll_attempts), (2, 2));
    assert_eq!(vis_meta.start_response, ir_meta.start_response);
}

#[test]
fn dual_pass_incomplete_visible_never_starts_ir() {
    // Visible raster read fails mid-transfer: clean abort, no handshake, no IR handle.
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &IDLE);
    w.fs_g(&ready(48, 2, 48));
    w.inn(&[1u8; 49]).out(&[0x06]);
    w.steps.push(Step::InErr(silvercraft_usb::UsbError::Stall));
    w.out(&[0x18]).inn(&[0x06]);
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let e = s.scan_dual_ir(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err();
    assert!(matches!(e, EpsonError::VisibleIncomplete(_)), "{e}");
    t.verify().unwrap();
    assert!(!t.writes().contains(&vec![0x1C, 0x53]));
    assert!(!t.writes().contains(&vec![0x1B, 0x23]));
    assert_eq!(t.writes().iter().filter(|w| **w == [0x1C, 0x47]).count(), 1);
}

#[test]
fn hostile_start_descriptor_is_refused_with_clean_abort() {
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &IDLE);
    w.fs_g(&ready(0xFFFF_FFFF, 0xFFFF_FFFF, 7));
    w.out(&[0x18]).inn(&[0x06]).postflight();
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    assert!(matches!(s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()), Err(EpsonError::Protocol(_))));
    t.verify().unwrap();
}

#[test]
fn targeted_scan_without_calibration_sends_nothing() {
    let t = Wire::new().transport();
    let mut s = session(&t, SessionConfig::default());
    let req = ScanRequest { region: Region::new(1.0, 1.0, 0.4, 0.4).unwrap(), dpi: Dpi::new(300).unwrap(), auto_calibrate: false };
    assert_eq!(s.scan_visible(&req, &CancelToken::new(), &mut noop()).unwrap_err(), EpsonError::NotCalibrated);
    assert_eq!(s.scan_dual_ir(&req, &CancelToken::new(), &mut noop()).unwrap_err(), EpsonError::NotCalibrated);
    assert!(t.writes().is_empty());
}

#[test]
fn not_clean_preflight_runs_recovery_then_power_cycle_required() {
    // Status 02 12 01 00, recovery cannot reach clean idle -> PowerCycleRequired, no scan bytes.
    let mut w = Wire::new();
    w.out(&[0x1B, 0x46]).inn(&[0x02, 0x12, 0x01, 0x00]);
    w.out(&[0x18]).inn(&[0x06]).out(&[0x1B, 0x40]).inn(&[0x06]).out(&[0x1B, 0x46]).inn(&[0x02, 0x12, 0x01, 0x00]);
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let e = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err();
    assert!(e.requires_power_cycle(), "{e}");
    assert!(e.user_message().contains("Turn the scanner off"));
    t.verify().unwrap();
    assert!(s.power_cycle_required().is_some());
    // Further scans are refused without I/O until recovery succeeds.
    let before = t.writes().len();
    assert!(s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err().requires_power_cycle());
    assert_eq!(t.writes().len(), before);
}

#[test]
fn repeated_partial_native_tails_escalate_to_power_cycle() {
    let mut w = Wire::new();
    // attempt 1 (fast_if_idle): not clean, then CAN, ESC @ -> lone 02 (tail drain times out)
    w.out(&[0x1B, 0x46]).inn(&[0x02, 0x12, 0x01, 0x00]);
    w.out(&[0x18]).inn(&[0x06]).out(&[0x1B, 0x40]).inn(&[0x02]);
    w.out(&[0x1B, 0x46]).inn(&[0x02, 0x12, 0x01, 0x00]);
    // attempt 2
    w.out(&[0x18]).inn(&[0x06]).out(&[0x1B, 0x40]).inn(&[0x02]);
    w.out(&[0x1B, 0x46]).inn(&[0x02, 0x12, 0x01, 0x00]);
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let e = s.recover().unwrap_err();
    assert!(matches!(e, EpsonError::PowerCycleRequired { ref reason } if reason.contains("partial native tails")), "{e}");
    t.verify().unwrap();
}

#[test]
fn recover_fast_path_when_idle() {
    let t = Wire::new().status_probe().transport();
    let mut s = session(&t, SessionConfig::default());
    let r = s.recover().unwrap();
    assert!(r.clean_idle);
    t.verify().unwrap();
}

#[test]
fn repeated_bad_postflight_escalates_to_power_cycle() {
    // Two scans whose postflight ESC F times out (power-cycle-if-repeated) -> refuse further scans.
    let mut w = Wire::new();
    for _ in 0..2 {
        w.status_probe().priming3();
        w.setup(&VIS, &params_8x3(PassKind::Visible { high_speed: false }), &IDLE);
        w.fs_g(&ready(144, 0, 144)).chunks(&[with_trailer(bil_8x3(), 0)]);
        w.out(&[0x1B, 0x46]); // postflight reply never comes
    }
    let t = w.transport();
    let mut s = session(&t, SessionConfig::default());
    let first = s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap();
    assert!(first.postflight.requires_power_cycle_if_repeated);
    assert!(s.power_cycle_required().is_none());
    s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap();
    assert!(s.power_cycle_required().is_some());
    t.verify().unwrap();
    assert!(s.scan_visible(&req_8x3(), &CancelToken::new(), &mut noop()).unwrap_err().requires_power_cycle());
}
