//! `FilmConfig`: every knob of the film pipeline, as serde/TOML.
//!
//! Keys are snake_case, enum values kebab-case. Every section is
//! `#[serde(default, deny_unknown_fields)]`, so a partial file is valid and a
//! typo is an error. Defaults reproduce the established reference numbers bit for bit
//! (the parity tests run on `FilmConfig::default()`).
//!
//! Field docs are written once, as doc comments; the `section!` macro turns
//! them into both rustdoc and the [`FilmConfig::field_docs`] table that
//! `silvercraft-cli config show --comments` prints.

use serde::{Deserialize, Serialize};

use crate::calibrate::MAX_LOG_CORRECTION;
use crate::diag::{Diagnostic, from_toml_error};

/// One documented config field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDoc {
    /// Dotted path, e.g. "scene.mid_grey".
    pub path: String,
    pub doc: String,
    /// Default value rendered as TOML ("unset" for an absent optional).
    pub default: String,
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

/// How the roll's film base is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaseMethod {
    /// Inter-frame gap: the whole capture margin above/below the frame (flatbed strips).
    Gap,
    /// Locate the thin rebate next to the frame (camera scans with a dark holder).
    AutoBand,
    /// Only `base.rows` rows adjacent to the frame.
    Rows,
    /// Per-frame content percentile (no rebate available; least trusted).
    Content,
    /// `base.dmin` (or the profile's dmin) only.
    Explicit,
}

/// What to do when the alpha/gamma curve fit is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CurveFallback {
    /// Identity gamma with the geomean neutral gain folded into alpha.
    IdentityWithGain,
    /// Pure identity (no balance at all).
    Identity,
}

/// How the roll-level mid-grey density `e_mid` (which sets `k`) is estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EMidEstimator {
    /// Median of the luminance-BINNED neutral selection (as first written).
    /// Lands at the middle of the density RANGE, which the
    /// film toe pushes above mid grey (-0.4 EV on the synthetic test).
    BinnedMedian,
    /// Median corrected luminance of ALL near-neutral pixels of the roll
    /// (area-weighted median metering): follows subject area, so a scene
    /// with more bright than dark neutrals renders grey darker.
    AllNeutralMedian,
    /// Default. Midpoint, in LOG EXPOSURE (stops), of the roll's neutral
    /// range between the symmetric `e_mid_percentiles`: the zone-system
    /// placement "the middle of the neutral range is mid grey". Working in
    /// log exposure (through the transform itself) removes the toe bias of
    /// the density-domain estimators, and a range midpoint does not depend on
    /// how much AREA each tone covers.
    NeutralRangeMidpoint,
    /// `scene.e_mid_fixed`.
    Fixed,
}

/// Per-frame exposure written to BaselineExposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExposureMode {
    /// `log2(mid_grey / median neutral L)` per frame, clamped to +-clamp_ev.
    Auto,
    /// 0: roll-consistent exposure only.
    Roll,
    /// `exposure.fixed_ev` for every frame.
    Fixed,
}

/// Positive (slide) rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PositiveMode {
    /// `L = T / T_white`, display-referred.
    #[default]
    AsIs,
    /// `L = mid_grey * (t / t_mid)^(1/gamma)`, scene-like (experimental).
    Unrender,
}

/// Where the slide white comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhiteSource {
    /// Per-channel `positive.white_percentile` of the frame.
    Auto,
    /// `positive.white`.
    Explicit,
}

section! {
    /// Film base (orange mask / D-min) estimation, spec Section 1.
    BaseConfig = "base" {
        /// Base source: gap | auto-band | rows | content | explicit.
        method: BaseMethod = BaseMethod::Gap,
        /// Percentile of the margin pixels taken as the gap base (gap, rows).
        percentile: f64 = 80.0,
        /// Rows adjacent to the frame sampled by method "rows".
        rows: usize = 32,
        /// Fewer margin pixels than this and the gap sample is refused.
        min_margin_pixels: usize = 1000,
        /// Strided cap on margin pixels sampled.
        max_samples: usize = 300_000,
        /// auto-band: rows walked outward, as a fraction of the frame height (min 8).
        auto_reach: f64 = 0.22,
        /// auto-band: keep rows whose median is at least this fraction of the brightest.
        auto_keep: f64 = 0.55,
        /// Per-frame (preview) base estimates are clamped to [hint/r, hint*r] of the roll base.
        hint_range: f64 = 1.25,
        /// Explicit base transmission R,G,B in (0,1]; when set it wins over every method and the profile dmin.
        #[serde(skip_serializing_if = "Option::is_none")]
        dmin: Option<[f64; 3]> = None,
        /// Use the profile's dmin (when it has one) instead of measuring.
        prefer_profile_dmin: bool = true,
        /// Gap methods: fall back to the content percentile when no frame has a gap base.
        fallback_to_content: bool = true,
        /// Per-channel content percentile used by method "content" (and the fallback).
        content_percentile: f64 = 95.0,
    }
}

