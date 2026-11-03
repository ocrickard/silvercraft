//! Live bring-up against a real Sony camera, recording all USB traffic:
//! `cargo run --release -p silvercraft-ptp --example sony_bringup -- OUT_DIR [--capture]`.
//! **Talks to real hardware.** Without `--capture` it only reads (device info, properties, one
//! live-view frame).

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use silvercraft_ptp::sony::{SonyCamera, SonyOptions, discover_sony_cameras};
use silvercraft_ptp::{ConnectionConfig, Session, SystemClock};
use silvercraft_usb::{NusbTransport, OpenOptions, RecordingTransport};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first().map(PathBuf::from) else {
        eprintln!("usage: sony_bringup OUT_DIR [--capture]");
        return;
    };
    if let Err(e) = run(&out, args.iter().any(|a| a == "--capture")) {
        eprintln!("error: {e}");
    }
}

fn run(out: &std::path::Path, capture: bool) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let found = discover_sony_cameras().map_err(|e| e.to_string())?;
    println!("found {} camera(s): {:?}", found.len(), found.iter().map(|d| (&d.product, &d.serial)).collect::<Vec<_>>());
    let dev = found.into_iter().next().ok_or("no Sony camera found")?;
    let log = BufWriter::new(File::create(out.join("traffic.log")).map_err(|e| e.to_string())?);
    let mut log = Some(log);
    let clock = SystemClock::new();
    let connect = || -> Result<RecordingTransport<NusbTransport, BufWriter<File>>, silvercraft_ptp::PtpError> {
        let t = NusbTransport::open(&dev, &OpenOptions::default())?;
        let w = match log.take() {
            Some(w) => w,
            None => BufWriter::new(File::create(out.join("traffic-retry.log")).map_err(|e| silvercraft_ptp::PtpError::Camera(e.to_string()))?),
        };
        Ok(RecordingTransport::new(t, w).with_payload_limit(4096))
    };
    let session = Session::open_with_recovery(connect, ConnectionConfig::default(), &clock).map_err(|e| e.to_string())?;
    let (mut cam, report) = SonyCamera::connect(session, clock, SonyOptions::default()).map_err(|e| e.to_string())?;
    println!("handshake: codes {:04x?} ext_info parsed: {}", report.response_codes, report.ext_info.is_some());
    if let Some(info) = &report.ext_info {
        println!("  ext info: {info:?}");
    } else {
        println!("  ext info raw ({} bytes): {:02x?}", report.ext_info_raw.len(), &report.ext_info_raw[..report.ext_info_raw.len().min(64)]);
    }
    let result = (|| -> Result<(), String> {
        let info = cam.device_info().map_err(|e| e.to_string())?;
        println!(
            "device: {} {} {} (ext {:#x} v{} '{}')",
            info.manufacturer, info.model, info.device_version, info.vendor_extension_id, info.vendor_extension_version, info.vendor_extension_desc
        );
        let raw = cam.all_props_raw().map_err(|e| e.to_string())?;
        std::fs::write(out.join("props-9209.bin"), &raw).map_err(|e| e.to_string())?;
        match cam.all_props() {
            Ok(p) => println!("props: method {:?}, {} properties", p.method, p.props.len()),
            Err(e) => println!("props parse failed: {e}"),
        }
        println!("object in memory: {:#06x}", cam.object_in_memory().map_err(|e| e.to_string())?);
        for i in 0..3 {
            match cam.live_view_frame() {
                Ok(f) => {
                    println!("live view {i}: {} bytes JPEG via {:?} (payload {})", f.jpeg.len(), f.source, f.payload_len);
                    std::fs::write(out.join(format!("liveview-{i}.jpg")), &f.jpeg).map_err(|e| e.to_string())?;
                }
                Err(e) => println!("live view {i} failed: {e}"),
            }
        }
        if capture {
            let mut file = File::create(out.join("capture.bin")).map_err(|e| e.to_string())?;
            let r = cam.capture(&mut file).map_err(|e| e.to_string())?;
            println!(
                "capture: {} bytes {:?} via {:?} handle {:#x} name {:?}",
                r.bytes,
                r.kind,
                r.path,
                r.handle,
                r.info.as_ref().map(|i| &i.filename)
            );
        }
        Ok(())
    })();
    let closed = cam.close().map_err(|e| e.to_string());
    println!("session closed: {closed:?}");
    result
}
