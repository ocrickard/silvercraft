//! `silvercraft-cli`: develop film captures into scene-linear DNGs and send them to LightCraft.
//!
//! A roll is developed in two passes so only one full frame is in memory at a time: pass 1
//! decodes each capture, measures its film base in the gaps around the frame and keeps a
//! subsample; the roll calibration is fitted from those; pass 2 decodes each capture again and
//! writes its scene-linear DNG.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use silvercraft_config::{ConfigBuilder, Resolved};
use silvercraft_engine::manifest::RollManifest;
use silvercraft_engine::{ConfigSources, Event, FrameSource, RollRequest};
use silvercraft_film::diag::{Diagnostic, Severity};
use silvercraft_film::profiles::{FilmKind, FilmProfile, ProfileRegistry};

const USAGE: &str = "\
usage: silvercraft-cli <command> [options]

developing
  develop [options] FILES...       develop camera-scan raws or scanner TIFFs as one roll
      --stock ID                   film stock (default generic-c41; see `profiles`)
      --box x0,y0,x1,y1            frame box in capture pixels (default: whole capture, no gap base)
      --ir FILE                    infrared pass for the one capture (Epson dual-pass scans)
      --dpi N                      scan resolution (dust repair sizes scale with it)
  roll DIR [options]               develop a SilverCraft capture session (DIR/roll.json)
      --only LABEL,...             write only these frames again (with the roll as last fitted)
  scan DIR [options]               Epson V800/V850: prescan, find the frames, scan each one into
                                   DIR (16-bit TIFF + infrared, roll.json), then develop DIR
      --dpi N                      scan resolution (default 3200)
      --no-ir                      skip the infrared pass (no dust repair)
      --scan-only                  capture only; develop later with `roll DIR`
      --simulate                   use the built-in scanner simulator (no hardware)

film stocks
  profiles                         list stocks and where each was loaded from
  profile show ID                  print a stock's profile file
  profile check FILES...           validate profile files (for submitting new stocks)
  profile new KIND ID [-o FILE]    start a profile (KIND: color-negative, bw-negative,
                                   color-positive, bw-positive)

configuration
  config show [--stock ID]         the effective pipeline config (all layers applied), as TOML
  config docs                      every config key with its meaning and default
  config check FILES...            validate config files

options for develop and config show
  --config FILE                    pipeline config file (repeatable; later files win)
  --set KEY=VALUE                  override one key, e.g. --set film.exposure.mode=roll (repeatable)
  --profiles DIR                   load stock profiles from DIR too (repeatable)
  --roll NAME                      roll name (album and keywords in LightCraft)
  --out DIR                        = --set output.dir=DIR
  --half                           = --set output.samples=half
  --send                           = --set handoff.send=true
  --port N                         = --set handoff.port=N

Layers, later winning: defaults < camera-scan defaults < the stock's [processing] < the user
config file (~/.config/silvercraft/config.toml or $SILVERCRAFT_CONFIG) < --config < --set.
User stock profiles load from ~/.config/silvercraft/profiles, $SILVERCRAFT_PROFILES and --profiles.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("develop") => develop_cmd(rest),
        Some("roll") => roll_cmd(rest),
        Some("scan") => scan_cmd(rest),
        Some("profiles") => profiles_cmd(rest),
        Some("profile") => profile_cmd(rest),
        Some("config") => config_cmd(rest),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------------- arguments

#[derive(Default)]
struct Args {
    pos: Vec<String>,
    configs: Vec<PathBuf>,
    sets: Vec<String>,
    profile_dirs: Vec<PathBuf>,
    stock: Option<String>,
    roll: Option<String>,
    frame_box: Option<[usize; 4]>,
    frames: Option<usize>,
    only: Option<Vec<String>>,
    output: Option<PathBuf>,
    ir: Option<PathBuf>,
    dpi: Option<u32>,
    no_ir: bool,
    scan_only: bool,
    simulate: bool,
}

