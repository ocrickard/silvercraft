//! Roll-consistent calibration FIT (`compute_roll_calibration_from_samples`).
//!
//! Given a set of f32 linear RGB frames (row-major, interleaved RGB, the same
//! buffer convention as `render`/`pipeline`) plus one gap-sampled film base
//! per frame and the stock profile's density scale / anchor percentiles, this
//! fits ONE calibration for the whole roll: the median film base, a FULL 3x3
//! density matrix (regularized least squares over the neutral samples, ridged
//! toward the diagonal anchor `diag(density_scale)` with the off-diagonals
//! pulled harder toward zero; diagonal fallback preserved), and the joint
//! per-channel achromatic-axis density
//! curve `d'_c = exp(alpha_c) * d_c^gamma_c` (sum alpha = 0, sum gamma = 3),
//! fit by Huber-IRLS constrained least squares over reference-free neutral
//! samples, with the same clamps and identity+geomean-gain fallbacks. All
//! fit math runs in f64; only the input buffers are f32.
//!
//! SilverCraft additions: `e_mid` (the
//! median corrected neutral luminance that sets the roll-level scene-linear
//! scale `k`), [`apply_calibration`] (stopping at the
//! excitation `e`), [`geomean_neutral_gain`] made public for per-strip gains
//!, and panic-free handling of hostile input.

use crate::config::{CalibrationConfig, CurveFallback, EMidEstimator, SceneConfig};
use crate::error::FilmError;
pub use crate::schema::RollCalibrationDocument;
use crate::stats::{LUMA, clamp_f64, median_f64 as median, percentile_f64 as percentile};

const D_FLOOR: f64 = 1e-4;
/// Gamut-safe correction clamp: 1.25x up / 0.8x down (symmetric in log).
/// Default of `calibration.max_log_correction`.
pub const MAX_LOG_CORRECTION: f64 = 0.22314355131420976; // ln(1.25)
const PAIRS: [(usize, usize); 3] = [(0, 1), (1, 2), (0, 2)];
/// Robust sigma from MAD.
const MAD_SIGMA: f64 = 1.4826;

/// Stock-profile inputs the fit needs (desktop `NegativeConversionProfile`).
#[derive(Debug, Clone, Copy)]
pub struct FitParams {
    pub density_scale: [f64; 3],
    pub low_percentile: f64,
    pub high_percentile: f64,
    /// Floor for the transmittance clip; `max(epsilon, 1e-6)` is applied
    /// inside, mirroring the Python oracle.
    pub epsilon: f64,
    pub neutralize: bool,
}

