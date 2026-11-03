//! Film base (orange mask / D-min) estimation, spec Section 1.
//!
//! Ordered by trust: the inter-frame gap base sampled from the capture
//! margins ([`sample_film_base`], with the auto rebate band for camera scans),
//! aggregated per roll by median ([`aggregate_roll_base`]); then the per-frame
//! content p95 clamped to a roll hint ([`estimate_frame_base`]). Also the
//! rebate trims and the capture-channel health guard.

use crate::image::RgbImage;
use crate::stats::{channel_percentiles, median_f64, percentile_f64};

/// The smallest signal the pipeline can represent: half of one 16-bit code.
/// Load-bearing: without it a zero pixel reads density 6..12 and poisons
/// every percentile anchor.
pub const SENSOR_FLOOR: f32 = 0.5 / 65535.0;

/// Per-frame base estimates are clamped to `[hint / 1.25, hint * 1.25]`.
pub const BASE_HINT_RANGE: f32 = 1.25;

/// A healthy negative record reaches p95 density >= 0.30 above base.
pub const HEALTH_OK_P95: f32 = 0.30;
/// Below this base transmission the channel is measuring read noise.
pub const HEALTH_FLOORED_BASE: f32 = 0.01;

/// Which margin rows to sample for the gap base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseBand {
    /// Every row above `y0` and below `y1` (flatbed / strip scanner: the
    /// whole margin is gap).
    FullMargin,
    /// Only this many rows adjacent to the frame (camera scan, known rebate).
    Rows(usize),
    /// Locate the rebate by walking outward from the frame: camera scan /
    /// tight gate with a dark holder.
    Auto,
}

#[derive(Debug, Clone, Copy)]
pub struct FilmBaseSampleOptions {
    /// Frame box top row (exclusive end of the top margin).
    pub y0: usize,
    /// Frame box bottom row (start of the bottom margin).
    pub y1: usize,
    pub percentile: f64,
    pub min_margin_pixels: usize,
    pub max_samples: usize,
    pub band: BaseBand,
    /// Auto band: rows walked outward as a fraction of the frame height (min 8).
    pub auto_reach: f64,
    /// Auto band: keep rows within this fraction of the brightest row median.
    pub auto_keep: f64,
}

impl FilmBaseSampleOptions {
    /// Defaults: p80, >= 1000 margin pixels, <= 300k samples.
    pub fn new(y0: usize, y1: usize) -> Self {
        Self {
            y0,
            y1,
            percentile: 80.0,
            min_margin_pixels: 1000,
            max_samples: 300_000,
            band: BaseBand::FullMargin,
            auto_reach: 0.22,
            auto_keep: 0.55,
        }
    }

    /// Sampling options for a `[base]` config section and a frame box rows
    /// `[y0, y1)`. `None` for methods that do not sample margins
    /// (content, explicit).
    pub fn from_config(config: &crate::config::BaseConfig, y0: usize, y1: usize) -> Option<Self> {
        use crate::config::BaseMethod;
        let band = match config.method {
            BaseMethod::Gap => BaseBand::FullMargin,
            BaseMethod::AutoBand => BaseBand::Auto,
            BaseMethod::Rows => BaseBand::Rows(config.rows),
            BaseMethod::Content | BaseMethod::Explicit => return None,
        };
        Some(Self {
            y0,
            y1,
            percentile: config.percentile,
            min_margin_pixels: config.min_margin_pixels,
            max_samples: config.max_samples,
            band,
            auto_reach: config.auto_reach,
            auto_keep: config.auto_keep,
        })
    }
}

