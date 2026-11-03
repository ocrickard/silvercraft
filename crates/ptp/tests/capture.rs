//! Scripted Sony capture flows: buffer path (poll ObjectInMemory, drain
//! stale RAM, download), card path (RAW+JPEG pick, not-ready retry), events,
//! live view.

mod common;

use std::time::Duration;

use common::{Script, event, ext_info, fake_arw, object_info, u32_list};
use silvercraft_ptp::codec::rc;
use silvercraft_ptp::datasets::format;
use silvercraft_ptp::sony::{CapturePath, JpegSource, ObjectKind, SonyCamera, SonyOptions};
use silvercraft_ptp::{ConnectionConfig, FakeClock, PtpError, Session};
use silvercraft_usb::{ScriptedTransport, Step};

const C001: u32 = 0xFFFF_C001;
const C002: u32 = 0xFFFF_C002;

fn camera(s: &mut Script, cfg: ConnectionConfig) -> (SonyCamera<ScriptedTransport, FakeClock>, ScriptedTransport, FakeClock) {
    s.close_session();
    let t = ScriptedTransport::new(s.build());
    let clock = FakeClock::new();
    let session = Session::open(t.clone(), cfg).unwrap();
    let (cam, _) = SonyCamera::connect(session, clock.clone(), SonyOptions::default()).unwrap();
    (cam, t, clock)
}

fn connected() -> Script {
    let mut s = Script::new();
    s.open_session().handshake(&ext_info());
    s
}