impl Args {
    fn sources(&self) -> ConfigSources {
        ConfigSources { profile_dirs: self.profile_dirs.clone(), configs: self.configs.clone(), sets: self.sets.clone(), ..Default::default() }
    }
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--config" => a.configs.push(value("--config")?.into()),
            "--set" => a.sets.push(value("--set")?),
            "--profiles" => a.profile_dirs.push(value("--profiles")?.into()),
            "--stock" => a.stock = Some(value("--stock")?),
            "--roll" => a.roll = Some(value("--roll")?),
            "--box" => a.frame_box = Some(parse_box(&value("--box")?)?),
            "--frames" => a.frames = Some(value("--frames")?.parse().map_err(|_| "bad --frames")?),
            "--only" => a.only = Some(value("--only")?.split(',').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()),
            "--no-ir" => a.no_ir = true,
            "--scan-only" => a.scan_only = true,
            "--simulate" => a.simulate = true,
            "--ir" => a.ir = Some(value("--ir")?.into()),
            "--dpi" => a.dpi = Some(value("--dpi")?.parse().map_err(|_| "bad --dpi")?),
            "-o" => a.output = Some(value("-o")?.into()),
            "--out" => a.sets.push(format!("output.dir={}", toml_string(&value("--out")?))),
            "--port" => a.sets.push(format!("handoff.port={}", value("--port")?)),
            "--half" => a.sets.push("output.samples=\"half\"".into()),
            "--send" => a.sets.push("handoff.send=true".into()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ => a.pos.push(arg.clone()),
        }
    }
    Ok(a)
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn parse_box(s: &str) -> Result<[usize; 4], String> {
    let v: Vec<usize> = s.split(',').map(|t| t.trim().parse().map_err(|_| format!("bad box {s}"))).collect::<Result<_, _>>()?;
    match v[..] {
        [x0, y0, x1, y1] if x1 > x0 && y1 > y0 => Ok([x0, y0, x1, y1]),
        _ => Err(format!("box must be x0,y0,x1,y1 with x1 > x0 and y1 > y0: {s}")),
    }
}

fn print_diags(diags: &[Diagnostic]) {
    for d in diags {
        eprintln!("{d}");
    }
}

fn registry(a: &Args) -> ProfileRegistry {
    let r = a.sources().registry();
    print_diags(r.diagnostics());
    r
}

/// The pipeline config for `stock` with every layer applied.
fn resolve_config(a: &Args, stock: Option<&FilmProfile>, camera_scan: bool) -> Result<Resolved, String> {
    match a.sources().resolve(stock, camera_scan) {
        Ok((resolved, warnings)) => {
            print_diags(&warnings);
            Ok(resolved)
        }
        Err(diags) => {
            print_diags(&diags);
            Err("the pipeline configuration is invalid".into())
        }
    }
}

// ---------------------------------------------------------------------------------- stocks

fn profiles_cmd(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    let r = registry(&a);
    for (i, p) in r.profiles().iter().enumerate() {
        let gamma = if p.negative.is_some() { format!("G_ref {:.2}", p.scene_gamma()) } else { String::new() };
        println!("{:28} {:16} {:7} {:10} {}", p.id, p.kind.as_str(), p.process, gamma, r.source_of(i).unwrap_or("?"));
    }
    Ok(())
}

