//! Film stock profiles: one TOML file per stock (schema
//! `silvercraft/profile/1`), the builtin set embedded at compile time, and a
//! [`ProfileRegistry`] that layers user directories over the builtins.
//!
//! A profile carries only what the film core needs (kind, G_ref, layer
//! gammas, base override, preview percentiles, positive mode) plus three
//! opaque tables preserved verbatim: `[characterization]` (datasheet
//! anchors), `[look]` (LightCraft preset material; `[look.print]` holds
//! the print/look fields) and `[processing]` (partial
//! [`crate::config::FilmConfig`] overrides for this stock).
//!
//! Layer gammas are stored as the measured R,G,B mid-gradients and the crate
//! derives the equalizing gain `gamma_G / gamma_c` itself, so the old
//! `density_scale` direction ambiguity cannot recur.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::config::{FilmConfig, PositiveMode};
use crate::diag::{Diagnostic, from_toml_error, has_errors};

/// Schema id every profile file must declare.
pub const PROFILE_SCHEMA: &str = "silvercraft/profile/1";
/// G_ref default for colour negatives without a characterized green
/// mid-gradient (Cineon printing-density convention).
pub const DEFAULT_COLOR_NEGATIVE_GAMMA: f64 = 0.60;
/// G_ref default for B&W negatives without a characterized mid-gradient.
pub const DEFAULT_BW_NEGATIVE_GAMMA: f64 = 0.60;
/// Valid open range for every gamma in a profile.
pub const GAMMA_RANGE: (f64, f64) = (0.1, 5.0);

/// Film kind (`kind` in a profile file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilmKind {
    #[serde(rename = "color-negative")]
    ColorNegative,
    #[serde(rename = "bw-negative")]
    BlackAndWhiteNegative,
    #[serde(rename = "color-positive")]
    ColorPositive,
    #[serde(rename = "bw-positive")]
    BlackAndWhitePositive,
}

impl FilmKind {
    /// Parse a profile-file kind ("color-negative", ...), also accepting the
    /// underscore spelling ("color_negative", ...). Unknown (incl. "digital")
    /// is `None`.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind.replace('_', "-").as_str() {
            "color-negative" => Some(Self::ColorNegative),
            "color-positive" => Some(Self::ColorPositive),
            "bw-negative" | "black-and-white-negative" => Some(Self::BlackAndWhiteNegative),
            "bw-positive" | "black-and-white-positive" => Some(Self::BlackAndWhitePositive),
            _ => None,
        }
    }

    pub fn is_negative(self) -> bool {
        matches!(self, Self::ColorNegative | Self::BlackAndWhiteNegative)
    }

    pub fn is_color(self) -> bool {
        matches!(self, Self::ColorNegative | Self::ColorPositive)
    }

    /// The profile-file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ColorNegative => "color-negative",
            Self::BlackAndWhiteNegative => "bw-negative",
            Self::ColorPositive => "color-positive",
            Self::BlackAndWhitePositive => "bw-positive",
        }
    }
}

/// Capture-device variant of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capture {
    #[default]
    Any,
    Camera,
    Scanner,
}

impl Capture {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "any" => Some(Self::Any),
            "camera" => Some(Self::Camera),
            "scanner" => Some(Self::Scanner),
            _ => None,
        }
    }
}

fn default_low() -> f64 {
    0.5
}
fn default_high() -> f64 {
    99.5
}
fn default_gray() -> f64 {
    50.0
}
fn default_true() -> bool {
    true
}

/// `[negative]`: required iff the kind is a negative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegativeSection {
    /// G_ref: green mid-gradient (density per log10 exposure); B&W: the single
    /// mid-gradient. Absent = 0.60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene_gamma: Option<f64>,
    /// R,G,B mid-gradients (colour only). Absent = no layer correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_gamma: Option<[f64; 3]>,
    /// Explicit scan-space base transmission R,G,B in (0, 1].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmin: Option<[f64; 3]>,
    /// Preview black anchor percentile; also the calibration's density_black.
    #[serde(default = "default_low")]
    pub low_percentile: f64,
    /// Preview white anchor percentile; also the calibration's density_white.
    #[serde(default = "default_high")]
    pub high_percentile: f64,
    /// Preview pass-1 gray percentile.
    #[serde(default = "default_gray")]
    pub gray_percentile: f64,
    /// Neutralize gray (preview) and fit the roll matrix/curve.
    #[serde(default = "default_true")]
    pub neutralize: bool,
}

