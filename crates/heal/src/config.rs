//! `HealConfig`: every knob of dust and scratch repair, as serde/TOML (`[heal]` in a pipeline
//! config).
//!
//! The section is `#[serde(default, deny_unknown_fields)]`, so a partial table is valid and a typo
//! is an error. Lengths and areas are in pixels **at 600 dpi** and scale with `dpi` (lengths
//! linearly, areas quadratically), so one config serves every scan resolution.
//!
//! Field docs are written once, as doc comments; the `section!` macro turns them into both rustdoc
//! and the [`HealConfig::field_docs`] table.

use serde::{Deserialize, Serialize};
pub use silvercraft_film::config::FieldDoc;
use silvercraft_film::diag::{Diagnostic, from_toml_error};

/// Resolution the pixel sizes in this config are expressed at.
pub const REFERENCE_DPI: f64 = 600.0;

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

            /// Every field with its doc comment and default, in declaration order.
            pub fn field_docs() -> Vec<FieldDoc> {
                let rendered = toml::Table::try_from(Self::default()).unwrap_or_default();
                let mut out = Vec::new();
                $(
                    let lines: &[&str] = &[$($doc),*];
                    let doc = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
                    let default = rendered.get(stringify!($field)).map_or_else(|| "unset".to_owned(), |v| v.to_string());
                    out.push(FieldDoc { path: format!("{}.{}", $key, stringify!($field)), doc: doc.trim().to_owned(), default });
                )*
                out
            }
        }
    };
}

