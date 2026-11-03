//! `ScanfixConfig`: every knob of scanner-artifact detection and repair, as serde.
//!
//! Keys are snake_case, enum values kebab-case. Every section is
//! `#[serde(default, deny_unknown_fields)]`, so a partial file is valid and a typo is an error.
//! Defaults are conservative: a clean scan is never altered (tested bit for bit).
//!
//! Field docs are written once, as doc comments; the `section!` macro turns them into both
//! rustdoc and the [`ScanfixConfig::field_docs`] table.

use serde::{Deserialize, Serialize};

use crate::ScanfixError;

/// One documented config field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDoc {
    /// Dotted path, e.g. "lines.max_width".
    pub path: String,
    pub doc: String,
    /// Default value rendered as TOML.
    pub default: String,
}

/// Renders a default value as a TOML literal (no `toml` dependency in the library).
trait TomlLiteral {
    fn literal(&self) -> String;
}

impl TomlLiteral for bool {
    fn literal(&self) -> String {
        self.to_string()
    }
}

impl TomlLiteral for usize {
    fn literal(&self) -> String {
        self.to_string()
    }
}

impl TomlLiteral for f32 {
    fn literal(&self) -> String {
        let s = self.to_string();
        if s.contains('.') || s.contains('e') || !self.is_finite() { s } else { format!("{s}.0") }
    }
}

impl TomlLiteral for StreakAxis {
    fn literal(&self) -> String {
        match self {
            Self::Columns => "\"columns\"".to_owned(),
            Self::Rows => "\"rows\"".to_owned(),
        }
    }
}

