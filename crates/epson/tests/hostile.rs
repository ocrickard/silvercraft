//! Hostile device replies never panic the driver, and the driver never sends a scan start for
//! an area outside the film area whatever the device says.

mod common;

use std::sync::Arc;

use common::*;
use silvercraft_epson::codec::{self, LutChannel, PassKind};

use proptest::prelude::*;
use silvercraft_epson::{CancelToken, Dpi, PrescanOptions, Region, ScanRequest, Session, SessionConfig, VirtualClock};
use silvercraft_usb::{ScriptedTransport, Step, UsbError};

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let reply = prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(Step::In),
        Just(Step::In(vec![0x06])),
        Just(Step::In(vec![0x02, 0x12, 0x00, 0x00])),
        Just(Step::InErr(UsbError::Stall)),
        Just(Step::InErr(UsbError::Timeout)),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u8>()).prop_map(|(u, f, r, s)| {
            let mut v = vec![0x02, s];
            v.extend(u.to_le_bytes());
            v.extend(f.to_le_bytes());
            v.extend(r.to_le_bytes());
            Step::In(v)
        }),
    ];
    proptest::collection::vec(prop_oneof![Just(Step::OutAny), reply], 0..120)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn session_operations_never_panic_on_hostile_replies(script in steps()) {
        let t = ScriptedTransport::new(script);
        let mut s = Session::with_transport_and_clock(t.clone(), Arc::new(VirtualClock::new()), SessionConfig { poll_attempts: 3, prescan_poll_attempts: 3, ..SessionConfig::default() });
        let c = CancelToken::new();
        let _ = s.status();
        let _ = s.recover();
        let _ = s.prescan(&PrescanOptions { dpi: Dpi::new(50).unwrap(), width_in: 0.5, height_in: 0.5 }, &c, &mut |_| {});
        let r = ScanRequest { region: Region::new(0.0, 0.0, 0.3, 0.3).unwrap(), dpi: Dpi::new(50).unwrap(), auto_calibrate: false };
        let _ = s.scan_visible(&r, &c, &mut |_| {});
        let _ = s.scan_dual_ir(&r, &c, &mut |_| {});
        // Every FS W block the driver sent is in range and ≤ 4096 px wide.
        let writes = t.writes();
        for w in writes.iter().filter(|w| w.len() == 64) {
            let p = silvercraft_epson::codec::parse_param_block(w).unwrap();
            prop_assert!(p.area.width <= 4096);
            prop_assert!(f64::from(p.area.x + p.area.width) / f64::from(p.dpi) <= 8.0 + 1e-6);
            prop_assert!(f64::from(p.area.y + p.area.height) / f64::from(p.dpi) <= 10.0 + 1e-6);
        }
        // A targeted (non-origin) FS W is never sent: nothing in this run calibrated a carriage
        // at a non-origin request... (all requests here are at origin).
        for w in writes.iter().filter(|w| w.len() == 64) {
            let p = silvercraft_epson::codec::parse_param_block(w).unwrap();
            prop_assert!(p.area.x == 0 && p.area.y == 0);
        }
    }
}

/// A complete, valid dual-pass 8x3 script (as in golden_wire.rs).
fn valid_dual_script() -> Vec<Step> {
    let r = Region::new(0.0, 0.0, 0.16, 0.06).unwrap();
    let d = Dpi::new(50).unwrap();
    let a = silvercraft_epson::area_pixels(&r, d);
    let fs_s: Vec<u8> = (0..64).collect();
    let raster = bil_8x3();
    let mut w = Wire::new();
    w.status_probe().priming3();
    w.setup(&[LutChannel::Red, LutChannel::Green, LutChannel::Blue], &codec::param_block(&a, d, PassKind::Visible { high_speed: false }), &IDLE);
    w.fs_g(&BUSY).fs_g(&ready(48, 2, 48));
    w.chunks(&[with_trailer(raster[..48].to_vec(), 0), with_trailer(raster[48..96].to_vec(), 0), with_trailer(raster[96..].to_vec(), 0)]);
    w.out(&[0x1C, 0x53]).inn(&fs_s);
    w.ack(&[0x1B, 0x23]).ack(&codec::handshake_payload_from_source(&fs_s).unwrap());
    w.setup(&[LutChannel::Master], &codec::param_block(&a, d, PassKind::Infrared), &IDLE);
    w.fs_g(&ready(48, 2, 48));
    w.chunks(&[with_trailer(raster[..48].to_vec(), 0), with_trailer(raster[48..96].to_vec(), 0), with_trailer(raster[96..].to_vec(), 0)]);
    w.postflight();
    // Writes are checked by the golden tests; here only the replies matter.
    w.steps.into_iter().map(|s| if matches!(s, Step::Out(_)) { Step::OutAny } else { s }).collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn mutated_valid_replies_never_panic(muts in proptest::collection::vec((any::<proptest::sample::Index>(), proptest::collection::vec(any::<u8>(), 0..20), any::<bool>()), 1..4)) {
        let mut script = valid_dual_script();
        for (idx, bytes, drop_step) in muts {
            let i = idx.index(script.len());
            if drop_step {
                script.remove(i);
            } else if let Step::In(_) = script[i] {
                script[i] = Step::In(bytes);
            }
        }
        let t = ScriptedTransport::new(script);
        let mut s = Session::with_transport_and_clock(t, Arc::new(VirtualClock::new()), SessionConfig::default());
        let r = ScanRequest { region: Region::new(0.0, 0.0, 0.16, 0.06).unwrap(), dpi: Dpi::new(50).unwrap(), auto_calibrate: false };
        if let Ok(d) = s.scan_dual_ir(&r, &CancelToken::new(), &mut |_| {}) {
            prop_assert_eq!((d.rgb.width(), d.ir.width()), (8, 8));
        }
    }
}

#[test]
fn unmutated_script_succeeds() {
    let t = ScriptedTransport::new(valid_dual_script());
    let mut s = Session::with_transport_and_clock(t.clone(), Arc::new(VirtualClock::new()), SessionConfig::default());
    let r = ScanRequest { region: Region::new(0.0, 0.0, 0.16, 0.06).unwrap(), dpi: Dpi::new(50).unwrap(), auto_calibrate: false };
    s.scan_dual_ir(&r, &CancelToken::new(), &mut |_| {}).unwrap();
    t.verify().unwrap();
}