section! {
    /// Infrared dust and scratch repair, run on the linear transmittance capture (the negative,
    /// before inversion).
    HealConfig = "heal" {
        /// Run dust and scratch repair at all.
        enabled: bool = true,
        /// Scan resolution; every pixel size below is at 600 dpi and scales with it. Unset = 600.
        #[serde(skip_serializing_if = "Option::is_none")]
        dpi: Option<f64> = None,
        /// Use the infrared plane when the capture has one (false = no-IR path only).
        use_ir: bool = true,
        /// Estimate the sub-pixel IR to RGB offset from the defects themselves.
        register: bool = true,
        /// Largest IR to RGB offset searched, pixels.
        max_shift: f64 = 3.0,
        /// Registration below this confidence (0..1) falls back to `ir_shift`.
        min_registration_confidence: f64 = 0.25,
        /// IR offset [dx, dy] used when registration is off or not confident: the IR feature of
        /// RGB pixel (x, y) sits at (x + dx, y + dy) in the IR plane.
        ir_shift: [f64; 2] = [0.0, 0.0],
        /// Gaussian sigma of the IR background (lamp falloff, vignetting), pixels.
        background_sigma: f64 = 16.0,
        /// Remove the image that leaks into IR (chromogenic dyes) by regressing IR on the visible
        /// log channels.
        leakage: bool = true,
        /// IR whose log slope against the visible channels exceeds this is image, not dust (silver
        /// B&W film): IR repair is refused.
        silver_slope: f64 = 0.5,
        /// Seed threshold: attenuation at least this many robust noise sigmas (MAD).
        seed_sigma: f64 = 5.0,
        /// Seed threshold floor: a seed blocks at least this fraction of the IR (keeps Newton
        /// rings and grain out).
        seed_depth: f64 = 0.03,
        /// Neighbourhood radius (pixels) of the median a defect must stand out from; broad
        /// low-contrast structure such as Newton ring fringes follows the median and is ignored.
        local_radius: f64 = 4.0,
        /// Anything blocking more than this fraction of the IR is a seed even when broad (large
        /// blobs, wide hairs); Newton rings stay far below it.
        broad_seed_depth: f64 = 0.12,
        /// Grow threshold, robust noise sigmas: pixels connected to a seed that are this attenuated
        /// join the defect.
        grow_sigma: f64 = 2.5,
        /// Grow threshold floor, fraction of the IR blocked.
        grow_depth: f64 = 0.015,
        /// Growth from a seed is bounded to this many pixel steps (no runaway through grain).
        grow_steps: f64 = 6.0,
        /// Grow IR defects up to this many pixels through visibly dark pixels, so occluders that
        /// block visible light more than IR (tinted flakes, fibres) are covered whole. 0 = off.
        visible_grow: f64 = 4.0,
        /// Visible growth: a pixel is at least this much darker (fraction) than its local median.
        visible_grow_depth: f64 = 0.04,
        /// Defects smaller than this (pixels, before the halo) are noise.
        min_area: f64 = 3.0,
        /// Every defect is dilated by this many pixels before filling (soft defect edges).
        halo: f64 = 2.0,
        /// A defect whose deepest IR attenuation is below this is opaque ("deep").
        deep_attenuation: f64 = 0.4,
        /// Digital-ICE-style correction: divide partial defects by their attenuation instead of
        /// filling them.
        partial_correction: bool = false,
        /// Partial correction only for defects whose deepest attenuation stays above this.
        partial_floor: f64 = 0.6,
        /// Defects up to this area (pixels, with halo) get the ring-average fill.
        small_max_area: f64 = 64.0,
        /// Defects at least this large (pixels, with halo) get the exemplar fill.
        large_min_area: f64 = 400.0,
        /// Scratches and hairs (exemplar fill): at least this long, pixels...
        scratch_min_length: f64 = 20.0,
        /// ...and at least this elongated (length over width).
        scratch_min_aspect: f64 = 4.0,
        /// Exemplar fill: block size, pixels.
        patch_size: f64 = 10.0,
        /// Exemplar fill: best-match search radius around each block, pixels.
        search_radius: f64 = 32.0,
        /// Add grain matched to the surroundings to the smooth (ring and membrane) fills.
        grain: bool = true,
        /// Grain amplitude relative to the measured surrounding grain.
        grain_strength: f64 = 1.0,
        /// A mask covering more than this percent of the frame is a misdetection: repair is
        /// refused.
        max_coverage: f64 = 15.0,
        /// Pixels next to the opaque holder or frame edge excluded from detection.
        border_margin: f64 = 3.0,
        /// No-IR captures (camera scans): detect small sharp dark specks on the negative.
        /// Conservative, and off by default.
        no_ir_detect: bool = false,
        /// No-IR: a speck is at least this much darker (fraction) than its local median.
        no_ir_contrast: f64 = 0.15,
        /// No-IR: and at least this many robust noise sigmas darker.
        no_ir_sigma: f64 = 6.0,
        /// No-IR: specks are at most this large (pixels), far smaller than image features.
        no_ir_max_area: f64 = 30.0,
        /// Visible-light repair of long thin scratches along the film strip ("rain" from winding
        /// and processing), which the IR pass does not show. Runs after IR dust repair and also
        /// on captures without IR.
        scratches: bool = false,
        /// Direction the film strip (and so the rain) runs in the capture: "auto" (the longer
        /// side, unless the other axis has decisively stronger long-line evidence), "vertical" or
        /// "horizontal".
        scratch_axis: ScratchAxis = ScratchAxis::Auto,
        /// A scratch is at least this long along the strip, pixels (300 = 12.7 mm). Shorter
        /// lines are image detail.
        scratch_length: f64 = 300.0,
        /// A scratch is at most this wide (half-maximum width), pixels.
        scratch_max_width: f64 = 2.5,
        /// Detection sensitivity: higher finds fainter scratches, with more risk of taking image
        /// lines (1 = the tested default).
        scratch_sensitivity: f64 = 1.0,
        /// Largest slant of a scratch from the strip direction, degrees.
        scratch_max_angle: f64 = 3.0,
        /// How scratches are repaired: "auto" (faint ones are corrected, strong ones
        /// interpolated), "correct" (divide out the scratch's smoothed attenuation, keeping the
        /// grain underneath) or "interpolate" (across the scratch from both sides, with matched
        /// grain).
        scratch_repair: ScratchRepair = ScratchRepair::Auto,
    }
}

/// Direction of the film strip in the capture (scratches run along it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScratchAxis {
    /// The longer side, unless the other axis has decisively stronger long thin line evidence.
    #[default]
    Auto,
    /// Scratches run top to bottom.
    Vertical,
    /// Scratches run left to right.
    Horizontal,
}

/// How long scratches are repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScratchRepair {
    /// Correct faint scratches, interpolate strong ones.
    #[default]
    Auto,
    /// Divide out the scratch's attenuation, smoothed along its length (grain is kept).
    Correct,
    /// Interpolate across the scratch from both sides, with matched grain.
    Interpolate,
}

fn check(out: &mut Vec<Diagnostic>, ok: bool, path: &str, message: impl Into<String>) {
    if !ok {
        out.push(Diagnostic::error(path, message));
    }
}

fn finite_in(v: f64, lo: f64, hi: f64) -> bool {
    v.is_finite() && v >= lo && v <= hi
}

impl HealConfig {
    /// Parse a (partial) `[heal]` table body; unknown keys are errors.
    pub fn from_toml_str(text: &str) -> Result<Self, Diagnostic> {
        toml::from_str(text).map_err(|e| from_toml_error(&e, text))
    }