fn profile_cmd(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    match a.pos.first().map(String::as_str) {
        Some("show") => {
            let id = a.pos.get(1).ok_or("which stock?")?;
            let r = registry(&a);
            let p = r.get(id).ok_or_else(|| format!("unknown stock `{id}`"))?;
            print!("{}", p.to_toml());
            Ok(())
        }
        Some("check") => {
            let files = a.pos.get(1..).unwrap_or_default();
            if files.is_empty() {
                return Err("which profile files?".into());
            }
            let mut bad = 0;
            for f in files {
                let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
                match FilmProfile::from_toml_str(&text, f) {
                    Ok(p) => {
                        let diags = p.validate();
                        let errors = diags.iter().filter(|d| d.severity == Severity::Error).count();
                        print_diags(&diags.into_iter().map(|d| d.with_source(f.as_str())).collect::<Vec<_>>());
                        if errors > 0 {
                            bad += 1;
                        } else {
                            println!("{f}: ok ({} {}, {})", p.id, p.kind.as_str(), p.name);
                        }
                    }
                    Err(diags) => {
                        print_diags(&diags);
                        bad += 1;
                    }
                }
            }
            if bad > 0 { Err(format!("{bad} of {} profile files have errors", files.len())) } else { Ok(()) }
        }
        Some("new") => {
            let kind = a.pos.get(1).ok_or("which kind? (color-negative, bw-negative, color-positive, bw-positive)")?;
            let id = a.pos.get(2).ok_or("which id? (lower-case, digits and dashes, e.g. kodak-ektar-100)")?;
            let kind = FilmKind::parse(kind).ok_or_else(|| format!("unknown kind `{kind}`"))?;
            let mut p = FilmProfile::template(kind);
            p.id = id.clone();
            print_diags(&p.validate());
            match &a.output {
                Some(path) => {
                    if path.exists() {
                        return Err(format!("{} exists; not overwriting", path.display()));
                    }
                    std::fs::write(path, p.to_toml()).map_err(|e| format!("{}: {e}", path.display()))?;
                    eprintln!("wrote {} — fill it in, then `silvercraft-cli profile check {}`", path.display(), path.display());
                }
                None => print!("{}", p.to_toml()),
            }
            Ok(())
        }
        _ => Err(format!("profile show|check|new\n\n{USAGE}")),
    }
}

// ---------------------------------------------------------------------------------- config

fn config_cmd(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    match a.pos.first().map(String::as_str) {
        Some("show") => {
            let r = registry(&a);
            let stock = a.stock.as_deref().map(|id| r.get(id).ok_or_else(|| format!("unknown stock `{id}`"))).transpose()?;
            let resolved = resolve_config(&a, stock, false)?;
            println!("# layers: {}", resolved.layers.join(" < "));
            print!("{}", resolved.config.to_toml());
            Ok(())
        }
        Some("docs") => {
            for d in silvercraft_config::docs::all() {
                println!("{} = {}\n    {}\n", d.path, d.default, d.doc.replace('\n', "\n    "));
            }
            Ok(())
        }
        Some("check") => {
            let files = a.pos.get(1..).unwrap_or_default();
            if files.is_empty() {
                return Err("which config files?".into());
            }
            for f in files {
                let b = ConfigBuilder::new().file(Path::new(f)).map_err(|e| e.to_string())?;
                match b.build() {
                    Ok((_, warnings)) => {
                        print_diags(&warnings);
                        println!("{f}: ok");
                    }
                    Err(diags) => {
                        print_diags(&diags);
                        return Err(format!("{f} is invalid"));
                    }
                }
            }
            Ok(())
        }
        _ => Err(format!("config show|docs|check\n\n{USAGE}")),
    }
}

// ---------------------------------------------------------------------------------- developing

fn develop_cmd(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    if a.pos.is_empty() {
        return Err(format!("no captures given\n\n{USAGE}"));
    }
    let frames = a
        .pos
        .iter()
        .map(|f| {
            let mut src = FrameSource::whole(f);
            src.frame_box = a.frame_box;
            src.ir_path = a.ir.clone();
            src.dpi = a.dpi;
            src
        })
        .collect::<Vec<_>>();
    if a.ir.is_some() && frames.len() != 1 {
        return Err("--ir pairs one infrared file with one capture".into());
    }
    let stock = a.stock.clone().unwrap_or_else(|| "generic-c41".into());
    let roll = a.roll.clone().unwrap_or_else(|| "Roll".into());
    run_roll(&a, frames, stock, roll)
}

