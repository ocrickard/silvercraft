//! `DetectConfig`: every knob of frame detection, as serde (TOML in the app).
//!
//! Keys are snake_case, enum values kebab-case. Every section is
//! `#[serde(default, deny_unknown_fields)]`, so a partial file is valid and a typo is an error.
//! Field docs are written once, as doc comments; the `section!` macro turns them into both
//! rustdoc and the [`DetectConfig::field_docs`] table (same scheme as `silvercraft-film`).

use serde::{Deserialize, Serialize};

/// One documented config field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDoc {
    /// Dotted path, e.g. "bed.holder".
    pub path: String,
    pub doc: String,
    /// Default value rendered as TOML.
    pub default: String,
}

/// TOML rendering of a default value for [`FieldDoc`] (this crate does not depend on `toml`).
trait DocValue {
    fn render(&self) -> String;
}

impl DocValue for f64 {
    fn render(&self) -> String {
        let s = format!("{self}");
        if self.is_finite() && !s.contains(['.', 'e', 'E']) { format!("{s}.0") } else { s }
    }
}
impl DocValue for usize {
    fn render(&self) -> String {
        self.to_string()
    }
}
impl DocValue for bool {
    fn render(&self) -> String {
        self.to_string()
    }
}

macro_rules! section {
    (
        $(#[doc = $sdoc:literal])*
        $name:ident = $key:literal {
            $(
                $(#[doc = $doc:literal])*
                $(#[serde($($sattr:tt)*)])*
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
                $(#[serde($($sattr)*)])*
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
                    out.push(FieldDoc { path: format!("{}.{}", $key, stringify!($field)), doc: doc.trim().to_owned(), default: d.$field.render() });
                )*
                out
            }
        }
    };
}

/// 35 mm frame format on a strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format35 {
    /// 36×24 mm frames at a ~38 mm pitch.
    #[default]
    Full,
    /// 18×24 mm half-frame at a ~19 mm pitch.
    Half,
    /// Panoramic (XPan style) ~65×24 mm frames at a ~67 mm pitch.
    Panoramic,
    /// No pitch prior: free gap detection (the pitch is measured from the strip).
    Free,
}

/// 120 roll-film frame format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Format120 {
    #[serde(rename = "6x4.5")]
    SixByFourFive,
    #[default]
    #[serde(rename = "6x6")]
    SixBySix,
    #[serde(rename = "6x7")]
    SixBySeven,
    #[serde(rename = "6x9")]
    SixByNine,
    /// No pitch prior.
    #[serde(rename = "free")]
    Free,
}

/// What is on the bed (a hint; `auto` classifies every opening by its size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Holder {
    #[default]
    Auto,
    /// 35 mm strip holder (three strips of up to six frames on the V800/V850).
    Strip35 {
        #[serde(default)]
        format: Format35,
    },
    /// 120 strip holder (two strips).
    Medium120 {
        #[serde(default)]
        format: Format120,
    },
    /// 4×5 in sheet holder.
    Sheet4x5,
    /// Mounted 35 mm slides.
    Slides35,
}

/// Film polarity: which way the inter-frame gap goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilmType {
    /// Negative: the gap is unexposed base, the highest transmittance on the film.
    #[default]
    Negative,
    /// Positive (slide film): the gap is unexposed, i.e. black.
    Positive,
}

impl DocValue for Holder {
    fn render(&self) -> String {
        match self {
            Holder::Auto => r#"{ kind = "auto" }"#.into(),
            Holder::Strip35 { format } => format!(r#"{{ kind = "strip35", format = "{}" }}"#, kebab35(*format)),
            Holder::Medium120 { format } => format!(r#"{{ kind = "medium120", format = "{}" }}"#, name120(*format)),
            Holder::Sheet4x5 => r#"{ kind = "sheet4x5" }"#.into(),
            Holder::Slides35 => r#"{ kind = "slides35" }"#.into(),
        }
    }
}

impl DocValue for FilmType {
    fn render(&self) -> String {
        match self {
            FilmType::Negative => "\"negative\"".into(),
            FilmType::Positive => "\"positive\"".into(),
        }
    }
}

fn kebab35(f: Format35) -> &'static str {
    match f {
        Format35::Full => "full",
        Format35::Half => "half",
        Format35::Panoramic => "panoramic",
        Format35::Free => "free",
    }
}

fn name120(f: Format120) -> &'static str {
    match f {
        Format120::SixByFourFive => "6x4.5",
        Format120::SixBySix => "6x6",
        Format120::SixBySeven => "6x7",
        Format120::SixByNine => "6x9",
        Format120::Free => "free",
    }
}