    /// Serialize the full section (unset optionals are omitted).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Pixel scale relative to the 600 dpi reference.
    pub fn scale(&self) -> f64 {
        match self.dpi {
            Some(d) if d.is_finite() && d > 0.0 => d / REFERENCE_DPI,
            _ => 1.0,
        }
    }

    /// Range and consistency checks; paths are prefixed with `heal.`.
    pub fn validate(&self) -> Vec<Diagnostic> {
        let mut d = Vec::new();
        let p = |f: &str| format!("{}.{f}", Self::SECTION);
        if let Some(dpi) = self.dpi {
            check(&mut d, finite_in(dpi, 50.0, 20_000.0), &p("dpi"), "must be in 50..=20000");
        }
        check(&mut d, finite_in(self.max_shift, 0.0, 64.0), &p("max_shift"), "must be in 0..=64");
        check(&mut d, finite_in(self.min_registration_confidence, 0.0, 1.0), &p("min_registration_confidence"), "must be in 0..=1");
        check(&mut d, self.ir_shift.iter().all(|v| finite_in(*v, -64.0, 64.0)), &p("ir_shift"), "must be finite and within +-64");
        check(&mut d, finite_in(self.background_sigma, 2.0, 500.0), &p("background_sigma"), "must be in 2..=500");
        check(&mut d, finite_in(self.silver_slope, 0.0, 5.0), &p("silver_slope"), "must be in 0..=5");
        check(&mut d, finite_in(self.seed_sigma, 0.5, 100.0), &p("seed_sigma"), "must be in 0.5..=100");
        check(&mut d, finite_in(self.grow_sigma, 0.0, 100.0) && self.grow_sigma <= self.seed_sigma, &p("grow_sigma"), "must be in 0..=seed_sigma");
        check(&mut d, finite_in(self.seed_depth, 0.0, 0.99), &p("seed_depth"), "must be in 0..=0.99");
        check(&mut d, finite_in(self.grow_depth, 0.0, 0.99) && self.grow_depth <= self.seed_depth, &p("grow_depth"), "must be in 0..=seed_depth");
        check(&mut d, finite_in(self.local_radius, 1.0, 7.0), &p("local_radius"), "must be in 1..=7");
        check(
            &mut d,
            finite_in(self.broad_seed_depth, 0.0, 0.99) && self.broad_seed_depth >= self.seed_depth,
            &p("broad_seed_depth"),
            "must be in seed_depth..=0.99",
        );
        check(&mut d, finite_in(self.grow_steps, 0.0, 200.0), &p("grow_steps"), "must be in 0..=200");
        check(&mut d, finite_in(self.visible_grow, 0.0, 100.0), &p("visible_grow"), "must be in 0..=100");
        check(&mut d, finite_in(self.visible_grow_depth, 0.0, 0.99), &p("visible_grow_depth"), "must be in 0..=0.99");
        check(&mut d, finite_in(self.min_area, 0.0, 1e6), &p("min_area"), "must be in 0..=1e6");
        check(&mut d, finite_in(self.halo, 0.0, 50.0), &p("halo"), "must be in 0..=50");
        check(&mut d, finite_in(self.deep_attenuation, 0.0, 1.0), &p("deep_attenuation"), "must be in 0..=1");
        check(&mut d, finite_in(self.partial_floor, 0.05, 1.0), &p("partial_floor"), "must be in 0.05..=1");
        check(&mut d, finite_in(self.small_max_area, 0.0, 1e7), &p("small_max_area"), "must be in 0..=1e7");
        check(
            &mut d,
            finite_in(self.large_min_area, 0.0, 1e7) && self.large_min_area >= self.small_max_area,
            &p("large_min_area"),
            "must be >= small_max_area",
        );
        check(&mut d, finite_in(self.scratch_min_length, 0.0, 1e5), &p("scratch_min_length"), "must be in 0..=1e5");
        check(&mut d, finite_in(self.scratch_min_aspect, 1.0, 1e3), &p("scratch_min_aspect"), "must be in 1..=1000");
        check(&mut d, finite_in(self.patch_size, 3.0, 64.0), &p("patch_size"), "must be in 3..=64");
        check(&mut d, finite_in(self.search_radius, 4.0, 512.0), &p("search_radius"), "must be in 4..=512");
        check(&mut d, finite_in(self.grain_strength, 0.0, 4.0), &p("grain_strength"), "must be in 0..=4");
        check(&mut d, finite_in(self.max_coverage, 0.0, 100.0), &p("max_coverage"), "must be in 0..=100");
        check(&mut d, finite_in(self.border_margin, 0.0, 100.0), &p("border_margin"), "must be in 0..=100");
        check(&mut d, finite_in(self.no_ir_contrast, 0.01, 1.0), &p("no_ir_contrast"), "must be in 0.01..=1");
        check(&mut d, finite_in(self.no_ir_sigma, 1.0, 100.0), &p("no_ir_sigma"), "must be in 1..=100");
        check(&mut d, finite_in(self.no_ir_max_area, 1.0, 1e4), &p("no_ir_max_area"), "must be in 1..=1e4");
        check(&mut d, finite_in(self.scratch_length, 20.0, 1e5), &p("scratch_length"), "must be in 20..=1e5");
        check(&mut d, finite_in(self.scratch_max_width, 0.5, 20.0), &p("scratch_max_width"), "must be in 0.5..=20");
        check(&mut d, finite_in(self.scratch_sensitivity, 0.25, 4.0), &p("scratch_sensitivity"), "must be in 0.25..=4");
        check(&mut d, finite_in(self.scratch_max_angle, 0.0, 10.0), &p("scratch_max_angle"), "must be in 0..=10");
        if self.dpi.is_none() && self.enabled {
            d.push(Diagnostic::note(p("dpi"), "unset: defect sizes assume a 600 dpi scan"));
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_and_validates() {
        let c = HealConfig::default();
        assert!(!silvercraft_film::diag::has_errors(&c.validate()), "{:?}", c.validate());
        let text = c.to_toml();
        assert_eq!(HealConfig::from_toml_str(&text).unwrap(), c);
        assert_eq!(HealConfig::from_toml_str("").unwrap(), c);
        assert!(!c.no_ir_detect, "the no-IR detector is opt-in");
        assert!(c.enabled);
    }

    #[test]
    fn partial_tables_and_unknown_keys() {
        let c = HealConfig::from_toml_str("dpi = 2400.0\nhalo = 3.0").unwrap();
        assert_eq!(c.dpi, Some(2400.0));
        assert_eq!(c.scale(), 4.0);
        assert_eq!(c.halo, 3.0);
        assert_eq!(c.seed_sigma, HealConfig::default().seed_sigma);
        let err = HealConfig::from_toml_str("hallo = 3.0").unwrap_err();
        assert!(err.message.contains("hallo"), "{err}");
    }

    #[test]
    fn scratch_keys_parse_and_validate() {
        let c = HealConfig::from_toml_str("scratches = true\nscratch_axis = \"horizontal\"\nscratch_repair = \"interpolate\"").unwrap();
        assert!(c.scratches);
        assert_eq!((c.scratch_axis, c.scratch_repair), (ScratchAxis::Horizontal, ScratchRepair::Interpolate));
        assert!(!HealConfig::default().scratches, "long-scratch repair is opt-in");
        assert!(HealConfig::from_toml_str("scratch_axis = \"diagonal\"").is_err());
        let bad =
            HealConfig { scratch_length: 1.0, scratch_max_width: 50.0, scratch_sensitivity: 0.0, scratch_max_angle: 45.0, ..HealConfig::default() };
        let paths: Vec<String> = bad.validate().into_iter().filter(Diagnostic::is_error).map(|d| d.path).collect();
        for p in ["heal.scratch_length", "heal.scratch_max_width", "heal.scratch_sensitivity", "heal.scratch_max_angle"] {
            assert!(paths.iter().any(|x| x == p), "{p} missing from {paths:?}");
        }
    }

    #[test]
    fn validation_reports_paths() {
        let c = HealConfig { seed_sigma: -1.0, grow_depth: 0.5, patch_size: 1.0, dpi: Some(f64::NAN), ..HealConfig::default() };
        let paths: Vec<String> = c.validate().into_iter().filter(Diagnostic::is_error).map(|d| d.path).collect();
        for p in ["heal.seed_sigma", "heal.grow_depth", "heal.patch_size", "heal.dpi"] {
            assert!(paths.iter().any(|x| x == p), "{p} missing from {paths:?}");
        }
        assert_eq!(c.scale(), 1.0, "a bad dpi never reaches the algorithms");
    }

    #[test]
    fn field_docs_cover_every_field() {
        let docs = HealConfig::field_docs();
        assert!(docs.iter().all(|d| !d.doc.is_empty()), "undocumented field");
        assert!(docs.iter().any(|d| d.path == "heal.dpi" && d.default == "unset"));
        assert!(docs.iter().any(|d| d.path == "heal.no_ir_detect" && d.default == "false"));
        let table = toml::Table::try_from(HealConfig::default()).unwrap();
        for key in table.keys() {
            assert!(docs.iter().any(|d| d.path == format!("heal.{key}")), "heal.{key} undocumented");
        }
        assert_eq!(docs.len(), table.len() + 1, "every field but the unset dpi serializes");
    }
}