/// Solve A x = b by Gaussian elimination with partial pivoting. Returns
/// `None` when a pivot column is exactly zero — the cases where LAPACK (and
/// therefore the Python oracle's `np.linalg.solve`) raises LinAlgError.
fn solve_linear(matrix: &[Vec<f64>], rhs: &[f64]) -> Option<Vec<f64>> {
    let n = rhs.len();
    let mut a = matrix.to_vec();
    let mut b = rhs.to_vec();
    for col in 0..n {
        let mut pivot = col;
        let mut best = a[col][col].abs();
        for (row, entry) in a.iter().enumerate().skip(col + 1) {
            let value = entry[col].abs();
            if value > best {
                best = value;
                pivot = row;
            }
        }
        if best == 0.0 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in (col + 1)..n {
            let factor = a[row][col] / a[col][col];
            if factor != 0.0 {
                for k in col..n {
                    a[row][k] -= factor * a[col][k];
                }
                b[row] -= factor * b[col];
            }
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut sum = b[i];
        for (k, item) in x.iter().enumerate().skip(i + 1) {
            sum -= a[i][k] * item;
        }
        x[i] = sum / a[i][i];
    }
    Some(x)
}

/// Solve the equality-constrained least-squares KKT system
/// `[H C^T; C 0] [x; lambda] = [g; q]`, returning x.
fn kkt(h: &[Vec<f64>], g: &[f64], c: &[Vec<f64>], q: &[f64]) -> Option<Vec<f64>> {
    let n = g.len();
    let m = q.len();
    let size = n + m;
    let mut matrix = vec![vec![0.0; size]; size];
    for i in 0..n {
        for j in 0..n {
            matrix[i][j] = h[i][j];
        }
        for (row, constraint) in c.iter().enumerate() {
            matrix[i][n + row] = constraint[i];
            matrix[n + row][i] = constraint[i];
        }
    }
    let mut rhs = Vec::with_capacity(size);
    rhs.extend_from_slice(g);
    rhs.extend_from_slice(q);
    solve_linear(&matrix, &rhs).map(|x| x[..n].to_vec())
}

/// rgb sample (interleaved) -> density excitation: film-base division,
/// -log10 transmittance, full 3x3 density matrix, clamped at zero. Mirrors
/// `roll_develop._to_density_excitation`.
fn to_density_excitation(frame: &[f32], film_base: [f64; 3], matrix: [[f64; 3]; 3], eps: f64) -> Vec<[f64; 3]> {
    let mut out = Vec::with_capacity(frame.len() / 3);
    for pixel in frame.chunks_exact(3) {
        let mut density = [0.0; 3];
        for channel in 0..3 {
            let transmittance = clamp_f64(pixel[channel] as f64 / film_base[channel], eps, 1.0);
            density[channel] = (-transmittance.log10()).max(0.0);
        }
        let mut row = [0.0; 3];
        for (c, item) in row.iter_mut().enumerate() {
            *item = (matrix[c][0] * density[0] + matrix[c][1] * density[1] + matrix[c][2] * density[2]).max(0.0);
        }
        out.push(row);
    }
    out
}

/// Reference-free neutral selection: per frame, per luminance bin, the most
/// achromatic pixels (smallest log-density spread across channels).
/// Mirrors `roll_develop.select_neutral_samples`.
fn select_neutral_samples(frame_densities: &[Vec<[f64; 3]>], cfg: &CalibrationConfig) -> Vec<[f64; 3]> {
    let nbins = cfg.neutral_bins.max(1);
    let mut out = Vec::new();
    for densities in frame_densities {
        let filtered: Vec<[f64; 3]> = densities.iter().copied().filter(|pixel| pixel.iter().all(|value| *value > D_FLOOR)).collect();
        if filtered.is_empty() {
            continue;
        }
        let mut achro = Vec::with_capacity(filtered.len());
        let mut lumd = Vec::with_capacity(filtered.len());
        for pixel in &filtered {
            let z = pixel.map(|value| value.max(D_FLOOR).ln());
            let mean = (z[0] + z[1] + z[2]) / 3.0;
            let variance = ((z[0] - mean).powi(2) + (z[1] - mean).powi(2) + (z[2] - mean).powi(2)) / 3.0;
            achro.push(variance.sqrt());
            lumd.push(pixel[0] * LUMA[0] + pixel[1] * LUMA[1] + pixel[2] * LUMA[2]);
        }
        let lo = percentile(&mut lumd.clone(), cfg.neutral_luma_range[0]);
        let hi = percentile(&mut lumd.clone(), cfg.neutral_luma_range[1]);
        for bin in 0..nbins {
            let edge_lo = lo + (hi - lo) * bin as f64 / nbins as f64;
            let edge_hi = lo + (hi - lo) * (bin + 1) as f64 / nbins as f64;
            let mut indices: Vec<usize> = (0..filtered.len())
                .filter(|&index| lumd[index] >= edge_lo && lumd[index] <= edge_hi && achro[index] < cfg.neutral_achro_ceiling)
                .collect();
            indices.sort_by(|a, b| achro[*a].total_cmp(&achro[*b]));
            indices.truncate(cfg.neutral_per_bin_cap);
            out.extend(indices.into_iter().map(|index| filtered[index]));
        }
    }
    out
}

/// Geomean-normalized per-channel gain that makes the near-neutral cloud
/// achromatic (the identity-curve fallback). Mirrors
/// `roll_develop.geomean_neutral_gain`.
pub fn geomean_neutral_gain(excitation: &[[f64; 3]]) -> [f64; 3] {
    geomean_neutral_gain_with(excitation, &CalibrationConfig::default())
}

/// [`geomean_neutral_gain`] with explicit `calibration.gain_*` knobs.
pub fn geomean_neutral_gain_with(excitation: &[[f64; 3]], cfg: &CalibrationConfig) -> [f64; 3] {
    let mut lum = Vec::with_capacity(excitation.len());
    let mut low_chroma = Vec::with_capacity(excitation.len());
    for pixel in excitation {
        let max = pixel[0].max(pixel[1]).max(pixel[2]);
        let min = pixel[0].min(pixel[1]).min(pixel[2]);
        let chroma = (max - min) / max.max(1e-12);
        lum.push(pixel[0] * LUMA[0] + pixel[1] * LUMA[1] + pixel[2] * LUMA[2]);
        low_chroma.push(chroma < cfg.gain_chroma_ceiling);
    }
    if lum.is_empty() {
        return [1.0; 3];
    }
    let p20 = percentile(&mut lum.clone(), cfg.gain_luma_range[0]);
    let p90 = percentile(&mut lum.clone(), cfg.gain_luma_range[1]);
    let mut count = 0usize;
    let mut reference = [0.0; 3];
    for (index, pixel) in excitation.iter().enumerate() {
        if low_chroma[index] && lum[index] > p20 && lum[index] < p90 {
            count += 1;
            for channel in 0..3 {
                reference[channel] += pixel[channel];
            }
        }
    }
    if count <= cfg.gain_min_count {
        return [1.0; 3];
    }
    for value in &mut reference {
        *value /= count as f64;
    }
    let mean = (reference[0] + reference[1] + reference[2]) / 3.0;
    let mut gain = [0.0; 3];
    for channel in 0..3 {
        gain[channel] = mean / reference[channel].max(1e-12);
    }
    let log_mean = (gain.map(|value| value.max(1e-12).ln()).iter().sum::<f64>()) / 3.0;
    let norm = log_mean.exp();
    gain.map(|value| value / norm)
}

/// Full 3x3 density-matrix fit over the neutral samples: per sample the three
/// channels' corrected densities should be equal, so for each channel pair
/// (c, k) the residual `(M.d)_c - (M.d)_k` (linear in M's 9 entries, row-major
/// x[3c+k] = M[c][k]) is least-squares minimized with the curve fit's 1/(3n)
/// scaling. Ridge toward the diagonal anchor D = diag(density_scale):
/// LAM_M_DIAG on (M_cc - D_cc)^2, LAM_M_OFF on M_ck^2 for c != k, and the
/// same 4-pass Huber-IRLS robustness as `fit_density_curve`. `None` on a
/// singular system (the Python `np.linalg.LinAlgError` case). Mirrors
/// `roll_develop._fit_density_matrix`.
fn fit_density_matrix(samples: &[[f64; 3]], scale: [f64; 3], cfg: &CalibrationConfig) -> Option<[[f64; 3]; 3]> {
    let n = samples.len();
    let s2 = 1.0 / (3.0 * n as f64);
    let s = s2.sqrt();
    let mut rows: Vec<[f64; 9]> = Vec::with_capacity(3 * n);
    for d in samples {
        for &(c, k) in &PAIRS {
            let mut row = [0.0; 9];
            for j in 0..3 {
                row[3 * c + j] = d[j];
                row[3 * k + j] = -d[j];
            }
            rows.push(row);
        }
    }
    let mut g_reg = [0.0; 9];
    for c in 0..3 {
        g_reg[3 * c + c] = cfg.lambda_diag * scale[c];
    }
    let mut weights = vec![1.0_f64; rows.len()];
    let mut theta = [0.0; 9];
    for _ in 0..cfg.huber_passes {
        let mut h = vec![vec![0.0; 9]; 9];
        for c in 0..3 {
            for k in 0..3 {
                h[3 * c + k][3 * c + k] = if c == k { cfg.lambda_diag } else { cfg.lambda_off };
            }
        }
        for (row, &weight) in rows.iter().zip(weights.iter()) {
            for i in 0..9 {
                if row[i] == 0.0 {
                    continue;
                }
                for j in 0..9 {
                    h[i][j] += s2 * weight * row[i] * row[j];
                }
            }
        }
        let solution = solve_linear(&h, &g_reg)?;
        theta.copy_from_slice(&solution);
        // Huber weights from the residuals of this pass.
        let residuals: Vec<f64> = rows.iter().map(|row| s * (0..9).map(|i| row[i] * theta[i]).sum::<f64>()).collect();
        let med = median(&mut residuals.clone());
        let mut deviations: Vec<f64> = residuals.iter().map(|r| (r - med).abs()).collect();
        let mad = median(&mut deviations) + 1e-12;
        let delta = cfg.huber_k * MAD_SIGMA * mad;
        for (weight, residual) in weights.iter_mut().zip(residuals.iter()) {
            *weight = if residual.abs() <= delta { 1.0 } else { delta / residual.abs().max(1e-12) };
        }
    }
    let mut matrix = [[0.0; 3]; 3];
    for c in 0..3 {
        for k in 0..3 {
            matrix[c][k] = theta[3 * c + k];
        }
    }
    Some(matrix)
}

/// Mean over samples of the cross-channel std of log-density — the achromatic
/// measure `select_neutral_samples` sorts by.
fn achromatic_spread(samples: &[[f64; 3]]) -> f64 {
    let mut sum = 0.0;
    for pixel in samples {
        let z = pixel.map(|value| value.max(D_FLOOR).ln());
        let mean = (z[0] + z[1] + z[2]) / 3.0;
        let variance = ((z[0] - mean).powi(2) + (z[1] - mean).powi(2) + (z[2] - mean).powi(2)) / 3.0;
        sum += variance.sqrt();
    }
    sum / samples.len() as f64
}

/// Matrix validation: keep the fitted matrix ONLY if it cuts the neutral
/// cloud's achromatic spread by at least 10% versus the diagonal and stays
/// near-diagonal (max |off-diagonal| <= 0.5). Otherwise the diagonal anchor
/// is kept. Mirrors `roll_develop._accept_density_matrix`.
fn accept_density_matrix(matrix: &[[f64; 3]; 3], samples: &[[f64; 3]], cfg: &CalibrationConfig) -> bool {
    let base = achromatic_spread(samples);
    let corrected: Vec<[f64; 3]> = samples
        .iter()
        .map(|d| {
            let mut row = [0.0; 3];
            for (c, item) in row.iter_mut().enumerate() {
                *item = (matrix[c][0] * d[0] + matrix[c][1] * d[1] + matrix[c][2] * d[2]).max(0.0);
            }
            row
        })
        .collect();
    let fitted = achromatic_spread(&corrected);
    let mut max_off = 0.0_f64;
    for c in 0..3 {
        for k in 0..3 {
            if c != k {
                max_off = max_off.max(matrix[c][k].abs());
            }
        }
    }
    fitted <= cfg.accept_spread_ratio * base && max_off <= cfg.max_off_diagonal
}

/// over channel pairs with hard constraints sum(alpha) = 0, sum(gamma) = 3,
/// Huber-IRLS robustness, and L2 regularization toward (0, 1). Mirrors
/// `roll_develop._fit_density_curve`; `None` on a singular KKT system (the
/// Python `np.linalg.LinAlgError` case).
#[allow(clippy::type_complexity)]
fn fit_density_curve(samples: &[[f64; 3]], cfg: &CalibrationConfig) -> Option<([f64; 3], [f64; 3], [f64; 3], [f64; 3])> {
    let n = samples.len();
    let z: Vec<[f64; 3]> = samples.iter().map(|pixel| pixel.map(|value| value.max(D_FLOOR).ln())).collect();
    // Aeq rows are scaled by 1/sqrt(3n); accumulate the s^2 factor directly.
    let s2 = 1.0 / (3.0 * n as f64);
    let s = s2.sqrt();
    // Per-(pair, sample) design rows r (6-dim) and their Huber weights.
    let mut rows: Vec<[f64; 6]> = Vec::with_capacity(3 * n);
    for zi in &z {
        for &(c, k) in &PAIRS {
            let mut row = [0.0; 6];
            row[c] = 1.0;
            row[k] = -1.0;
            row[3 + c] = zi[c];
            row[3 + k] = -zi[k];
            rows.push(row);
        }
    }
    let mut weights = vec![1.0_f64; rows.len()];
    let mut theta = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0];
    for _ in 0..cfg.huber_passes {
        // H = (Aeq * w)^T Aeq + Areg^T Areg ; g_reg = Areg^T breg.
        let mut h = vec![vec![0.0; 6]; 6];
        for (channel, item) in h.iter_mut().enumerate() {
            item[channel] = if channel < 3 { cfg.lambda_alpha } else { cfg.lambda_gamma };
        }
        for (row, &weight) in rows.iter().zip(weights.iter()) {
            for i in 0..6 {
                if row[i] == 0.0 {
                    continue;
                }
                for j in 0..6 {
                    h[i][j] += s2 * weight * row[i] * row[j];
                }
            }
        }
        let g_reg = [0.0, 0.0, 0.0, cfg.lambda_gamma, cfg.lambda_gamma, cfg.lambda_gamma];
        let constraints = vec![vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0], vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0]];
        let solution = kkt(&h, &g_reg, &constraints, &[0.0, 3.0])?;
        theta.copy_from_slice(&solution);
        // Huber weights from the residuals of this pass.
        let residuals: Vec<f64> = rows.iter().map(|row| s * (0..6).map(|i| row[i] * theta[i]).sum::<f64>()).collect();
        let med = median(&mut residuals.clone());
        let mut deviations: Vec<f64> = residuals.iter().map(|r| (r - med).abs()).collect();
        let mad = median(&mut deviations) + 1e-12;
        let delta = cfg.huber_k * MAD_SIGMA * mad;
        for (weight, residual) in weights.iter_mut().zip(residuals.iter()) {
            *weight = if residual.abs() <= delta { 1.0 } else { delta / residual.abs().max(1e-12) };
        }
    }
    let mut gamma = [3, 4, 5].map(|i| clamp_f64(theta[i], cfg.gamma_clamp[0], cfg.gamma_clamp[1]));
    let gamma_mean = (gamma[0] + gamma[1] + gamma[2]) / 3.0;
    for value in &mut gamma {
        *value = *value - gamma_mean + 1.0; // renormalize sum -> 3 after clamping
    }
    // Refit alpha (sum 0) with gamma fixed, so balance and slope share one gauge.
    let mut normal = vec![vec![0.0; 3]; 3];
    for channel in 0..3 {
        normal[channel][channel] = cfg.lambda_alpha;
    }
    let mut target = [0.0; 3];
    for zi in &z {
        for &(c, k) in &PAIRS {
            let b = -(gamma[c] * zi[c] - gamma[k] * zi[k]);
            normal[c][c] += s2;
            normal[c][k] -= s2;
            normal[k][c] -= s2;
            normal[k][k] += s2;
            target[c] += s2 * b;
            target[k] -= s2 * b;
        }
    }
    let alpha = kkt(&normal, &target, &[vec![1.0, 1.0, 1.0]], &[0.0])?;
    let mut alpha_out = [0.0; 3];
    alpha_out.copy_from_slice(&alpha);
    let mut log_min = [0.0; 3];
    let mut log_max = [0.0; 3];
    for channel in 0..3 {
        let mut values: Vec<f64> = z.iter().map(|zi| zi[channel]).collect();
        log_min[channel] = percentile(&mut values, 1.0);
        log_max[channel] = percentile(&mut values, 99.0);
    }
    Some((alpha_out, gamma, log_min, log_max))
}

