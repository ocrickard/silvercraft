//! Turn a bring-up traffic log (`v800_bringup`'s `*.traffic.log`) into a commit-safe replay
//! fixture: image data out, protocol in.
//!
//! ```text
//! cargo run -p silvercraft-epson --example scrub_traffic -- IN.log OUT.log [--threshold N] [--note TEXT]...
//! ```
//!
//! Every device payload (`in` / `int`) longer than the threshold (default 512 bytes) is raster
//! data and becomes `z:<total length>` (shown bytes plus the `..+N` cut-off of a truncated
//! line), so the replay still sees the exact transfer lengths the chunk/trailer protocol
//! depends on, and none of the content. Host writes (`out`), errors, comments and short
//! device replies (status bytes, ACKs, the 14-byte FS G start/extinfo reply, the FS F / FS I /
//! ESC I / ESC f / FS S blocks, all ≤ 106 bytes on a V850) are copied verbatim.
//!
//! Chunk trailers are the last byte of a raster transfer, so they become 0x00 too; the driver
//! only counts non-zero trailers (production never validates them), and the live runs
//! recorded zero non-zero trailers, so zeros are faithful there.
//!
//! The output is checked before it is written: it must parse with `parse_recording`, and no
//! device payload in it may be longer than the threshold. `--note` lines become the header.

use std::path::Path;

use silvercraft_usb::parse_recording;

const DEFAULT_THRESHOLD: usize = 512;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        eprintln!("usage: scrub_traffic IN.log OUT.log [--threshold N] [--note TEXT]...");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut notes = Vec::new();
    let mut threshold = DEFAULT_THRESHOLD;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--threshold" => threshold = it.next().ok_or("--threshold needs a value")?.parse().map_err(|e| format!("bad threshold: {e}"))?,
            "--note" => notes.push(it.next().ok_or("--note needs a value")?.clone()),
            other => positional.push(other.to_string()),
        }
    }
    let [input, output] = &positional[..] else { return Err("need IN.log and OUT.log".into()) };
    let text = std::fs::read_to_string(input).map_err(|e| format!("{input}: {e}"))?;
    let scrubbed = scrub(&text, threshold, &notes)?;
    verify(&scrubbed.text, threshold)?;
    std::fs::write(Path::new(output), &scrubbed.text).map_err(|e| format!("{output}: {e}"))?;
    println!(
        "{input} -> {output}: {} lines, {} device payloads ({} bytes) scrubbed to zero runs, {} kept verbatim; {} bytes written",
        scrubbed.lines,
        scrubbed.scrubbed,
        scrubbed.scrubbed_bytes,
        scrubbed.kept,
        scrubbed.text.len()
    );
    Ok(())
}

struct Scrubbed {
    text: String,
    lines: usize,
    scrubbed: usize,
    scrubbed_bytes: usize,
    kept: usize,
}

/// Total length of a payload field (`-`, `z:LEN`, `HEX` or truncated `HEX..+N`) and whether
/// it was truncated.
fn payload_len(p: &str, line_no: usize) -> Result<(usize, bool), String> {
    if p == "-" {
        return Ok((0, false));
    }
    if let Some(n) = p.strip_prefix("z:") {
        return n.parse().map(|n| (n, false)).map_err(|e| format!("line {line_no}: bad zero run {p:?}: {e}"));
    }
    let (shown, cut) = match p.split_once("..+") {
        Some((s, n)) => (s, n.parse::<usize>().map_err(|e| format!("line {line_no}: bad truncation {n:?}: {e}"))?),
        None => (p, 0),
    };
    if !shown.len().is_multiple_of(2) || !shown.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("line {line_no}: payload is not hex"));
    }
    Ok((shown.len() / 2 + cut, cut > 0))
}

fn scrub(text: &str, threshold: usize, notes: &[String]) -> Result<Scrubbed, String> {
    let mut out = String::new();
    for n in notes {
        out.push_str(&format!("# {}\n", n.replace('\n', " ")));
    }
    let mut body = String::new();
    let (mut lines, mut scrubbed, mut scrubbed_bytes, mut kept) = (0, 0, 0, 0);
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        lines += 1;
        let line = raw.trim();
        let parts: Vec<&str> = line.split_whitespace().collect();
        // (prefix kept as is, payload field) for device payloads; None for everything else.
        let device = match parts.first().copied() {
            None => None,
            Some(c) if c.starts_with('#') => None,
            Some("out" | "out-err" | "in-err" | "int-err" | "clear-halt" | "reset") => None,
            Some("in") => match &parts[..] {
                [_, req, p] => Some((format!("in {req}"), *p)),
                _ => return Err(format!("line {line_no}: malformed in line")),
            },
            Some("int") => match &parts[..] {
                [_, p] => Some(("int".to_string(), *p)),
                _ => return Err(format!("line {line_no}: malformed int line")),
            },
            Some(other) => return Err(format!("line {line_no}: unknown op {other:?}")),
        };
        match device {
            None => body.push_str(line),
            Some((head, p)) => {
                let (len, truncated) = payload_len(p, line_no)?;
                if len > threshold {
                    body.push_str(&format!("{head} z:{len}"));
                    scrubbed += 1;
                    scrubbed_bytes += len;
                } else if truncated {
                    return Err(format!("line {line_no}: short device payload ({len} bytes) was truncated when recorded; cannot keep it"));
                } else {
                    body.push_str(line);
                    kept += 1;
                }
            }
        }
        body.push('\n');
    }
    out.push_str(&format!(
        "# Scrubbed by crates/epson/examples/scrub_traffic.rs: {scrubbed} device payloads longer than {threshold} bytes \
         ({scrubbed_bytes} bytes of raster) replaced by z:LEN zero runs; host writes and short replies verbatim.\n"
    ));
    out.push_str(&body);
    Ok(Scrubbed { text: out, lines, scrubbed, scrubbed_bytes, kept })
}

/// The output parses, and no device payload in it carries more than `threshold` bytes.
fn verify(text: &str, threshold: usize) -> Result<(), String> {
    parse_recording(text).map_err(|e| format!("scrubbed log does not parse: {e}"))?;
    for (i, line) in text.lines().enumerate() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let payload = match &parts[..] {
            ["in", _, p] | ["int", p] => *p,
            _ => continue,
        };
        let hex_run = payload.bytes().take_while(u8::is_ascii_hexdigit).count();
        if hex_run > threshold * 2 {
            return Err(format!("line {}: {hex_run} hex digits of device data survived scrubbing", i + 1));
        }
    }
    Ok(())
}
