//! SilverCraft pipeline configuration: one TOML document controls every stage.
//!
//! ```toml
//! [decode]        # capture decoding (demosaic, camera matrix, infrared)
//! [film.base]     # film-base measurement      ┐
//! [film.calibration] ...                       │ silvercraft_film::FilmConfig
//! [film.scene]    # scene-linear transform      ┘
//! [output]        # DNG format, folders, file names, XMP
//! [handoff]       # sending frames to LightCraft
//! ```
//!
//! Layers, later winning key by key: built-in defaults < the stock's `[processing]` (applied under
//! `film`) < the user config file < `--config` files in order < `--set section.key=value`. The
//! resolved config and the list of layers are recorded with every roll, so a result can be
//! reproduced exactly.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod docs;
pub mod layer;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use silvercraft_decode::DecodeConfig;
use silvercraft_film::FilmConfig;
use silvercraft_film::diag::{Diagnostic, Severity};
use silvercraft_film::profiles::FilmProfile;
use silvercraft_handoff::{HandoffConfig, OutputConfig};

pub use layer::{Layer, LayerError};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    pub decode: DecodeConfig,
    pub film: FilmConfig,
    pub scanfix: silvercraft_scanfix::ScanfixConfig,
    pub heal: silvercraft_heal::HealConfig,
    pub detect: silvercraft_detect::DetectConfig,
    pub output: OutputConfig,
    pub handoff: HandoffConfig,
}

impl PipelineConfig {
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Every problem in every section, with dotted paths from the document root.
    pub fn validate(&self) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = self.film.validate().into_iter().map(|d| d.with_path_prefix("film")).collect();
        out.extend(self.heal.validate().into_iter().filter(|d| d.is_error()).map(|d| d.with_path_prefix("heal")));
        if let Err(e) = self.scanfix.validate() {
            out.push(Diagnostic { source: None, path: "scanfix".into(), message: e.to_string(), severity: Severity::Error });
        }
        out.extend(self.detect.validate().into_iter().map(|(path, message)| Diagnostic {
            source: None,
            path: format!("detect.{path}"),
            message,
            severity: Severity::Error,
        }));
        let plain = |(path, message): (String, String)| Diagnostic { source: None, path, message, severity: Severity::Error };
        out.extend(self.decode.validate().into_iter().map(plain));
        out.extend(self.output.validate().into_iter().map(plain));
        out.extend(self.handoff.validate().into_iter().map(plain));
        out
    }
}

/// The user's config file: `$SILVERCRAFT_CONFIG`, else the platform config folder's
/// `silvercraft/config.toml` (`~/.config/silvercraft/config.toml` on Linux).
pub fn user_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SILVERCRAFT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    config_dir().map(|d| d.join("config.toml"))
}

/// SilverCraft's folder in the platform config location.
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "windows") {
        return env("APPDATA").map(|d| d.join("SilverCraft"));
    }
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Application Support/SilverCraft"));
    }
    env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|d| d.join("silvercraft"))
}

/// Builds a [`PipelineConfig`] from layers.
#[derive(Clone, Debug)]
pub struct ConfigBuilder {
    layers: Vec<Layer>,
}

