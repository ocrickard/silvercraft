//! Staged live bring-up of an Epson Perfection V800/V850. **TALKS TO REAL HARDWARE.**
//!
//! ```text
//! cargo run --release -p silvercraft-epson --example v800_bringup -- --i-am-at-the-scanner status
//! cargo run ... -- --i-am-at-the-scanner recover
//! cargo run ... -- --i-am-at-the-scanner prescan OUT.tiff
//! cargo run ... -- --i-am-at-the-scanner scan --region X,Y,W,H --dpi N OUT.tiff
//! cargo run ... -- --i-am-at-the-scanner dual --region X,Y,W,H --dpi N OUT_RGB.tiff OUT_IR.tiff
//! ```
//!
//! Regions are inches from the film-area origin. Every USB transfer of every handle is logged
//! (payloads truncated to 256 bytes) to `<first output>.traffic.log`, or
//! `v800-<command>.traffic.log` in the current directory for `status` / `recover`.
//!
//! To cancel a scan, type `STOP` and Enter or press Ctrl-C (SIGTERM too): the driver aborts cleanly
//! (drain, CAN, status). Never `kill -9` mid-transfer: that wedges the firmware.
//! **Never Ctrl-C or kill this process mid-scan** — a hard kill mid-transfer can wedge the
//! scanner until it is power-cycled.

use std::fs::File;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use silvercraft_epson::{
    CancelToken, Dpi, EpsonError, FoundScanner, GrayImage, HandleOpener, NusbOpener, Phase, PrescanOptions, Region, RgbImage, ScanRequest, Session,
    SessionConfig, V800,
};
use silvercraft_usb::{NusbTransport, RecordingTransport};

/// A `Write` shared by every handle's recorder, so one log holds the whole session in order.
#[derive(Clone)]
struct SharedLog(Arc<Mutex<BufWriter<File>>>);

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_err(|_| std::io::Error::other("log poisoned"))?.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_err(|_| std::io::Error::other("log poisoned"))?.flush()
    }
}

/// Wraps every opened handle in a `RecordingTransport` writing to the shared log.
struct RecordingOpener {
    inner: NusbOpener,
    log: SharedLog,
    handles: usize,
}

impl HandleOpener for RecordingOpener {
    type Handle = RecordingTransport<NusbTransport, SharedLog>;
    fn open(&mut self) -> Result<Self::Handle, EpsonError> {
        self.handles += 1;
        let _ = writeln!(self.log, "# --- open handle {} ---", self.handles);
        let t = self.inner.open()?;
        Ok(RecordingTransport::new(t, self.log.clone()).with_payload_limit(1024))
    }
    fn enumeration_id(&self) -> Option<String> {
        self.inner.enumeration_id()
    }
}

const USAGE: &str = "usage: v800_bringup --i-am-at-the-scanner <status | recover | prescan OUT.tiff | \
scan --region X,Y,W,H --dpi N OUT.tiff | dual --region X,Y,W,H --dpi N OUT_RGB.tiff OUT_IR.tiff>";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--i-am-at-the-scanner") {
        eprintln!("{USAGE}");
        eprintln!("refusing to run: this talks to a real Epson V800/V850. Pass --i-am-at-the-scanner only when someone is at the scanner.");
        std::process::exit(2);
    }
    let args: Vec<String> = args.into_iter().filter(|a| a != "--i-am-at-the-scanner").collect();
    eprintln!("==================================================================================");
    eprintln!(" WARNING: LIVE HARDWARE. This drives a real Epson V800/V850 scanner.");
    eprintln!(" Someone must be at the scanner, ready to power-cycle it.");
    eprintln!(" Cancel with STOP + Enter or Ctrl-C (both cancel cleanly). Never kill -9 mid-scan.");
    eprintln!("==================================================================================");
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn parse_region(s: &str) -> Result<Region, String> {
    let v: Vec<f64> = s.split(',').map(|p| p.trim().parse::<f64>().map_err(|e| format!("bad region {s:?}: {e}"))).collect::<Result<_, _>>()?;
    let [x, y, w, h] = v[..] else { return Err(format!("region needs X,Y,W,H, got {s:?}")) };
    Region::new(x, y, w, h).map_err(|e| e.to_string())
}

