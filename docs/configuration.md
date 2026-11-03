# Pipeline configuration

One TOML document controls every stage. `silvercraft-cli config docs` lists every key with its
meaning and default; `silvercraft-cli config show --stock ID` prints the effective config with all
layers applied.

```toml
[decode]              # capture decoding: demosaic, camera matrix, orange-mask level, infrared
[film.base]           # film-base measurement: gap / auto-band / rows / content / explicit
[film.layer_gamma]    # per-roll layer-gamma measurement
[film.calibration]    # roll density matrix and per-channel curve: neutral selection, ridge
                      # weights, robust fitting, acceptance and trust thresholds, fallbacks
[film.strip_balance]  # per-strip balance for multi-strip flatbed scans
[film.scene]          # scene-linear transform: G_ref, mid grey, mid-grey estimator, highlight cap
[film.exposure]       # per-frame exposure: auto / roll / fixed
[film.white_balance]  # per-frame residual white balance (written as AsShotNeutral, never baked)
[film.positive]       # slides: mode, white point
[output]              # DNG sample format, folders and file names (templates), XMP placement
[handoff]             # sending to LightCraft: route, port, album, keywords, preset
```

## Layers

Later layers win, key by key:

1. built-in defaults
2. camera-scan defaults (camera raws only: `film.base.method = "auto-band"`)
3. the stock profile's `[processing]` table
4. the user config file: `$SILVERCRAFT_CONFIG`, else `~/.config/silvercraft/config.toml`
   (macOS `~/Library/Application Support/SilverCraft/config.toml`, Windows
   `%APPDATA%\SilverCraft\config.toml`)
5. `--config FILE` (repeatable, in order)
6. `--set key=value` (repeatable), e.g. `--set film.exposure.mode=roll`

Unknown keys and wrong types are errors that name the layer they came from. Every roll records the
resolved config and its layers in `roll-model.json` and in each DNG's `sc:provenance`, so a result
can be reproduced exactly.

## Templates

`output.dir`, `output.file_name`, `handoff.album`, `handoff.keywords` and `handoff.preset` expand
`{roll}`, `{frame}`, `{stock}` (profile id) and `{stock_name}`.

## Example: send every roll to LightCraft as half-float DNGs

```toml
[output]
samples = "half"
dir = "/home/me/Scans/{roll}"   # no ~ expansion: use a full path

[handoff]
send = true
album = "Film/{roll}"
keywords = ["roll:{roll}", "film:{stock_name}", "scanned"]
```