fn roll_cmd(args: &[String]) -> Result<(), String> {
    let a = parse_args(args)?;
    let dir = PathBuf::from(a.pos.first().ok_or("which roll folder?")?);
    let m = RollManifest::load(&dir).map_err(|e| e.to_string())?;
    let mut req = m.to_request(&dir, a.sources());
    // developed frames go beside the captures unless --out / --set output.dir says otherwise
    if !req.sources.sets.iter().any(|s| s.starts_with("output.dir")) {
        req.sources.sets.insert(0, format!("output.dir={}", toml_string(&dir.join("developed").to_string_lossy())));
    }
    if let Some(s) = &a.stock {
        req.stock = s.clone();
    }
    if let Some(r) = &a.roll {
        req.roll = r.clone();
    }
    if let Some(n) = a.frames {
        req.frames.truncate(n);
    }
    if let Some(only) = &a.only
        && let Some(l) = only.iter().find(|l| req.frames.iter().all(|f| &f.label != *l))
    {
        return Err(format!("no frame {l} in {}", dir.display()));
    }
    run_request_only(req, a.only.as_deref())
}

fn scan_cmd(args: &[String]) -> Result<(), String> {
    use silvercraft_engine::scanner::{ScanJob, ScannerCommand, ScannerEvent};
    let a = parse_args(args)?;
    let dir = PathBuf::from(a.pos.first().ok_or("which roll folder?")?);
    let roll = a.roll.clone().unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Roll".into()));
    let stock = a.stock.clone().unwrap_or_else(|| "generic-c41".into());
    let factory = if a.simulate {
        silvercraft_engine::hardware::simulated_scanner(silvercraft_epson::SimScanner::default())
    } else {
        silvercraft_engine::hardware::epson_scanner()
    };
    let worker = factory(dir.join(".work"), std::sync::Arc::new(|| {}));
    // Ctrl-C cancels at a safe point instead of killing the process mid-transfer
    let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let f = cancel_flag.clone();
        let _ = ctrlc::set_handler(move || f.store(true, std::sync::atomic::Ordering::SeqCst));
    }
    // run one command to its Idle, printing progress; returns its events
    let run = |cmd: ScannerCommand| -> Result<Vec<ScannerEvent>, String> {
        worker.send(cmd);
        let mut out = Vec::new();
        let mut last = String::new();
        loop {
            if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
                worker.cancel();
            }
            let Some(e) = worker.recv() else { return Err("the scanner worker stopped".into()) };
            match &e {
                ScannerEvent::Phase(p) => {
                    let line = format!("{p:?}");
                    let key: String = line.chars().take_while(|c| *c != '{').collect();
                    if key != last {
                        eprintln!("  {line}");
                        last = key;
                    }
                }
                ScannerEvent::Failed { message, power_cycle } => {
                    return Err(if *power_cycle { format!("{message} — switch the scanner off and on") } else { message.clone() });
                }
                ScannerEvent::Cancelled => return Err("cancelled".into()),
                ScannerEvent::Idle => {
                    out.push(e);
                    return Ok(out);
                }
                _ => {}
            }
            out.push(e);
        }
    };
    match worker.recv() {
        Some(ScannerEvent::Connected) => eprintln!("scanner connected"),
        Some(ScannerEvent::Failed { message, .. }) => return Err(message),
        other => return Err(format!("unexpected {other:?}")),
    }
    eprintln!("prescan…");
    let ev = run(ScannerCommand::Prescan(silvercraft_epson::session::PrescanOptions::default()))?;
    let preview =
        ev.iter().find_map(|e| if let ScannerEvent::Preview { preview, .. } = e { Some(preview.clone()) } else { None }).ok_or("no preview")?;
    let resolved = a.sources().resolve(None, false).map_err(|d| d.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))?.0;
    let (frames, warnings) = silvercraft_engine::frames::bed_frames(&preview, &resolved.config.detect);
    for w in warnings {
        eprintln!("frame detection: {w}");
    }
    if frames.is_empty() {
        return Err("no frames found on the bed (see the prescan in DIR/.work/prescan.tif)".into());
    }
    eprintln!("found {} frames", frames.len());
    let job = ScanJob {
        frames,
        preview_size: (preview.image.width() as u32, preview.image.height() as u32),
        preview_dpi: preview.dpi.get(),
        dpi: a.dpi.unwrap_or(3200),
        infrared: !a.no_ir,
        padding_mm: 1.5,
        dir: dir.clone(),
        roll,
        stock,
    };
    let ev = run(ScannerCommand::Scan(job))?;
    for e in &ev {
        if let ScannerEvent::FrameScanned { index, total, frame, .. } = e {
            eprintln!("scanned {}/{total}: {}", index + 1, frame.capture.display());
        }
    }
    drop(worker);
    if a.scan_only {
        return Ok(());
    }
    roll_cmd(args)
}

