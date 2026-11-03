//! Replays the USB traffic recorded from a real Sony ILCE-1 during bring-up (serial zeroed):
//! the handshake, device info and property snapshot must parse as they did live.

use silvercraft_ptp::sony::{SonyCamera, SonyOptions};
use silvercraft_ptp::{ConnectionConfig, FakeClock, Session};
use silvercraft_usb::{ScriptedTransport, parse_recording};

#[test]
fn ilce1_handshake_device_info_and_properties_replay() {
    let steps = parse_recording(include_str!("fixtures/ilce1-handshake.log")).unwrap();
    let transport = ScriptedTransport::new(steps);
    let session = Session::open(transport, ConnectionConfig::default()).unwrap();
    let (mut cam, report) = SonyCamera::connect(session, FakeClock::new(), SonyOptions::default()).unwrap();
    assert_eq!(report.response_codes, [0x2001; 4]);
    let ext = report.ext_info.as_ref().expect("0x9202 parses");
    assert_eq!(ext.version, 200);
    assert_eq!(ext.props.len(), 25);
    let info = cam.device_info().unwrap();
    assert_eq!(info.model, "ILCE-1");
    assert_eq!(info.vendor_extension_desc, "Sony PTP Extensions");
    // the live session read the raw snapshot, then the parsed one, then ObjectInMemory
    let raw = cam.all_props_raw().unwrap();
    assert_eq!(raw.len(), 532);
    let props = cam.all_props().unwrap();
    assert_eq!(props.props.len(), 25);
    assert!(format!("{:?}", props.method).starts_with("Structured"), "{:?}", props.method);
    assert_eq!(cam.object_in_memory().unwrap(), 0);
    cam.close().unwrap();
}