macro_rules! section {
    (
        $(#[doc = $sdoc:literal])*
        $name:ident = $key:literal {
            $(
                $(#[doc = $doc:literal])*
                $field:ident : $ty:ty = $default:expr
            ),* $(,)?
        }
    ) => {
        $(#[doc = $sdoc])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name {
            $(
                $(#[doc = $doc])*
                pub $field: $ty,
            )*
        }

        impl Default for $name {
            fn default() -> Self {
                Self { $( $field: $default, )* }
            }
        }

        impl $name {
            /// The TOML table name of this section.
            pub const SECTION: &'static str = $key;

            fn docs() -> Vec<FieldDoc> {
                let d = Self::default();
                let mut out = Vec::new();
                $(
                    let lines: &[&str] = &[$($doc),*];
                    let doc = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
                    out.push(FieldDoc {
                        path: format!("{}.{}", $key, stringify!($field)),
                        doc: doc.trim().to_owned(),
                        default: TomlLiteral::literal(&d.$field),
                    });
                )*
                out
            }
        }
    };
}

/// The direction scanner line defects run in.
///
/// On an Epson flatbed the raster rows are the scan lines: one CCD element images one column
/// for the whole pass, so its streak is a column and lamp/stepper banding varies by row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StreakAxis {
    /// Streaks are columns, banding is per row (Epson V800/V850 rasters).
    #[default]
    Columns,
    /// Streaks are rows, banding is per column (a raster rotated by a quarter turn).
    Rows,
}

section! {
    /// Line defects: narrow streaks along the scan direction from a bad CCD element or dust on
    /// the sensor, mirror or lens.
    LineConfig = "lines" {
        /// Detect line defects.
        detect: bool = true,
        /// Correct mild line defects with a per-column gain/offset fit to their neighbours.
        correct_gain: bool = true,
        /// Replace dead or uncorrectable line defects by interpolating their clean neighbours.
        interpolate_dead: bool = true,
        /// Pixels on each side of a pixel whose medians form its baseline. Must exceed `max_width`
        /// so a streak never dominates its own baseline.
        baseline_radius: usize = 12,
        /// Widest streak (px) accepted as a scanner defect; anything wider is image content.
        max_width: usize = 6,
        /// A pixel between two clean sides whose difference exceeds their slopes by more than this
        /// times the pixel's own deviation sits on a step edge and carries no evidence (lower is
        /// stricter on edges, less sensitive on steep gradients).
        step_ratio: f32 = 1.0,
        /// Half-length (px) of the along-line running mean of the clipped z-score.
        smooth_radius: usize = 16,
        /// Per-pixel z-scores are clipped to ±this before averaging, so content cannot dominate.
        z_clip: f32 = 4.0,
        /// Mean clipped z-score along the line that marks a pixel as part of a streak.
        z_threshold: f32 = 1.5,
        /// Floor of the relative noise (fraction of the baseline) used in the z-score, so
        /// noiseless regions cannot make an invisible deviation significant.
        min_rel_noise: f32 = 0.002,
        /// Floor of the absolute noise (transmittance) used in the z-score: about 4 codes of 16 bit,
        /// so the sensor's black-level column pattern (a few codes) is never taken for a streak.
        min_abs_noise: f32 = 0.00006,
        /// Transmittance below which the baseline is treated as this value when forming relative
        /// deviations (keeps the black holder from exploding the z-score).
        signal_floor: f32 = 0.002,
        /// Whole-column profile: a column whose mean z over the scan is at least this ...
        full_z_threshold: f32 = 0.5,
        /// ... and this many robust sigmas (1.4826·MAD) from the median of all columns is a faint
        /// full-length streak.
        full_mad_k: f32 = 6.0,
        /// Smallest relative deviation of a defect that is reported and repaired (0.3 %); fainter
        /// column patterns are invisible and left alone.
        min_deviation: f32 = 0.003,
        /// Fraction of the line's length a defect must span to count as full length.
        full_length_fraction: f32 = 0.9,
        /// Also detect partial-length streaks (dust that arrives or leaves mid-scan).
        detect_partial: bool = true,
        /// Shortest partial streak, as a fraction of the line length.
        min_partial_fraction: f32 = 0.5,
        /// A partial streak must reach the start or end of the scan (dust that lands stays until
        /// the end; dust that leaves was there from the start). Rejects poles and walls.
        partial_must_touch_edge: bool = true,
        /// Distance from the scan start/end (fraction of the line length) that still counts as
        /// touching it.
        edge_tolerance_fraction: f32 = 0.05,
        /// Fraction of a defect's extent that must be individually flagged (texture may hide a
        /// streak locally; the gaps are bridged).
        min_coverage: f32 = 0.6,
        /// Largest unflagged gap bridged inside one defect, as a fraction of the line length.
        max_gap_fraction: f32 = 0.25,
        /// A defect is split into this many segments along its length; a scanner streak keeps
        /// a constant gain, offset or stuck value across them, image content does not.
        consistency_segments: usize = 8,
        /// Largest robust coefficient of variation (1.4826·MAD / median) of the segments'
        /// deviation for a constant gain or offset, and of their values (relative to the deviation)
        /// for a stuck element. Bell-shaped content profiles exceed it.
        max_variation: f32 = 0.35,
        /// Widest group of lines that may be interpolated as dead; a wider group that does not
        /// follow the scene is image content (a frame gap seen edge-on), not a dead element.
        max_dead_width: usize = 3,
        /// A defect beside a persistent column-aligned edge more than this many times its own
        /// size (within twice `baseline_radius`) is that edge's halo and is left alone.
        halo_ratio: f32 = 10.0,
        /// Largest |gain - 1| corrected by the gain/offset fit; stronger defects are interpolated.
        max_gain_deviation: f32 = 0.5,
        /// Fitted gain below which a line is dead (its signal no longer follows the scene).
        dead_gain: f32 = 0.25,
        /// Smallest baseline spread (p90 - p10) for a full gain+offset fit; below it gain only.
        min_fit_spread: f32 = 0.05,
        /// Largest residual of a gain-corrected line against its baseline, as a multiple of what
        /// its clean neighbours show, before falling back to interpolation.
        max_fit_residual: f32 = 3.0,
    }
}

section! {
    /// Row banding: row-to-row gain flicker from the lamp or the carriage stepper.
    BandingConfig = "banding" {
        /// Detect banding.
        detect: bool = true,
        /// Correct significant banded rows with a gain.
        repair: bool = true,
        /// Half-height (rows) of the along-scan median that serves as each pixel's clean baseline.
        radius: usize = 10,
        /// Half-length (rows) of the running median that removes slow (scene) variation from the
        /// row-gain profile; bands up to this many rows tall are corrected.
        profile_radius: usize = 5,
        /// A row's gain is significant at this many standard errors (from the pixel noise, i.e.
        /// the MAD of second differences along the scan).
        z_threshold: f32 = 6.0,
        /// Smallest relative gain deviation of an isolated banded row that is corrected (0.8 %;
        /// real frames show content rows at 0.5 % that pass every other test).
        min_gain_deviation: f32 = 0.008,
        /// Banding score (robust RMS of the line gains) from which banding counts as pervasive
        /// flicker and every measured line is corrected (0.25 %; clean scans measure about 0.05 %).
        pervasive_rms: f32 = 0.0025,
        /// Largest relative gain deviation treated as banding; stronger rows are reported as
        /// strong row defects instead (thin content lines can look like this).
        max_gain_deviation: f32 = 0.05,
        /// Fraction of a row's pixels that must be usable (above `signal_floor`, below
        /// `clip_level`) to measure it.
        min_valid_fraction: f32 = 0.25,
        /// The row is split into this many segments that must each show 0.5–1.5× the row's gain.
        segments: usize = 4,
        /// Transmittance below which pixels are too dark to measure a gain on.
        signal_floor: f32 = 0.01,
        /// Transmittance at or above which pixels are treated as clipped and ignored.
        clip_level: f32 = 0.998,
        /// Longest run of adjacent strong rows still reported as a row defect; longer runs are
        /// image content (a frame gap or horizon line), never sensor transients.
        max_strong_run: usize = 2,
        /// Interpolate strong row defects (single-row transients) from the rows around them.
        interpolate_strong_rows: bool = false,
    }
}

section! {
    /// Scan health report thresholds.
    HealthConfig = "health" {
        /// Transmittance at or above which a value counts as sensor-saturated.
        clip_high: f32 = 0.998,
        /// Transmittance at or below which a value counts as black-clipped (about 3 codes of 16 bit).
        clip_low: f32 = 0.00005,
        /// Warn when more than this fraction of a channel is clipped at either end.
        warn_clipped_fraction: f32 = 0.001,
        /// Warn when a channel's p99.9 stays below this (exposure left unused).
        warn_low_peak: f32 = 0.25,
        /// Warn when the RMS row-to-row gain variation exceeds this fraction.
        warn_banding: f32 = 0.002,
        /// Values sampled for percentiles (bigger is slower and steadier).
        max_samples: usize = 1 << 20,
    }
}

/// Every knob of scanner-artifact detection and repair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanfixConfig {
    /// Master switch: when false nothing is detected or repaired (the health report still runs).
    pub enabled: bool,
    /// Direction the line defects run in.
    pub axis: StreakAxis,
    /// Also process the infrared plane when one is supplied (it shows the same streaks).
    pub process_ir: bool,
    pub lines: LineConfig,
    pub banding: BandingConfig,
    pub health: HealthConfig,
}

impl Default for ScanfixConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            axis: StreakAxis::Columns,
            process_ir: true,
            lines: LineConfig::default(),
            banding: BandingConfig::default(),
            health: HealthConfig::default(),
        }
    }
}