struct Opts {
    region: Option<Region>,
    dpi: Option<Dpi>,
    positional: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { region: None, dpi: None, positional: Vec::new() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--region" => o.region = Some(parse_region(it.next().ok_or("--region needs a value")?)?),
            "--dpi" => {
                let n: u32 = it.next().ok_or("--dpi needs a value")?.parse().map_err(|e| format!("bad dpi: {e}"))?;
                o.dpi = Some(Dpi::new(n).map_err(|e| e.to_string())?);
            }
            other => o.positional.push(other.to_string()),
        }
    }
    Ok(o)
}

fn session_for(found: &FoundScanner, log_path: &Path) -> Result<Session<RecordingOpener>, String> {
    let file = File::create(log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let log = SharedLog(Arc::new(Mutex::new(BufWriter::new(file))));
    println!("traffic log: {}", log_path.display());
    V800::open(found, SessionConfig::default()).map_err(|e| e.to_string())?; // platform check only
    Ok(Session::with_opener(RecordingOpener { inner: NusbOpener::new(found.clone()), log, handles: 0 }, SessionConfig::default()))
}

fn stop_watcher() -> CancelToken {
    let token = CancelToken::new();
    // A hard kill mid-transfer wedges the firmware (power cycle needed): Ctrl-C and SIGTERM only
    // request the same clean cancel as STOP, and the process exits once the driver has finished it.
    let sig = token.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        eprintln!("signal received: cancelling cleanly (the scanner is drained and sent CAN; this can take a few seconds)...");
        sig.cancel();
    }) {
        eprintln!("warning: could not install the Ctrl-C handler ({e}); use STOP + Enter to cancel");
    }
    let t = token.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) if l.trim().eq_ignore_ascii_case("stop") => {
                    eprintln!("STOP received: cancelling cleanly...");
                    t.cancel();
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    token
}