section! {
    /// Per-roll layer-gamma (chemistry) measurement.
    LayerGammaConfig = "layer_gamma" {
        /// Measure the roll's residual layer gammas and fold them into the calibration anchor.
        measure: bool = true,
        /// Fewest usable frames for a roll estimate.
        min_frames: usize = 3,
        /// Fewest samples per frame worth regressing.
        min_samples: usize = 2000,
        /// Lower percentile of the green-density regression band.
        band_low: f64 = 2.0,
        /// Upper percentile of the green-density regression band.
        band_high: f64 = 98.0,
        /// Plausible per-frame slope range [min, max]; outside is a broken measurement.
        slope_range: [f64; 2] = [0.4, 2.5],
        /// Blue-slope IQR [full trust, no trust]; linear shrink toward 1 between.
        confidence_spread: [f64; 2] = [0.10, 0.30],
    }
}

section! {
    /// Roll calibration fit (3x3 density matrix + alpha/gamma curve).
    CalibrationConfig = "calibration" {
        /// Fit a calibration; false = diagonal layer gain only (still roll-level).
        enabled: bool = true,
        /// Override the profile's `neutralize` (gates the matrix and curve fits).
        #[serde(skip_serializing_if = "Option::is_none")]
        neutralize: Option<bool> = None,
        /// Strided samples kept per frame for the roll fit.
        max_samples_per_frame: usize = 100_000,
        /// Transmittance clip floor inside the fit (>= 1e-6 is enforced).
        epsilon: f64 = 1e-6,
        /// Neutral selection: max std of log density across channels.
        neutral_achro_ceiling: f64 = 0.12,
        /// Neutral selection: luminance bins.
        neutral_bins: usize = 6,
        /// Neutral selection: most-achromatic samples kept per bin per frame.
        neutral_per_bin_cap: usize = 1200,
        /// Fewest neutrals for the matrix/curve fits (else fallback).
        neutral_min_count: usize = 500,
        /// Luminance percentiles [lo, hi] spanned by the bins.
        neutral_luma_range: [f64; 2] = [5.0, 97.0],
        /// Fit the full 3x3 density matrix (false = diagonal anchor).
        fit_matrix: bool = true,
        /// Matrix ridge on (M_cc - anchor_c)^2.
        lambda_diag: f64 = 1e-4,
        /// Matrix ridge on off-diagonals M_ck^2.
        lambda_off: f64 = 1e-2,
        /// Accept the matrix only if the neutral spread drops to <= this ratio of the diagonal's.
        accept_spread_ratio: f64 = 0.9,
        /// Accept the matrix only if every |off-diagonal| <= this.
        max_off_diagonal: f64 = 0.5,
        /// Huber-IRLS passes for the matrix and curve fits.
        huber_passes: usize = 4,
        /// Huber threshold in robust sigmas (delta = k * 1.4826 * MAD).
        huber_k: f64 = 1.345,
        /// Fit the alpha/gamma achromatic curve.
        fit_curve: bool = true,
        /// Curve ridge on alpha toward 0.
        lambda_alpha: f64 = 1e-4,
        /// Curve ridge on gamma toward 1.
        lambda_gamma: f64 = 1e-3,
        /// Fitted gammas are clamped to [lo, hi] (then re-centred to sum 3).
        gamma_clamp: [f64; 2] = [0.90, 1.12],
        /// Per-pixel log correction clamp (default ln 1.25).
        max_log_correction: f64 = MAX_LOG_CORRECTION,
        /// Reject the curve when more than this fraction of samples hit the clamp.
        trust_clamp_fraction: f64 = 0.05,
        /// Curve fallback: identity-with-gain | identity.
        fallback: CurveFallback = CurveFallback::IdentityWithGain,
        /// Fallback gain: max (max-min)/max chroma of a "neutral".
        gain_chroma_ceiling: f64 = 0.12,
        /// Fallback gain: luminance percentile window (exclusive) [lo, hi].
        gain_luma_range: [f64; 2] = [20.0, 90.0],
        /// Fallback gain: more than this many neutrals needed, else unit gain.
        gain_min_count: usize = 100,
    }
}