#[test]
fn buffer_capture_polls_then_downloads_multi_chunk() {
    let arw = fake_arw(9000, 1);
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK) // no card
        .object_in_memory(0) // RAM clean
        .shutter()
        .object_in_memory(0) // not ready yet
        .object_in_memory(0x8001) // ready
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 9000, "DSC00001.ARW")), rc::OK)
        .data_in_split(0x1009, &[C001], &arw, rc::OK, &[1000, 2048, 2048, 2048]);
    let (mut cam, t, clock) = camera(&mut s, ConnectionConfig { transfer_size: 2048, ..Default::default() });

    let mut file = Vec::new();
    let r = cam.capture(&mut file).unwrap();
    assert_eq!(r.path, CapturePath::Buffer);
    assert_eq!(r.kind, ObjectKind::Raw);
    assert_eq!(r.bytes, 9000);
    assert_eq!(r.handle, C001);
    assert_eq!(r.info.as_ref().unwrap().filename, "DSC00001.ARW");
    assert!(r.warning.is_none());
    assert_eq!(file, arw);
    // AF settle 1.0 + S2 hold 0.8 + one 0.2 status poll.
    assert_eq!(clock.total_slept(), Duration::from_millis(2000));
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn buffer_capture_drains_stale_backlog_first() {
    // Two stale shots in RAM must drain before the new one is taken.
    let (old1, old2, new) = (fake_arw(3000, 1), fake_arw(3000, 2), fake_arw(3000, 3));
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK)
        .object_in_memory(0x8002)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 3000, "OLD1.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&old1), rc::OK)
        .object_in_memory(0x8001)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 3000, "OLD2.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&old2), rc::OK)
        .object_in_memory(0)
        .shutter()
        .object_in_memory(0x8001)
        // Not primed yet on the first try: 0x2009, no data.
        .data_in(0x1008, &[C001], None, rc::INVALID_OBJECT_HANDLE)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 3000, "NEW.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&new), rc::OK);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let mut file = Vec::new();
    let r = cam.capture(&mut file).unwrap();
    assert_eq!(file, new, "never the stale backlog");
    assert_eq!(r.info.unwrap().filename, "NEW.ARW");
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn stuck_ram_is_reported() {
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK).object_in_memory(0x8001).data_in(0x1008, &[C001], None, rc::INVALID_OBJECT_HANDLE);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let e = cam.capture(&mut Vec::new()).unwrap_err();
    assert!(e.to_string().contains("replug"), "{e}");
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn interrupt_event_wakes_the_poll_early() {
    let arw = fake_arw(2000, 4);
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK)
        .object_in_memory(0)
        .shutter()
        .object_in_memory(0)
        .raw(Step::Interrupt(event(0xC201, &[C001])))
        .object_in_memory(0x8001)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 2000, "A.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&arw), rc::OK);
    let (mut cam, t, clock) = camera(&mut s, ConnectionConfig::default());
    cam.capture(&mut Vec::new()).unwrap();
    assert!(cam.object_event_seen());
    // Only the shutter timings: the 0.2 s status poll was cut short by the event.
    assert_eq!(clock.total_slept(), Duration::from_millis(1800));
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn card_capture_picks_raw_of_raw_plus_jpeg_and_retries_until_readable() {
    let arw = fake_arw(4000, 9);
    let storage = 0x0001_0001;
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[0, storage])), rc::OK)
        .data_in(0x1007, &[storage, 0, 0], Some(&u32_list(&[1, 2])), rc::OK)
        .shutter()
        .data_in(0x1007, &[storage, 0, 0], Some(&u32_list(&[1, 2])), rc::OK)
        .data_in(0x1007, &[storage, 0, 0], Some(&u32_list(&[1, 2, 3, 4])), rc::OK)
        .data_in(0x1008, &[3], Some(&object_info(format::SONY_ARW, 4000, "DSC00003.ARW")), rc::OK)
        .data_in(0x1008, &[4], Some(&object_info(format::EXIF_JPEG, 900, "DSC00003.JPG")), rc::OK)
        .data_in(0x1009, &[3], None, rc::DEVICE_BUSY)
        .data_in(0x1009, &[3], Some(&arw), rc::OK);
    let (mut cam, t, clock) = camera(&mut s, ConnectionConfig::default());
    let mut file = Vec::new();
    let r = cam.capture(&mut file).unwrap();
    assert_eq!(r.path, CapturePath::Card);
    assert_eq!(r.handle, 3, "the ARW, not the higher-numbered JPEG");
    assert_eq!(file, arw);
    // 1.8 s shutter + 0.25 s handle poll + 0.3 s object retry.
    assert_eq!(clock.total_slept(), Duration::from_millis(2350));
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn jpeg_capture_carries_warning() {
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    jpeg.resize(2000, 0x11);
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK)
        .object_in_memory(0)
        .shutter()
        .object_in_memory(0x8001)
        .data_in(0x1008, &[C001], Some(&object_info(format::EXIF_JPEG, 2000, "DSC.JPG")), rc::OK)
        .data_in(0x1009, &[C001], Some(&jpeg), rc::OK);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let r = cam.capture(&mut Vec::new()).unwrap();
    assert_eq!(r.kind, ObjectKind::Jpeg);
    assert!(r.warning.unwrap().contains("RAW"));
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn capture_times_out_when_shutter_never_fires() {
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK).object_in_memory(0).shutter();
    // Deadline 20 s from the start; polls at 1.8 s (after the shutter) .. 19.8 s.
    for _ in 0..91 {
        s.object_in_memory(0);
    }
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let e = cam.capture(&mut Vec::new()).unwrap_err();
    assert!(matches!(e, PtpError::Camera(ref m) if m.contains("did the shutter fire")), "{e}");
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn partial_download_is_flagged() {
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK)
        .object_in_memory(0)
        .shutter()
        .object_in_memory(0x8001)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 5000, "A.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&fake_arw(5000, 1)), rc::GENERAL_ERROR);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let e = cam.capture(&mut Vec::new()).unwrap_err();
    assert!(matches!(e, PtpError::PartialDownload { bytes: 5000, code: rc::GENERAL_ERROR }), "{e}");
    drop(cam);
    t.verify().unwrap();
}

