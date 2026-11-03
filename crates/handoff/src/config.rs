//! Output and handoff configuration (`[output]` and `[handoff]` in a pipeline config).
//!
//! Names, albums, keywords and presets are templates over `{roll}`, `{frame}`, `{stock}` (profile
//! id) and `{stock_name}`. File names are slugged after expansion.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dng::Samples;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// DNG sample format: `float` (32-bit, archival) or `half` (16-bit float, about half the size).
    pub samples: Samples,
    /// Folder for developed frames; relative paths are taken from the working directory.
    pub dir: String,
    /// File name (without extension) of each frame.
    pub file_name: String,
    /// Write `<name>.xmp` beside each DNG (LightCraft reads it on import).
    pub sidecar: bool,
    /// Embed the same XMP packet in the DNG.
    pub embed_xmp: bool,
    /// Write the fitted roll model (`roll-model.json`) beside the frames.
    pub roll_model: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            samples: Samples::Float,
            dir: "silvercraft-out/{roll}".into(),
            file_name: "{roll}-{frame}".into(),
            sidecar: true,
            embed_xmp: true,
            roll_model: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    /// Control channel, else the watched folder, else leave the files.
    Auto,
    /// Control channel only (an error when LightCraft isn't listening).
    Control,
    /// LightCraft's watched folder only.
    WatchedFolder,
    /// Never send.
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    /// Send developed frames to LightCraft.
    pub send: bool,
    pub route: Route,
    /// LightCraft control port (`lightcraft --control <port>`).
    pub port: u16,
    /// LightCraft library folder (for its watched folder); empty = `$LIGHTCRAFT_LIBRARY` or the default.
    pub library: String,
    /// Album the roll goes into; empty = none.
    pub album: String,
    pub keywords: Vec<String>,
    /// LightCraft preset applied on import; empty = none.
    pub preset: String,
    /// When `preset` is empty, install the stock's look as a LightCraft preset and apply it.
    pub stock_look: bool,
    /// Copy into LightCraft's library folders instead of adding the files where they are.
    pub copy: bool,
    /// Open the first frame in LightCraft's Develop view.
    pub reveal: bool,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            send: false,
            route: Route::Auto,
            port: crate::lightcraft::DEFAULT_PORT,
            library: String::new(),
            album: "{roll}".into(),
            keywords: vec!["roll:{roll}".into(), "film:{stock_name}".into(), "SilverCraft".into()],
            preset: String::new(),
            stock_look: true,
            copy: false,
            reveal: true,
        }
    }
}

/// Values for template expansion.
#[derive(Clone, Debug, Default)]
pub struct TemplateVars<'a> {
    pub roll: &'a str,
    pub frame: &'a str,
    pub stock: &'a str,
    pub stock_name: &'a str,
}

/// Expand `{roll}`, `{frame}`, `{stock}`, `{stock_name}`; unknown `{names}` are an error.
pub fn expand(template: &str, vars: &TemplateVars<'_>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let j = after.find('}').ok_or_else(|| format!("unclosed `{{` in {template:?}"))?;
        out.push_str(match &after[..j] {
            "roll" => vars.roll,
            "frame" => vars.frame,
            "stock" => vars.stock,
            "stock_name" => vars.stock_name,
            other => return Err(format!("unknown template field {{{other}}} in {template:?}")),
        });
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A file-name-safe slug: ASCII letters, digits and single `-` (keeps case).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "frame".into() } else { out }
}

fn check(path: &str, t: &str, out: &mut Vec<(String, String)>) {
    if let Err(e) = expand(t, &TemplateVars::default()) {
        out.push((path.into(), e));
    }
}

impl OutputConfig {
    pub fn validate(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        check("output.dir", &self.dir, &mut out);
        check("output.file_name", &self.file_name, &mut out);
        if self.file_name.trim().is_empty() {
            out.push(("output.file_name".into(), "empty".into()));
        }
        if !self.sidecar && !self.embed_xmp {
            out.push(("output.sidecar".into(), "with neither a sidecar nor embedded XMP, LightCraft gets no roll metadata".into()));
        }
        out
    }

