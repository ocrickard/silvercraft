//! End-to-end behaviour against the simulator, plus the SilverCraft safety
//! guarantees (each §6.2 wedge is impossible or refused before the device).

use std::time::Duration;

use silvercraft_epson::codec::{FS_G, FS_W, PassKind, param_block};
use silvercraft_epson::sim::SimOpener;
use silvercraft_epson::{
    CancelToken, Dpi, EpsonError, FrameBox, FramePlanOptions, Pass, Phase, PixelRect, PrescanOptions, Region, ScanRequest, Session, SessionConfig,
    SimConfig, SimFault, SimScanner, area_pixels, plan_frames,
};
use silvercraft_usb::Transport;

fn session(sim: &SimScanner) -> Session<SimOpener> {
    Session::with_opener(sim.opener(), SessionConfig::default())
}

fn req(l: f64, t: f64, w: f64, h: f64, dpi: u32) -> ScanRequest {
    ScanRequest { region: Region::new(l, t, w, h).unwrap(), dpi: Dpi::new(dpi).unwrap(), auto_calibrate: false }
}

fn small_prescan() -> PrescanOptions {
    // Same origin geometry as the bed prescan, at 100 dpi to keep tests fast.
    PrescanOptions { dpi: Dpi::new(100).unwrap(), width_in: 5.9, height_in: 9.7 }
}

fn go() -> CancelToken {
    CancelToken::new()
}

fn noop(_: Phase) {}

// --- TS: test_handshake_ack_timeout_resets_pipe_instead_of_wedging --------------------------
#[test]
fn handshake_ack_failure_is_cleaned_up_not_wedged() {
    let sim = SimScanner::default();
    sim.inject(SimFault::FailReadAfterWrite(vec![0x03])); // the ESC Z value ack read fails
    let mut s = session(&sim);
    let r = s.scan_visible(&req(0.0, 0.0, 0.5, 0.5, 200), &go(), &mut noop);
    assert!(r.is_err());
    assert!(!sim.wedged(), "{}", sim.wedge_reason());
    assert!(sim.cans() >= 1, "CAN not issued on handshake failure");
    // and the next scan works
    s.scan_visible(&req(0.0, 0.0, 0.5, 0.5, 200), &go(), &mut noop).unwrap();
}

// --- TS: test_targeted_visible_from_cold_wedges ---------------------------------------------
#[test]
fn targeted_visible_from_cold_is_refused_before_the_device() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    let e = s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap_err();
    assert_eq!(e, EpsonError::NotCalibrated);
    assert!(!sim.wedged());
    assert_eq!(sim.handles_opened(), 0);
}

#[test]
fn targeted_visible_from_cold_auto_calibrates() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    let r = ScanRequest { auto_calibrate: true, ..req(1.0, 1.0, 0.4, 0.4, 300) };
    let scan = s.scan_visible(&r, &go(), &mut noop).unwrap();
    assert!(sim.calibrated() && !sim.wedged());
    assert_eq!((scan.image.width(), scan.image.height()), (120, 120));
    assert_eq!(sim.started_scans(), 2); // origin prescan, then the targeted scan
}

#[test]
fn calibration_does_not_survive_a_power_cycle_in_the_same_session() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert!(sim.calibrated() && !sim.wedged());
    // Same Session object, scanner power-cycled underneath it: the calibration was
    // earned on the previous enumeration and must not arm a targeted FS G now.
    sim.power_cycle();
    assert_eq!(s.calibration(), silvercraft_epson::Calibration::Unknown);
    let e = s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap_err();
    assert_eq!(e, EpsonError::NotCalibrated);
    assert!(!sim.wedged(), "{}", sim.wedge_reason());
    // A fresh prescan recalibrates and the session works again.
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert!(!sim.wedged());
}