fn run_roll(a: &Args, frames: Vec<FrameSource>, stock: String, roll: String) -> Result<(), String> {
    run_request(RollRequest { roll, stock, frames, sources: a.sources() })
}

fn run_request(req: RollRequest) -> Result<(), String> {
    run_request_only(req, None)
}

fn run_request_only(req: RollRequest, only: Option<&[String]>) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let mut print = |e: Event| match e {
        Event::Diagnostic(d) => eprintln!("{d}"),
        Event::Config { layers } => eprintln!("config layers: {}", layers.join(" < ")),
        Event::Measured { index, total, label, capture, frame, gap_base } => eprintln!(
            "[1/2] {:>3}/{total} {label}  {}×{} → frame {}×{}  base {}",
            index + 1,
            capture.0,
            capture.1,
            frame.0,
            frame.1,
            gap_base.map(|b| format!("{:.4} {:.4} {:.4}", b[0], b[1], b[2])).unwrap_or_else(|| "—".into())
        ),
        Event::Fitted { summary } => eprintln!("{summary}"),
        Event::Developed { index, total, label, exposure_ev, neutral, clipped_fraction, path, .. } => eprintln!(
            "[2/2] {:>3}/{total} {label}  EV {exposure_ev:+.2}  neutral {:.3} {:.3} {:.3}  clipped {:.4}%  → {}",
            index + 1,
            neutral[0],
            neutral[1],
            neutral[2],
            clipped_fraction * 100.0,
            path.display()
        ),
        Event::Finished { frames, seconds } => eprintln!("developed {frames} frames in {seconds:.1} s"),
        Event::Delivered { message } => eprintln!("{message}"),
        Event::ScanChecked { label, lines_repaired, banding_repaired, warnings } => {
            if lines_repaired > 0 || banding_repaired {
                eprintln!("      {label}  scanner fixes: {lines_repaired} line defects{}", if banding_repaired { ", banding" } else { "" });
            }
            for w in warnings {
                eprintln!("      {label}  {w}");
            }
        }
        Event::Healed { label, coverage_pct, defects, line_scratches, ir_shift, warnings } => {
            let shift = ir_shift.map(|(x, y)| format!(" · infrared shift {x:+.2},{y:+.2}")).unwrap_or_default();
            let lines = if line_scratches > 0 { format!(" · {line_scratches} long scratches") } else { String::new() };
            eprintln!("      {label}  repaired {defects} defects ({coverage_pct:.2}% of the capture){lines}{shift}");
            for w in warnings {
                eprintln!("      {label}  {w}");
            }
        }
        other => eprintln!("{other:?}"),
    };
    silvercraft_engine::develop_roll_frames(&req, only, &cancel, &mut print).map(|_| ()).map_err(|e| e.to_string())
}