section! {
    /// Per-strip residual gain for scanner-bed non-uniformity.
    StripBalanceConfig = "strip_balance" {
        /// Group frames by strip label and apply a geomean neutral gain per strip.
        enabled: bool = false,
        /// Strip key = this prefix followed by digits in the frame label.
        label_prefix: String = "strip-".to_owned(),
    }
}

section! {
    /// The scene-linear output transform.
    SceneConfig = "scene" {
        /// G_ref override (density per log10 exposure); unset = the profile's scene_gamma.
        #[serde(skip_serializing_if = "Option::is_none")]
        g_ref: Option<f64> = None,
        /// Scene value the roll's mid grey maps to.
        mid_grey: f64 = 0.18,
        /// e_mid estimator: neutral-range-midpoint | binned-median | all-neutral-median | fixed.
        e_mid: EMidEstimator = EMidEstimator::NeutralRangeMidpoint,
        /// neutral-range-midpoint: neutral luminance percentiles [lo, hi] spanning the range (keep symmetric).
        e_mid_percentiles: [f64; 2] = [3.0, 97.0],
        /// e_mid (density above base) for estimator "fixed".
        #[serde(skip_serializing_if = "Option::is_none")]
        e_mid_fixed: Option<f64> = None,
        /// e_mid when nothing can be measured.
        e_mid_default: f64 = 0.65,
        /// e_mid is clamped to [lo, hi] so k stays sane.
        e_mid_range: [f64; 2] = [0.05, 4.0],
        /// Median/midpoint estimators: with this many neutrals or fewer, all pixels are used.
        e_mid_min_neutrals: usize = 100,
        /// Clamp density at the scan's resolvable maximum Dcap and count clipped pixels.
        highlight_cap: bool = true,
        /// Percentile of the frame per channel defining the noise floor for Dcap.
        noise_floor_percentile: f64 = 0.05,
        /// Explicit per-channel noise floor (transmittance) for Dcap.
        #[serde(skip_serializing_if = "Option::is_none")]
        noise_floor: Option<[f64; 3]> = None,
        /// When exactly one channel is at the cap, raise it to the mean of the other two
        /// (corrected density, hue-preserving; never lowers).
        single_channel_highlight_blend: bool = false,
        /// Transmittance/base ratio floor in the transform.
        epsilon: f64 = 1e-6,
    }
}

section! {
    /// Per-frame BaselineExposure.
    ExposureConfig = "exposure" {
        /// auto | roll | fixed.
        mode: ExposureMode = ExposureMode::Auto,
        /// EV for mode "fixed".
        fixed_ev: f64 = 0.0,
        /// Auto exposure is clamped to +-this many EV.
        clamp_ev: f64 = 3.0,
    }
}

section! {
    /// Per-frame residual white balance on L, reported as AsShotNeutral (never baked).
    WhiteBalanceConfig = "white_balance" {
        /// Measure the residual white balance (false = AsShotNeutral 1,1,1).
        residual: bool = true,
        /// Pass-1 gray percentile.
        gray_percentile: f64 = 50.0,
        /// Lower bound of the WB gains.
        gain_min: f64 = 0.75,
        /// Upper bound of the WB gains.
        gain_max: f64 = 4.0 / 3.0,
        /// Pass-2 neutral selection: max std of log values.
        neutral_achro_ceiling: f64 = 0.12,
        /// Pass-2 neutral selection: luminance bins.
        neutral_bins: usize = 6,
        /// Pass-2 neutral selection: cap per bin.
        neutral_per_bin_cap: usize = 1200,
        /// Pass 2 is used only with more neutrals than this.
        neutral_min_count: usize = 100,
        /// Pass-2 luminance percentile range [lo, hi].
        neutral_luma_range: [f64; 2] = [5.0, 97.0],
        /// Shrink gains toward 1 by the scene confidence (g^conf).
        confidence_shrink: bool = true,
        /// Scene confidence: chromaticity spread at which a cluster is "wide" (safe).
        confidence_spread: f64 = 0.06,
        /// Scene confidence: centroid bias from neutral at which a cluster is "far".
        confidence_bias: f64 = 0.10,
        /// Scene confidence is 0 with fewer valid pixels than this.
        confidence_min_pixels: usize = 500,
    }
}