fn finite_in(v: f32, lo: f32, hi: f32) -> bool {
    v.is_finite() && v >= lo && v <= hi
}

impl ScanfixConfig {
    /// Every field with its doc comment and default, in declaration order.
    pub fn field_docs() -> Vec<FieldDoc> {
        let d = Self::default();
        let mut out = vec![
            FieldDoc {
                path: "enabled".into(),
                doc: "Master switch: when false nothing is detected or repaired (the health report still runs).".into(),
                default: d.enabled.literal(),
            },
            FieldDoc { path: "axis".into(), doc: "Direction the line defects run in (\"columns\" or \"rows\").".into(), default: d.axis.literal() },
            FieldDoc {
                path: "process_ir".into(),
                doc: "Also process the infrared plane when one is supplied (it shows the same streaks).".into(),
                default: d.process_ir.literal(),
            },
        ];
        out.extend(LineConfig::docs());
        out.extend(BandingConfig::docs());
        out.extend(HealthConfig::docs());
        out
    }

    /// Range checks. Detection never panics on a bad config, but running with one is refused.
    pub fn validate(&self) -> Result<(), ScanfixError> {
        let l = &self.lines;
        let b = &self.banding;
        let h = &self.health;
        let checks: &[(bool, &'static str)] = &[
            (l.baseline_radius >= 2 && l.baseline_radius <= 64, "lines.baseline_radius must be in 2..=64"),
            (l.max_width >= 1 && l.max_width < l.baseline_radius, "lines.max_width must be in 1..baseline_radius"),
            (l.smooth_radius <= 256, "lines.smooth_radius must be <= 256"),
            (finite_in(l.step_ratio, 0.0, 100.0), "lines.step_ratio must be in [0, 100]"),
            (l.consistency_segments >= 1 && l.consistency_segments <= 64, "lines.consistency_segments must be in 1..=64"),
            (finite_in(l.max_variation, 0.0, 10.0), "lines.max_variation must be in [0, 10]"),
            (finite_in(l.z_clip, 0.5, 100.0), "lines.z_clip must be in [0.5, 100]"),
            (finite_in(l.z_threshold, 0.1, 100.0) && l.z_threshold < l.z_clip, "lines.z_threshold must be in [0.1, z_clip)"),
            (finite_in(l.min_rel_noise, 1e-6, 1.0), "lines.min_rel_noise must be in [1e-6, 1]"),
            (finite_in(l.signal_floor, 1e-6, 1.0), "lines.signal_floor must be in [1e-6, 1]"),
            (finite_in(l.min_abs_noise, 0.0, 1.0), "lines.min_abs_noise must be in [0, 1]"),
            (finite_in(l.halo_ratio, 0.0, 1e6), "lines.halo_ratio must be in [0, 1e6]"),
            (finite_in(l.full_z_threshold, 0.0, 100.0), "lines.full_z_threshold must be in [0, 100]"),
            (finite_in(l.full_mad_k, 0.0, 1000.0), "lines.full_mad_k must be in [0, 1000]"),
            (finite_in(l.min_deviation, 0.0, 1.0), "lines.min_deviation must be in [0, 1]"),
            (finite_in(l.full_length_fraction, 0.01, 1.0), "lines.full_length_fraction must be in (0, 1]"),
            (finite_in(l.min_partial_fraction, 0.01, 1.0), "lines.min_partial_fraction must be in (0, 1]"),
            (finite_in(l.edge_tolerance_fraction, 0.0, 1.0), "lines.edge_tolerance_fraction must be in [0, 1]"),
            (finite_in(l.min_coverage, 0.0, 1.0), "lines.min_coverage must be in [0, 1]"),
            (finite_in(l.max_gap_fraction, 0.0, 1.0), "lines.max_gap_fraction must be in [0, 1]"),
            (finite_in(l.max_gain_deviation, 0.0, 10.0), "lines.max_gain_deviation must be in [0, 10]"),
            (finite_in(l.dead_gain, 0.0, 1.0), "lines.dead_gain must be in [0, 1]"),
            (finite_in(l.min_fit_spread, 0.0, 1.0), "lines.min_fit_spread must be in [0, 1]"),
            (finite_in(l.max_fit_residual, 0.0, 1000.0), "lines.max_fit_residual must be in [0, 1000]"),
            (b.radius >= 1 && b.radius <= 64, "banding.radius must be in 1..=64"),
            (finite_in(b.z_threshold, 0.0, 1000.0), "banding.z_threshold must be in [0, 1000]"),
            (finite_in(b.min_gain_deviation, 0.0, 1.0), "banding.min_gain_deviation must be in [0, 1]"),
            (
                finite_in(b.max_gain_deviation, 0.0, 1.0) && b.max_gain_deviation >= b.min_gain_deviation,
                "banding.max_gain_deviation must be in [min_gain_deviation, 1]",
            ),
            (finite_in(b.min_valid_fraction, 0.0, 1.0), "banding.min_valid_fraction must be in [0, 1]"),
            (finite_in(b.pervasive_rms, 0.0, 1.0), "banding.pervasive_rms must be in [0, 1]"),
            (b.max_strong_run >= 1, "banding.max_strong_run must be >= 1"),
            (b.profile_radius >= 1 && b.profile_radius <= b.radius, "banding.profile_radius must be in 1..=radius"),
            (b.segments >= 1 && b.segments <= 64, "banding.segments must be in 1..=64"),
            (finite_in(b.signal_floor, 1e-6, 1.0), "banding.signal_floor must be in [1e-6, 1]"),
            (finite_in(b.clip_level, 0.0, 10.0) && b.clip_level > b.signal_floor, "banding.clip_level must exceed signal_floor"),
            (finite_in(h.clip_high, 0.0, 10.0), "health.clip_high must be in [0, 10]"),
            (finite_in(h.clip_low, 0.0, 1.0) && h.clip_low < h.clip_high, "health.clip_low must be in [0, clip_high)"),
            (finite_in(h.warn_clipped_fraction, 0.0, 1.0), "health.warn_clipped_fraction must be in [0, 1]"),
            (finite_in(h.warn_low_peak, 0.0, 1.0), "health.warn_low_peak must be in [0, 1]"),
            (finite_in(h.warn_banding, 0.0, 1.0), "health.warn_banding must be in [0, 1]"),
            (h.max_samples >= 1024, "health.max_samples must be >= 1024"),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, msg)) => Err(ScanfixError::Config(msg)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        ScanfixConfig::default().validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_values() {
        let mut c = ScanfixConfig::default();
        c.lines.z_threshold = f32::NAN;
        assert!(c.validate().is_err());
        let mut c = ScanfixConfig::default();
        c.lines.max_width = 40;
        assert!(c.validate().is_err());
        let mut c = ScanfixConfig::default();
        c.banding.radius = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn field_docs_cover_every_field() {
        let docs = ScanfixConfig::field_docs();
        assert!(docs.iter().all(|d| !d.doc.is_empty()), "undocumented field");
        // Every key the serialized default has appears in the docs table, and vice versa.
        let value = toml::Table::try_from(ScanfixConfig::default()).unwrap();
        let mut keys = Vec::new();
        for (k, v) in &value {
            match v.as_table() {
                Some(t) => keys.extend(t.keys().map(|f| format!("{k}.{f}"))),
                None => keys.push(k.clone()),
            }
        }
        let mut paths: Vec<String> = docs.iter().map(|d| d.path.clone()).collect();
        keys.sort();
        paths.sort();
        assert_eq!(keys, paths);
        // Rendered defaults parse back to the default value.
        for d in &docs {
            let snippet = match d.path.split_once('.') {
                Some((section, field)) => format!("[{section}]\n{field} = {}\n", d.default),
                None => format!("{} = {}\n", d.path, d.default),
            };
            let parsed: ScanfixConfig = toml::from_str(&snippet).unwrap_or_else(|e| panic!("{}: {e}", d.path));
            assert_eq!(parsed, ScanfixConfig::default(), "{}", d.path);
        }
        let mw = docs.iter().find(|d| d.path == "lines.max_width").unwrap();
        assert_eq!(mw.default, "6");
    }

    #[test]
    fn unknown_fields_are_errors_and_partial_files_valid() {
        assert!(toml::from_str::<ScanfixConfig>("[lines]\nmax_widht = 3\n").is_err());
        assert!(toml::from_str::<ScanfixConfig>("bogus = 1\n").is_err());
        let c: ScanfixConfig = toml::from_str("axis = \"rows\"\n[banding]\nrepair = false\n").unwrap();
        assert_eq!(c.axis, StreakAxis::Rows);
        assert!(!c.banding.repair);
        assert_eq!(c.lines, LineConfig::default());
    }
}