section! {
    /// Flatbed prescan frame detection.
    BedConfig = "bed" {
        /// Holder hint: { kind = "auto" | "strip35" | "medium120" | "sheet4x5" | "slides35" }, with `format` for strips ("full" | "half" | "panoramic" | "free"; "6x4.5" | "6x6" | "6x7" | "6x9" | "free").
        holder: Holder = Holder::Auto,
        /// Film polarity: "negative" (gaps are bright base) or "positive" (gaps are black).
        film: FilmType = FilmType::Negative,
        /// Pixels brighter than this fraction of the bare light level are open (film or light), not holder.
        open_threshold: f64 = 0.02,
        /// Along-strip lines brighter than this fraction of the light level, and neutral, are bare light (no film).
        bare_level: f64 = 0.85,
        /// Largest relative chroma ((max-min)/mean) of bare light; film base carries a cast.
        bare_max_chroma: f64 = 0.08,
        /// Openings narrower than this (mm) are holder features (registration marks, calibration windows).
        min_across_mm: f64 = 15.0,
        /// Film runs shorter than this along the strip (mm) are ignored.
        min_along_mm: f64 = 15.0,
        /// Openings separated by at most this (mm) along a common axis are one strip split by a dense band.
        merge_gap_mm: f64 = 12.0,
        /// Fraction of the strip width excluded at each side when sampling across it.
        edge_inset: f64 = 0.1,
        /// Negative gaps: a line within this log-transmittance of the base level counts as base.
        gap_level_tolerance: f64 = 0.1,
        /// Positive gaps: a line within this log-transmittance of the black level counts as gap.
        gap_level_tolerance_positive: f64 = 0.7,
        /// Relative spread (p90-p10 over the median) across the strip at which a line stops looking uniform.
        gap_uniformity: f64 = 0.08,
        /// Search range around the format pitch, as a fraction (35 mm full and half frame).
        pitch_tolerance: f64 = 0.04,
        /// Search range around the format pitch for panoramic 35 mm and 120 (spacing varies by camera).
        pitch_tolerance_wide: f64 = 0.07,
        /// Free pitch search: prefer the smallest pitch scoring at least this fraction of the best (2x pitch matches every other gap equally well).
        smaller_pitch_preference: f64 = 0.85,
        /// Below this mean gap score the comb is not trusted; frames are placed at the nominal pitch with low confidence.
        min_comb_score: f64 = 0.2,
        /// Below this mean gap score a fitted strip carries a warning that the format may be wrong.
        weak_comb_score: f64 = 0.45,
        /// Free-pitch strips of one kind share the pitch that fits them best together (one roll, one camera).
        consensus_pitch: bool = true,
        /// Drop blank (all base) frames at the ends of a strip: leader, not photographs.
        drop_blank_end_frames: bool = true,
    }
}

