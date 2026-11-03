//! Field documentation for `silvercraft-cli config docs`: every key of a pipeline config with
//! its meaning and default.

use silvercraft_film::FilmConfig;

use crate::PipelineConfig;

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDoc {
    /// Dotted path from the document root, e.g. `film.scene.mid_grey`.
    pub path: String,
    pub doc: String,
    /// Default rendered as TOML.
    pub default: String,
}

/// Docs for the sections outside the film crate (which documents its own fields).
const OTHER: &[(&str, &str)] = &[
    ("decode.demosaic", "Demosaic for camera-scan raws: \"ahd\" (sharpest), \"ppg\", or \"bilinear\" (fastest)."),
    (
        "decode.camera_matrix",
        "Camera colour matrix: \"none\" measures density in camera-native channels (the roll's density matrix absorbs crosstalk), \"file\" uses a DNG camera scan's own matrix, \"custom\" uses decode.xyz_to_camera.",
    ),
    ("decode.xyz_to_camera", "XYZ (D65) → camera matrix, row-major, for camera_matrix = \"custom\"."),
    ("decode.mask_percentile", "Percentile of each channel taken as the orange-mask level in the mask-aware conversion (50..100)."),
    ("decode.infrared", "Infrared plane of RGBI scans: \"use\" it for dust repair, or \"ignore\" it."),
    ("output.samples", "DNG samples: \"float\" (32-bit, archival) or \"half\" (16-bit float, about half the size)."),
    ("output.dir", "Folder for developed frames (template: {roll} {frame} {stock} {stock_name})."),
    ("output.file_name", "File name of each frame without extension (template; slugged)."),
    ("output.sidecar", "Write <name>.xmp beside each DNG (LightCraft reads it on import)."),
    ("output.embed_xmp", "Embed the same XMP packet in the DNG."),
    ("output.roll_model", "Write the fitted roll model (roll-model.json) beside the frames."),
    ("handoff.send", "Send developed frames to LightCraft."),
    ("handoff.route", "\"auto\" (control channel, else watched folder, else leave the files), \"control\", \"watched-folder\", or \"none\"."),
    ("handoff.port", "LightCraft control port (start LightCraft with --control <port>)."),
    ("handoff.library", "LightCraft library folder, for finding its watched folder; empty = $LIGHTCRAFT_LIBRARY or the default."),
    ("handoff.album", "Album the roll goes into (template); empty = none."),
    ("handoff.keywords", "Keywords added on import (templates)."),
    ("handoff.preset", "LightCraft preset id applied on import (template); empty = none."),
    (
        "handoff.stock_look",
        "When handoff.preset is empty, install the stock's look (from its profile's [look]) as a LightCraft preset in \"SilverCraft Film Stocks\" and apply it on import.",
    ),
    ("handoff.copy", "Copy into LightCraft's library folders instead of adding the files where they are."),
    ("handoff.reveal", "Open the first frame in LightCraft's Develop view."),
];

/// The default of `path` (a dotted path into the defaults document) as TOML.
fn default_of(defaults: &toml::Table, path: &str) -> String {
    let mut cur: Option<&toml::Value> = None;
    let mut table = Some(defaults);
    for part in path.split('.') {
        cur = table.and_then(|t| t.get(part));
        table = cur.and_then(toml::Value::as_table);
    }
    match cur {
        Some(v) => v.to_string(),
        None => "unset".into(),
    }
}

/// Every documented field, film sections included.
pub fn all() -> Vec<FieldDoc> {
    let defaults = toml::Table::try_from(PipelineConfig::default()).unwrap_or_default();
    let mut out: Vec<FieldDoc> =
        OTHER.iter().map(|(path, doc)| FieldDoc { path: (*path).into(), doc: (*doc).into(), default: default_of(&defaults, path) }).collect();
    let film = FilmConfig::field_docs().into_iter().map(|d| FieldDoc { path: format!("film.{}", d.path), doc: d.doc, default: d.default });
    // decode first, then film (the pipeline order), then output and handoff
    let split = out.iter().position(|d| !d.path.starts_with("decode.")).unwrap_or(out.len());
    let tail = out.split_off(split);
    out.extend(film);
    out.extend(silvercraft_detect::DetectConfig::field_docs().into_iter().map(|d| FieldDoc {
        path: format!("detect.{}", d.path),
        doc: d.doc,
        default: d.default,
    }));
    out.extend(silvercraft_scanfix::ScanfixConfig::field_docs().into_iter().map(|d| FieldDoc {
        path: format!("scanfix.{}", d.path),
        doc: d.doc,
        default: d.default,
    }));
    out.extend(silvercraft_heal::HealConfig::field_docs().into_iter().map(|d| FieldDoc { path: d.path, doc: d.doc, default: d.default }));
    out.extend(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every leaf of the default config is documented, and every documented path exists.
    #[test]
    fn docs_cover_the_config_exactly() {
        fn leaves(prefix: &str, t: &toml::Table, out: &mut Vec<String>) {
            for (k, v) in t {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match v {
                    toml::Value::Table(sub) => leaves(&p, sub, out),
                    _ => out.push(p),
                }
            }
        }
        let defaults = toml::Table::try_from(PipelineConfig::default()).unwrap();
        let mut keys = Vec::new();
        leaves("", &defaults, &mut keys);
        let docs = all();
        let documented: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
        for k in &keys {
            // a tagged table (e.g. detect.bed.holder) is documented as a whole
            let covered = documented.iter().any(|d| k == d || k.starts_with(&format!("{d}.")));
            assert!(covered, "{k} is not documented");
        }
        for d in &docs {
            assert!(!d.doc.trim().is_empty(), "{} has no doc", d.path);
            // optional fields without a default are documented but absent from the defaults
            let exists = keys.iter().any(|k| k == &d.path || k.starts_with(&format!("{}.", d.path)));
            assert!(exists || d.default == "unset", "{} is documented but not a config key", d.path);
        }
    }
}
