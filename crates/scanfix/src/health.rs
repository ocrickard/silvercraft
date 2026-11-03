//! Scan health: saturation and clipping, black level and noise floor, dynamic-range usage, plus
//! the warnings built from them and from the line-defect and banding detectors.
//!
//! the original capture-channel health guarded camera captures against a light that misses a dye
//! layer (a compressed density record). For a flatbed the failure modes are sensor-side, so this
//! report measures what the sensor recorded: how much of each channel is clipped at either end,
//! where the black level and noise floor sit, how many stops of the sensor the scan used, and
//! what the artifact detectors found.

use serde::{Deserialize, Serialize};
use silvercraft_imgproc::{Plane, stats};

use crate::Channel;
use crate::banding::Banding;
use crate::config::{HealthConfig, StreakAxis};
use crate::lines::LineDefect;
use crate::util::{MAD_SIGMA, median_in_place};

/// One 16-bit code: the floor for noise and black level in stop computations.
const ONE_CODE: f32 = 1.0 / 65535.0;

/// Health of one channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelHealth {
    pub channel: Channel,
    pub mean: f32,
    pub median: f32,
    /// 0.1th percentile: the effective black level.
    pub black_level: f32,
    /// 99.9th percentile: the effective peak.
    pub white_level: f32,
    /// Fraction of finite values at or above `health.clip_high` (sensor saturated).
    pub clipped_high: f32,
    /// Fraction of finite values at or below `health.clip_low`.
    pub clipped_low: f32,
    /// NaN or infinite values (treated as missing everywhere).
    pub non_finite: usize,
    /// Sigma of pixel-to-pixel noise in the darkest 10 % of the frame.
    pub noise_floor: f32,
    /// log2(white level / noise floor): the stops of usable signal.
    pub dynamic_range_stops: f32,
    /// white level − black level, as a fraction of full scale.
    pub range_used: f32,
    /// log2(1 / white level): exposure stops left unused at the top.
    pub headroom_stops: f32,
}

/// Kind of a health warning, for UIs that want icons or filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WarningKind {
    HighlightsClipped,
    ShadowsClipped,
    WeakSignal,
    NonFinite,
    LineDefect,
    Banding,
    StrongRows,
}

/// One human-readable warning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub kind: WarningKind,
    pub channel: Option<Channel>,
    pub message: String,
}

/// Everything scanfix found on a scan (and what it repaired).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub width: usize,
    pub height: usize,
    pub axis: StreakAxis,
    pub channels: Vec<ChannelHealth>,
    pub line_defects: Vec<LineDefect>,
    pub banding: Vec<Banding>,
    pub warnings: Vec<Warning>,
    /// Whether any pixel was changed.
    pub repaired: bool,
}

/// Measure one channel.
pub fn channel_health(p: &Plane, channel: Channel, cfg: &HealthConfig) -> ChannelHealth {
    let data = p.data();
    let non_finite = data.iter().filter(|v| !v.is_finite()).count();
    let finite = (data.len() - non_finite).max(1) as f32;
    let high = data.iter().filter(|v| v.is_finite() && **v >= cfg.clip_high).count() as f32 / finite;
    let low = data.iter().filter(|v| v.is_finite() && **v <= cfg.clip_low).count() as f32 / finite;
    let mean = {
        let (s, n) = data.iter().filter(|v| v.is_finite()).fold((0.0f64, 0usize), |(s, n), &v| (s + f64::from(v), n + 1));
        if n > 0 { (s / n as f64) as f32 } else { 0.0 }
    };
    let sample = stats::subsample(data, cfg.max_samples);
    let q = stats::percentiles(&sample, &[0.1, 50.0, 99.9]).unwrap_or_else(|| vec![0.0; 3]);
    let (black, median, white) = (q[0], q[1], q[2]);
    let noise = noise_floor(p, cfg.max_samples);
    let white_c = white.max(ONE_CODE);
    ChannelHealth {
        channel,
        mean,
        median,
        black_level: black,
        white_level: white,
        clipped_high: high,
        clipped_low: low,
        non_finite,
        noise_floor: noise,
        dynamic_range_stops: (white_c / noise.max(ONE_CODE)).log2().max(0.0),
        range_used: (white - black).max(0.0),
        headroom_stops: (1.0 / white_c).log2().max(0.0),
    }
}

/// Sigma of horizontal first differences over the darkest 10 % of neighbour pairs (by pair mean),
/// from the MAD; the whole frame when that is too small.
fn noise_floor(p: &Plane, max_samples: usize) -> f32 {
    let stride = (p.len() / max_samples.max(1)).max(1);
    let mut pairs: Vec<(f32, f32)> = Vec::new();
    for y in (0..p.height()).step_by(stride) {
        for pair in p.row(y).windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a.is_finite() && b.is_finite() {
                pairs.push((0.5 * (a + b), b - a));
            }
        }
    }
    let means: Vec<f32> = pairs.iter().map(|p| p.0).collect();
    let Some(thr) = stats::percentile(&means, 10.0) else { return 0.0 };
    let mut d: Vec<f32> = pairs.iter().filter(|p| p.0 <= thr).map(|p| p.1).collect();
    if d.len() < 256 {
        d = pairs.iter().map(|p| p.1).collect();
    }
    let Some(m) = median_in_place(&mut d) else { return 0.0 };
    for v in d.iter_mut() {
        *v = (*v - m).abs();
    }
    median_in_place(&mut d).map_or(0.0, |mad| MAD_SIGMA * mad / std::f32::consts::SQRT_2)
}

