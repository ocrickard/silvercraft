//! 16-bit TIFF writers for captures (lossless originals beside the roll manifest).

use std::io::BufWriter;
use std::path::Path;

use crate::Result;

/// Write interleaved 16-bit RGB atomically (temp file + rename).
pub fn write_rgb16(path: &Path, width: usize, height: usize, data: &[u16]) -> Result<()> {
    write_with(path, |f| {
        let mut enc = tiff::encoder::TiffEncoder::new(f).map_err(|e| e.to_string())?;
        enc.write_image::<tiff::encoder::colortype::RGB16>(dim(width)?, dim(height)?, data).map_err(|e| e.to_string())
    })
}

/// Write a 16-bit single-channel image atomically.
pub fn write_gray16(path: &Path, width: usize, height: usize, data: &[u16]) -> Result<()> {
    write_with(path, |f| {
        let mut enc = tiff::encoder::TiffEncoder::new(f).map_err(|e| e.to_string())?;
        enc.write_image::<tiff::encoder::colortype::Gray16>(dim(width)?, dim(height)?, data).map_err(|e| e.to_string())
    })
}

fn dim(v: usize) -> std::result::Result<u32, String> {
    u32::try_from(v).map_err(|_| format!("image dimension {v} is too large for TIFF"))
}

fn write_with(path: &Path, f: impl FnOnce(&mut BufWriter<std::fs::File>) -> std::result::Result<(), String>) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut w = BufWriter::new(file);
    f(&mut w).map_err(|e| format!("{}: {e}", path.display()))?;
    w.into_inner().map_err(|e| format!("{}: {e}", path.display()))?.sync_all().map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}