section! {
    /// Positives (slides, positive B&W).
    PositiveConfig = "positive" {
        /// Override the profile's positive mode: as-is | unrender.
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<PositiveMode> = None,
        /// Slide gamma for unrender when the profile has none (or override with `gamma`).
        #[serde(skip_serializing_if = "Option::is_none")]
        gamma: Option<f64> = None,
        /// Gamma used by unrender when neither the profile nor `gamma` gives one.
        default_gamma: f64 = 2.0,
        /// White source: auto | explicit.
        white_source: WhiteSource = WhiteSource::Auto,
        /// Per-channel percentile of the frame taken as white (auto).
        white_percentile: f64 = 99.9,
        /// Explicit white transmittance R,G,B (clear edge / light source).
        #[serde(skip_serializing_if = "Option::is_none")]
        white: Option<[f64; 3]> = None,
    }
}

/// Every knob of the film pipeline.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilmConfig {
    pub base: BaseConfig,
    pub layer_gamma: LayerGammaConfig,
    pub calibration: CalibrationConfig,
    pub strip_balance: StripBalanceConfig,
    pub scene: SceneConfig,
    pub exposure: ExposureConfig,
    pub white_balance: WhiteBalanceConfig,
    pub positive: PositiveConfig,
}

fn check(out: &mut Vec<Diagnostic>, ok: bool, path: &str, message: impl Into<String>) {
    if !ok {
        out.push(Diagnostic::error(path, message));
    }
}

fn finite_in(v: f64, lo: f64, hi: f64) -> bool {
    v.is_finite() && v >= lo && v <= hi
}

fn pct(out: &mut Vec<Diagnostic>, v: f64, path: &str) {
    check(out, finite_in(v, 0.0, 100.0), path, format!("percentile {v} must be in 0..=100"));
}

fn pct_range(out: &mut Vec<Diagnostic>, v: [f64; 2], path: &str) {
    check(
        out,
        finite_in(v[0], 0.0, 100.0) && finite_in(v[1], 0.0, 100.0) && v[0] < v[1],
        path,
        format!("{v:?} must be increasing percentiles in 0..=100"),
    );
}

fn positive(out: &mut Vec<Diagnostic>, v: f64, path: &str) {
    check(out, v.is_finite() && v > 0.0, path, format!("{v} must be positive and finite"));
}

fn transmittance(out: &mut Vec<Diagnostic>, v: Option<[f64; 3]>, path: &str) {
    if let Some(v) = v {
        check(out, v.iter().all(|x| x.is_finite() && *x > 0.0 && *x <= 1.0), path, format!("{v:?} must be in (0, 1] per channel"));
    }
}

impl FilmConfig {
    /// Parse a (partial) config; unknown keys are errors.
    pub fn from_toml_str(text: &str) -> Result<Self, Diagnostic> {
        toml::from_str(text).map_err(|e| from_toml_error(&e, text))
    }

    /// Serialize the full config (unset optionals are omitted).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// This config with a partial TOML table merged over it (later wins).
    /// Used for a profile's `[processing]` table and user overrides.
    pub fn merged(&self, overrides: &toml::Table) -> Result<Self, Vec<Diagnostic>> {
        let mut base = toml::Table::try_from(self).map_err(|e| vec![Diagnostic::error("", e.to_string())])?;
        merge_tables(&mut base, overrides);
        let text = toml::to_string(&base).map_err(|e| vec![Diagnostic::error("", e.to_string())])?;
        let merged = Self::from_toml_str(&text).map_err(|d| vec![d])?;
        let diags = merged.validate();
        if crate::diag::has_errors(&diags) { Err(diags) } else { Ok(merged) }
    }