fn pct(f: f32) -> String {
    let p = 100.0 * f;
    if p < 0.1 { format!("{p:.2}%") } else { format!("{p:.1}%") }
}

/// Build the warnings for a report.
pub(crate) fn warnings(report: &ScanReport, cfg: &HealthConfig) -> Vec<Warning> {
    let mut out = Vec::new();
    for c in &report.channels {
        let ch = Some(c.channel);
        let name = c.channel.name();
        if c.non_finite > 0 {
            out.push(Warning {
                kind: WarningKind::NonFinite,
                channel: ch,
                message: format!("{} non-finite values in {name} (treated as missing)", c.non_finite),
            });
        }
        if c.clipped_high > cfg.warn_clipped_fraction {
            out.push(Warning {
                kind: WarningKind::HighlightsClipped,
                channel: ch,
                message: format!("Highlights clipped in {name}: {} of frame (sensor saturated)", pct(c.clipped_high)),
            });
        }
        if c.clipped_low > cfg.warn_clipped_fraction {
            out.push(Warning {
                kind: WarningKind::ShadowsClipped,
                channel: ch,
                message: format!("Shadows clipped in {name}: {} of frame at the black level", pct(c.clipped_low)),
            });
        }
        if c.white_level < cfg.warn_low_peak {
            out.push(Warning {
                kind: WarningKind::WeakSignal,
                channel: ch,
                message: format!("Weak signal in {name}: peak at {} of the sensor range, {:.1} stops unused", pct(c.white_level), c.headroom_stops),
            });
        }
    }
    for d in &report.line_defects {
        let mut msg = d.describe();
        if let Some(first) = msg.get(..1) {
            msg = first.to_uppercase() + msg.get(1..).unwrap_or("");
        }
        out.push(Warning { kind: WarningKind::LineDefect, channel: Some(d.channel), message: msg });
    }
    let across = match report.axis {
        StreakAxis::Columns => "row",
        StreakAxis::Rows => "column",
    };
    for b in &report.banding {
        let name = b.channel.name();
        if b.rms > cfg.warn_banding || !b.banded.is_empty() {
            out.push(Warning {
                kind: WarningKind::Banding,
                channel: Some(b.channel),
                message: format!(
                    "{}{} banding in {name}: {} RMS {across}-to-{across} gain variation, {} {across}{} {}",
                    across.get(..1).unwrap_or("").to_uppercase(),
                    across.get(1..).unwrap_or(""),
                    pct(b.rms),
                    b.banded.len(),
                    if b.banded.len() == 1 { "" } else { "s" },
                    if b.repaired { "corrected" } else { "significant" }
                ),
            });
        }
        if !b.strong.is_empty() {
            let list: Vec<String> = b.strong.iter().take(5).map(|s| s.index.to_string()).collect();
            let more = if b.strong.len() > 5 { ", …" } else { "" };
            out.push(Warning {
                kind: WarningKind::StrongRows,
                channel: Some(b.channel),
                message: format!(
                    "{} strong {across} defect(s) in {name} ({across}s {}{more}): {}",
                    b.strong.len(),
                    list.join(", "),
                    if b.repaired { "interpolated" } else { "left unrepaired (may be image content)" }
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_fractions_and_levels() {
        // 10 % saturated, 5 % at zero, rest a ramp.
        let p = Plane::from_fn(100, 100, |x, y| {
            let i = y * 100 + x;
            if i < 1000 {
                1.0
            } else if i < 1500 {
                0.0
            } else {
                0.1 + 0.5 * (i as f32 / 10000.0)
            }
        })
        .unwrap();
        let h = channel_health(&p, Channel::Blue, &HealthConfig::default());
        assert!((h.clipped_high - 0.10).abs() < 1e-6, "{}", h.clipped_high);
        assert!((h.clipped_low - 0.05).abs() < 1e-6, "{}", h.clipped_low);
        assert_eq!(h.white_level, 1.0);
        assert_eq!(h.black_level, 0.0);
        assert_eq!(h.headroom_stops, 0.0);
    }

    #[test]
    fn noise_floor_measures_dark_noise() {
        // Dark half with ±0.001 alternating noise, bright textured half.
        let p = Plane::from_fn(200, 100, |x, y| {
            if x < 100 { 0.01 + if (x * 7 + y * 13) % 2 == 0 { 0.001 } else { -0.001 } } else { 0.5 + 0.3 * ((x * y) as f32).sin() }
        })
        .unwrap();
        let h = channel_health(&p, Channel::Red, &HealthConfig::default());
        // Differences of ±0.002: sigma = 1.4826·0.002/√2 ≈ 0.0021, not the bright texture.
        assert!(h.noise_floor > 0.001 && h.noise_floor < 0.003, "{}", h.noise_floor);
        assert!(h.dynamic_range_stops > 7.0, "{}", h.dynamic_range_stops);
    }
}
