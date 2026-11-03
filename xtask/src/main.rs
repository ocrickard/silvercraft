//! Workspace tooling: `cargo xtask <command>`.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde_json::Value;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  ci               siblings present, fmt --check, clippy -D warnings (--locked),
                   tests (debug and release, --locked), layers; stops at the first failure
  layers           enforce the crate dependency layering (see LAYERS in xtask/src/main.rs)
  siblings         report the sibling checkout (../lightcraft) vs siblings.json
  siblings --check as above, but fail unless each is clean and at the pinned revision
";

/// Crate layers (package names without `silvercraft-`): a crate may depend only on crates in
/// lower layers. Apps and xtask are exempt.
const LAYERS: &[(&str, u8)] = &[
    ("imgproc", 0),
    ("usb", 0),
    ("film", 1),
    ("scanfix", 1),
    ("detect", 1),
    ("epson", 1),
    ("ptp", 1),
    ("decode", 2),
    ("heal", 2),
    ("handoff", 2),
    ("config", 3),
    ("engine", 4),
    ("ui-egui", 5),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("ci") => ci(),
        Some("layers") => layers(),
        Some("siblings") => siblings(args.get(1).map(String::as_str) == Some("--check")),
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

fn root() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.parent().map(PathBuf::from).unwrap_or(dir)
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    c.current_dir(root());
    c
}

fn run(mut c: Command, what: &str) -> Result<(), String> {
    eprintln!("» {what}");
    let status = c.status().map_err(|e| format!("{what}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what} failed ({status})")) }
}

fn ci() -> Result<(), String> {
    // Fail with clone instructions instead of an opaque cargo metadata error when
    // a sibling checkout is missing; warn (don't fail) on revision drift locally.
    siblings(false)?;
    let mut c = cargo();
    c.args(["fmt", "--all", "--", "--check"]);
    run(c, "cargo fmt --all -- --check")?;
    let mut c = cargo();
    c.args(["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]);
    run(c, "cargo clippy --workspace --all-targets --locked -- -D warnings")?;
    let mut c = cargo();
    c.args(["test", "--workspace", "--locked"]);
    run(c, "cargo test --workspace --locked")?;
    // Release too: overflow checks and optimisation differ from the dev profile,
    // and release is what ships. A green debug run alone proved nothing about it.
    let mut c = cargo();
    c.args(["test", "--workspace", "--release", "--locked"]);
    run(c, "cargo test --workspace --release --locked")?;
    layers()?;
    eprintln!("ci: all green");
    Ok(())
}

/// Sibling checkouts vs `siblings.json`. Loose mode (local `ci`): missing checkout
/// is an error with clone instructions, drift is a warning. `--check` (hosted CI):
/// anything but a clean tree at the pinned revision fails.
fn siblings(strict: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(root().join("siblings.json")).map_err(|e| format!("siblings.json: {e}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("siblings.json: {e}"))?;
    let mut errors = Vec::new();
    for s in doc["siblings"].as_array().into_iter().flatten() {
        let (name, dir, repo, rev) =
            (s["name"].as_str().unwrap_or("?"), s["dir"].as_str().unwrap_or(""), s["repo"].as_str().unwrap_or(""), s["rev"].as_str().unwrap_or(""));
        let path = root().join(dir);
        if !path.join(".git").exists() {
            errors.push(format!(
                "{name}: no checkout at {dir} (needed by path dependencies). Clone it at the pinned revision:\n  git clone {repo} {dir} && git -C {dir} checkout {rev}"
            ));
            continue;
        }
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let head = git(&["rev-parse", "HEAD"]).unwrap_or_default();
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        if head == rev && !dirty {
            eprintln!("siblings: {name} at pinned {} (clean)", &rev[..rev.len().min(12)]);
            continue;
        }
        let msg = format!(
            "{name}: at {}{}, pinned is {} — builds and parity tests may drift; bump siblings.json deliberately",
            if head.is_empty() { "unknown revision".to_string() } else { head[..head.len().min(12)].to_string() },
            if dirty { " (dirty working tree)" } else { "" },
            &rev[..rev.len().min(12)]
        );
        if strict {
            errors.push(msg);
        } else {
            eprintln!("siblings: warning: {msg}");
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

fn layer_of(name: &str) -> Option<u8> {
    let short = name.strip_prefix("silvercraft-")?;
    LAYERS.iter().find(|(n, _)| *n == short).map(|(_, l)| *l)
}

fn layers() -> Result<(), String> {
    let out = cargo().args(["metadata", "--format-version", "1", "--no-deps"]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let meta: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let mut errors = Vec::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        let name = pkg["name"].as_str().unwrap_or_default();
        let exempt = !name.starts_with("silvercraft-") || pkg["manifest_path"].as_str().is_some_and(|p| p.contains("/apps/"));
        if exempt {
            continue;
        }
        let Some(own) = layer_of(name) else {
            errors.push(format!("{name} is not in the LAYERS table"));
            continue;
        };
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            let dname = dep["name"].as_str().unwrap_or_default();
            if dep["kind"].as_str() == Some("dev") || !dname.starts_with("silvercraft-") {
                continue;
            }
            match layer_of(dname) {
                Some(l) if l < own => {}
                Some(l) => errors.push(format!("{name} (L{own}) depends on {dname} (L{l})")),
                None => errors.push(format!("{name} depends on {dname}, which is not in the LAYERS table")),
            }
        }
    }
    if errors.is_empty() {
        eprintln!("layers: ok");
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
