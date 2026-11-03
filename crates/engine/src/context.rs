//! Stock profiles and pipeline configuration for a roll.

use std::path::{Path, PathBuf};

use silvercraft_config::{ConfigBuilder, Resolved};
use silvercraft_film::diag::Diagnostic;
use silvercraft_film::profiles::{FilmProfile, ProfileRegistry};

use crate::{EngineError, Result};

/// Raw camera formats (a capture in one of these is a camera scan; anything else is a scanner
/// TIFF).
pub const RAW_EXTENSIONS: &[&str] = &["arw", "dng", "nef", "cr2", "cr3", "raf", "rw2", "orf", "pef", "srw", "3fr", "iiq"];

pub fn is_camera_scan(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| RAW_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Where profiles and config layers come from beyond the built-ins.
#[derive(Clone, Debug, Default)]
pub struct ConfigSources {
    /// Extra stock folders (`--profiles`), loaded after the user folder.
    pub profile_dirs: Vec<PathBuf>,
    /// Config files (`--config`), in order.
    pub configs: Vec<PathBuf>,
    /// `key=value` overrides (`--set`), in order.
    pub sets: Vec<String>,
    /// Layers given as text (name, TOML), applied after `configs` and before `sets` (the app's
    /// own settings).
    pub texts: Vec<(String, String)>,
    /// Skip the user config file and user profile folder (tests, reproducible runs).
    pub no_user: bool,
}

impl ConfigSources {
    /// The stock registry: built-ins, user folders (unless `no_user`), then `profile_dirs`.
    pub fn registry(&self) -> ProfileRegistry {
        let mut r = if self.no_user { ProfileRegistry::builtin() } else { ProfileRegistry::with_user_dirs() };
        for d in &self.profile_dirs {
            r.load_dir(d);
        }
        r
    }

    /// The pipeline config with every layer applied, and its warnings.
    pub fn resolve(&self, stock: Option<&FilmProfile>, camera_scan: bool) -> std::result::Result<(Resolved, Vec<Diagnostic>), Vec<Diagnostic>> {
        let one = |e: silvercraft_config::LayerError| vec![Diagnostic::error("", e.to_string())];
        let mut b = ConfigBuilder::new();
        if camera_scan {
            // a camera sees a thin rebate and then the dark holder: find the rebate band, don't
            // take the whole margin
            b = b.text("camera-scan defaults", "[film.base]\nmethod = \"auto-band\"").map_err(one)?;
        }
        if let Some(p) = stock {
            b = b.stock(p);
        }
        if !self.no_user {
            b = b.user_file().map_err(one)?;
        }
        for c in &self.configs {
            b = b.file(c).map_err(one)?;
        }
        for (name, text) in &self.texts {
            b = b.text(name, text).map_err(one)?;
        }
        b = b.set(&self.sets).map_err(one)?;
        b.build()
    }

    /// Like [`resolve`](Self::resolve), with the diagnostics folded into an error.
    pub fn resolve_or_err(&self, stock: Option<&FilmProfile>, camera_scan: bool, diags: &mut Vec<Diagnostic>) -> Result<Resolved> {
        match self.resolve(stock, camera_scan) {
            Ok((r, w)) => {
                diags.extend(w);
                Ok(r)
            }
            Err(d) => {
                diags.extend(d);
                Err(EngineError::InvalidConfig)
            }
        }
    }
}
