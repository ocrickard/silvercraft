# SilverCraft: contributor and agent guide

SilverCraft is a pure-Rust film-scanning app: capture (Epson V800/V850 flatbed with infrared,
Sony tethered camera scanning), film-base and roll calibration, negative inversion to a
**scene-linear positive**, dust and scanner-artifact repair, and handoff to
[LightCraft](../lightcraft) for editing and library management. It has no library and no general
develop tools of its own: LightCraft owns those.

## Rules

- **Pure Rust.** No C libraries (no libusb, SANE, libgphoto2, LibRaw, OpenCV). USB goes through
  `nusb`. Every library crate has `#![forbid(unsafe_code)]` and denies
  `unwrap/expect/panic/unimplemented/todo/unreachable` (tests may use them, see `clippy.toml`).
- **Hostile input never panics**: files, USB replies and LightCraft replies are untrusted.
- **Every feature has a test that fails without it**; every bug fix a regression test.
- **Green before done**: `cargo xtask ci` (fmt, clippy `-D warnings`, tests in debug and
  release with `--locked`, crate layering). Hosted CI (`.github/workflows/ci.yml`) runs the
  same gate against the sibling revisions pinned in `siblings.json`
  (`cargo xtask siblings --check`).
- **Layers** (`cargo xtask layers`, table in `xtask/src/main.rs`): a crate depends only on lower
  layers. L0 imgproc, usb · L1 film, scanfix, detect, epson, ptp · L2 decode, heal, handoff ·
  L3 config · L4 engine · L5 ui-egui · apps exempt.
- **LightCraft is a sibling checkout** at `../lightcraft`, used through path dependencies
  (`raw`, `color`, `meta`, `tiff`), pinned in `siblings.json` — bump the pin deliberately,
  never let it drift silently.
  Never write LightCraft's catalog or `prefs.json`; talk to it
  through its control channel, watched folder or CLI (`crates/handoff`).
- **Our XMP namespace is `sc:`** (`http://ns.silvercraft.app/sc/1.0/`). Never write our data
  under `lc:`; LightCraft replaces that namespace on every sidecar save.

## Hardware safety

The V800 firmware hangs (power cycle needed) on: a scan before the power-on origin prescan, an
area beyond 8×10 in, an IR pass after an incomplete visible pass, a host timeout shorter than the
~30 s cold start, unread trailing bytes. The driver makes these unrepresentable; do not add a
path around them. Only the production visible and dual-pass IR paths exist here; a tiled
scan path locks the firmware and is deliberately absent. Run hardware tests only with
someone at the scanner.

## Handoff contract

One LinearRaw DNG per frame (float32 by default, half optional): linear sRGB primaries, D65,
roll-level mid-grey at 0.18, film base at 0, highlights above 1 kept; peak scaled to 0.95 with
the scale in `BaselineExposure` (LightCraft treats ≥ 0.99 as clipped); per-frame residual white
balance in `AsShotNeutral`, per-frame auto-exposure added to `BaselineExposure`. No tone curve,
paper grade or look is baked in: stock looks ship as LightCraft presets.
