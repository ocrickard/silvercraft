# Film stock profiles

Each film stock is one TOML file. SilverCraft ships 23 (`crates/film/profiles/`) and loads more
from disk, so anyone can add a stock without rebuilding.

## Where profiles load from

Later sources override earlier ones with the same `id` (a note says which file won):

1. built-in profiles
2. `$SILVERCRAFT_PROFILES` (a path list)
3. the user folder: `~/.config/silvercraft/profiles/` (Linux), `~/Library/Application
   Support/SilverCraft/profiles/` (macOS), `%APPDATA%\SilverCraft\profiles\` (Windows)
4. `--profiles DIR` on the command line

A broken file is reported and skipped; the others still load.

## Writing one

```sh
silvercraft-cli profile new color-negative lomography-color-400 -o lomography-color-400.toml
# edit it
silvercraft-cli profile check lomography-color-400.toml
silvercraft-cli develop --profiles . --stock lomography-color-400 scans/*.tif
```

```toml
schema = "silvercraft/profile/1"
id = "kodak-portra-400"          # lower-case letters, digits and dashes; unique
name = "Kodak Portra 400"
manufacturer = "Kodak"
kind = "color-negative"          # color-negative | bw-negative | color-positive | bw-positive
process = "C-41"                 # free text: C-41, ECN-2, E-6, BW …
capture = "any"                  # any | camera | scanner (a variant tuned for one capture type)
author = "Your Name"
source = "Kodak datasheet E-4050 (2016)"
license = "CC0-1.0"
notes = "How the numbers were obtained."

[negative]                       # required for negatives; not allowed for positives
scene_gamma = 0.55               # mid-scale gradient of the green layer (B&W: the film's gradient)
layer_gamma = [0.57, 0.55, 0.62] # R, G, B layer gradients; SilverCraft equalises them (gain = γG / γc)
# dmin = [0.80, 0.52, 0.31]      # film-base transmission per channel, if you measured it
# low_percentile, high_percentile, gray_percentile, neutralize: preview and calibration knobs

[positive]                       # required for slides; not allowed for negatives
mode = "as-is"                   # as-is: the slide's own rendering | unrender: undo its gamma
gamma = 2.6                      # slide gamma, used by unrender

[characterization]               # optional datasheet anchors, kept verbatim
[look]                           # optional: this stock's look as a LightCraft preset (below)
[processing]                     # optional pipeline overrides for this stock (keys as in
                                 # `[film.*]` of a pipeline config, without the `film.` prefix)
```

Unknown keys are errors, so a misspelt field never silently does nothing.

### Where the numbers come from

- **`scene_gamma` / `layer_gamma`**: the slope of each layer's characteristic curve (density vs
  log exposure) in its straight section, from the manufacturer's datasheet. Datasheets list
  layers blue, green, red; the file wants **red, green, blue**.
- Leave `layer_gamma` out when you don't know it: the roll calibration measures the layer
  balance from the scans themselves, and the profile only gives it a starting point.
- **Slides**: `as-is` is right for most uses (a slide is already a finished rendering).

### Looks

A developed frame is a neutral scene-linear capture; the stock's character is a LightCraft preset
("SilverCraft Film Stocks" ▸ the stock's name) installed and applied when the roll is sent to
LightCraft (`handoff.stock_look`, on by default), so it stays editable and removable there. The
preset is also written beside the roll as `<stock>.lcpreset` for importing by hand.

```toml
[look.print]             # print-look values: mapped to LightCraft sliders
paper_grade = 0.82       # print contrast: < 1 softer, > 1 harder      → Contrast
saturation = 1.18        # colour saturation multiplier                → Saturation
paper_black = 0.0        # lifted print black                          → Blacks
exposure = 1.0           # print exposure multiplier                   → Exposure

[look.lightcraft]        # LightCraft develop settings, merged verbatim (as in a .lcpreset)
grain = { amount = 18.0, size = 25.0 }
color = { vibrance = 6.0 }
```

B&W stocks get the black-and-white treatment.

## How SilverCraft uses a profile

Negatives are inverted in density and converted to scene-linear light with
`L = k (10^(e / scene_gamma) − 1)` (base → 0, roll mid-grey → 0.18). The layer gammas anchor the
roll calibration. Nothing about the stock's "look" is baked in: that belongs to LightCraft.