/// Gap-base sample from the capture margins (rows `[0, y0)` and `[y1, H)`)
/// over 0..1 f32 input.
///
/// Returns `None` when there is not enough margin (or nothing finite in it):
/// the caller must then require an explicit dmin rather than silently sampling
/// image content. Non-finite pixels are skipped.
pub fn sample_film_base(image: &RgbImage, options: &FilmBaseSampleOptions) -> Option<[f64; 3]> {
    let height = image.height();
    let y0 = options.y0.min(height);
    let y1 = options.y1.clamp(y0, height);
    let (above, below): (Vec<usize>, Vec<usize>) = match options.band {
        BaseBand::Auto => return auto_film_base_with(image, y0, y1, options.auto_reach, options.auto_keep),
        BaseBand::FullMargin | BaseBand::Rows(0) => ((0..y0).collect(), (y1..height).collect()),
        BaseBand::Rows(band) => ((y0.saturating_sub(band)..y0).collect(), (y1..(y1.saturating_add(band)).min(height)).collect()),
    };
    let all: Vec<usize> = above.iter().chain(&below).copied().collect();
    let both = margin_base(image, &all, options)?;
    // A frame at the end of a strip has the film's end on one side: bare light, brighter than
    // any film base. When the two margins clearly disagree, the darker one is the film.
    let half = FilmBaseSampleOptions { min_margin_pixels: (options.min_margin_pixels / 2).max(1), ..*options };
    if let (Some(a), Some(b)) = (margin_base(image, &above, &half), margin_base(image, &below, &half)) {
        let ratio = (0..3).map(|c| (a[c].max(1e-6) / b[c].max(1e-6)).ln().abs()).fold(0.0, f64::max);
        if ratio > SIDE_DISAGREEMENT.ln() {
            let mean = |v: &[f64; 3]| v.iter().sum::<f64>();
            return Some(if mean(&a) < mean(&b) { a } else { b });
        }
    }
    Some(both)
}

/// Margins whose base differs by more than this factor in some channel are not the same film.
const SIDE_DISAGREEMENT: f64 = 1.25;

/// Per-channel percentile over `rows` of the image (subsampled), or `None` if too few pixels.
fn margin_base(image: &RgbImage, rows: &[usize], options: &FilmBaseSampleOptions) -> Option<[f64; 3]> {
    let width = image.width();
    let margin_pixels = rows.len() * width;
    if margin_pixels < options.min_margin_pixels || margin_pixels == 0 {
        return None;
    }
    let step = (margin_pixels / options.max_samples.max(1)).max(1);
    let data = image.data();
    let mut channels = [Vec::new(), Vec::new(), Vec::new()];
    for margin_index in (0..margin_pixels).step_by(step) {
        let row = rows[margin_index / width];
        let pixel_index = row * width + margin_index % width;
        for channel in 0..3 {
            let value = data[pixel_index * 3 + channel];
            if value.is_finite() {
                channels[channel].push(f64::from(value));
            }
        }
    }
    if channels.iter().any(Vec::is_empty) {
        return None;
    }
    Some(channels.map(|mut values| percentile_f64(&mut values, options.percentile)))
}

#[cfg(test)]
mod side_tests {
    use super::*;

    /// A frame at a strip end: film base above, bare light below.
    #[test]
    fn bare_light_past_the_film_end_is_not_film_base() {
        let (w, h) = (40, 100);
        let mut data = Vec::new();
        for y in 0..h {
            for _ in 0..w {
                let px = if y < 20 {
                    [0.47, 0.38, 0.52]
                } else if y < 80 {
                    [0.2, 0.15, 0.25]
                } else {
                    [0.93, 0.93, 0.93]
                };
                data.extend_from_slice(&px);
            }
        }
        let img = RgbImage::new(w, h, data).unwrap();
        let mut o = FilmBaseSampleOptions::new(20, 80);
        o.min_margin_pixels = 100;
        let b = sample_film_base(&img, &o).unwrap();
        assert!((b[1] - 0.38).abs() < 1e-3, "{b:?}");
        // both margins film: unchanged behaviour (pooled percentile)
        let same: Vec<f32> = (0..w * h).flat_map(|i| if (20..80).contains(&(i / w)) { [0.2, 0.15, 0.25] } else { [0.47, 0.38, 0.52] }).collect();
        let b = sample_film_base(&RgbImage::new(w, h, same).unwrap(), &o).unwrap();
        assert!((b[0] - 0.47).abs() < 1e-3);
    }
}

/// Walk outward from the frame
/// `limit = max(8, (y1-y0)*0.22)` rows each side (skipping the 2 rows next to
/// the box), take per-row medians over all values, keep rows within 55% of the
/// brightest; fewer than 3 -> the single brightest row. Base = per-channel
/// median of the kept rows. Rebate is the brightest thing outside the frame,
/// the holder the darkest.
pub fn auto_film_base(image: &RgbImage, y0: usize, y1: usize) -> Option<[f64; 3]> {
    auto_film_base_with(image, y0, y1, 0.22, 0.55)
}