// --- TS: test_wedge_persists_until_power_cycle ----------------------------------------------
#[test]
fn wedge_persists_until_power_cycle() {
    let sim = SimScanner::default();
    // Wedge the device behind the driver's back (a raw targeted FS G on a cold carriage).
    {
        let mut h = sim.open_handle();
        let a = area_pixels(&Region::new(1.0, 1.0, 0.4, 0.4).unwrap(), Dpi::new(300).unwrap());
        let t = Duration::from_millis(200);
        h.bulk_out(&FS_W, t).unwrap();
        h.bulk_in(1, t).unwrap();
        h.bulk_out(&param_block(&a, Dpi::new(300).unwrap(), PassKind::Visible { high_speed: false }), t).unwrap();
        h.bulk_in(1, t).unwrap();
        h.bulk_out(&FS_G, t).unwrap();
    }
    assert!(sim.wedged() && sim.wedge_reason().contains("uncalibrated"));
    let mut s = session(&sim);
    // Even a valid origin prescan fails while wedged — and the driver says why.
    let e = s.prescan(&small_prescan(), &go(), &mut noop).unwrap_err();
    assert!(e.requires_power_cycle(), "{e}");
    assert!(s.power_cycle_required().is_some());
    assert!(s.scan_visible(&req(0.0, 0.0, 0.4, 0.4, 100), &go(), &mut noop).unwrap_err().requires_power_cycle());
    sim.power_cycle();
    assert!(!sim.wedged() && !sim.calibrated());
    s.recover().unwrap();
    assert!(s.power_cycle_required().is_none());
    let p = s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    assert_eq!((p.image.width(), p.image.height()), (584, 970));
}

// --- TS: test_origin_prescan_calibrates_and_streams -----------------------------------------
#[test]
fn origin_prescan_calibrates_and_streams() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    let p = s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    assert!(sim.calibrated());
    assert_eq!(s.calibration(), silvercraft_epson::Calibration::Calibrated);
    assert!(p.image.data().iter().any(|&v| v > 0));
    assert_eq!(p.passes[0].poll_attempts, 2 + 1); // cold lamp busy-polled before arming
    assert!(!sim.wedged());
}

// --- TS: test_targeted_visible_after_prescan_arms_first_poll --------------------------------
#[test]
fn targeted_visible_after_prescan_arms_first_poll() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    let scan = s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert!(!sim.wedged());
    assert_eq!((scan.image.width(), scan.image.height()), (120, 120));
    assert_eq!(scan.passes[0].poll_attempts, 1);
    assert!(scan.postflight.safe_to_continue);
}

// --- TS: test_scan_b8_visible_production_full_flow_via_sim ----------------------------------
#[test]
fn visible_full_flow_produces_the_film_image() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    let scan = s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert!(scan.image.data().iter().any(|&v| v > 0));
    // Content is the synthetic film at absolute bed coordinates (BIL assembly correct).
    for (x, y) in [(0usize, 0usize), (57, 33), (119, 119)] {
        for c in 0..3 {
            let u = (300.0 + x as f64) / 300.0;
            let v = (300.0 + y as f64) / 300.0;
            assert_eq!(scan.image.get(x, y, c), silvercraft_epson::sim::film_visible(u, v, c));
        }
    }
}

// --- TS: test_oversized_area_blocked_by_guard_before_device ---------------------------------
#[test]
fn oversized_area_cannot_be_expressed() {
    assert!(Region::new(0.0, 0.0, 8.5, 11.6).is_err());
    assert!(Region::new(7.9, 0.0, 0.2, 1.0).is_err());
    assert!(Region::new(0.0, 0.0, 8.0, 10.0).is_ok());
}
// --- TS: test_device_model_wedges_on_oversized_area -> sim::tests (unit) -----------------------