fn liveview_payload(jpeg: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&16u32.to_le_bytes());
    p.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
    p.extend_from_slice(&[0; 8]);
    p.extend_from_slice(jpeg);
    p.extend_from_slice(b"focus\xff\xd9info");
    p
}

#[test]
fn live_view_frame_retries_empty_object_then_extracts_jpeg() {
    let jpeg = b"\xff\xd8\xff\xe0frame-bytes\xff\xd9".to_vec();
    let mut s = connected();
    s.data_in(0x1009, &[C002], Some(&[]), rc::OK) // starting up: empty
        .data_in(0x1009, &[C002], None, rc::INVALID_OBJECT_HANDLE) // no data at all
        .data_in(0x1009, &[C002], Some(&liveview_payload(&jpeg)), rc::OK)
        .data_in(0x1009, &[C002], Some(&liveview_payload(&jpeg)), rc::OK);
    let (mut cam, t, clock) = camera(&mut s, ConnectionConfig::default());
    let f = cam.live_view_frame().unwrap();
    assert_eq!(f.jpeg, jpeg);
    assert_eq!(f.source, JpegSource::Header);
    assert_eq!(f.sequence, 1);
    assert_eq!(clock.total_slept(), Duration::from_millis(100));
    assert_eq!(cam.live_view_frame().unwrap().sequence, 2);
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn live_view_gives_up_after_two_seconds() {
    let mut s = connected();
    for _ in 0..41 {
        s.data_in(0x1009, &[C002], Some(b"no jpeg"), rc::OK);
    }
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    let e = cam.live_view_frame().unwrap_err();
    assert!(e.to_string().contains("live view did not produce a JPEG"), "{e}");
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn object_in_memory_falls_back_to_byte_scan() {
    // the original fake blob layout: 15 d2, six bytes, value at +8.
    let mut blob = vec![0x15, 0xD2, 0, 0, 0, 0, 0, 0];
    blob.extend_from_slice(&0x8003u16.to_le_bytes());
    let mut s = connected();
    s.data_in(0x9209, &[0xC8], Some(&blob), rc::OK);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    assert_eq!(cam.object_in_memory().unwrap(), 0x8003);
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn set_ext_prop_sends_value_in_data_phase() {
    let mut s = connected();
    s.data_out(0x9205, &[0xD222], &[0x01, 0x00], rc::OK).data_out(0x9205, &[0xD222], &[0x10, 0x00], 0xA001);
    let (mut cam, t, _clock) = camera(&mut s, ConnectionConfig::default());
    use silvercraft_ptp::sony::props::{PropValue, datatype};
    cam.set_ext_prop(0xD222, datatype::UINT16, &PropValue::UInt(1)).unwrap();
    assert!(matches!(cam.set_ext_prop(0xD222, datatype::UINT16, &PropValue::UInt(0x10)), Err(PtpError::Response { op: 0x9205, code: 0xA001 })));
    drop(cam);
    t.verify().unwrap();
}

#[test]
fn unrelated_event_does_not_cut_the_poll_short() {
    let arw = fake_arw(2000, 5);
    let mut s = connected();
    s.data_in(0x1004, &[], Some(&u32_list(&[])), rc::OK)
        .object_in_memory(0)
        .shutter()
        .object_in_memory(0)
        .raw(Step::Interrupt(event(0xC203, &[0x5007])))
        .object_in_memory(0x8001)
        .data_in(0x1008, &[C001], Some(&object_info(format::SONY_ARW, 2000, "A.ARW")), rc::OK)
        .data_in(0x1009, &[C001], Some(&arw), rc::OK);
    let (mut cam, t, clock) = camera(&mut s, ConnectionConfig::default());
    cam.capture(&mut Vec::new()).unwrap();
    assert!(!cam.object_event_seen());
    assert_eq!(clock.total_slept(), Duration::from_millis(2000));
    drop(cam);
    t.verify().unwrap();
}
