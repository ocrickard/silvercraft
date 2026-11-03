# SilverCraft

Film scanning for [LightCraft](https://github.com/storytold/lightcraft). SilverCraft drives the
scanner or camera, turns negatives into clean scene-linear positives, and hands each frame to
LightCraft, which handles editing and the library.

- **Capture**: Epson Perfection V800/V850 over USB with no vendor driver, including a separate
  infrared pass for dust; Sony Alpha tethered camera scanning with live view.
- **Develop**: film-base measurement from the gaps between frames, roll calibration (a fitted
  density matrix and per-channel curve), and inversion of colour negative, ECN-2, B&W and slide
  film to linear light, with 23 built-in stock profiles and your own as TOML files ([docs/profiles.md](docs/profiles.md)).
  Every stage is configurable ([docs/configuration.md](docs/configuration.md)).
- **Repair**: infrared dust masks, scanner row and line defects, and grain-preserving fill.
- **Handoff**: one LinearRaw DNG per frame plus XMP. LightCraft opens it as a raw: its own tone
  curve, Kelvin white balance and full editing range apply. Frames go into a roll album with the
  stock's preset, over LightCraft's control channel or its watched folder.

Status: early development; the scanner and camera paths have been run on real hardware
(Epson Perfection V850, Sony α1). See [docs/app.md](docs/app.md) for the workflow and `CLAUDE.md`
for the rules this codebase follows.

## Build and run

Needs a Rust toolchain (1.90+) and a [LightCraft](https://github.com/storytold/lightcraft)
checkout next to this one at `../lightcraft` (path dependencies), at the revision pinned
in [`siblings.json`](siblings.json). A fresh clone of this repo alone does not build —
`cargo xtask siblings` reports what is missing or drifted, with the exact clone commands.

```sh
cargo run --release -p silvercraft                       # the app
cargo run --release -p silvercraft -- --simulate-scanner # try it without a scanner
cargo run --release -p silvercraft-cli -- --help         # develop from the command line
cargo xtask ci                                            # format, lint, test (debug+release), layering
```

Start LightCraft with `lightcraft --control 7980` and developed rolls arrive in it directly:
one album per roll, keywords for roll and stock, and the stock's look as a preset.

## Layout

| crate | layer | what |
|---|---|---|
| `imgproc` | 0 | float planes, masks, filters, morphology, components |
| `usb` | 0 | pure-Rust USB transport (nusb), recording and scripted replay |
| `film` | 1 | film base, roll calibration, inversion to scene-linear, stock profiles |
| `epson` | 1 | Epson V800/V850 driver (ESC/I + B8), infrared dual pass, simulator |
| `ptp` | 1 | PTP/USB and Sony tether (live view, capture, autocapture) |
| `scanfix` | 1 | scanner line/row defect detection and repair, scan health |
| `detect` | 1 | frame detection on prescans and camera captures |
| `decode` | 2 | camera raws and scanner TIFFs to linear captures |
| `heal` | 2 | infrared dust and scratch repair |
| `handoff` | 2 | LinearRaw DNG + XMP writer, LightCraft control channel and watched folder |
| `config` | 3 | layered pipeline configuration |
| `engine` | 4 | roll pipeline, roll manifests, scanner and camera workflows, stock looks |
| `ui-egui` | 5 | the app's interface |

## License

MIT OR Apache-2.0.