// --- TS: test_targeted_ir_cold_wedges_even_after_visible / test_ir_capture_succeeds_when_warm /
//     test_ir_streaming_path_well_formed_when_warm: the standalone/streaming IR paths are not
//     ported. Their device rule is checked here on the raw wire: a bare IR FS G
//     after a plain visible scan wedges; on a warm IR engine it arms.
fn raw_fs_g(sim: &SimScanner, l: f64, pass: PassKind) {
    let mut h = sim.open_handle();
    let t = Duration::from_millis(200);
    let a = area_pixels(&Region::new(l, l, 0.4, 0.4).unwrap(), Dpi::new(100).unwrap());
    h.bulk_out(&FS_W, t).unwrap();
    h.bulk_in(1, t).unwrap();
    h.bulk_out(&param_block(&a, Dpi::new(100).unwrap(), pass), t).unwrap();
    h.bulk_in(1, t).unwrap();
    h.bulk_out(&FS_G, t).unwrap();
    // Read the whole reply / raster so the handle closes cleanly when armed.
    let mut n = 0;
    while let Ok(d) = h.bulk_in(1 << 20, t) {
        n += d.len();
        let _ = h.bulk_out(&[0x06], t);
    }
    let _ = n;
}

#[test]
fn bare_ir_fs_g_wedges_cold_and_arms_warm() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    s.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    raw_fs_g(&sim, 1.0, PassKind::Infrared);
    assert!(sim.wedged() && sim.wedge_reason().contains("IR"));

    let sim = SimScanner::default();
    sim.set_ir_warm(true);
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    raw_fs_g(&sim, 1.0, PassKind::Infrared);
    assert!(sim.log().iter().any(|l| l.starts_with("armed IR")));
}

// --- TS: test_dual_pass_arms_ir_and_registers -----------------------------------------------
#[test]
fn dual_pass_arms_ir_and_registers() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    assert!(sim.calibrated() && !sim.ir_armed());
    let d = s.scan_dual_ir(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert!(sim.log().iter().any(|l| l == "IR armed"));
    assert!(!sim.ir_armed()); // the arming was consumed by the IR pass
    assert!(!sim.wedged());
    assert_eq!((d.rgb.width(), d.rgb.height(), d.ir.width(), d.ir.height()), (120, 120, 120, 120));
    let (vis, ir) = (&d.passes[0], &d.passes[1]);
    assert_eq!((vis.pass, ir.pass), (Pass::Visible, Pass::Infrared));
    assert_eq!(vis.poll_attempts, 1); // lamp warm from the prescan
    assert_eq!(ir.poll_attempts, 2 + 1); // the IR lamp has its own warm-up
    assert_eq!(vis.start_response, ir.start_response); // identical descriptor: same geometry
    assert!(d.postflight.safe_to_continue);
    assert!(d.registration.same_geometry);
    // IR plane = collapse of the 3 sub-samples = the synthetic IR at the same bed position.
    assert_eq!(d.ir.get(10, 20), silvercraft_epson::sim::film_infrared(310.0 / 300.0, 320.0 / 300.0));
}

// --- TS: test_dual_pass_ir_hangs_without_calibration ----------------------------------------
#[test]
fn dual_pass_without_calibration_is_refused() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    assert_eq!(s.scan_dual_ir(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap_err(), EpsonError::NotCalibrated);
    assert!(!sim.wedged());
    assert_eq!(sim.handles_opened(), 0);
}

