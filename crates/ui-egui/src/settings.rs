//! App settings (`app.json` in SilverCraft's config folder): where rolls go and capture defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Each roll is a folder in here (captures, `roll.json`, developed frames).
    pub rolls_dir: PathBuf,
    pub default_stock: String,
    /// Flatbed scan resolution.
    pub scan_dpi: u32,
    /// Dual pass with infrared for dust repair.
    pub infrared: bool,
    /// Margin scanned around each frame (for the film base in the gaps).
    pub padding_mm: f64,
    /// Send developed rolls to LightCraft.
    pub send_to_lightcraft: bool,
    /// Develop automatically after capturing: when a scan finishes, and in the background as
    /// camera frames come in. (A new key: the old `auto_develop` was off by default.)
    pub develop_after_capture: bool,
    /// How the camera's live view and captures are turned to stand upright.
    pub camera_rotation: silvercraft_engine::rotation::Rotation,
    /// Show negatives as positives in previews and live view.
    pub show_positive: bool,
    /// Write half-float DNGs (half the size).
    pub half_float: bool,
    /// The sidebar is shown.
    pub left_panel: bool,
    /// Sidebar and right panel widths (points), as last resized.
    pub left_width: f32,
    pub right_width: f32,
}

/// Sidebar width limits (LightCraft's).
pub const LEFT_WIDTH: (f32, f32, f32) = (200.0, 268.0, 480.0);
/// Right panel width limits (LightCraft's).
pub const RIGHT_WIDTH: (f32, f32, f32) = (250.0, 290.0, 520.0);

/// The user's home folder.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            rolls_dir: home_dir().map(|h| h.join("Pictures").join("SilverCraft")).unwrap_or_else(|| PathBuf::from("SilverCraft")),
            default_stock: "generic-c41".into(),
            scan_dpi: 3200,
            infrared: true,
            padding_mm: 1.5,
            send_to_lightcraft: true,
            develop_after_capture: true,
            camera_rotation: Default::default(),
            show_positive: true,
            half_float: false,
            left_panel: true,
            left_width: LEFT_WIDTH.1,
            right_width: RIGHT_WIDTH.1,
        }
    }
}

impl AppSettings {
    /// Settings from `path`; defaults when missing or unreadable (a note is logged).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("{}: {e}; using defaults", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Config overrides these settings imply (applied as an app layer before `--set`).
    pub fn config_layer(&self) -> String {
        let mut s = String::new();
        if self.send_to_lightcraft {
            s.push_str("[handoff]\nsend = true\n");
        }
        if self.half_float {
            s.push_str("[output]\nsamples = \"half\"\n");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerant_load() {
        let dir = std::env::temp_dir().join(format!("sc-settings-{}", std::process::id()));
        let p = dir.join("app.json");
        let mut s = AppSettings { scan_dpi: 4800, ..Default::default() };
        s.save(&p).unwrap();
        assert_eq!(AppSettings::load(&p), s);
        std::fs::write(&p, "{\"scan_dpi\": 2400}").unwrap();
        assert_eq!(AppSettings::load(&p).scan_dpi, 2400, "missing keys take defaults");
        std::fs::write(&p, "{\"auto_develop\": false, \"camera_rotation\": 270}").unwrap();
        let old = AppSettings::load(&p);
        assert!(old.develop_after_capture, "develops after capture unless turned off since");
        assert_eq!(old.camera_rotation.degrees(), 270);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(AppSettings::load(&p), AppSettings::default());
        s.send_to_lightcraft = false;
        s.half_float = true;
        assert!(!s.config_layer().contains("handoff"));
        assert!(s.config_layer().contains("half"));
    }
}