/// [`auto_film_base`] with explicit reach/keep fractions.
///
/// Candidate rows are strictly outside the frame, skipping the 2 rows adjacent to
/// the box. When the frame touches an image edge there simply are no candidates on
/// that side; when it touches both, the result is `None` (insufficient rebate) —
/// an earlier version clamped the ranges back *into* the frame and reported
/// picture content as a confident film base.
pub fn auto_film_base_with(image: &RgbImage, y0: usize, y1: usize, reach: f64, keep: f64) -> Option<[f64; 3]> {
    let height = image.height();
    let y0 = y0.min(height);
    let y1 = y1.clamp(y0, height);
    if y1 == y0 {
        return None; // an empty (or inverted) frame has no rebate to find
    }
    let reach = if reach.is_finite() { reach.max(0.0) } else { 0.0 };
    let limit = 8usize.max((y1.saturating_sub(y0) as f64 * reach) as usize);
    let above = y0.saturating_sub(limit)..y0.saturating_sub(2);
    let below = y1.saturating_add(2).min(height)..y1.saturating_add(limit).min(height);
    let mut rows = Vec::new();
    let mut medians = Vec::new();
    for row in above.chain(below) {
        let mut values: Vec<f64> = image.row(row).iter().filter(|v| v.is_finite()).map(|&v| f64::from(v)).collect();
        if values.is_empty() {
            continue;
        }
        rows.push(row);
        medians.push(median_f64(&mut values));
    }
    if rows.is_empty() {
        return None;
    }
    let peak = medians.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if peak.is_nan() || peak <= 0.0 {
        return None;
    }
    let mut chosen: Vec<usize> = rows.iter().zip(&medians).filter(|(_, m)| **m >= keep * peak).map(|(r, _)| *r).collect();
    if chosen.len() < 3 {
        // np.argmax: the FIRST maximal row.
        let best = medians.iter().enumerate().fold(0, |best, (i, m)| if *m > medians[best] { i } else { best });
        chosen = vec![rows[best]];
    }
    let mut channels = [Vec::new(), Vec::new(), Vec::new()];
    for row in chosen {
        for pixel in image.row(row).chunks_exact(3) {
            for channel in 0..3 {
                if pixel[channel].is_finite() {
                    channels[channel].push(f64::from(pixel[channel]));
                }
            }
        }
    }
    if channels.iter().any(Vec::is_empty) {
        return None;
    }
    Some(channels.map(|mut values| median_f64(&mut values)))
}

/// Roll aggregation: the per-channel median of the frames' gap bases
/// (non-finite or non-positive entries ignored). `None` if nothing usable.
pub fn aggregate_roll_base(bases: &[[f64; 3]]) -> Option<[f64; 3]> {
    let mut out = [0.0; 3];
    for (channel, slot) in out.iter_mut().enumerate() {
        let mut values: Vec<f64> = bases.iter().map(|b| b[channel]).filter(|v| v.is_finite() && *v > 0.0).collect();
        if values.is_empty() {
            return None;
        }
        *slot = median_f64(&mut values);
    }
    Some(out)
}

/// Clamp a per-frame base to within [`BASE_HINT_RANGE`] of the roll hint.
pub fn clamp_to_hint(base: [f32; 3], hint: [f32; 3], range: f32, epsilon: f32) -> [f32; 3] {
    let mut out = base;
    for channel in 0..3 {
        let reference = hint[channel].max(epsilon);
        out[channel] = crate::stats::clamp_f32(base[channel], reference / range, reference * range);
    }
    out
}

/// Per-frame fallback base: explicit `dmin`
/// wins; otherwise the per-channel p95 of the frame, clamped to the roll hint
/// when one exists.
pub fn estimate_frame_base(rgb: &[f32], dmin: Option<[f32; 3]>, hint: Option<[f32; 3]>, hint_range: f32, epsilon: f32) -> [f32; 3] {
    if let Some(dmin) = dmin {
        return dmin;
    }
    let base = channel_percentiles(rgb, 95.0);
    match hint {
        Some(hint) => clamp_to_hint(base, hint, hint_range, epsilon),
        None => base,
    }
}

/// True when a base can be divided by: finite and above the sensor floor in
/// every channel.
pub fn base_is_usable(base: [f64; 3]) -> bool {
    base.iter().all(|v| v.is_finite() && *v > f64::from(SENSOR_FLOOR))
}