/// Identity gamma + geomean neutral gain folded into alpha — the fallback
/// for too few reliable neutrals or an excessive-correction fit.
fn identity_curve_with_gain(pooled: &[[f64; 3]], cfg: &CalibrationConfig) -> ([f64; 3], [f64; 3], [f64; 3], [f64; 3]) {
    let gain = match cfg.fallback {
        CurveFallback::IdentityWithGain => geomean_neutral_gain_with(pooled, cfg),
        CurveFallback::Identity => [1.0; 3],
    };
    (gain.map(|value| value.max(1e-12).ln()), [1.0; 3], [-20.0; 3], [20.0; 3])
}

/// One channel of the achromatic-axis curve:
/// `h = clamp(ln max(d, 1e-4), lmin, lmax)`, `e = d * exp(clamp(a + (g-1) h, +-ln 1.25))`.
/// Panic-free for any (even inverted or NaN) bounds.
#[inline]
pub fn apply_curve_channel(d: f64, alpha: f64, gamma: f64, log_min: f64, log_max: f64, max_log_correction: f64) -> f64 {
    let h = clamp_f64(d.max(D_FLOOR).ln(), log_min, log_max);
    let delta = clamp_f64(alpha + (gamma - 1.0) * h, -max_log_correction, max_log_correction);
    (d * delta.exp()).max(0.0)
}

