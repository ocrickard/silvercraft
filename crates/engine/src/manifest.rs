//! The roll manifest: what was captured, where, and where each frame sits, so a roll can be
//! developed (again) at any time. Written beside the captures as `roll.json` after every frame,
//! so an interrupted session keeps what it captured.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::ConfigSources;
use crate::roll::{FrameSource, RollRequest};
use crate::rotation::Rotation;
use crate::{EngineError, Result};

pub const SCHEMA: &str = "silvercraft/roll/1";
pub const FILE_NAME: &str = "roll.json";

/// How the roll was captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureSource {
    Scanner,
    Camera,
    Files,
}

/// One captured frame. Paths are relative to the manifest's folder (absolute paths are kept as
/// they are).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFrame {
    pub label: String,
    pub capture: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infrared: Option<PathBuf>,
    /// Exposed frame `x0, y0, x1, y1` in capture pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_box: Option<[usize; 4]>,
    /// Capture size `frame_box` refers to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_size: Option<(usize, usize)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
    /// Scanned region on the bed, inches `[left, top, width, height]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_in: Option<[f64; 4]>,
    /// The scanner's measured infrared → visible offset (a hint for dust repair).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ir_offset: Option<(i32, i32)>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<u64>,
    /// How the capture is turned to stand upright (clockwise degrees; written into the DNG's
    /// orientation, the capture itself is never rotated).
    #[serde(default, skip_serializing_if = "Rotation::is_none")]
    pub rotation: Rotation,
    /// The picture cut out of the capture, set by hand after capture: `x0, y0, x1, y1` as
    /// fractions of the capture (not turned). The frame box still places the film base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<[f64; 4]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollManifest {
    pub schema: String,
    pub roll: String,
    pub stock: String,
    pub source: CaptureSource,
    #[serde(default)]
    pub frames: Vec<ManifestFrame>,
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl RollManifest {
    pub fn new(roll: impl Into<String>, stock: impl Into<String>, source: CaptureSource) -> Self {
        Self { schema: SCHEMA.into(), roll: roll.into(), stock: stock.into(), source, frames: Vec::new() }
    }

    fn file_for(path: &Path) -> PathBuf {
        if path.is_dir() { path.join(FILE_NAME) } else { path.to_path_buf() }
    }

    /// Read `dir/roll.json` (or a manifest file path).
    pub fn load(path: &Path) -> Result<Self> {
        match Self::load_existing(path)? {
            Some(m) => Ok(m),
            None => Err(EngineError::Message(format!("{}: no such file", Self::file_for(path).display()))),
        }
    }

    /// Like [`load`](Self::load) but distinguishes *absent* from *unreadable*: `Ok(None)`
    /// only when there is no manifest file. A torn, edited or foreign-schema file is an
    /// error — callers that would otherwise start from an empty manifest and overwrite
    /// the file on their next save must fail closed instead of destroying the roll.
    pub fn load_existing(path: &Path) -> Result<Option<Self>> {
        let file = Self::file_for(path);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(EngineError::Message(format!("{}: {e}", file.display()))),
        };
        let m: Self = serde_json::from_str(&text)
            .map_err(|e| EngineError::Message(format!("{}: unreadable roll manifest ({e}); refusing to overwrite it", file.display())))?;
        if m.schema != SCHEMA {
            return Err(EngineError::Message(format!("{}: schema `{}` is not {SCHEMA}; refusing to overwrite it", file.display(), m.schema)));
        }
        Ok(Some(m))
    }

    /// Merge what is on disk into this in-memory copy before saving over it: frames only
    /// on disk are kept, and a crop set on disk (the frame viewer) wins for frames known
    /// to both. The disk copy's roll identity (roll, stock, source) wins.
    pub fn merge_disk(&mut self, disk: RollManifest) {
        self.roll = disk.roll;
        self.stock = disk.stock;
        self.source = disk.source;
        for df in disk.frames {
            match self.frames.iter_mut().find(|f| f.label == df.label) {
                Some(f) => {
                    if df.crop.is_some() {
                        f.crop = df.crop;
                    }
                }
                None => self.frames.push(df),
            }
        }
    }

    /// Write `dir/roll.json` atomically: a uniquely named temp file in the same folder,
    /// synced, renamed over the old file, then the folder synced so the rename itself
    /// survives a crash. The unique name means a stale temp (or two writers racing) can
    /// never corrupt the save.
    pub fn save(&self, dir: &Path) -> Result<PathBuf> {
        use std::io::Write as _;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = dir.join(FILE_NAME);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{FILE_NAME}.{}-{n}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &file)?;
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
            Ok::<(), std::io::Error>(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(EngineError::Message(format!("{}: {e}", file.display())));
        }
        Ok(file)
    }

    /// The roll as a develop request; relative paths resolve against `dir`.
    pub fn to_request(&self, dir: &Path, sources: ConfigSources) -> RollRequest {
        let abs = |p: &Path| if p.is_absolute() { p.to_path_buf() } else { dir.join(p) };
        let frames = self
            .frames
            .iter()
            .map(|f| FrameSource {
                path: abs(&f.capture),
                label: f.label.clone(),
                frame_box: f.frame_box,
                box_space: f.capture_size,
                ir_path: f.infrared.as_deref().map(abs),
                dpi: f.dpi,
                ir_offset: f.ir_offset,
                rotation: f.rotation,
                crop: f.crop,
            })
            .collect();
        RollRequest { roll: self.roll.clone(), stock: self.stock.clone(), frames, sources }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_resolves_paths() {
        let dir = std::env::temp_dir().join(format!("sc-manifest-{}", std::process::id()));
        let mut m = RollManifest::new("R1", "ilford-xp2-super", CaptureSource::Scanner);
        m.frames.push(ManifestFrame {
            label: "001".into(),
            capture: "001.tif".into(),
            infrared: Some("001.ir.tif".into()),
            frame_box: Some([1, 2, 30, 40]),
            capture_size: Some((32, 48)),
            dpi: Some(4800),
            region_in: Some([1.0, 2.0, 1.1, 1.6]),
            ir_offset: Some((1, 0)),
            captured_at: Some(1),
            rotation: Rotation::from_degrees(270),
            crop: Some([0.1, 0.2, 0.9, 0.8]),
        });
        m.save(&dir).unwrap();
        let back = RollManifest::load(&dir).unwrap();
        assert_eq!(back, m);
        let req = back.to_request(&dir, ConfigSources::default());
        assert_eq!(req.frames[0].path, dir.join("001.tif"));
        assert_eq!(req.frames[0].ir_path.as_deref(), Some(dir.join("001.ir.tif").as_path()));
        assert_eq!(req.frames[0].box_space, Some((32, 48)));
        assert_eq!(req.frames[0].rotation.degrees(), 270);
        assert_eq!(req.frames[0].crop, Some([0.1, 0.2, 0.9, 0.8]));
        assert!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap().contains("\"rotation\": 270"));
    }

    #[test]
    fn absent_is_none_but_corrupt_is_an_error_and_survives() {
        let dir = std::env::temp_dir().join(format!("sc-manifest-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(RollManifest::load_existing(&dir).unwrap(), None);
        // A torn write (half a JSON document) must not read as "no roll here": the
        // old behaviour started from an empty manifest and overwrote it.
        let torn = r#"{"schema":"silvercraft/roll/1","roll":"R1","frames":[{"label":"0"#;
        std::fs::write(dir.join(FILE_NAME), torn).unwrap();
        assert!(RollManifest::load_existing(&dir).is_err());
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), torn, "the corrupt file is left untouched for repair");
        // Saving does not trip over a stale fixed-name temp left by an older version.
        std::fs::create_dir_all(dir.join(format!(".{FILE_NAME}.tmp"))).unwrap();
        RollManifest::new("R2", "generic-c41", CaptureSource::Files).save(&dir).unwrap();
        assert_eq!(RollManifest::load(&dir).unwrap().roll, "R2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_disk_keeps_disk_frames_and_crops() {
        let frame = |label: &str, crop: Option<[f64; 4]>| ManifestFrame {
            label: label.into(),
            capture: format!("{label}.tif").into(),
            infrared: None,
            frame_box: None,
            capture_size: None,
            dpi: None,
            region_in: None,
            ir_offset: None,
            captured_at: None,
            rotation: Default::default(),
            crop,
        };
        let mut mem = RollManifest::new("MEM", "stock-a", CaptureSource::Scanner);
        mem.frames.push(frame("001", None));
        mem.frames.push(frame("002", None));
        let mut disk = RollManifest::new("DISK", "stock-b", CaptureSource::Scanner);
        disk.frames.push(frame("001", Some([0.1, 0.1, 0.9, 0.9])));
        disk.frames.push(frame("003", None));
        mem.merge_disk(disk);
        assert_eq!(mem.roll, "DISK");
        let labels: Vec<_> = mem.frames.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["001", "002", "003"]);
        assert_eq!(mem.frames[0].crop, Some([0.1, 0.1, 0.9, 0.9]), "a viewer crop saved mid-scan survives the scan's next save");
    }

    #[test]
    fn rejects_other_schemas_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("sc-manifest-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), r#"{"schema":"x","roll":"r","stock":"s","source":"files"}"#).unwrap();
        assert!(RollManifest::load(&dir).is_err());
        std::fs::write(dir.join(FILE_NAME), format!(r#"{{"schema":"{SCHEMA}","roll":"r","stock":"s","source":"files","bogus":1}}"#)).unwrap();
        assert!(RollManifest::load(&dir).is_err());
        assert!(RollManifest::load(&dir.join("missing")).is_err());
    }
}