/// A resolved configuration and where it came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolved {
    pub config: PipelineConfig,
    /// Layer names in the order applied (`defaults`, `stock kodak-portra-400`, file paths, `--set`).
    pub layers: Vec<String>,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigBuilder {
    pub fn new() -> Self {
        let defaults = Layer::from_value("defaults", &PipelineConfig::default()).unwrap_or_else(|_| Layer::new("defaults", toml::Table::new()));
        Self { layers: vec![defaults] }
    }

    /// The stock's `[processing]` table, applied under `film`.
    pub fn stock(mut self, profile: &FilmProfile) -> Self {
        if !profile.processing.is_empty() {
            self.layers.push(Layer::new(format!("stock {}", profile.id), profile.processing.clone()).nested("film"));
        }
        self
    }

    pub fn file(mut self, path: &Path) -> Result<Self, LayerError> {
        let text = std::fs::read_to_string(path).map_err(|e| LayerError::Parse { layer: path.display().to_string(), message: e.to_string() })?;
        self.layers.push(Layer::parse(path.display().to_string(), &text)?);
        Ok(self)
    }

    /// The user config file, when it exists.
    pub fn user_file(self) -> Result<Self, LayerError> {
        match user_config_path() {
            Some(p) if p.is_file() => self.file(&p),
            _ => Ok(self),
        }
    }

    pub fn text(mut self, name: &str, text: &str) -> Result<Self, LayerError> {
        self.layers.push(Layer::parse(name, text)?);
        Ok(self)
    }

    pub fn set(mut self, assignments: &[String]) -> Result<Self, LayerError> {
        if !assignments.is_empty() {
            self.layers.push(Layer::from_assignments(assignments)?);
        }
        Ok(self)
    }

    /// Merge and deserialise; then validate. Errors (not warnings) fail the build.
    pub fn build(self) -> Result<(Resolved, Vec<Diagnostic>), Vec<Diagnostic>> {
        let config: PipelineConfig = layer::resolve(&self.layers).map_err(|e| vec![Diagnostic::error("", e.to_string())])?;
        let diags = config.validate();
        if diags.iter().any(|d| d.severity == Severity::Error) {
            return Err(diags);
        }
        Ok((Resolved { config, layers: self.layers.iter().map(|l| l.source.name.clone()).collect() }, diags))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silvercraft_film::profiles::FilmKind;

    #[test]
    fn defaults_round_trip_and_validate() {
        let c = PipelineConfig::default();
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        let back: PipelineConfig = toml::from_str(&c.to_toml()).unwrap();
        assert_eq!(back, c);
        let (r, _) = ConfigBuilder::new().build().unwrap();
        assert_eq!(r.config, c);
        assert_eq!(r.layers, ["defaults"]);
    }

    #[test]
    fn layers_apply_in_order_across_every_section() {
        let mut stock = FilmProfile::template(FilmKind::ColorNegative);
        stock.id = "test-stock".into();
        stock.processing = toml::from_str("[exposure]\nmode = \"fixed\"\nfixed_ev = 0.5").unwrap();
        let (r, _) = ConfigBuilder::new()
            .stock(&stock)
            .text("roll.toml", "[decode]\ndemosaic = \"ppg\"\n[film.exposure]\nfixed_ev = 1.0\n[output]\nsamples = \"half\"")
            .unwrap()
            .set(&["handoff.port=7999".into(), "film.exposure.fixed_ev=1.5".into()])
            .unwrap()
            .build()
            .unwrap();
        let c = &r.config;
        assert_eq!(c.decode.demosaic, silvercraft_decode::Demosaic::Ppg);
        assert_eq!(c.output.samples, silvercraft_handoff::Samples::Half);
        assert_eq!(c.handoff.port, 7999);
        assert_eq!(c.film.exposure.fixed_ev, 1.5);
        assert_eq!(r.layers, ["defaults", "stock test-stock", "roll.toml", "--set"]);
        let t = toml::to_string(&c.film.exposure).unwrap();
        assert!(t.contains("fixed"), "{t}");
    }

    #[test]
    fn typos_and_bad_values_are_reported_with_their_layer() {
        let e = ConfigBuilder::new().text("mine.toml", "[film.scene]\nmid_gray = 0.2").unwrap().build().unwrap_err();
        assert!(e[0].message.starts_with("mine.toml"), "{e:?}");
        let e = ConfigBuilder::new().set(&["output.file_name={nope}".into()]).unwrap().build().unwrap_err();
        assert_eq!(e[0].path, "output.file_name");
        let e = ConfigBuilder::new().set(&["decode.mask_percentile=10".into()]).unwrap().build().unwrap_err();
        assert_eq!(e[0].path, "decode.mask_percentile");
    }
}
