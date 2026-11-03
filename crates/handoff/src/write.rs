//! Writing a finished frame to disk: the `<stem>.xmp` sidecar first (LightCraft reads it when it
//! prepares the import), then the DNG through a dot-prefixed temporary file that is synced and
//! renamed, so a watched folder never sees half a file.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::dng::{DngFrameInfo, LinearFrame};
use crate::xmp::XmpPacket;
use crate::{HandoffError, Result};

/// Write bytes to `path` atomically (temporary dotfile in the same folder, fsync, rename).
///
/// The temporary name is unique per call (pid + counter) and created with `O_EXCL`, so
/// two writers of the same path — or a stale `.part` from a crashed run, or a planted
/// file/symlink at the predictable name — can never alias, truncate or redirect a write.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| HandoffError::Invalid(format!("no file name in {}", path.display())))?;
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..16 {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{}.{n}-{}.part", name.to_string_lossy(), std::process::id()));
        let opened = fs::OpenOptions::new().write(true).create_new(true).open(&tmp);
        let mut f = match opened {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let result = (|| {
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, path)?;
            if let Ok(d) = File::open(dir) {
                // directory fsync makes the rename durable; not supported everywhere (Windows)
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
    Err(last_err.map_or_else(|| HandoffError::Invalid("could not create a temporary file".into()), Into::into))
}

/// Where the XMP packet goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmpPlacement {
    /// `<stem>.xmp` beside the DNG.
    pub sidecar: bool,
    /// Embedded in the DNG, so the file carries its metadata when moved alone.
    pub embed: bool,
}

impl Default for XmpPlacement {
    fn default() -> Self {
        Self { sidecar: true, embed: true }
    }
}

/// Write `<dir>/<stem>.dng` (and `<dir>/<stem>.xmp` when asked); returns the DNG path.
pub fn write_frame(dir: &Path, stem: &str, frame: &LinearFrame<'_>, info: &DngFrameInfo, xmp: &XmpPacket) -> Result<PathBuf> {
    write_frame_with(dir, stem, frame, info, xmp, XmpPlacement::default())
}

pub fn write_frame_with(
    dir: &Path,
    stem: &str,
    frame: &LinearFrame<'_>,
    info: &DngFrameInfo,
    xmp: &XmpPacket,
    placement: XmpPlacement,
) -> Result<PathBuf> {
    if stem.is_empty() || stem.starts_with('.') || stem.contains(['/', '\\']) {
        return Err(HandoffError::Invalid(format!("bad file stem {stem:?}")));
    }
    fs::create_dir_all(dir)?;
    let packet = xmp.to_xml();
    let info = DngFrameInfo { xmp: placement.embed.then(|| packet.clone()), ..info.clone() };
    let bytes = crate::dng::encode(frame, &info)?;
    let side = dir.join(format!("{stem}.xmp"));
    if placement.sidecar {
        write_atomic(&side, packet.as_bytes())?;
    } else if side.is_file() {
        // a stale sidecar from an earlier run would override the new DNG's metadata in LightCraft
        fs::remove_file(&side)?;
    }
    let dng = dir.join(format!("{stem}.dng"));
    write_atomic(&dng, &bytes)?;
    Ok(dng)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_and_sidecar_land_with_no_temporaries() {
        let dir = std::env::temp_dir().join(format!("sc-write-{}", std::process::id()));
        let rgb: Vec<f32> = (0..32 * 16 * 3).map(|i| (i % 97) as f32 / 50.0).collect();
        let xmp = XmpPacket { title: Some("Roll 1 · 3".into()), keywords: vec!["roll:1".into()], ..Default::default() };
        let path = write_frame(&dir, "R001-03", &LinearFrame { width: 32, height: 16, rgb: &rgb }, &DngFrameInfo::default(), &xmp).unwrap();
        let names: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let raw = lightcraft_raw::decode(&fs::read(&path).unwrap()).unwrap();
        assert_eq!((raw.width, raw.height), (32, 16));
        let side = fs::read_to_string(dir.join("R001-03.xmp")).unwrap();
        assert_eq!(lightcraft_meta::parse_xmp(&side).unwrap().metadata.keywords, vec!["roll:1"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_part_files_never_block_or_redirect_a_write() {
        let dir = std::env::temp_dir().join(format!("sc-write-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The old predictable temporary name, occupied (a crashed run, or planted).
        fs::create_dir_all(dir.join(".f.txt.part")).unwrap();
        write_atomic(&dir.join("f.txt"), b"hello").unwrap();
        assert_eq!(fs::read(dir.join("f.txt")).unwrap(), b"hello");
        // Concurrent writers of the same path: both succeed, the result is one whole
        // payload, and no temporaries are left behind.
        let path = dir.join("g.txt");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || write_atomic(&p, format!("writer-{i}").as_bytes()))
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        let got = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        assert!(got.starts_with("writer-") && got.len() == 8, "{got:?}");
        let parts: Vec<_> =
            fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(".g.txt.")).collect();
        assert!(parts.is_empty(), "{parts:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_stems_are_refused() {
        let rgb = vec![0.1f32; 3];
        let f = LinearFrame { width: 1, height: 1, rgb: &rgb };
        for stem in ["", ".hidden", "a/b", "a\\b"] {
            assert!(write_frame(Path::new("/nonexistent"), stem, &f, &DngFrameInfo::default(), &XmpPacket::default()).is_err());
        }
    }
}