fn progress() -> impl FnMut(Phase) {
    let mut last_pct = u64::MAX;
    move |p| match p {
        Phase::Reading { pass, bytes_done, bytes_total, tile, tiles } => {
            let pct = bytes_done * 100 / bytes_total.max(1);
            if pct != last_pct && pct % 5 == 0 {
                last_pct = pct;
                eprintln!("  {pass:?} tile {}/{tiles}: {pct}% ({bytes_done}/{bytes_total} bytes)", tile + 1);
            }
        }
        other => {
            last_pct = u64::MAX;
            eprintln!("  {other:?}");
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().ok_or(USAGE)?.clone();
    let opts = parse_opts(&args[1..])?;
    let found = V800::discover().map_err(|e| e.to_string())?;
    println!("found {} scanner(s)", found.len());
    for f in &found {
        println!("  {} product={:?} serial={:?}", f.id(), f.product(), f.serial());
    }
    let dev = found.into_iter().next().ok_or("no Epson V800/V850 (04b8:0151) found")?;
    let log_for = |out: Option<&String>| -> PathBuf {
        match out {
            Some(o) => PathBuf::from(format!("{o}.traffic.log")),
            None => PathBuf::from(format!("v800-{cmd}.traffic.log")),
        }
    };
    match cmd.as_str() {
        "status" => {
            // Sends exactly `1B 46` and reads 4 bytes on one fresh handle.
            let mut s = session_for(&dev, &log_for(None))?;
            let r = s.status().map_err(|e| e.to_string())?;
            println!("status {} clean_idle={}", r.status.hex(), r.clean_idle);
        }
        "recover" => {
            let mut s = session_for(&dev, &log_for(None))?;
            let r = s.recover().map_err(|e| e.to_string())?;
            println!("recovery: {:?} final {:02x?} drained {} bytes", r.class, r.final_status, r.drained_bytes);
            for st in &r.steps {
                println!(
                    "  {:<44} {:<14} {:02x?} {}",
                    st.name,
                    st.command,
                    &st.response[..st.response.len().min(16)],
                    st.error.as_deref().unwrap_or("")
                );
            }
        }
        "prescan" => {
            let out = opts.positional.first().ok_or("prescan needs OUT.tiff")?;
            let mut s = session_for(&dev, &log_for(Some(out)))?;
            let cancel = stop_watcher();
            let p = s.prescan(&PrescanOptions::default(), &cancel, &mut progress()).map_err(|e| e.to_string())?;
            write_rgb(Path::new(out), &p.image)?;
            println!("prescan {}x{} @ {} dpi -> {out}; calibration {:?}", p.image.width(), p.image.height(), p.dpi.get(), s.calibration());
            print_passes(&p.passes);
        }
        "scan" => {
            let out = opts.positional.first().ok_or("scan needs OUT.tiff")?;
            let (region, dpi) = (opts.region.ok_or("scan needs --region")?, opts.dpi.ok_or("scan needs --dpi")?);
            let mut s = session_for(&dev, &log_for(Some(out)))?;
            let cancel = stop_watcher();
            // auto_calibrate: a targeted scan on a carriage this process has not calibrated runs
            // the origin prescan first (a fresh process never assumes calibration).
            let r = ScanRequest { region, dpi, auto_calibrate: true };
            let scan = s.scan_visible(&r, &cancel, &mut progress()).map_err(|e| e.to_string())?;
            write_rgb(Path::new(out), &scan.image)?;
            println!("scan {}x{} tiles={} postflight={} -> {out}", scan.image.width(), scan.image.height(), scan.tiles, scan.postflight.reason);
            print_passes(&scan.passes);
        }
        "dual" => {
            let (rgb_out, ir_out) = match &opts.positional[..] {
                [a, b, ..] => (a.clone(), b.clone()),
                _ => return Err("dual needs OUT_RGB.tiff OUT_IR.tiff".into()),
            };
            let (region, dpi) = (opts.region.ok_or("dual needs --region")?, opts.dpi.ok_or("dual needs --dpi")?);
            let mut s = session_for(&dev, &log_for(Some(&rgb_out)))?;
            let cancel = stop_watcher();
            let r = ScanRequest { region, dpi, auto_calibrate: true };
            let d = s.scan_dual_ir(&r, &cancel, &mut progress()).map_err(|e| e.to_string())?;
            write_rgb(Path::new(&rgb_out), &d.rgb)?;
            write_gray(Path::new(&ir_out), &d.ir)?;
            println!(
                "dual {}x{} tiles={} ir_offset={:?} seams={:?} postflight={} -> {rgb_out}, {ir_out}",
                d.rgb.width(),
                d.rgb.height(),
                d.tiles,
                d.registration.ir_offset,
                d.registration.seams,
                d.postflight.reason
            );
            print_passes(&d.passes);
        }
        other => return Err(format!("unknown command {other:?}\n{USAGE}")),
    }
    Ok(())
}

fn print_passes(passes: &[silvercraft_epson::PassMeta]) {
    for p in passes {
        println!(
            "  tile {} {:?}: area {:?} polls {} chunks {} nonzero-trailers {} raster {}/{} start {:02x?} pre-start {:02x?}",
            p.tile, p.pass, p.area, p.poll_attempts, p.chunks, p.nonzero_trailers, p.raster_bytes, p.expected_bytes, p.start_response, p.pre_start
        );
    }
}

fn write_rgb(path: &Path, img: &RgbImage) -> Result<(), String> {
    let f = BufWriter::new(File::create(path).map_err(|e| format!("{}: {e}", path.display()))?);
    let mut enc = tiff::encoder::TiffEncoder::new(f).map_err(|e| e.to_string())?;
    enc.write_image::<tiff::encoder::colortype::RGB16>(img.width() as u32, img.height() as u32, img.data()).map_err(|e| e.to_string())
}

fn write_gray(path: &Path, img: &GrayImage) -> Result<(), String> {
    let f = BufWriter::new(File::create(path).map_err(|e| format!("{}: {e}", path.display()))?);
    let mut enc = tiff::encoder::TiffEncoder::new(f).map_err(|e| e.to_string())?;
    enc.write_image::<tiff::encoder::colortype::Gray16>(img.width() as u32, img.height() as u32, img.data()).map_err(|e| e.to_string())
}