/// Rebate trims `[top, bottom, left, right]` on a POSITIVE: a line is
/// edge when its grey mean is under half the
/// interior (20-80% box) median and its std < 0.11; walk inward up to
/// `max_trim`, then add `clearance` on trimmed sides. Images too small to
/// have an interior return zeros.
pub fn film_base_trims(image: &RgbImage, max_trim: usize, clearance: usize) -> [usize; 4] {
    let (height, width) = (image.height(), image.width());
    let grey: Vec<f32> = image.data().chunks_exact(3).map(|p| (p[0] + p[1] + p[2]) / 3.0).collect();
    let x0 = (width as f32 * 0.2) as usize;
    let x1 = (width as f32 * 0.8) as usize;
    let y0 = (height as f32 * 0.2) as usize;
    let y1 = (height as f32 * 0.8) as usize;
    if x1 <= x0 || y1 <= y0 {
        return [0; 4];
    }
    let mut interior: Vec<f32> = (y0..y1).flat_map(|y| grey[y * width + x0..y * width + x1].iter().copied()).collect();
    let interior_median = crate::stats::percentile_f32(&mut interior, 50.0);
    let is_edge = |values: &[f32]| {
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let variance = values.iter().map(|value| (value - mean).powi(2)).sum::<f32>() / values.len() as f32;
        mean < interior_median * 0.5 && variance.sqrt() < 0.11
    };
    let limit_y = max_trim.min(height.saturating_sub(1));
    let limit_x = max_trim.min(width.saturating_sub(1));
    let mut top = 0;
    while top < limit_y && is_edge(&grey[top * width + x0..top * width + x1]) {
        top += 1;
    }
    let mut bottom = 0;
    while bottom < limit_y && is_edge(&grey[(height - 1 - bottom) * width + x0..(height - 1 - bottom) * width + x1]) {
        bottom += 1;
    }
    let column = |x: usize| (y0..y1).map(|y| grey[y * width + x]).collect::<Vec<_>>();
    let mut left = 0;
    while left < limit_x && is_edge(&column(left)) {
        left += 1;
    }
    let mut right = 0;
    while right < limit_x && is_edge(&column(width - 1 - right)) {
        right += 1;
    }
    let pad = |trim: usize| if trim > 0 { trim.saturating_add(clearance) } else { 0 };
    [pad(top), pad(bottom), pad(left), pad(right)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    Ok,
    /// The dye record's density range collapsed (p95 below [`HEALTH_OK_P95`]):
    /// the capture light does not overlap this dye's absorption band.
    Compressed,
    /// The base itself sits at the noise floor: the channel got almost no light.
    Floored,
}

#[derive(Debug, Clone, Copy)]
pub struct ChannelHealth {
    pub base: [f32; 3],
    pub density_p50: [f32; 3],
    pub density_p95: [f32; 3],
    pub status: [ChannelStatus; 3],
}

/// Capture channel health (illuminant guard): base = p90 of `rebate`,
/// floored densities of `image`, status per channel. A capture-time warning.
pub fn capture_channel_health(image: &[f32], rebate: &[f32]) -> ChannelHealth {
    let base = channel_percentiles(rebate, 90.0);
    let mut density = Vec::with_capacity(image.len());
    for pixel in image.chunks_exact(3) {
        for channel in 0..3 {
            let transmittance = (pixel[channel].max(SENSOR_FLOOR) / base[channel].max(1e-6)).clamp(1e-6, 1.0);
            density.push(-transmittance.log10());
        }
    }
    let density_p50 = channel_percentiles(&density, 50.0);
    let density_p95 = channel_percentiles(&density, 95.0);
    let mut status = [ChannelStatus::Ok; 3];
    for channel in 0..3 {
        status[channel] = if base[channel].is_nan() || base[channel] < HEALTH_FLOORED_BASE {
            ChannelStatus::Floored
        } else if density_p95[channel].is_nan() || density_p95[channel] < HEALTH_OK_P95 {
            ChannelStatus::Compressed
        } else {
            ChannelStatus::Ok
        };
    }
    ChannelHealth { base, density_p50, density_p95, status }
}

impl ChannelHealth {
    /// One-line summary for logs/UI: e.g. "R:compressed B:floored".
    pub fn summary(&self) -> String {
        const NAMES: [&str; 3] = ["R", "G", "B"];
        let mut parts = Vec::new();
        for channel in 0..3 {
            match self.status[channel] {
                ChannelStatus::Ok => {}
                ChannelStatus::Compressed => parts.push(format!("{}:compressed", NAMES[channel])),
                ChannelStatus::Floored => parts.push(format!("{}:floored", NAMES[channel])),
            }
        }
        if parts.is_empty() { "ok".to_string() } else { parts.join(" ") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An orange-base strip: bright rebate rows above/below a darker frame.
    fn strip(width: usize, height: usize, y0: usize, y1: usize, base: [f32; 3]) -> RgbImage {
        let mut data = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                if y < y0 || y >= y1 {
                    let jitter = 1.0 + 0.01 * (((x * 7 + y * 13) % 11) as f32 / 10.0 - 0.5);
                    data.extend(base.map(|b| b * jitter));
                } else {
                    let t = ((x * 31 + y * 17) % 100) as f32 / 100.0;
                    data.extend(base.map(|b| b * (0.05 + 0.6 * t)));
                }
            }
        }
        RgbImage::new(width, height, data).unwrap()
    }

    #[test]
    fn gap_base_recovers_orange_mask_and_declines_without_margin() {
        let base = [0.80, 0.55, 0.35];
        let image = strip(100, 80, 15, 65, base);
        let sampled = sample_film_base(&image, &FilmBaseSampleOptions::new(15, 65)).unwrap();
        assert!(sampled[0] > sampled[1] && sampled[1] > sampled[2], "{sampled:?}");
        for c in 0..3 {
            assert!((sampled[c] / f64::from(base[c]) - 1.0).abs() < 0.01);
        }
        // No margin -> None (caller must require an explicit dmin).
        assert!(sample_film_base(&image, &FilmBaseSampleOptions::new(0, 80)).is_none());
        // Band limited to rows next to the frame still finds the rebate.
        let mut opts = FilmBaseSampleOptions::new(15, 65);
        opts.band = BaseBand::Rows(12);
        let banded = sample_film_base(&image, &opts).unwrap();
        assert!((banded[0] / 0.8 - 1.0).abs() < 0.01);
    }

    #[test]
    fn auto_band_finds_thin_rebate_and_ignores_dark_holder() {
        // Rows: holder (black) | thin rebate (4 rows) | frame | rebate | holder.
        let (width, height) = (60, 120);
        let base = [0.32_f32, 0.09, 0.04];
        let mut data = Vec::new();
        for y in 0..height {
            for _x in 0..width {
                let px = if (17..20).contains(&y) || (100..103).contains(&y) {
                    base
                } else if (20..100).contains(&y) {
                    base.map(|b| b * 0.3)
                } else {
                    [0.004, 0.001, 0.0]
                };
                data.extend(px);
            }
        }
        let image = RgbImage::new(width, height, data).unwrap();
        let full = sample_film_base(&image, &FilmBaseSampleOptions::new(20, 100)).unwrap();
        // The full-margin p80 is dominated by the holder: near black.
        assert!(full[0] < 0.1, "{full:?}");
        let mut opts = FilmBaseSampleOptions::new(20, 100);
        opts.band = BaseBand::Auto;
        let auto = sample_film_base(&image, &opts).unwrap();
        for c in 0..3 {
            assert!((auto[c] - f64::from(base[c])).abs() < 1e-6, "{auto:?}");
        }
    }

    #[test]
    fn auto_band_at_the_image_edges_never_samples_the_frame() {
        // Frame filling the whole height: there is no rebate anywhere, so no base —
        // not the brightest row of picture content.
        let image = strip(40, 80, 0, 80, [0.8, 0.55, 0.35]);
        let mut opts = FilmBaseSampleOptions::new(0, 80);
        opts.band = BaseBand::Auto;
        assert!(sample_film_base(&image, &opts).is_none());
        // Bright picture content above, true (darker) rebate below: the frame's own
        // first row must not win just for being brightest.
        let (w, h) = (40, 100);
        let mut data = Vec::new();
        for y in 0..h {
            for _ in 0..w {
                data.extend(if y < 60 { [0.9, 0.9, 0.9] } else { [0.47, 0.38, 0.52] });
            }
        }
        let img = RgbImage::new(w, h, data).unwrap();
        let b = auto_film_base(&img, 0, 60).expect("rebate below the frame");
        assert!((b[0] - 0.47).abs() < 1e-6, "{b:?}");
        // Frame at the bottom edge: rebate above is found, content below ignored.
        let b = auto_film_base(&img, 70, 100).expect("rebate above the frame");
        assert!((b[0] - 0.47).abs() < 1e-6, "{b:?}");
        // Degenerate boxes decline instead of panicking.
        assert!(auto_film_base(&img, 100, 100).is_none());
        assert!(auto_film_base(&img, 50, 10).is_none());
    }

    #[test]
    fn roll_aggregation_is_channel_median_and_skips_garbage() {
        let bases = [[0.8, 0.5, 0.3], [0.82, 0.52, 0.31], [0.1, f64::NAN, 0.29], [0.81, 0.51, 0.0]];
        let agg = aggregate_roll_base(&bases).unwrap();
        assert!((agg[0] - 0.805).abs() < 1e-12);
        assert!((agg[1] - 0.51).abs() < 1e-12);
        assert!((agg[2] - 0.30).abs() < 1e-12);
        assert!(aggregate_roll_base(&[]).is_none());
        assert!(aggregate_roll_base(&[[f64::NAN; 3]]).is_none());
    }

    #[test]
    fn frame_base_p95_is_clamped_to_the_roll_hint() {
        // A dark scene: p95 far below the true base.
        let rgb: Vec<f32> = (0..1000).flat_map(|i| [0.2 + 0.0001 * i as f32, 0.1, 0.05]).collect();
        let free = estimate_frame_base(&rgb, None, None, BASE_HINT_RANGE, 1e-6);
        assert!(free[0] < 0.4);
        let hinted = estimate_frame_base(&rgb, None, Some([0.8, 0.5, 0.3]), BASE_HINT_RANGE, 1e-6);
        assert!((hinted[0] - 0.8 / 1.25).abs() < 1e-6);
        assert!((hinted[2] - 0.3 / 1.25).abs() < 1e-6);
        assert_eq!(estimate_frame_base(&rgb, Some([0.7, 0.4, 0.2]), Some([0.8, 0.5, 0.3]), BASE_HINT_RANGE, 1e-6), [0.7, 0.4, 0.2]);
    }

    #[test]
    fn trims_remove_a_dark_strip_only() {
        let (width, height) = (100, 80);
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let v = if x < 25 { 0.05 } else { 0.5 + 0.2 * (((x + y) % 7) as f32 / 7.0) };
                data.extend([v, v, v]);
            }
        }
        let image = RgbImage::new(width, height, data).unwrap();
        // The interior box (x 20..80) includes 5 dark columns but the median
        // is still bright; left strip trimmed (25 + clearance 7), others not.
        assert_eq!(film_base_trims(&image, 70, 7), [0, 0, 32, 0]);
        let tiny = RgbImage::filled(2, 2, [0.0; 3]).unwrap();
        assert_eq!(film_base_trims(&tiny, 70, 7), [0; 4]);
    }

    #[test]
    fn capture_channel_health_flags_compressed_and_floored_records() {
        let rebate: Vec<f32> = (0..3000).flat_map(|i| [0.58 + 0.04 * (i as f32 / 3000.0), 0.34, 0.28]).collect();
        let healthy: Vec<f32> = (0..3000)
            .flat_map(|i| {
                let t = i as f32 / 3000.0;
                [0.58 * (0.05 + 0.9 * t), 0.34 * (0.05 + 0.9 * t), 0.28 * (0.05 + 0.9 * t)]
            })
            .collect();
        let report = capture_channel_health(&healthy, &rebate);
        assert_eq!(report.status, [ChannelStatus::Ok; 3], "{}", report.summary());
        let compressed: Vec<f32> = (0..3000)
            .flat_map(|i| {
                let t = i as f32 / 3000.0;
                [0.58 * (0.55 + 0.4 * t), 0.34 * (0.05 + 0.9 * t), 0.28 * (0.05 + 0.9 * t)]
            })
            .collect();
        let report = capture_channel_health(&compressed, &rebate);
        assert_eq!(report.status, [ChannelStatus::Compressed, ChannelStatus::Ok, ChannelStatus::Ok]);
        assert_eq!(report.summary(), "R:compressed");
        let rebate_dim: Vec<f32> = (0..3000).flat_map(|i| [0.24 + 0.02 * (i as f32 / 3000.0), 0.043, 0.004]).collect();
        let dim_image: Vec<f32> = (0..3000)
            .flat_map(|i| {
                let t = i as f32 / 3000.0;
                [0.24 * (0.05 + 0.9 * t), 0.043 * (0.05 + 0.9 * t), 0.004 * (0.3 + 0.6 * t)]
            })
            .collect();
        let report = capture_channel_health(&dim_image, &rebate_dim);
        assert_eq!(report.status[2], ChannelStatus::Floored);
        // Empty input degrades to "floored", never panics.
        assert_eq!(capture_channel_health(&[], &[]).status, [ChannelStatus::Floored; 3]);
    }
}