    /// Every field with its doc comment and default, in declaration order.
    pub fn field_docs() -> Vec<FieldDoc> {
        let mut out = BaseConfig::docs();
        out.extend(LayerGammaConfig::docs());
        out.extend(CalibrationConfig::docs());
        out.extend(StripBalanceConfig::docs());
        out.extend(SceneConfig::docs());
        out.extend(ExposureConfig::docs());
        out.extend(WhiteBalanceConfig::docs());
        out.extend(PositiveConfig::docs());
        out
    }

    /// Range and consistency checks (errors and warnings).
    pub fn validate(&self) -> Vec<Diagnostic> {
        let mut d = Vec::new();
        let b = &self.base;
        pct(&mut d, b.percentile, "base.percentile");
        pct(&mut d, b.content_percentile, "base.content_percentile");
        check(&mut d, b.method != BaseMethod::Rows || b.rows > 0, "base.rows", "method \"rows\" needs rows > 0");
        check(&mut d, b.max_samples > 0, "base.max_samples", "must be > 0");
        check(&mut d, finite_in(b.auto_reach, 0.0, 1.0) && b.auto_reach > 0.0, "base.auto_reach", "must be in (0, 1]");
        check(&mut d, finite_in(b.auto_keep, 0.0, 1.0), "base.auto_keep", "must be in [0, 1]");
        check(&mut d, b.hint_range.is_finite() && b.hint_range >= 1.0, "base.hint_range", "must be >= 1");
        transmittance(&mut d, b.dmin, "base.dmin");

        let lg = &self.layer_gamma;
        check(&mut d, lg.min_frames >= 1, "layer_gamma.min_frames", "must be >= 1");
        check(&mut d, lg.min_samples >= 2, "layer_gamma.min_samples", "must be >= 2");
        pct_range(&mut d, [lg.band_low, lg.band_high], "layer_gamma.band_low");
        check(
            &mut d,
            lg.slope_range[0] > 0.0 && lg.slope_range[0] < lg.slope_range[1] && lg.slope_range[1].is_finite(),
            "layer_gamma.slope_range",
            "must be 0 < min < max",
        );
        check(
            &mut d,
            finite_in(lg.confidence_spread[0], 0.0, 10.0) && lg.confidence_spread[0] < lg.confidence_spread[1] && lg.confidence_spread[1].is_finite(),
            "layer_gamma.confidence_spread",
            "must be 0 <= lo < hi",
        );

        let c = &self.calibration;
        check(&mut d, c.max_samples_per_frame > 0, "calibration.max_samples_per_frame", "must be > 0");
        check(&mut d, finite_in(c.epsilon, 0.0, 0.1), "calibration.epsilon", "must be in [0, 0.1]");
        positive(&mut d, c.neutral_achro_ceiling, "calibration.neutral_achro_ceiling");
        check(&mut d, c.neutral_bins > 0, "calibration.neutral_bins", "must be > 0");
        check(&mut d, c.neutral_per_bin_cap > 0, "calibration.neutral_per_bin_cap", "must be > 0");
        check(&mut d, c.neutral_min_count > 0, "calibration.neutral_min_count", "must be > 0");
        pct_range(&mut d, c.neutral_luma_range, "calibration.neutral_luma_range");
        positive(&mut d, c.lambda_diag, "calibration.lambda_diag");
        positive(&mut d, c.lambda_off, "calibration.lambda_off");
        check(&mut d, finite_in(c.accept_spread_ratio, 0.0, 1.0), "calibration.accept_spread_ratio", "must be in [0, 1]");
        check(&mut d, finite_in(c.max_off_diagonal, 0.0, 10.0), "calibration.max_off_diagonal", "must be in [0, 10]");
        check(&mut d, (1..=50).contains(&c.huber_passes), "calibration.huber_passes", "must be in 1..=50");
        positive(&mut d, c.huber_k, "calibration.huber_k");
        positive(&mut d, c.lambda_alpha, "calibration.lambda_alpha");
        positive(&mut d, c.lambda_gamma, "calibration.lambda_gamma");
        check(
            &mut d,
            c.gamma_clamp[0] > 0.0 && c.gamma_clamp[0] <= 1.0 && c.gamma_clamp[1] >= 1.0 && c.gamma_clamp[1].is_finite(),
            "calibration.gamma_clamp",
            "must be 0 < lo <= 1 <= hi",
        );
        check(&mut d, finite_in(c.max_log_correction, 0.0, 5.0), "calibration.max_log_correction", "must be in [0, 5]");
        check(&mut d, finite_in(c.trust_clamp_fraction, 0.0, 1.0), "calibration.trust_clamp_fraction", "must be in [0, 1]");
        positive(&mut d, c.gain_chroma_ceiling, "calibration.gain_chroma_ceiling");
        pct_range(&mut d, c.gain_luma_range, "calibration.gain_luma_range");

        check(&mut d, !self.strip_balance.label_prefix.is_empty(), "strip_balance.label_prefix", "must not be empty");

        let s = &self.scene;
        if let Some(g) = s.g_ref {
            check(&mut d, finite_in(g, 0.1, 5.0) && g > 0.1, "scene.g_ref", "must be in (0.1, 5)");
        }
        check(&mut d, s.mid_grey.is_finite() && s.mid_grey > 0.0 && s.mid_grey < 100.0, "scene.mid_grey", "must be in (0, 100)");
        check(&mut d, s.e_mid != EMidEstimator::Fixed || s.e_mid_fixed.is_some(), "scene.e_mid_fixed", "estimator \"fixed\" needs e_mid_fixed");
        if let Some(e) = s.e_mid_fixed {
            positive(&mut d, e, "scene.e_mid_fixed");
        }
        pct_range(&mut d, s.e_mid_percentiles, "scene.e_mid_percentiles");
        check(
            &mut d,
            s.e_mid_range[0] > 0.0 && s.e_mid_range[0] < s.e_mid_range[1] && s.e_mid_range[1].is_finite(),
            "scene.e_mid_range",
            "must be 0 < lo < hi",
        );
        check(&mut d, s.e_mid_default.is_finite() && s.e_mid_default > 0.0, "scene.e_mid_default", "must be positive");
        pct(&mut d, s.noise_floor_percentile, "scene.noise_floor_percentile");
        transmittance(&mut d, s.noise_floor, "scene.noise_floor");
        check(&mut d, finite_in(s.epsilon, 1e-12, 0.1), "scene.epsilon", "must be in [1e-12, 0.1]");

        let e = &self.exposure;
        check(&mut d, finite_in(e.fixed_ev, -20.0, 20.0), "exposure.fixed_ev", "must be in [-20, 20]");
        check(&mut d, finite_in(e.clamp_ev, 0.0, 20.0), "exposure.clamp_ev", "must be in [0, 20]");

        let w = &self.white_balance;
        pct(&mut d, w.gray_percentile, "white_balance.gray_percentile");
        check(
            &mut d,
            w.gain_min > 0.0 && w.gain_min <= 1.0 && w.gain_max >= 1.0 && w.gain_max.is_finite(),
            "white_balance.gain_min",
            "need 0 < gain_min <= 1 <= gain_max",
        );
        positive(&mut d, w.neutral_achro_ceiling, "white_balance.neutral_achro_ceiling");
        check(&mut d, w.neutral_bins > 0 && w.neutral_per_bin_cap > 0, "white_balance.neutral_bins", "bins and per-bin cap must be > 0");
        pct_range(&mut d, w.neutral_luma_range, "white_balance.neutral_luma_range");
        positive(&mut d, w.confidence_spread, "white_balance.confidence_spread");
        positive(&mut d, w.confidence_bias, "white_balance.confidence_bias");

        let p = &self.positive;
        if let Some(g) = p.gamma {
            check(&mut d, finite_in(g, 0.1, 5.0) && g > 0.1, "positive.gamma", "must be in (0.1, 5)");
        }
        check(&mut d, finite_in(p.default_gamma, 0.1, 5.0) && p.default_gamma > 0.1, "positive.default_gamma", "must be in (0.1, 5)");
        pct(&mut d, p.white_percentile, "positive.white_percentile");
        check(&mut d, p.white_source != WhiteSource::Explicit || p.white.is_some(), "positive.white", "white_source \"explicit\" needs white");
        transmittance(&mut d, p.white, "positive.white");
        if b.method == BaseMethod::Explicit && b.dmin.is_none() {
            d.push(Diagnostic::warning("base.dmin", "method \"explicit\" without base.dmin relies on the profile's dmin"));
        }
        d
    }
}

