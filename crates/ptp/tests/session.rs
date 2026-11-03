//! Scripted sessions: open, handshake, close, close-on-drop, wedged recovery.

mod common;

use common::{Script, ext_info, response};
use silvercraft_ptp::codec::{encode_command, rc};
use silvercraft_ptp::sony::{SonyCamera, SonyOptions};
use silvercraft_ptp::{ConnectionConfig, FakeClock, PtpError, Session};
use silvercraft_usb::{MockEvent, ScriptedTransport, Step, UsbError};

#[test]
fn open_handshake_close_exact_bytes() {
    let mut s = Script::new();
    s.open_session().handshake(&ext_info()).close_session();
    let t = ScriptedTransport::new(s.build());
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let (cam, report) = SonyCamera::connect(session, FakeClock::new(), SonyOptions::default()).unwrap();
    assert_eq!(report.response_codes, [0xA101, 0xA101, rc::OK, 0xA101]);
    assert!(!report.retried);
    let info = report.ext_info.unwrap();
    assert_eq!(info.version, 200);
    assert_eq!(info.controls, vec![0xD2C1, 0xD2C2]);
    cam.close().unwrap();
    t.verify().unwrap();
    // OpenSession used transaction id 1 and the drain
    // before it found an idle pipe.
    assert_eq!(t.writes()[0], encode_command(0x1002, 1, &[1]).unwrap());
    assert!(matches!(t.events()[0], MockEvent::InErr(UsbError::Timeout)));
}

#[test]
fn session_is_closed_on_drop() {
    let mut s = Script::new();
    s.open_session().no_data(0x1001, &[], rc::OK).close_session();
    let t = ScriptedTransport::new(s.build());
    {
        let mut session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
        let _ = session.transaction_vec(0x1001, &[], None).unwrap();
        assert!(session.is_open());
    }
    t.verify().unwrap();
    assert_eq!(t.writes().last().unwrap(), &encode_command(0x1003, 3, &[]).unwrap());
}

#[test]
fn camera_drop_closes_session() {
    let mut s = Script::new();
    s.open_session().handshake(&ext_info()).close_session();
    let t = ScriptedTransport::new(s.build());
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let (cam, _) = SonyCamera::connect(session, FakeClock::new(), SonyOptions::default()).unwrap();
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn drop_with_dead_camera_does_not_panic_or_hang() {
    let mut s = Script::new();
    s.open_session();
    // CloseSession is written, but nothing ever answers.
    s.raw(Step::Out(encode_command(0x1003, 2, &[]).unwrap()));
    let t = ScriptedTransport::new(s.build());
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    drop(session);
    t.verify().unwrap();
}

#[test]
fn explicit_close_then_no_second_close() {
    let mut s = Script::new();
    s.open_session().close_session();
    let t = ScriptedTransport::new(s.build());
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let (r, transport) = session.close_into_transport();
    r.unwrap();
    assert!(transport.is_some());
    t.verify().unwrap();
    assert_eq!(t.writes().len(), 2);
}

#[test]
fn session_already_open_is_success() {
    let mut s = Script::new();
    s.no_data(0x1002, &[1], rc::SESSION_ALREADY_OPEN).close_session();
    let t = ScriptedTransport::new(s.build());
    Session::open(t.clone(), ConnectionConfig::default()).unwrap().close().unwrap();
    t.verify().unwrap();
}

#[test]
fn stale_in_data_is_drained_before_open() {
    let mut steps = vec![Step::In(vec![0xEE; 700]), Step::In(response(rc::OK, 77))];
    let mut s = Script::new();
    s.open_session().close_session();
    steps.extend(s.build());
    let t = ScriptedTransport::new(steps);
    Session::open(t.clone(), ConnectionConfig::default()).unwrap().close().unwrap();
    t.verify().unwrap();
}

#[test]
fn wedged_session_resets_and_retries_once() {
    // First transport: OpenSession times out (previous host never closed).
    let wedged = ScriptedTransport::new([Step::Out(encode_command(0x1002, 1, &[1]).unwrap()), Step::InErr(UsbError::Timeout)]);
    let mut s = Script::new();
    s.open_session().close_session();
    let fresh = ScriptedTransport::new(s.build());
    let mut pool = vec![fresh.clone(), wedged.clone()];
    let clock = FakeClock::new();
    let session = Session::open_with_recovery(|| Ok(pool.pop().unwrap()), ConnectionConfig::default(), &clock).unwrap();
    assert_eq!(wedged.reset_count(), 1);
    assert_eq!(clock.total_slept().as_millis(), 750);
    session.close().unwrap();
    wedged.verify().unwrap();
    fresh.verify().unwrap();
}

#[test]
fn wedged_twice_gives_up() {
    let mk = || ScriptedTransport::new([Step::Out(encode_command(0x1002, 1, &[1]).unwrap()), Step::InErr(UsbError::Timeout)]);
    let mut pool = vec![mk(), mk(), mk()];
    let clock = FakeClock::new();
    let r = Session::open_with_recovery(|| Ok(pool.pop().unwrap()), ConnectionConfig::default(), &clock);
    assert!(r.is_err());
    assert_eq!(pool.len(), 1, "exactly one retry");
}

#[test]
fn handshake_retries_once_when_ext_info_fails() {
    let mut s = Script::new();
    s.open_session()
        .data_in(0x9201, &[1, 0, 0], None, 0xA101)
        .data_in(0x9201, &[2, 0, 0], None, 0xA101)
        .data_in(0x9202, &[0xC8], None, rc::GENERAL_ERROR)
        .data_in(0x9201, &[3, 0, 0], None, 0xA101)
        .handshake(&ext_info())
        .close_session();
    let t = ScriptedTransport::new(s.build());
    let clock = FakeClock::new();
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let (cam, report) = SonyCamera::connect(session, clock.clone(), SonyOptions::default()).unwrap();
    assert!(report.retried);
    assert!(report.ext_info.is_some());
    assert_eq!(clock.total_slept().as_millis(), 500);
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn device_info_parses_over_the_wire() {
    let mut info = Vec::new();
    info.extend_from_slice(&100u16.to_le_bytes());
    info.extend_from_slice(&0x11u32.to_le_bytes());
    info.extend_from_slice(&100u16.to_le_bytes());
    info.push(0);
    info.extend_from_slice(&0u16.to_le_bytes());
    info.extend_from_slice(&1u32.to_le_bytes());
    info.extend_from_slice(&0x9201u16.to_le_bytes());
    let mut s = Script::new();
    s.open_session().data_in(0x1001, &[], Some(&info), rc::OK).close_session();
    let t = ScriptedTransport::new(s.build());
    let mut session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let d = session.device_info().unwrap();
    assert!(d.supports(0x9201));
    assert_eq!(d.model, "");
    drop(session);
    t.verify().unwrap();
}

#[test]
fn closed_session_rejects_transactions() {
    let mut s = Script::new();
    s.open_session().close_session();
    let t = ScriptedTransport::new(s.build());
    let session = Session::open(t.clone(), ConnectionConfig::default()).unwrap();
    let (r, _t) = session.close_into_transport();
    r.unwrap();
    // A fresh session object over a dead transport fails cleanly.
    let mut s2 = Script::new();
    s2.raw(Step::Out(encode_command(0x1002, 1, &[1]).unwrap()));
    let t2 = ScriptedTransport::new(s2.build());
    assert!(matches!(Session::open(t2, ConnectionConfig::default()), Err(PtpError::Usb(UsbError::Timeout))));
}