/// `roll_develop._apply_density_curve`: gamut-safe per-channel log-density
/// correction with the fitted range clip and the +-ln(1.25) clamp.
fn apply_density_curve(
    density: &[[f64; 3]],
    alpha: [f64; 3],
    gamma: [f64; 3],
    log_min: [f64; 3],
    log_max: [f64; 3],
    max_log_correction: f64,
) -> Vec<[f64; 3]> {
    density
        .iter()
        .map(|pixel| {
            let mut out = [0.0; 3];
            for channel in 0..3 {
                out[channel] =
                    apply_curve_channel(pixel[channel], alpha[channel], gamma[channel], log_min[channel], log_max[channel], max_log_correction);
            }
            out
        })
        .collect()
}

/// Fit ONE roll calibration over a set of f32 linear RGB frames (row-major,
/// interleaved RGB in [0, 1], pre-subsampled by the caller) with one
/// gap-sampled film base per frame. Mirrors
/// `roll_develop.compute_roll_calibration_from_samples`, with the default
/// [`CalibrationConfig`] / [`SceneConfig`] (bit-identical).
pub fn fit_roll_calibration(frames: &[&[f32]], film_bases: &[[f64; 3]], params: &FitParams) -> Result<RollCalibrationDocument, FilmError> {
    let scene = SceneConfig::default();
    fit_roll_calibration_with(frames, film_bases, params, &CalibrationConfig::default(), &scene, crate::profiles::DEFAULT_COLOR_NEGATIVE_GAMMA)
}