    /// The output folder for these variables. Variable *values* are slugged before
    /// expansion — `{roll}` and friends come from manifests, the CLI and the UI, and
    /// a roll named `../../x` must stay inside the configured tree. The template's
    /// own literal path (separators, absolute roots, `..`) is the config author's
    /// intent and is kept as written.
    pub fn dir_for(&self, vars: &TemplateVars<'_>) -> Result<PathBuf, String> {
        let (roll, frame, stock, stock_name) = (slug(vars.roll), slug(vars.frame), slug(vars.stock), slug(vars.stock_name));
        let safe = TemplateVars { roll: &roll, frame: &frame, stock: &stock, stock_name: &stock_name };
        Ok(PathBuf::from(expand(&self.dir, &safe)?))
    }

    pub fn stem_for(&self, vars: &TemplateVars<'_>) -> Result<String, String> {
        Ok(slug(&expand(&self.file_name, vars)?))
    }
}

impl HandoffConfig {
    pub fn validate(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        check("handoff.album", &self.album, &mut out);
        check("handoff.preset", &self.preset, &mut out);
        for (i, k) in self.keywords.iter().enumerate() {
            check(&format!("handoff.keywords[{i}]"), k, &mut out);
        }
        if self.port == 0 {
            out.push(("handoff.port".into(), "0 is not a port".into()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars<'static> {
        TemplateVars { roll: "R-048", frame: "007", stock: "kodak-portra-400", stock_name: "Kodak Portra 400" }
    }

    #[test]
    fn templates_expand_and_reject_unknown_fields() {
        assert_eq!(expand("{roll} · {frame} ({stock_name})", &vars()).unwrap(), "R-048 · 007 (Kodak Portra 400)");
        assert!(expand("{rol}", &vars()).is_err());
        assert!(expand("{roll", &vars()).is_err());
        assert_eq!(OutputConfig::default().stem_for(&vars()).unwrap(), "R-048-007");
        let c = OutputConfig { file_name: "{stock}/{frame}".into(), ..Default::default() };
        assert_eq!(c.stem_for(&vars()).unwrap(), "kodak-portra-400-007");
    }

    #[test]
    fn roll_names_cannot_escape_the_output_folder() {
        let hostile = TemplateVars { roll: "../../x", frame: "../f", stock: "kodak-portra-400", stock_name: "Kodak Portra 400" };
        let dir = OutputConfig::default().dir_for(&hostile).unwrap();
        assert_eq!(dir, std::path::Path::new("silvercraft-out/x"), "{dir:?}");
        assert!(!dir.components().any(|c| matches!(c, std::path::Component::ParentDir)));
        let abs = OutputConfig { dir: "/tmp/out/{roll}/{stock_name}".into(), ..Default::default() };
        assert_eq!(abs.dir_for(&TemplateVars { roll: "/etc", ..hostile }).unwrap(), std::path::Path::new("/tmp/out/etc/Kodak-Portra-400"));
        // Ordinary names pass through unchanged.
        assert_eq!(OutputConfig::default().dir_for(&vars()).unwrap(), std::path::Path::new("silvercraft-out/R-048"));
    }

    #[test]
    fn configs_round_trip_validate_and_reject_typos() {
        for text in [toml::to_string(&OutputConfig::default()).unwrap(), "samples = \"half\"\nsidecar = false".into()] {
            let c: OutputConfig = toml::from_str(&text).unwrap();
            assert!(c.validate().is_empty(), "{:?}", c.validate());
        }
        let h: HandoffConfig = toml::from_str("route = \"watched-folder\"\nkeywords = [\"{frame}\"]").unwrap();
        assert_eq!(h.route, Route::WatchedFolder);
        assert!(h.validate().is_empty());
        assert!(toml::from_str::<HandoffConfig>("albm = \"x\"").is_err());
        let bad = HandoffConfig { album: "{nope}".into(), port: 0, ..Default::default() };
        assert_eq!(bad.validate().len(), 2);
        let none = OutputConfig { sidecar: false, embed_xmp: false, ..Default::default() };
        assert_eq!(none.validate().len(), 1);
    }
}