/// Deep-merge `over` into `base` (tables merge recursively, everything else replaces).
pub fn merge_tables(base: &mut toml::Table, over: &toml::Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_tables(b, o),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_and_validates() {
        let c = FilmConfig::default();
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        let text = c.to_toml();
        assert_eq!(FilmConfig::from_toml_str(&text).unwrap(), c);
        assert_eq!(FilmConfig::from_toml_str("").unwrap(), c);
        assert!(text.contains("method = \"gap\""), "{text}");
    }

    #[test]
    fn partial_files_and_unknown_keys() {
        let c = FilmConfig::from_toml_str("[scene]\nmid_grey = 0.2\n[exposure]\nmode = \"fixed\"\nfixed_ev = 1.5").unwrap();
        assert_eq!(c.scene.mid_grey, 0.2);
        assert_eq!(c.exposure.mode, ExposureMode::Fixed);
        assert_eq!(c.calibration, CalibrationConfig::default());
        let err = FilmConfig::from_toml_str("[scene]\nmid_gray = 0.2").unwrap_err();
        assert!(err.message.contains("mid_gray"), "{err}");
        assert!(FilmConfig::from_toml_str("[sceen]\n").is_err());
        assert!(FilmConfig::from_toml_str("[base]\nmethod = \"auto_band\"").is_err(), "enum values are kebab-case");
        assert!(FilmConfig::from_toml_str("[base]\nmethod = \"auto-band\"").is_ok());
    }

    #[test]
    fn validation_reports_paths() {
        let mut c = FilmConfig::default();
        c.scene.mid_grey = -1.0;
        c.base.percentile = 101.0;
        c.calibration.gamma_clamp = [1.2, 0.9];
        c.scene.e_mid = EMidEstimator::Fixed;
        c.positive.white_source = WhiteSource::Explicit;
        c.base.dmin = Some([0.5, 0.0, 2.0]);
        let paths: Vec<String> = c.validate().into_iter().filter(Diagnostic::is_error).map(|d| d.path).collect();
        for p in ["scene.mid_grey", "base.percentile", "calibration.gamma_clamp", "scene.e_mid_fixed", "positive.white", "base.dmin"] {
            assert!(paths.iter().any(|x| x == p), "{p} missing from {paths:?}");
        }
    }

    #[test]
    fn merge_overrides_and_rejects_bad_values() {
        let over: toml::Table = toml::from_str("[scene]\nhighlight_cap = false\n[calibration]\nlambda_off = 0.5").unwrap();
        let m = FilmConfig::default().merged(&over).unwrap();
        assert!(!m.scene.highlight_cap);
        assert_eq!(m.calibration.lambda_off, 0.5);
        assert_eq!(m.calibration.lambda_diag, 1e-4);
        let bad: toml::Table = toml::from_str("[scene]\nmid_grey = -3.0").unwrap();
        assert!(FilmConfig::default().merged(&bad).is_err());
        let typo: toml::Table = toml::from_str("[scene]\nmidgrey = 1.0").unwrap();
        assert!(FilmConfig::default().merged(&typo).is_err());
    }

    #[test]
    fn field_docs_cover_every_field() {
        let docs = FilmConfig::field_docs();
        assert!(docs.len() > 60, "{}", docs.len());
        assert!(docs.iter().all(|d| !d.doc.is_empty()), "undocumented field");
        let mid = docs.iter().find(|d| d.path == "scene.mid_grey").unwrap();
        assert_eq!(mid.default, "0.18");
        assert!(docs.iter().any(|d| d.path == "scene.g_ref" && d.default == "unset"));
        assert!(docs.iter().any(|d| d.path == "base.method" && d.default == "\"gap\""));
        // Every serialized key of the default config is documented.
        let table = toml::Table::try_from(FilmConfig::default()).unwrap();
        for (section, fields) in &table {
            for key in fields.as_table().unwrap().keys() {
                assert!(docs.iter().any(|d| d.path == format!("{section}.{key}")), "{section}.{key} undocumented");
            }
        }
    }
}