// --- TS: test_cold_start_fsg_read_timeout_wedges / ..._longer_read_timeout_rides_through ---------
#[test]
fn cold_start_silence_is_ridden_out_even_with_a_short_session_timeout() {
    // 12 s of silence on the first FS G; the session's general timeout is only 5 s, but FS G
    // reads never use less than the floor, so the scan arms instead of wedging.
    let sim = SimScanner::new(SimConfig { cold_start_fsg_latency: Duration::from_secs(12), ..SimConfig::default() });
    let mut s = Session::with_opener(sim.opener(), SessionConfig { timeout: Duration::from_secs(5), ..SessionConfig::default() });
    let p = s.prescan(&PrescanOptions { dpi: Dpi::new(200).unwrap(), width_in: 0.6, height_in: 0.6 }, &go(), &mut noop).unwrap();
    assert!(!sim.wedged(), "{}", sim.wedge_reason());
    assert!(sim.calibrated());
    assert_eq!(p.passes[0].poll_attempts, 1); // warm-up ridden out, armed first poll
    // 45 s (beyond a 30 s cold start) is still ridden out with the 60 s floor.
    let sim = SimScanner::new(SimConfig { cold_start_fsg_latency: Duration::from_secs(45), ..SimConfig::default() });
    let mut s = session(&sim);
    s.prescan(&PrescanOptions { dpi: Dpi::new(200).unwrap(), width_in: 0.6, height_in: 0.6 }, &go(), &mut noop).unwrap();
    assert!(!sim.wedged());
}

#[test]
fn cancel_during_cold_start_waits_for_the_reply_then_aborts_cleanly() {
    let sim = SimScanner::new(SimConfig { cold_start_fsg_latency: Duration::from_secs(12), ..SimConfig::default() });
    let mut s = session(&sim);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let mut progress = move |p: Phase| {
        if let Phase::Setup { .. } = p {
            c2.cancel();
        }
    };
    let e = s.prescan(&PrescanOptions { dpi: Dpi::new(200).unwrap(), width_in: 0.6, height_in: 0.6 }, &cancel, &mut progress).unwrap_err();
    assert_eq!(e, EpsonError::Cancelled);
    assert!(!sim.wedged(), "{}", sim.wedge_reason());
    assert!(s.status().unwrap().clean_idle);
}

// --- TS: test_level_match_columns_removes_per_tile_level_steps -> raster::tests (unit) -------

// --- TS: test_scan_b8_visible_production_auto_tiles_wide_scan -------------------------------
#[test]
fn visible_auto_tiles_wide_scan() {
    let sim = SimScanner::default();
    let mut s = Session::with_opener(sim.opener(), SessionConfig { max_single_read_width_px: 400, ..SessionConfig::default() });
    let scan = s.scan_visible(&req(0.0, 0.0, 4.0, 0.5, 200), &go(), &mut noop).unwrap();
    assert_eq!(scan.tiles, 2);
    assert_eq!((scan.image.width(), scan.image.height()), (800, 100));
    assert!(scan.image.data().iter().any(|&v| v > 0));
    // Tiles abut on continuous film: tile 0 is the film exactly; tile 1 is level-matched
    // (§7.1: edge-median gain, clipped to [0.85, 1.18]). On this steep synthetic gradient the
    // 6-column edge medians differ by a few percent, so tile 1 lands within ~8 % of the film.
    for x in [0usize, 399] {
        assert_eq!(scan.image.get(x, 50, 0), silvercraft_epson::sim::film_visible(x as f64 / 200.0, 50.0 / 200.0, 0));
    }
    for x in [400usize, 799] {
        let film = f64::from(silvercraft_epson::sim::film_visible(x as f64 / 200.0, 50.0 / 200.0, 0));
        assert!((f64::from(scan.image.get(x, 50, 0)) - film).abs() / film < 0.08, "x={x} got {} film {film}", scan.image.get(x, 50, 0));
    }
    // priming ran once (before the first tile only): 3 handles + status + 2 scan handles
    assert_eq!(sim.handles_opened(), 1 + 3 + 2);
    assert!(!sim.wedged());
}

// --- TS: test_scan_b8_infrared_dual_pass_writes_registered_rgb_and_ir -----------------------
#[test]
fn dual_pass_produces_registered_rgb_and_ir() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    let d = s.scan_dual_ir(&req(1.0, 1.0, 0.4, 0.4, 300), &go(), &mut noop).unwrap();
    assert_eq!((d.rgb.width(), d.rgb.height()), (120, 120));
    assert_eq!((d.ir.width(), d.ir.height()), (120, 120));
    assert!(d.rgb.data().iter().any(|&v| v > 0) && d.ir.data().iter().any(|&v| v > 0));
}