impl Default for NegativeSection {
    fn default() -> Self {
        Self {
            scene_gamma: None,
            layer_gamma: None,
            dmin: None,
            low_percentile: default_low(),
            high_percentile: default_high(),
            gray_percentile: default_gray(),
            neutralize: true,
        }
    }
}

/// `[positive]`: required iff the kind is a positive.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositiveSection {
    /// as-is | unrender.
    #[serde(default = "default_positive_mode")]
    pub mode: PositiveMode,
    /// Slide gamma (used by unrender).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamma: Option<f64>,
}

fn default_positive_mode() -> PositiveMode {
    PositiveMode::AsIs
}

/// One film stock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilmProfile {
    /// Must be `silvercraft/profile/1`.
    pub schema: String,
    /// Unique slug `[a-z0-9-]`.
    pub id: String,
    /// Display name.
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manufacturer: String,
    pub kind: FilmKind,
    /// Free text: C-41, ECN-2, E-6, BW, ...
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub process: String,
    #[serde(default)]
    pub capture: Capture,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub license: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<NegativeSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positive: Option<PositiveSection>,
    /// Datasheet anchors, preserved verbatim, never read by the pipeline.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub characterization: toml::Table,
    /// LightCraft develop-preset material, opaque to the film crate.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub look: toml::Table,
    /// Partial [`FilmConfig`] overrides for this stock.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub processing: toml::Table,
}

/// Compatibility alias for the pre-file-format name.
pub type FilmProfileSpec = FilmProfile;

fn is_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

fn gamma_ok(v: f64) -> bool {
    v.is_finite() && v > GAMMA_RANGE.0 && v < GAMMA_RANGE.1
}

impl FilmProfile {
    pub fn kind(&self) -> FilmKind {
        self.kind
    }

    /// The negative section (defaults when absent).
    pub fn negative_params(&self) -> NegativeSection {
        self.negative.clone().unwrap_or_default()
    }

    /// G_ref: `[negative].scene_gamma`, default 0.60.
    pub fn scene_gamma(&self) -> f64 {
        let default = if self.kind == FilmKind::BlackAndWhiteNegative { DEFAULT_BW_NEGATIVE_GAMMA } else { DEFAULT_COLOR_NEGATIVE_GAMMA };
        self.negative.as_ref().and_then(|n| n.scene_gamma).filter(|g| gamma_ok(*g)).unwrap_or(default)
    }

    /// The layer EQUALIZER `gamma_G / gamma_c` (R,G,B), a multiplier on
    /// density above base (direction verified by
    /// `tests/density_scale_direction.rs`). Unit for B&W, positives, or
    /// without `layer_gamma`.
    pub fn layer_gain(&self) -> [f64; 3] {
        match (self.kind, self.negative.as_ref().and_then(|n| n.layer_gamma)) {
            (FilmKind::ColorNegative, Some(g)) if g.iter().all(|v| gamma_ok(*v)) => [g[1] / g[0], 1.0, g[1] / g[2]],
            _ => [1.0; 3],
        }
    }

    /// Usable explicit base override.
    pub fn dmin_f64(&self) -> Option<[f64; 3]> {
        self.negative.as_ref().and_then(|n| n.dmin).filter(|d| d.iter().all(|v| v.is_finite() && *v > 0.0 && *v <= 1.0))
    }

    /// Positive mode (as-is when absent).
    pub fn positive_mode(&self) -> PositiveMode {
        self.positive.as_ref().map_or(PositiveMode::AsIs, |p| p.mode)
    }