/// [`fit_roll_calibration`] with every knob explicit. `scene` supplies the
/// `e_mid` estimator and `g_ref` the G_ref it works in (log-exposure
/// estimators). `cfg.enabled = false` skips both fits (diagonal anchor,
/// identity curve) but still reports black/white and `e_mid`.
pub fn fit_roll_calibration_with(
    frames: &[&[f32]],
    film_bases: &[[f64; 3]],
    params: &FitParams,
    cfg: &CalibrationConfig,
    scene: &SceneConfig,
    g_ref: f64,
) -> Result<RollCalibrationDocument, FilmError> {
    if frames.len() != film_bases.len() {
        return Err(FilmError::MismatchedBases { frames: frames.len(), bases: film_bases.len() });
    }
    if frames.iter().any(|frame| frame.len() % 3 != 0) {
        return Err(FilmError::BadFrameLength);
    }
    let eps = clamp_f64(params.epsilon, 1e-6, 1.0);
    let max_corr = if cfg.max_log_correction.is_nan() { MAX_LOG_CORRECTION } else { cfg.max_log_correction.max(0.0) };
    let mut film_base = [0.0; 3];
    for (channel, base) in film_base.iter_mut().enumerate() {
        let mut values: Vec<f64> = film_bases.iter().map(|bases| bases[channel]).collect();
        *base = if values.is_empty() { 0.0 } else { median(&mut values) };
    }
    let scale = params.density_scale;
    let diagonal_matrix = [[scale[0], 0.0, 0.0], [0.0, scale[1], 0.0], [0.0, 0.0, scale[2]]];
    let neutralize = params.neutralize && cfg.enabled;

    // Pass 1: diagonal-anchor densities -> neutral selection -> full 3x3
    // matrix fit (diagonal fallback on too few neutrals, a singular system,
    // or a failed validation margin).
    let density_matrix = if neutralize && cfg.fit_matrix {
        let initial_densities: Vec<Vec<[f64; 3]>> =
            frames.iter().map(|frame| to_density_excitation(frame, film_base, diagonal_matrix, eps)).collect();
        let neutrals = select_neutral_samples(&initial_densities, cfg);
        if neutrals.len() >= cfg.neutral_min_count {
            match fit_density_matrix(&neutrals, scale, cfg) {
                Some(matrix) if accept_density_matrix(&matrix, &neutrals, cfg) => matrix,
                _ => diagonal_matrix,
            }
        } else {
            diagonal_matrix
        }
    } else {
        diagonal_matrix
    };

    // Pass 2: densities with the FITTED matrix, neutral RE-selection on the
    // corrected densities (the fit's neutral selection is otherwise
    // self-referential), then the achromatic curve fit unchanged.
    let frame_densities: Vec<Vec<[f64; 3]>> = frames.iter().map(|frame| to_density_excitation(frame, film_base, density_matrix, eps)).collect();
    let pooled: Vec<[f64; 3]> = frame_densities.iter().flatten().copied().collect();

    let (alpha, gamma, log_min, log_max) = if neutralize && !pooled.is_empty() {
        let neutrals = select_neutral_samples(&frame_densities, cfg);
        let fitted = if cfg.fit_curve && neutrals.len() >= cfg.neutral_min_count { fit_density_curve(&neutrals, cfg) } else { None };
        match fitted {
            Some((alpha, gamma, log_min, log_max)) => {
                // The fit is untrustworthy when it wants excessive correction.
                let mut clamped = 0usize;
                for pixel in &pooled {
                    for channel in 0..3 {
                        let h = clamp_f64(pixel[channel].max(D_FLOOR).ln(), log_min[channel], log_max[channel]);
                        let delta = alpha[channel] + (gamma[channel] - 1.0) * h;
                        if !(-max_corr..=max_corr).contains(&delta) {
                            clamped += 1;
                        }
                    }
                }
                let clamp_hit = clamped as f64 / (3 * pooled.len()) as f64;
                if clamp_hit > cfg.trust_clamp_fraction { identity_curve_with_gain(&pooled, cfg) } else { (alpha, gamma, log_min, log_max) }
            }
            None => identity_curve_with_gain(&pooled, cfg),
        }
    } else {
        ([0.0; 3], [1.0; 3], [-20.0; 3], [20.0; 3])
    };

    let corrected = apply_density_curve(&pooled, alpha, gamma, log_min, log_max, max_corr);
    let e_mid = estimate_e_mid(&frame_densities, &corrected, (alpha, gamma, log_min, log_max), max_corr, cfg, scene, g_ref);
    let (density_black, density_white) = if corrected.is_empty() {
        (0.0, 0.0)
    } else {
        let mut lum: Vec<f64> = corrected.iter().map(|pixel| pixel[0] * LUMA[0] + pixel[1] * LUMA[1] + pixel[2] * LUMA[2]).collect();
        (percentile(&mut lum, params.low_percentile), percentile(&mut lum, params.high_percentile))
    };

    Ok(RollCalibrationDocument {
        film_base,
        density_matrix,
        curve_alpha: alpha,
        curve_gamma: gamma,
        curve_log_min: log_min,
        curve_log_max: log_max,
        curve_max_log_correction: max_corr,
        density_black,
        density_white,
        e_mid,
        extra: serde_json::Map::new(),
    })
}

/// Cross-channel std of log density (the achromatic measure).
fn achro_of(pixel: &[f64; 3]) -> f64 {
    let z = pixel.map(|value| value.max(D_FLOOR).ln());
    let mean = (z[0] + z[1] + z[2]) / 3.0;
    (((z[0] - mean).powi(2) + (z[1] - mean).powi(2) + (z[2] - mean).powi(2)) / 3.0).sqrt()
}

/// e_mid (`scene.e_mid` estimator): the roll's mid-grey
/// corrected density, which sets the scene-linear scale k.
#[allow(clippy::type_complexity)]
fn estimate_e_mid(
    frame_densities: &[Vec<[f64; 3]>],
    corrected: &[[f64; 3]],
    curve: ([f64; 3], [f64; 3], [f64; 3], [f64; 3]),
    max_corr: f64,
    cfg: &CalibrationConfig,
    scene: &SceneConfig,
    g_ref: f64,
) -> Option<f64> {
    let lum_of = |pixel: &[f64; 3]| pixel[0] * LUMA[0] + pixel[1] * LUMA[1] + pixel[2] * LUMA[2];
    let finite_median = |values: Vec<f64>| {
        let mut values: Vec<f64> = values.into_iter().filter(|v| v.is_finite()).collect();
        if values.is_empty() { None } else { Some(median(&mut values)) }
    };
    match scene.e_mid {
        EMidEstimator::Fixed => scene.e_mid_fixed,
        EMidEstimator::BinnedMedian => {
            let neutrals = select_neutral_samples(frame_densities, cfg);
            let (alpha, gamma, log_min, log_max) = curve;
            if neutrals.len() > scene.e_mid_min_neutrals {
                finite_median(apply_density_curve(&neutrals, alpha, gamma, log_min, log_max, max_corr).iter().map(lum_of).collect())
            } else {
                finite_median(corrected.iter().map(lum_of).collect())
            }
        }
        EMidEstimator::AllNeutralMedian | EMidEstimator::NeutralRangeMidpoint => {
            let neutral: Vec<f64> =
                corrected.iter().filter(|p| p.iter().all(|v| *v > D_FLOOR) && achro_of(p) < cfg.neutral_achro_ceiling).map(lum_of).collect();
            let lum = if neutral.len() > scene.e_mid_min_neutrals { neutral } else { corrected.iter().map(lum_of).collect() };
            if scene.e_mid == EMidEstimator::AllNeutralMedian {
                return finite_median(lum);
            }
            // Log exposure above base through the transform itself:
            // s = log10(10^(e/G) - 1); midpoint in s; back to density.
            let g = if g_ref.is_finite() && g_ref > 1e-3 { g_ref } else { crate::profiles::DEFAULT_COLOR_NEGATIVE_GAMMA };
            let mut s: Vec<f64> = lum.iter().filter(|e| e.is_finite() && **e > D_FLOOR).map(|e| (10f64.powf(e / g) - 1.0).log10()).collect();
            if s.is_empty() {
                return None;
            }
            let lo = percentile(&mut s, scene.e_mid_percentiles[0]);
            let hi = percentile(&mut s, scene.e_mid_percentiles[1]);
            let mid = (lo + hi) / 2.0;
            let e = g * (1.0 + 10f64.powf(mid)).log10();
            e.is_finite().then_some(e)
        }
    }
}

