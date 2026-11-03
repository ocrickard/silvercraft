# Using SilverCraft

SilverCraft captures film, develops it into neutral scene-linear DNGs and sends each roll to
LightCraft. It is laid out like LightCraft:

- **Top bar**: the sidebar toggle and the File / Capture / View / Help menus on the left, the
  status of what you are doing in the middle (with progress along its bottom edge), and on the
  right whether developed rolls go to LightCraft (click to switch) and the keyboard shortcuts.
- **Sidebar** (Ctrl+Shift+L): the flatbed scanner and the camera, your rolls (+ starts a new
  roll name) and the film stocks (● marks the one new rolls use). Pick one to show it.
- **Canvas**: the bed preview, the live view or a roll's frames.
- **Right panel**, chosen in the tool strip at the far right: the view's settings in
  collapsible sections, its activity log, or details (Info). Click the active tool to hide it.
- **Bottom bar**: negatives as positives on or off, full-screen preview (F), and the view's
  main actions in the middle.

Both side panels can be resized by dragging their edge; widths and the sidebar are remembered.
F shows the canvas alone, Ctrl+Shift+F makes the window full screen, Esc leaves either. On macOS
the top bar is the title bar: drag it to move the window, double-click it to zoom.

## Flatbed scanner (Epson Perfection V800/V850)

1. **Connect Scanner** (bottom bar). The scanner must be on; no vendor driver is needed.
2. **Prescan Bed.** A quick 200 dpi pass over the film area. It also calibrates the carriage,
   which every targeted scan needs after the scanner is switched on.
3. **Frames.** Detected frames are drawn on the preview (shown as a positive). Drag on the film
   to add one, click to select, drag to move, Delete to remove, Renumber to order them strip by
   strip.
4. **Scan N Frames.** Choose the roll name, stock, resolution and whether to make an infrared
   pass (for dust repair) in the right panel's Scan section. Each frame is saved as a 16-bit TIFF (and `.ir.tif`) in the roll folder with its
   place recorded in `roll.json`, so an interrupted session keeps every frame it finished.

Cancel stops at a safe point. If the scanner ever asks for a power cycle, switch it off and on
and connect again: the driver refuses to continue rather than risk a hung firmware.

## Camera (Sony, PC Remote)

Connect, turn on live view (negatives are shown as positives), then capture by hand or switch on
**Autocapture**: SilverCraft fires once each time a new frame has been advanced and has settled,
and waits for the film to move before firing again. **New frame** clears that guard when it
guesses wrong. Raws are saved in the roll folder with `roll.json`.

## Rolls

Every folder with a `roll.json` in the rolls folder (default `~/Pictures/SilverCraft`), listed in
the sidebar; the canvas shows the selected roll's frames as they develop. **Develop Roll** measures the film base and fits the roll calibration across all frames, then writes one
DNG per frame into `developed/`. With **Send to LightCraft** on and LightCraft running with
`--control 7980`, the roll goes into an album with keywords and the stock's look preset; without
it, the files wait in the folder (or go to LightCraft's watched folder if one is set).

Pipeline overrides (Develop ▸ Pipeline Overrides, one `key = value` per line) apply on top of the
usual layers, for example `heal.scratches = true`; every key is listed under Film Stocks ▸
Pipeline Settings and in
[configuration.md](configuration.md).

## Film stocks

Every stock profile (built-in and yours) with where it came from (Info) and its file. **Use for
New Rolls** makes it the default. Add stocks as TOML files: see
[profiles.md](profiles.md).

## Command line

`silvercraft-cli roll DIR` develops a roll folder, `silvercraft-cli develop FILES…` loose
captures (`--ir FILE` pairs an infrared scan with one capture), `--send` hands the result to
LightCraft. `silvercraft-cli --help` lists everything.