    /// Slide gamma, when the profile states one.
    pub fn positive_gamma(&self) -> Option<f64> {
        self.positive.as_ref().and_then(|p| p.gamma).filter(|g| gamma_ok(*g))
    }

    /// Parse and validate one profile file's text. `source` labels diagnostics.
    pub fn from_toml_str(text: &str, source: &str) -> Result<Self, Vec<Diagnostic>> {
        let profile: Self = toml::from_str(text).map_err(|e| vec![from_toml_error(&e, text).with_source(source)])?;
        let diags: Vec<Diagnostic> = profile.validate().into_iter().map(|d| d.with_source(source)).collect();
        if has_errors(&diags) { Err(diags) } else { Ok(profile) }
    }

    /// Serialize as a profile file.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// A starting point for `silvercraft-cli profile new`.
    pub fn template(kind: FilmKind) -> Self {
        let (negative, positive, process) = match kind {
            FilmKind::ColorNegative => {
                (Some(NegativeSection { scene_gamma: Some(0.60), layer_gamma: Some([0.60, 0.60, 0.60]), ..NegativeSection::default() }), None, "C-41")
            }
            FilmKind::BlackAndWhiteNegative => (Some(NegativeSection { scene_gamma: Some(0.60), ..NegativeSection::default() }), None, "BW"),
            FilmKind::ColorPositive => (None, Some(PositiveSection { mode: PositiveMode::AsIs, gamma: Some(2.0) }), "E-6"),
            FilmKind::BlackAndWhitePositive => (None, Some(PositiveSection { mode: PositiveMode::AsIs, gamma: None }), "BW reversal"),
        };
        Self {
            schema: PROFILE_SCHEMA.to_owned(),
            id: "my-stock".to_owned(),
            name: "My Stock".to_owned(),
            manufacturer: String::new(),
            kind,
            process: process.to_owned(),
            capture: Capture::Any,
            author: String::new(),
            source: String::new(),
            license: "CC0-1.0".to_owned(),
            notes: String::new(),
            negative,
            positive,
            characterization: toml::Table::new(),
            look: toml::Table::new(),
            processing: toml::Table::new(),
        }
    }

    /// The effective pipeline config for this stock: `base`, then this
    /// profile's `[processing]`, then `user_overrides` (later wins).
    pub fn effective_config(&self, base: &FilmConfig, user_overrides: Option<&toml::Table>) -> Result<FilmConfig, Vec<Diagnostic>> {
        let mut config = if self.processing.is_empty() {
            base.clone()
        } else {
            base.merged(&self.processing).map_err(|ds| ds.into_iter().map(|d| d.with_path_prefix("processing")).collect::<Vec<_>>())?
        };
        if let Some(over) = user_overrides {
            config = config.merged(over)?;
        }
        Ok(config)
    }