/// The f32 render-time calibration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollCalibration {
    pub film_base: [f32; 3],
    pub density_matrix: [f32; 9],
    pub curve_alpha: [f32; 3],
    pub curve_gamma: [f32; 3],
    pub curve_log_min: [f32; 3],
    pub curve_log_max: [f32; 3],
    pub curve_max_log_correction: f32,
    pub density_black: f32,
    pub density_white: f32,
}

/// Spec 5 step 9: transmittance -> corrected density excitation `e` with
/// the roll calibration (base division, `T/b` clamped to `[eps, 1]`, sensor
/// floor, 3x3 matrix clamped at 0, alpha/gamma curve). STOPS at `e`; the
/// display-referred half of a calibrated develop is not carried over.
/// `dcap` caps the raw density per channel (highlight cap).
#[inline]
pub fn apply_calibration(pixel: [f64; 3], document: &RollCalibrationDocument, eps: f64, dcap: [f64; 3]) -> [f64; 3] {
    let mut density = [0.0; 3];
    for channel in 0..3 {
        let t = pixel[channel].max(f64::from(crate::base::SENSOR_FLOOR));
        let ratio = clamp_f64(t / document.film_base[channel], eps, 1.0);
        let d = if ratio.is_nan() { 0.0 } else { (-ratio.log10()).max(0.0) };
        density[channel] = d.min(dcap[channel]);
    }
    let m = &document.density_matrix;
    let mut out = [0.0; 3];
    for c in 0..3 {
        let mixed = (m[c][0] * density[0] + m[c][1] * density[1] + m[c][2] * density[2]).max(0.0);
        out[c] = apply_curve_channel(
            mixed,
            document.curve_alpha[c],
            document.curve_gamma[c],
            document.curve_log_min[c],
            document.curve_log_max[c],
            document.curve_max_log_correction,
        );
    }
    out
}

/// A pure diagonal calibration (no fit): `diag(scale)`, identity curve. The
/// fallback when a roll calibration is unavailable.
pub fn diagonal_calibration(film_base: [f64; 3], scale: [f64; 3]) -> RollCalibrationDocument {
    RollCalibrationDocument {
        film_base,
        density_matrix: [[scale[0], 0.0, 0.0], [0.0, scale[1], 0.0], [0.0, 0.0, scale[2]]],
        curve_alpha: [0.0; 3],
        curve_gamma: [1.0; 3],
        curve_log_min: [-20.0; 3],
        curve_log_max: [20.0; 3],
        curve_max_log_correction: MAX_LOG_CORRECTION,
        density_black: 0.0,
        density_white: 0.0,
        e_mid: None,
        extra: serde_json::Map::new(),
    }
}