section! {
    /// Camera-scan frame box (one frame per capture over a light table).
    CameraConfig = "camera" {
        /// Working size (longest side, px) the capture is area-downsampled to.
        work_size: usize = 900,
        /// Film vs dark gate: the threshold is the geometric mean of the gate level (5th percentile) and the top, at most this fraction of the top.
        film_threshold: f64 = 0.35,
        /// Edge response (4-px step of normalised RGB) counted as an edge.
        edge_threshold: f64 = 0.035,
        /// Candidate edge positions kept per side (strongest first); content edges can outrank a faint frame edge.
        edge_candidates: usize = 8,
        /// Fewest fraction of a side's length with edge response for it to be a candidate.
        min_support: f64 = 0.65,
        /// Every side must reach this support for a confident detection.
        confident_support: f64 = 0.8,
        /// Largest per-channel spread between the four outside (rebate) colours.
        max_outside_spread: f64 = 0.12,
        /// Largest relative variation ((p75-p25)/median) along an outside strip: rebate and gaps are flat, image content is not.
        max_outside_roughness: f64 = 0.1,
        /// Four smooth inner sides (largest std below this) describe a gate, not a photograph.
        min_inside_std: f64 = 0.045,
        /// Frame long/short side ratio (either orientation; 1.5 for 35 mm, 1.0 for 6x6, 0 disables the shape prior).
        aspect: f64 = 1.5,
        /// Accepted relative deviation from `aspect`.
        aspect_tolerance: f64 = 0.07,
        /// Estimate and remove a small rotation before finding edges.
        deskew: bool = true,
        /// Largest deskew angle searched (degrees).
        max_skew_deg: f64 = 5.0,
        /// Detect a camera roll's frames together: frames that are not confident are found again with the roll's frame size (a copy stand keeps it fixed), searching only for position.
        roll_prior: bool = true,
        /// Confident frames within this relative size of their median form the roll's frame size; a confident frame further off is detected again with it.
        prior_size_tolerance: f64 = 0.015,
        /// Density step (40th percentile along the side) for a side to count as an edge when placing a frame of the roll's size.
        prior_edge_step: f64 = 0.025,
    }
}

section! {
    /// Live-view frame measurement for tethered autocapture.
    LiveConfig = "live" {
        /// Working size (longest side, px) the live frame is area-downsampled to.
        work_size: usize = 320,
        /// Confidence at or above which a detection is "confident" (four edges found).
        confident_threshold: f64 = 0.8,
        /// A frame closer than this fraction of the view to any view edge is not wholly in view.
        edge_margin: f64 = 0.01,
        /// A frame centre within this fraction of the view size from the view centre is centred.
        centre_tolerance: f64 = 0.15,
        /// Estimate the deskew angle on live frames too (costs a little time).
        deskew: bool = false,
    }
}

/// Every knob of frame detection.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectConfig {
    pub bed: BedConfig,
    pub camera: CameraConfig,
    pub live: LiveConfig,
}

impl DetectConfig {
    /// Every field with its doc comment and default, in declaration order.
    pub fn field_docs() -> Vec<FieldDoc> {
        let mut out = BedConfig::docs();
        out.extend(CameraConfig::docs());
        out.extend(LiveConfig::docs());
        out
    }