    /// Schema, id, ranges, kind/section consistency, `[processing]` keys.
    pub fn validate(&self) -> Vec<Diagnostic> {
        let mut d = Vec::new();
        if self.schema != PROFILE_SCHEMA {
            d.push(Diagnostic::error("schema", format!("expected \"{PROFILE_SCHEMA}\", got \"{}\"", self.schema)));
        }
        if !is_slug(&self.id) {
            d.push(Diagnostic::error("id", format!("\"{}\" must be a slug of [a-z0-9-]", self.id)));
        }
        if self.name.trim().is_empty() {
            d.push(Diagnostic::error("name", "must not be empty"));
        }
        let negative = self.kind.is_negative();
        match (negative, &self.negative, &self.positive) {
            (true, None, _) => d.push(Diagnostic::error("negative", format!("kind \"{}\" requires a [negative] section", self.kind.as_str()))),
            (false, _, None) => d.push(Diagnostic::error("positive", format!("kind \"{}\" requires a [positive] section", self.kind.as_str()))),
            _ => {}
        }
        if negative && self.positive.is_some() {
            d.push(Diagnostic::error("positive", format!("[positive] is not allowed for kind \"{}\"", self.kind.as_str())));
        }
        if !negative && self.negative.is_some() {
            d.push(Diagnostic::error("negative", format!("[negative] is not allowed for kind \"{}\"", self.kind.as_str())));
        }
        if let Some(n) = &self.negative {
            if let Some(g) = n.scene_gamma
                && !gamma_ok(g)
            {
                d.push(Diagnostic::error("negative.scene_gamma", format!("{g} must be in (0.1, 5)")));
            }
            if let Some(g) = n.layer_gamma {
                if !g.iter().all(|v| gamma_ok(*v)) {
                    d.push(Diagnostic::error("negative.layer_gamma", format!("{g:?} must be in (0.1, 5) per channel")));
                }
                if self.kind == FilmKind::BlackAndWhiteNegative {
                    d.push(Diagnostic::warning("negative.layer_gamma", "ignored for B&W (use scene_gamma)"));
                }
            }
            if let Some(m) = n.dmin
                && !m.iter().all(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
            {
                d.push(Diagnostic::error("negative.dmin", format!("{m:?} must be in (0, 1] per channel")));
            }
            for (path, v) in [
                ("negative.low_percentile", n.low_percentile),
                ("negative.high_percentile", n.high_percentile),
                ("negative.gray_percentile", n.gray_percentile),
            ] {
                if !(v.is_finite() && (0.0..=100.0).contains(&v)) {
                    d.push(Diagnostic::error(path, format!("{v} must be in 0..=100")));
                }
            }
            if n.low_percentile >= n.high_percentile {
                d.push(Diagnostic::error("negative.low_percentile", "must be below high_percentile"));
            }
        }
        if let Some(p) = &self.positive
            && let Some(g) = p.gamma
            && !gamma_ok(g)
        {
            d.push(Diagnostic::error("positive.gamma", format!("{g} must be in (0.1, 5)")));
        }
        if !self.processing.is_empty()
            && let Err(ds) = FilmConfig::default().merged(&self.processing)
        {
            d.extend(ds.into_iter().map(|x| x.with_path_prefix("processing")));
        }
        d
    }
}

/// Builtin profile files, embedded at compile time.
pub const BUILTIN_PROFILE_FILES: &[(&str, &str)] = &[
    ("cinestill-50d", include_str!("../profiles/cinestill-50d.toml")),
    ("cinestill-800t", include_str!("../profiles/cinestill-800t.toml")),
    ("fuji-pro-400h", include_str!("../profiles/fuji-pro-400h.toml")),
    ("fuji-provia-100f", include_str!("../profiles/fuji-provia-100f.toml")),
    ("fuji-superia-400", include_str!("../profiles/fuji-superia-400.toml")),
    ("fuji-velvia-50", include_str!("../profiles/fuji-velvia-50.toml")),
    ("generic-c41", include_str!("../profiles/generic-c41.toml")),
    ("ilford-hp5-plus", include_str!("../profiles/ilford-hp5-plus.toml")),
    ("ilford-xp2-super", include_str!("../profiles/ilford-xp2-super.toml")),
    ("kodak-ektachrome-e100", include_str!("../profiles/kodak-ektachrome-e100.toml")),
    ("kodak-ektar-100", include_str!("../profiles/kodak-ektar-100.toml")),
    ("kodak-gold-200", include_str!("../profiles/kodak-gold-200.toml")),
    ("kodak-portra-160", include_str!("../profiles/kodak-portra-160.toml")),
    ("kodak-portra-400", include_str!("../profiles/kodak-portra-400.toml")),
    ("kodak-portra-400-legacy", include_str!("../profiles/kodak-portra-400-legacy.toml")),
    ("kodak-portra-400-prior", include_str!("../profiles/kodak-portra-400-prior.toml")),
    ("kodak-portra-800", include_str!("../profiles/kodak-portra-800.toml")),
    ("kodak-tri-x-400", include_str!("../profiles/kodak-tri-x-400.toml")),
    ("kodak-ultramax-400", include_str!("../profiles/kodak-ultramax-400.toml")),
    ("positive-slide", include_str!("../profiles/positive-slide.toml")),
    ("vision3-250d", include_str!("../profiles/vision3-250d.toml")),
    ("vision3-500t", include_str!("../profiles/vision3-500t.toml")),
    ("vision3-50d", include_str!("../profiles/vision3-50d.toml")),
];

/// Profiles from builtins and user files, later sources overriding earlier
/// ones by (id, capture). Bad files are reported, never fatal.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    profiles: Vec<FilmProfile>,
    sources: Vec<String>,
    diagnostics: Vec<Diagnostic>,
}