// --- TS: test_scan_b8_infrared_dual_pass_auto_tiles_wide_scan -------------------------------
#[test]
fn dual_pass_auto_tiles_and_stitches_exactly() {
    let sim = SimScanner::default();
    let mut s = Session::with_opener(sim.opener(), SessionConfig { max_single_read_width_px: 400, ..SessionConfig::default() });
    let d = s.scan_dual_ir(&req(0.0, 0.0, 4.0, 0.5, 200), &go(), &mut noop).unwrap();
    // 64 px overlap must fit the cap: 3 tiles.
    assert_eq!(d.tiles, 3);
    // Real cross-tile content lets the stitch land exactly (the original sim allowed ±64): the
    // result spans to the end of the last tile, which area_pixels floors to 8 px (798 of 800).
    let last = area_pixels(
        &silvercraft_epson::plan_tiles(&req(0.0, 0.0, 4.0, 0.5, 200).region, Dpi::new(200).unwrap(), 400, 64).unwrap()[2],
        Dpi::new(200).unwrap(),
    );
    assert_eq!(last.x + last.width, 798);
    assert_eq!((d.rgb.width(), d.rgb.height()), (798, 100));
    assert_eq!((d.ir.width(), d.ir.height()), (798, 100));
    assert!(d.registration.seams.iter().all(|s| s.dy == 0));
    for x in [0usize, 300, 533, 797] {
        assert_eq!(d.ir.get(x, 40), silvercraft_epson::sim::film_infrared(x as f64 / 200.0, 40.0 / 200.0), "x={x}");
    }
    assert!(!sim.wedged());
}

// --- SilverCraft additions -------------------------------------------------------------------

#[test]
fn prescan_plan_visible_dual_end_to_end_with_registration_offset() {
    let sim = SimScanner::new(SimConfig { ir_offset: (-2, 2), ..SimConfig::default() });
    let mut s = session(&sim);
    let preview = s.prescan(&PrescanOptions::default(), &go(), &mut noop).unwrap();
    assert_eq!((preview.image.width(), preview.image.height()), (1176, 1940));
    // A "detected" frame at (200, 300) px of the 200-dpi preview, ~0.85 in square.
    let frames = vec![FrameBox { bounds: PixelRect { x: 200, y: 300, width: 170, height: 170 }, label: "f1".into() }];
    let plans = plan_frames(&preview, &frames, &FramePlanOptions { target_dpi: Some(600), ..Default::default() }).unwrap();
    assert_eq!(plans.len(), 1);
    let r = ScanRequest { region: plans[0].region, dpi: plans[0].dpi, auto_calibrate: false };
    let vis = s.scan_visible(&r, &go(), &mut noop).unwrap();
    assert_eq!(vis.image.width() as u32 / 8 * 8, vis.image.width() as u32);
    let d = s.scan_dual_ir(&r, &go(), &mut noop).unwrap();
    assert_eq!((d.rgb.width(), d.rgb.height()), (d.ir.width(), d.ir.height()));
    assert_eq!(d.registration.ir_offset, (-2, 2), "measured IR offset");
    assert!(!sim.wedged());
    assert!(s.status().unwrap().clean_idle);
}

#[test]
fn cancel_mid_transfer_leaves_the_scanner_clean_idle() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let mut progress = move |p: Phase| {
        if let Phase::Reading { bytes_done, .. } = p
            && bytes_done > 0
        {
            c2.cancel();
        }
    };
    let e = s.prescan(&small_prescan(), &cancel, &mut progress).unwrap_err();
    assert_eq!(e, EpsonError::Cancelled);
    assert!(!sim.wedged(), "{}", sim.wedge_reason());
    assert!(sim.cans() >= 1);
    assert!(s.status().unwrap().clean_idle);
    // Not calibrated by an incomplete origin scan? The sim calibrates on arming (the carriage
    // homes at FS G); the driver conservatively only marks calibration on a complete raster.
    assert_eq!(s.calibration(), silvercraft_epson::Calibration::Unknown);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
}