    /// Range checks, as `(path, message)`.
    pub fn validate(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut frac = |v: f64, path: &str, lo: f64, hi: f64| {
            if !(v.is_finite() && v >= lo && v <= hi) {
                out.push((path.to_owned(), format!("{v} is outside {lo}..={hi}")));
            }
        };
        let b = &self.bed;
        frac(b.open_threshold, "bed.open_threshold", 0.0, 1.0);
        frac(b.bare_level, "bed.bare_level", 0.0, 1.0);
        frac(b.bare_max_chroma, "bed.bare_max_chroma", 0.0, 10.0);
        frac(b.min_across_mm, "bed.min_across_mm", 0.0, 1000.0);
        frac(b.min_along_mm, "bed.min_along_mm", 0.0, 1000.0);
        frac(b.merge_gap_mm, "bed.merge_gap_mm", 0.0, 1000.0);
        frac(b.edge_inset, "bed.edge_inset", 0.0, 0.45);
        frac(b.gap_level_tolerance, "bed.gap_level_tolerance", 1e-6, 10.0);
        frac(b.gap_level_tolerance_positive, "bed.gap_level_tolerance_positive", 1e-6, 10.0);
        frac(b.gap_uniformity, "bed.gap_uniformity", 1e-6, 10.0);
        frac(b.pitch_tolerance, "bed.pitch_tolerance", 0.0, 0.5);
        frac(b.pitch_tolerance_wide, "bed.pitch_tolerance_wide", 0.0, 0.5);
        frac(b.smaller_pitch_preference, "bed.smaller_pitch_preference", 0.0, 1.0);
        frac(b.min_comb_score, "bed.min_comb_score", 0.0, 1.0);
        frac(b.weak_comb_score, "bed.weak_comb_score", 0.0, 1.0);
        let c = &self.camera;
        frac(c.work_size as f64, "camera.work_size", 64.0, 8192.0);
        frac(c.film_threshold, "camera.film_threshold", 0.0, 1.0);
        frac(c.edge_threshold, "camera.edge_threshold", 0.0, 1.0);
        frac(c.edge_candidates as f64, "camera.edge_candidates", 1.0, 16.0);
        frac(c.min_support, "camera.min_support", 0.0, 1.0);
        frac(c.confident_support, "camera.confident_support", 0.0, 1.0);
        frac(c.max_outside_spread, "camera.max_outside_spread", 0.0, 10.0);
        frac(c.min_inside_std, "camera.min_inside_std", 0.0, 10.0);
        frac(c.max_outside_roughness, "camera.max_outside_roughness", 0.0, 10.0);
        frac(c.max_skew_deg, "camera.max_skew_deg", 0.0, 20.0);
        frac(c.aspect, "camera.aspect", 0.0, 10.0);
        frac(c.aspect_tolerance, "camera.aspect_tolerance", 0.0, 1.0);
        frac(c.prior_size_tolerance, "camera.prior_size_tolerance", 0.0, 0.5);
        frac(c.prior_edge_step, "camera.prior_edge_step", 0.0, 2.0);
        let l = &self.live;
        frac(l.work_size as f64, "live.work_size", 64.0, 4096.0);
        frac(l.confident_threshold, "live.confident_threshold", 0.0, 1.0);
        frac(l.edge_margin, "live.edge_margin", 0.0, 0.4);
        frac(l.centre_tolerance, "live.centre_tolerance", 0.0, 1.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_and_validates() {
        let d = DetectConfig::default();
        assert!(d.validate().is_empty(), "{:?}", d.validate());
        let text = toml::to_string(&d).unwrap();
        let back: DetectConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn partial_files_hints_and_unknown_keys() {
        let c: DetectConfig =
            toml::from_str("[bed]\nfilm = \"positive\"\nholder = { kind = \"medium120\", format = \"6x7\" }\n[live]\nwork_size = 256\n").unwrap();
        assert_eq!(c.bed.film, FilmType::Positive);
        assert_eq!(c.bed.holder, Holder::Medium120 { format: Format120::SixBySeven });
        assert_eq!(c.live.work_size, 256);
        assert_eq!(c.camera, CameraConfig::default());
        let s: BedConfig = toml::from_str("holder = { kind = \"strip35\" }").unwrap();
        assert_eq!(s.holder, Holder::Strip35 { format: Format35::Full });
        assert!(toml::from_str::<DetectConfig>("[bed]\nopen_treshold = 0.1\n").is_err());
        assert!(toml::from_str::<DetectConfig>("[nope]\n").is_err());
        let bad = DetectConfig { bed: BedConfig { edge_inset: f64::NAN, ..Default::default() }, ..Default::default() };
        assert_eq!(bad.validate().len(), 1);
    }

    #[test]
    fn field_docs_cover_every_key_with_toml_defaults() {
        let docs = DetectConfig::field_docs();
        let d = DetectConfig::default();
        let table: toml::Table = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
        let mut n = 0;
        for (section, v) in &table {
            for (key, value) in v.as_table().unwrap() {
                n += 1;
                let path = format!("{section}.{key}");
                let doc = docs.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("no doc for {path}"));
                assert!(!doc.doc.is_empty(), "{path}");
                // The rendered default parses back to the serialized default.
                let parsed: toml::Table = toml::from_str(&format!("v = {}", doc.default)).unwrap();
                assert_eq!(parsed["v"], *value, "{path}");
            }
        }
        assert_eq!(n, docs.len());
    }
}