impl ProfileRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The embedded builtin profiles.
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        for (id, text) in BUILTIN_PROFILE_FILES {
            registry.load_str(text, &format!("builtin:{id}"));
        }
        registry
    }

    /// Builtins, then every existing directory of [`ProfileRegistry::user_dirs`].
    pub fn with_user_dirs() -> Self {
        let mut registry = Self::builtin();
        for dir in Self::user_dirs() {
            if dir.is_dir() {
                registry.load_dir(&dir);
            }
        }
        registry
    }

    /// User profile directories, in load order (later overrides earlier):
    /// every path of `$SILVERCRAFT_PROFILES` (OS path-list syntax), then the
    /// per-user config dir: `$XDG_CONFIG_HOME/silvercraft/profiles` (default
    /// `~/.config/...`) on Linux/BSD, `~/Library/Application
    /// Support/SilverCraft/profiles` on macOS, `%APPDATA%\SilverCraft\profiles`
    /// on Windows.
    pub fn user_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::env::var_os("SILVERCRAFT_PROFILES")
            .map(|list| std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()).collect())
            .unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config = if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("SilverCraft").join("profiles"))
        } else if cfg!(target_os = "macos") {
            home.map(|h| h.join("Library").join("Application Support").join("SilverCraft").join("profiles"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| home.map(|h| h.join(".config")))
                .map(|c| c.join("silvercraft").join("profiles"))
        };
        dirs.extend(config);
        dirs
    }

    /// Load every `*.toml` in `dir` (sorted by file name).
    pub fn load_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.diagnostics.push(Diagnostic::error("", format!("cannot read profile directory: {e}")).with_source(dir.display().to_string()));
                return;
            }
        };
        let mut files: Vec<PathBuf> =
            entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "toml")).collect();
        files.sort();
        for file in files {
            self.load_file(&file);
        }
    }

    /// Load one profile file. Returns whether it was accepted.
    pub fn load_file(&mut self, path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(text) => self.load_str(&text, &path.display().to_string()),
            Err(e) => {
                self.diagnostics.push(Diagnostic::error("", format!("cannot read profile: {e}")).with_source(path.display().to_string()));
                false
            }
        }
    }

    /// Load one profile from text. Returns whether it was accepted.
    pub fn load_str(&mut self, text: &str, source: &str) -> bool {
        match FilmProfile::from_toml_str(text, source) {
            Ok(profile) => {
                let warnings = profile.validate().into_iter().map(|d| d.with_source(source));
                self.diagnostics.extend(warnings);
                self.insert(profile, source.to_owned());
                true
            }
            Err(diags) => {
                self.diagnostics.extend(diags);
                false
            }
        }
    }

    /// Insert (or override by id + capture) a validated profile.
    pub fn insert(&mut self, profile: FilmProfile, source: String) {
        let existing = self.profiles.iter().position(|p| p.id == profile.id && p.capture == profile.capture);
        match existing {
            Some(index) => {
                self.diagnostics.push(
                    Diagnostic::note("id", format!("profile \"{}\" overrides the one from {}", profile.id, self.sources[index]))
                        .with_source(source.clone()),
                );
                self.profiles[index] = profile;
                self.sources[index] = source;
            }
            None => {
                self.profiles.push(profile);
                self.sources.push(source);
            }
        }
    }

    pub fn profiles(&self) -> &[FilmProfile] {
        &self.profiles
    }

    /// Where profile `index` (in [`ProfileRegistry::profiles`] order) came from.
    pub fn source_of(&self, index: usize) -> Option<&str> {
        self.sources.get(index).map(String::as_str)
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The generic (capture "any") entry for `id`, else any variant.
    pub fn get(&self, id: &str) -> Option<&FilmProfile> {
        self.profiles.iter().find(|p| p.id == id && p.capture == Capture::Any).or_else(|| self.profiles.iter().find(|p| p.id == id))
    }

    /// A capture-specific variant wins over the generic entry.
    pub fn get_for_capture(&self, id: &str, capture: Option<Capture>) -> Option<&FilmProfile> {
        match capture {
            Some(c) if c != Capture::Any => self.profiles.iter().find(|p| p.id == id && p.capture == c).or_else(|| self.get(id)),
            _ => self.get(id),
        }
    }
}