#[test]
fn cancel_during_dual_visible_pass_never_starts_ir() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let mut progress = move |p: Phase| {
        if let Phase::Reading { pass: Pass::Visible, .. } = p {
            c2.cancel();
        }
    };
    let e = s.scan_dual_ir(&req(1.0, 1.0, 2.0, 1.0, 300), &cancel, &mut progress).unwrap_err();
    assert_eq!(e, EpsonError::Cancelled);
    assert!(!sim.wedged());
    assert!(!sim.log().iter().any(|l| l.starts_with("armed IR") || l == "IR armed"));
    assert!(s.status().unwrap().clean_idle);
}

#[test]
fn partial_writes_resume_without_resending() {
    let sim = SimScanner::default();
    // The first write longer than 100 bytes (an ESC z LUT) only gets 100 bytes through.
    sim.inject(SimFault::SplitNextWrite(100));
    let mut s = session(&sim);
    let scan = s.scan_visible(&req(0.0, 0.0, 0.4, 0.4, 100), &go(), &mut noop).unwrap();
    assert!(sim.log().iter().any(|l| l.starts_with("partial write")));
    assert_eq!(scan.image.width(), 40);
    assert!(!sim.wedged());
}

#[test]
fn power_cycle_invalidates_calibration_across_sessions() {
    let sim = SimScanner::default();
    let mut a = session(&sim);
    a.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    // A second session on the same enumeration knows the carriage is calibrated.
    let mut b = session(&sim);
    assert_eq!(b.calibration(), silvercraft_epson::Calibration::Calibrated);
    b.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 100), &go(), &mut noop).unwrap();
    // After a power cycle (re-enumeration), nobody assumes calibration.
    sim.power_cycle();
    let mut c = session(&sim);
    assert_eq!(c.calibration(), silvercraft_epson::Calibration::Unknown);
    assert_eq!(c.scan_visible(&req(1.0, 1.0, 0.4, 0.4, 100), &go(), &mut noop).unwrap_err(), EpsonError::NotCalibrated);
    assert!(!sim.wedged());
}

#[test]
fn progress_reports_every_phase() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    let mut phases = Vec::new();
    let mut progress = |p: Phase| phases.push(p);
    s.prescan(&PrescanOptions { dpi: Dpi::new(100).unwrap(), width_in: 1.0, height_in: 1.0 }, &go(), &mut progress).unwrap();
    assert!(phases.contains(&Phase::Preflight));
    assert!(phases.iter().any(|p| matches!(p, Phase::Priming { cycle: 3, of: 3 })));
    assert!(phases.iter().any(|p| matches!(p, Phase::WarmingUp { .. })));
    let last_read = phases
        .iter()
        .rev()
        .find_map(|p| if let Phase::Reading { bytes_done, bytes_total, .. } = p { Some((*bytes_done, *bytes_total)) } else { None });
    // 1.0 in @ 100 dpi = 100 px, floored to a multiple of 8 = 96.
    assert_eq!(last_read, Some((96 * 100 * 6, 96 * 100 * 6)));
    assert!(phases.contains(&Phase::Assembling));
}

#[test]
fn many_back_to_back_scans_stay_clean() {
    let sim = SimScanner::default();
    let mut s = session(&sim);
    s.prescan(&small_prescan(), &go(), &mut noop).unwrap();
    for i in 0..4 {
        let l = 0.5 + f64::from(i) * 0.5;
        s.scan_visible(&req(l, l, 0.3, 0.3, 200), &go(), &mut noop).unwrap();
        s.scan_dual_ir(&req(l, l, 0.3, 0.3, 200), &go(), &mut noop).unwrap();
    }
    assert!(!sim.wedged());
    assert_eq!(sim.handles_open(), 0);
}