/// Narrow a fitted calibration document to the f32 render-time struct.
pub fn to_render_calibration(document: &RollCalibrationDocument) -> RollCalibration {
    let mut density_matrix = [0.0_f32; 9];
    for row in 0..3 {
        for column in 0..3 {
            density_matrix[row * 3 + column] = document.density_matrix[row][column] as f32;
        }
    }
    RollCalibration {
        film_base: document.film_base.map(|value| value as f32),
        density_matrix,
        curve_alpha: document.curve_alpha.map(|value| value as f32),
        curve_gamma: document.curve_gamma.map(|value| value as f32),
        curve_log_min: document.curve_log_min.map(|value| value as f32),
        curve_log_max: document.curve_log_max.map(|value| value as f32),
        curve_max_log_correction: document.curve_max_log_correction as f32,
        density_black: document.density_black as f32,
        density_white: document.density_white as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift64* so tests need no rand dependency.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            // Box-Muller
            let u1 = self.uniform(1e-12, 1.0);
            let u2 = self.uniform(0.0, 1.0);
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    /// A synthetic roll: neutral density ramp with a per-channel gain/gamma
    /// crossover distortion, mapped back through the film base into RGB.
    fn synthetic_roll(rng: &mut Rng, frames: usize, samples_per_frame: usize, base: [f64; 3]) -> (Vec<Vec<f32>>, Vec<[f64; 3]>) {
        let mut roll = Vec::new();
        let mut bases = Vec::new();
        for frame in 0..frames {
            let frame_base = base.map(|value| value * rng.uniform(0.999, 1.001));
            bases.push(frame_base);
            let mut pixels = Vec::with_capacity(samples_per_frame * 3);
            for _ in 0..samples_per_frame {
                let d = rng.uniform(0.02, 1.6);
                let distorted = [1.08 * d.powf(1.06), d, 0.94 * d.powf(0.95)];
                for channel in 0..3 {
                    let noisy = distorted[channel] * (0.01 * rng.normal()).exp();
                    let transmittance = 10.0_f64.powf(-noisy);
                    // 1% chromatic content so the roll is not purely neutral.
                    let chroma = if frame % 2 == 0 && channel == 1 { rng.uniform(0.85, 1.0) } else { 1.0 };
                    pixels.push((frame_base[channel] * transmittance * chroma) as f32);
                }
            }
            roll.push(pixels);
        }
        (roll, bases)
    }

    fn params() -> FitParams {
        FitParams { density_scale: [1.0, 1.0, 1.0], low_percentile: 0.5, high_percentile: 99.5, epsilon: 1e-12, neutralize: true }
    }

    const UNIT_MATRIX: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    /// A synthetic roll with a KNOWN cross-channel structure: a near-neutral
    /// density cloud (a shared level `d` plus a zero-sum chromatic `tilt`
    /// direction, so the cloud spans the chromatic directions the matrix fit
    /// is identified from) mixed by a near-diagonal 3x3 `coupling` matrix,
    /// mapped back through the film base into RGB.
    fn coupled_roll(
        rng: &mut Rng,
        frames: usize,
        samples_per_frame: usize,
        base: [f64; 3],
        coupling: [[f64; 3]; 3],
        tilt: [f64; 3],
    ) -> (Vec<Vec<f32>>, Vec<[f64; 3]>) {
        let mut roll = Vec::new();
        let mut bases = Vec::new();
        for _ in 0..frames {
            let frame_base = base.map(|value| value * rng.uniform(0.999, 1.001));
            bases.push(frame_base);
            let mut pixels = Vec::with_capacity(samples_per_frame * 3);
            for _ in 0..samples_per_frame {
                let d = rng.uniform(0.02, 1.6);
                let t = rng.normal();
                let true_density = [(d * (1.0 + t * tilt[0])).max(0.01), (d * (1.0 + t * tilt[1])).max(0.01), (d * (1.0 + t * tilt[2])).max(0.01)];
                for channel in 0..3 {
                    let mixed =
                        coupling[channel][0] * true_density[0] + coupling[channel][1] * true_density[1] + coupling[channel][2] * true_density[2];
                    let noisy = mixed * (0.005 * rng.normal()).exp();
                    let transmittance = 10.0_f64.powf(-noisy);
                    pixels.push((frame_base[channel] * transmittance) as f32);
                }
            }
            roll.push(pixels);
        }
        (roll, bases)
    }

    #[test]
    fn matrix_fit_absorbs_coupling_on_achromatic_roll() {
        // Achromatic truth mixed by a known coupling C: the observed neutral
        // cloud is a single LINE d * (C . 1), so the off-diagonals are
        // unidentifiable and the ridge keeps them ~0; the coupling shows up
        // entirely as per-channel gain, which the diagonal recovers as
        // ~1/row-sums(C). The fitted matrix must achromatize the cloud.
        let coupling = [[1.0, 0.06, 0.0], [0.03, 1.0, 0.048], [0.0, 0.024, 1.0]];
        let mut rng = Rng(0xC09F);
        let (roll, bases) = coupled_roll(&mut rng, 4, 8000, [0.8, 0.7, 0.5], coupling, [0.0; 3]);
        let frames: Vec<&[f32]> = roll.iter().map(Vec::as_slice).collect();
        let document = fit_roll_calibration(&frames, &bases, &params()).unwrap();
        let m = document.density_matrix;

        // Not the identity fallback: the coupling was detected and undone.
        // (The diagonal lands a few percent below 1/row-sums: the ridge toward
        // the anchor plus the overall-scale gauge shared with the curve's
        // alpha shrink it slightly.)
        let row_sums = [1.06, 1.078, 1.024];
        for c in 0..3 {
            assert!((m[c][c] - 1.0 / row_sums[c]).abs() < 0.05, "M[{c}][{c}]={} should recover gain 1/{}", m[c][c], row_sums[c]);
        }
        // Near-diagonal constraint: off-diagonals stay tiny on a line cloud.
        for c in 0..3 {
            for k in 0..3 {
                if c != k {
                    assert!(m[c][k].abs() < 2e-3, "M[{c}][{k}]={}", m[c][k]);
                }
            }
        }
        // The corrected neutral cloud is achromatic: M . (C . 1) is constant
        // across channels (the coupling's cast is removed).
        let corrected: Vec<f64> = (0..3).map(|c| (0..3).map(|j| m[c][j] * (coupling[j][0] + coupling[j][1] + coupling[j][2])).sum::<f64>()).collect();
        let mean = corrected.iter().sum::<f64>() / 3.0;
        for c in 0..3 {
            assert!((corrected[c] / mean - 1.0).abs() < 0.02, "corrected channel {c} = {}", corrected[c]);
        }
    }

    #[test]
    fn matrix_fit_recovers_known_cross_channel_tilt() {
        // A zero-sum chromatic tilt w = (2a, -a, -a) is a KNOWN cross-channel
        // structure the diagonal alone cannot fix (any gain that achromatizes
        // the mean line leaves the tilt). The fit must use the full matrix,
        // with the ridge-limited off-diagonal sign pattern the min-ridge
        // achromatizer predicts: M01, M02, M10, M20 > 0 and M12, M21 < 0.
        let coupling = [[1.0, 0.06, 0.0], [0.03, 1.0, 0.048], [0.0, 0.024, 1.0]];
        let tilt = [0.06, -0.03, -0.03];
        let mut rng = Rng(0x7117);
        let (roll, bases) = coupled_roll(&mut rng, 4, 8000, [0.8, 0.7, 0.5], coupling, tilt);
        let frames: Vec<&[f32]> = roll.iter().map(Vec::as_slice).collect();
        let document = fit_roll_calibration(&frames, &bases, &params()).unwrap();
        let m = document.density_matrix;

        // Off-diagonal sign/magnitude pattern of the known tilt response
        // (magnitudes are capped near lam_diag/lam_off x tilt by the ridge).
        assert!(m[0][1] > 0.003 && m[0][1] < 0.05, "M01={}", m[0][1]);
        assert!(m[0][2] > 0.003 && m[0][2] < 0.05, "M02={}", m[0][2]);
        assert!(m[1][0] > 0.0 && m[1][0] < 0.05, "M10={}", m[1][0]);
        assert!(m[2][0] > 0.0 && m[2][0] < 0.05, "M20={}", m[2][0]);
        assert!(m[1][2] < -0.002 && m[1][2] > -0.05, "M12={}", m[1][2]);
        assert!(m[2][1] < -0.002 && m[2][1] > -0.05, "M21={}", m[2][1]);
        // The diagonal slides off the anchor (gauge freedom shared with the
        // curve's alpha): the matrix genuinely fits, not the fallback.
        for c in 0..3 {
            assert!((m[c][c] - 1.0).abs() > 0.05, "M[{c}][{c}]={}", m[c][c]);
        }
    }

    #[test]
    fn falls_back_to_diagonal_when_matrix_margin_unmet() {
        // A near-achromatic roll with NO coupling and NO tilt: no matrix can
        // cut the (noise-only) neutral spread by 10%, so the diagonal anchor
        // stands.
        let mut rng = Rng(0xD1A6);
        let (roll, bases) = coupled_roll(&mut rng, 4, 8000, [0.8, 0.7, 0.5], UNIT_MATRIX, [0.0; 3]);
        let frames: Vec<&[f32]> = roll.iter().map(Vec::as_slice).collect();
        let document = fit_roll_calibration(&frames, &bases, &params()).unwrap();
        assert_eq!(document.density_matrix, UNIT_MATRIX);
    }

    #[test]
    fn recovers_gain_gamma_crossover() {
        let mut rng = Rng(0x5EED);
        let base = [0.8, 0.7, 0.5];
        let (roll, bases) = synthetic_roll(&mut rng, 3, 6000, base);
        let frames: Vec<&[f32]> = roll.iter().map(Vec::as_slice).collect();
        let document = fit_roll_calibration(&frames, &bases, &params()).unwrap();

        assert!((document.curve_gamma.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!(document.curve_alpha.iter().sum::<f64>().abs() < 1e-9);
        // R rose steepest, so its correction slope is the shallowest; B the
        // reverse (same expectation as the desktop crossover test).
        assert!(document.curve_gamma[0] < document.curve_gamma[2]);
        // Median film base is recovered.
        for channel in 0..3 {
            assert!((document.film_base[channel] - base[channel]).abs() < 0.01);
        }

        // The fitted matrix + curve collapse the channel spread of the neutral
        // cloud (the matrix absorbs the per-channel gain, the curve the
        // gamma crossover).
        let eps = 1e-6;
        let densities = to_density_excitation(frames[0], document.film_base, document.density_matrix, eps);
        let corrected = apply_density_curve(
            &densities,
            document.curve_alpha,
            document.curve_gamma,
            document.curve_log_min,
            document.curve_log_max,
            document.curve_max_log_correction,
        );
        let spread = |rows: &[[f64; 3]]| -> f64 { rows.iter().map(|pixel| (pixel[0].ln() - pixel[2].ln()).abs()).sum::<f64>() / rows.len() as f64 };
        assert!(spread(&corrected) < spread(&densities) * 0.5);
    }

    #[test]
    fn falls_back_to_identity_with_few_neutrals() {
        // Independent uniform channels: almost no achromatic pixels, so the
        // neutral cloud stays below the 500-sample minimum.
        let mut rng = Rng(0xBADD);
        let frame: Vec<f32> = (0..5000 * 3).map(|_| rng.uniform(0.05, 0.6) as f32).collect();
        let frames: Vec<&[f32]> = vec![&frame];
        let document = fit_roll_calibration(&frames, &[[0.8, 0.7, 0.5]], &params()).unwrap();
        assert_eq!(document.curve_gamma, [1.0, 1.0, 1.0]);
        assert_eq!(document.curve_log_min, [-20.0; 3]);
        assert_eq!(document.curve_log_max, [20.0; 3]);
        assert!(document.density_white > document.density_black);
    }

    #[test]
    fn neutralize_disabled_is_identity() {
        let mut rng = Rng(0xCAFE);
        let frame: Vec<f32> = (0..3000 * 3).map(|_| rng.uniform(0.05, 0.6) as f32).collect();
        let frames: Vec<&[f32]> = vec![&frame];
        let mut params = params();
        params.neutralize = false;
        let document = fit_roll_calibration(&frames, &[[0.8, 0.7, 0.5]], &params).unwrap();
        assert_eq!(document.curve_alpha, [0.0; 3]);
        assert_eq!(document.curve_gamma, [1.0; 3]);
        assert_eq!(document.curve_log_min, [-20.0; 3]);
    }

    #[test]
    fn document_json_round_trips_through_pipeline_parse() {
        let mut rng = Rng(0xFACE);
        let (roll, bases) = synthetic_roll(&mut rng, 2, 4000, [0.8, 0.7, 0.5]);
        let frames: Vec<&[f32]> = roll.iter().map(Vec::as_slice).collect();
        let document = fit_roll_calibration(&frames, &bases, &params()).unwrap();
        let json = serde_json::to_string(&document).unwrap();
        let parsed_document: RollCalibrationDocument = serde_json::from_str(&json).unwrap();
        let parsed = to_render_calibration(&parsed_document);
        let render = to_render_calibration(&document);
        assert_eq!(parsed_document.e_mid, document.e_mid);
        assert!(document.e_mid.unwrap() > 0.0);
        for channel in 0..3 {
            assert!((parsed.curve_alpha[channel] - render.curve_alpha[channel]).abs() < 1e-6);
            assert!((parsed.curve_gamma[channel] - render.curve_gamma[channel]).abs() < 1e-6);
        }
        assert_eq!(parsed.density_black, render.density_black);
        assert_eq!(parsed.density_white, render.density_white);
    }

    #[test]
    fn rejects_mismatched_arguments() {
        let frame: Vec<f32> = vec![0.5; 3];
        let frames: Vec<&[f32]> = vec![&frame];
        assert!(fit_roll_calibration(&frames, &[], &params()).is_err());
        let bad: Vec<f32> = vec![0.5; 4];
        let frames: Vec<&[f32]> = vec![&bad];
        assert!(fit_roll_calibration(&frames, &[[0.5; 3]], &params()).is_err());
    }

    #[test]
    fn solve_linear_handles_kkt_shapes() {
        // SPD system with a known solution.
        let h = vec![vec![4.0, 1.0], vec![1.0, 3.0]];
        let g = vec![1.0, 2.0];
        let x = solve_linear(&h, &g).unwrap();
        assert!((x[0] - 1.0 / 11.0).abs() < 1e-12);
        assert!((x[1] - 7.0 / 11.0).abs() < 1e-12);
        // Exactly singular -> None (the Python LinAlgError case).
        let singular = vec![vec![1.0, 2.0], vec![2.0, 4.0]];
        assert!(solve_linear(&singular, &g).is_none());
    }
}