/// The process-wide builtin registry.
pub fn builtin_registry() -> &'static ProfileRegistry {
    static REGISTRY: OnceLock<ProfileRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ProfileRegistry::builtin)
}

/// The builtin profiles.
pub fn builtin_profiles() -> &'static [FilmProfile] {
    builtin_registry().profiles()
}

/// Builtin lookup by id (generic entry preferred).
pub fn film_profile(id: &str) -> Option<&'static FilmProfile> {
    builtin_registry().get(id)
}

/// Builtin lookup with a capture class ("camera" | "scanner").
pub fn profile_for_capture(id: &str, capture_class: Option<&str>) -> Option<&'static FilmProfile> {
    builtin_registry().get_for_capture(id, capture_class.and_then(Capture::parse))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_loads_and_validates_cleanly() {
        let registry = ProfileRegistry::builtin();
        assert!(registry.diagnostics().is_empty(), "{:?}", registry.diagnostics());
        assert_eq!(registry.profiles().len(), BUILTIN_PROFILE_FILES.len());
        for ((id, _), profile) in BUILTIN_PROFILE_FILES.iter().zip(registry.profiles()) {
            assert_eq!(*id, profile.id, "file name must equal id");
        }
        assert!(film_profile("digital").is_none(), "digital belongs to LightCraft");
        assert!(film_profile("generic-c41").is_some());
        assert_eq!(profile_for_capture("kodak-portra-400", Some("camera")).unwrap().id, "kodak-portra-400");
    }

    #[test]
    fn scene_gamma_and_layer_gain() {
        let get = |id| film_profile(id).unwrap();
        assert_eq!(get("kodak-portra-400").scene_gamma(), 0.55);
        assert_eq!(get("vision3-500t").scene_gamma(), 0.53);
        assert_eq!(get("kodak-gold-200").scene_gamma(), 0.57);
        assert_eq!(get("generic-c41").scene_gamma(), 0.60);
        assert_eq!(get("ilford-hp5-plus").scene_gamma(), 0.70);
        assert_eq!(get("ilford-xp2-super").scene_gamma(), 0.55);
        let g = get("kodak-portra-400").layer_gain();
        assert!((g[0] - 0.55 / 0.57).abs() < 1e-12 && g[1] == 1.0 && (g[2] - 0.55 / 0.62).abs() < 1e-12);
        assert_eq!(get("ilford-hp5-plus").layer_gain(), [1.0; 3]);
        assert_eq!(get("generic-c41").layer_gain(), [1.0; 3]);
    }

    #[test]
    fn template_round_trips_for_every_kind() {
        for kind in [FilmKind::ColorNegative, FilmKind::BlackAndWhiteNegative, FilmKind::ColorPositive, FilmKind::BlackAndWhitePositive] {
            let t = FilmProfile::template(kind);
            assert!(t.validate().is_empty(), "{kind:?}: {:?}", t.validate());
            let back = FilmProfile::from_toml_str(&t.to_toml(), "t").unwrap();
            assert_eq!(back, t);
        }
    }
}
