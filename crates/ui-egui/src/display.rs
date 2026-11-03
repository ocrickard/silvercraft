//! Quick display renderings for capture previews: a negative shown as a positive (density,
//! stretched per channel) or a capture shown as it is. Only for the screen; developing uses the
//! film pipeline.

use egui::ColorImage;

/// Pixels darker than this share of the brightest are the opaque holder, not film.
const OPAQUE: f32 = 0.02;
/// Pixels within this density of the clearest are bare light (no film), past the ends of a strip.
const BARE_LIGHT_DENSITY: f32 = 0.1;
/// Bare light reads near full scale: its density is at most this. Film base never is, so a
/// single frame (no bare light) keeps its base and shadows.
const BARE_LIGHT_MAX_DENSITY: f32 = 0.15;
/// Pixels this close to the holder fade into it and don't count as film: at least this many,
/// more in large images (the fade is a fixed distance on the film, more pixels at 3200 ppi).
const HOLDER_MARGIN: usize = 4;
/// ... one pixel of margin per this many pixels of the image's longer side.
const HOLDER_MARGIN_PER: usize = 400;
/// Pixels darker than this share of the film's median brightness, touching the holder, are its
/// shadow fading in.
const HOLDER_SHADOW: f32 = 0.33;
/// Film denser than its median by more than this is dust, hair or a scratch (or the holder's
/// edge), not picture: no negative's highlights sit that far above its typical density.
const SPECK_DENSITY: f32 = 1.0;
/// The holder's grey in a positive view.
const HOLDER: u8 = 24;

/// How to show a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Invert a negative: per-channel density stretched between robust percentiles.
    Positive,
    /// As captured (linear values auto-exposed and encoded for display).
    AsIs,
}

/// Linear-light samples (0..1) of an image, any channel count (1 or 3), downsampled so the
/// longer side is at most `max_side`.
pub struct Linear {
    pub width: usize,
    pub height: usize,
    /// RGB, `width * height * 3`.
    pub rgb: Vec<f32>,
}

impl Linear {
    /// From interleaved 16-bit RGB.
    pub fn from_rgb16(width: usize, height: usize, data: &[u16], max_side: usize) -> Option<Self> {
        Self::sampled(width, height, max_side, |i| Some(f32::from(*data.get(i)?) / 65535.0))
    }

    /// From 8-bit sRGB-encoded RGB (a live-view JPEG).
    pub fn from_srgb8(width: usize, height: usize, data: &[u8], max_side: usize) -> Option<Self> {
        Self::sampled(width, height, max_side, |i| Some(srgb_to_linear(f32::from(*data.get(i)?) / 255.0)))
    }

    fn sampled(width: usize, height: usize, max_side: usize, at: impl Fn(usize) -> Option<f32>) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let step = width.max(height).div_ceil(max_side.max(1)).max(1);
        let (w, h) = (width.div_ceil(step), height.div_ceil(step));
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                // box average over the step×step block
                let (x0, y0) = (x * step, y * step);
                let (x1, y1) = ((x0 + step).min(width), (y0 + step).min(height));
                for c in 0..3 {
                    let mut s = 0.0;
                    for yy in y0..y1 {
                        for xx in x0..x1 {
                            s += at((yy * width + xx) * 3 + c)?;
                        }
                    }
                    rgb.push(s / ((x1 - x0) * (y1 - y0)) as f32);
                }
            }
        }
        Some(Self { width: w, height: h, rgb })
    }

    pub fn render(&self, look: Look) -> ColorImage {
        self.render_within(look, None)
    }

    /// As [`Linear::render`], measuring the positive's range only inside `frame` (`x0, y0, x1,
    /// y1` in this image's pixels): a frame scan's padding (holder edges, gaps, bare light at
    /// the film's edges) can outweigh a thin negative's whole range.
    pub fn render_within(&self, look: Look, frame: Option<[usize; 4]>) -> ColorImage {
        match look {
            Look::Positive => self.positive(frame),
            Look::AsIs => self.as_is(),
        }
    }

    fn channel_percentiles(&self, values: impl Fn(f32) -> f32, lo: f64, hi: f64) -> [(f32, f32); 3] {
        self.channel_percentiles_where(values, lo, hi, |_| true)
    }

    /// Per-channel percentiles of `values` over the pixels `keep` accepts (by pixel index).
    fn channel_percentiles_where(&self, values: impl Fn(f32) -> f32, lo: f64, hi: f64, keep: impl Fn(usize) -> bool) -> [(f32, f32); 3] {
        let mut out = [(0.0, 1.0); 3];
        for (c, o) in out.iter_mut().enumerate() {
            let mut v: Vec<f32> =
                self.rgb.iter().skip(c).step_by(3).enumerate().filter(|(i, _)| keep(*i)).map(|(_, &x)| values(x)).filter(|x| x.is_finite()).collect();
            if v.is_empty() {
                continue;
            }
            v.sort_unstable_by(f32::total_cmp);
            let at = |q: f64| v[((q / 100.0) * (v.len() - 1) as f64).round() as usize];
            *o = (at(lo), at(hi));
        }
        out
    }

    /// Mean of a pixel's channels.
    fn mean(&self, i: usize) -> f32 {
        self.rgb.get(i * 3..i * 3 + 3).map_or(0.0, |p| (p[0] + p[1] + p[2]) / 3.0)
    }

    fn positive(&self, frame: Option<[usize; 4]>) -> ColorImage {
        let density = |t: f32| -(t.max(1e-5)).log10();
        // Only film sets the stretch: a flatbed bed is mostly opaque holder, its edges fade
        // through every density, and there is bare light past the strip ends; any of them would
        // squash the photos into black.
        let (w, n) = (self.width, self.width * self.height);
        let means: Vec<f32> = (0..n).map(|i| self.mean(i)).collect();
        let mut sorted = means.clone();
        sorted.sort_unstable_by(f32::total_cmp);
        let peak = sorted.get((n.saturating_sub(1) as f64 * 0.999) as usize).copied().unwrap_or(1.0).max(1e-4);
        let opaque: Vec<bool> = means.iter().map(|&m| m <= OPAQUE * peak).collect();
        // the holder's shadow fades in over a band wider than any fixed margin at 3200 ppi: dense
        // pixels connected to the holder are holder too (for measuring; they still draw)
        let mut t_film: Vec<f32> = means.iter().zip(&opaque).filter(|(_, o)| !**o).map(|(&m, _)| m).collect();
        let shadow = if t_film.is_empty() {
            0.0
        } else {
            let k = t_film.len() / 2;
            *t_film.select_nth_unstable_by(k, f32::total_cmp).1 * HOLDER_SHADOW
        };
        let holder = grow(&opaque, w, self.height, |i| means[i] < shadow);
        let margin = HOLDER_MARGIN.max(w.max(self.height) / HOLDER_MARGIN_PER);
        let near_holder = dilate(&holder, w, self.height, margin);
        let mut clear: Vec<f32> = means.iter().zip(&opaque).filter(|(_, o)| !**o).map(|(&m, _)| density(m)).collect();
        clear.sort_unstable_by(f32::total_cmp);
        let d_min = clear.get(clear.len() / 200).copied().unwrap_or(0.0);
        // bare light: near full scale, and a minority of what isn't holder (else it is film base)
        let bare = |i: usize| density(means[i]) <= d_min + BARE_LIGHT_DENSITY;
        let candidates = (0..n).filter(|&i| !opaque[i] && bare(i)).count();
        let has_bare_light = d_min <= BARE_LIGHT_MAX_DENSITY && candidates * 10 < clear.len() * 4;
        let inside = |i: usize| frame.is_none_or(|[x0, y0, x1, y1]| (x0..x1).contains(&(i % w)) && (y0..y1).contains(&(i / w)));
        let film_or_specks = |i: usize| inside(i) && !near_holder[i] && !(has_bare_light && bare(i));
        let mut d: Vec<f32> = (0..n).filter(|&i| film_or_specks(i)).map(|i| density(means[i])).collect();
        let median = if d.is_empty() {
            f32::INFINITY
        } else {
            let k = d.len() / 2;
            *d.select_nth_unstable_by(k, f32::total_cmp).1
        };
        let film = |i: usize| film_or_specks(i) && density(means[i]) <= median + SPECK_DENSITY;
        let count = (0..n).filter(|&i| film(i)).count();
        let mut range = self.channel_percentiles_where(density, 1.0, 99.0, film);
        if count * 100 < n || range.iter().any(|(lo, hi)| hi - lo < 0.05) {
            // no clear film/bare-light split (a slide, a single frame): everything but the holder
            range = self.channel_percentiles_where(density, 1.0, 99.0, |i| inside(i) && !opaque[i]);
        }
        let mut px = Vec::with_capacity(self.rgb.len());
        for (i, &o) in opaque.iter().enumerate() {
            if o {
                px.extend_from_slice(&[HOLDER; 3]);
                continue;
            }
            for (c, &(lo, hi)) in range.iter().enumerate() {
                let t = self.rgb.get(i * 3 + c).copied().unwrap_or(0.0);
                let v = ((density(t) - lo) / (hi - lo).max(1e-3)).clamp(0.0, 1.0);
                px.push((v.powf(1.25) * 255.0).round() as u8); // a gentle print-like contrast curve
            }
        }
        ColorImage::from_rgb([self.width, self.height], &px)
    }

    fn as_is(&self) -> ColorImage {
        let peak = self.channel_percentiles(|x| x, 0.0, 99.8).iter().map(|r| r.1).fold(1e-4f32, f32::max);
        let px: Vec<u8> = self.rgb.iter().map(|&v| (linear_to_srgb((v / peak).clamp(0.0, 1.0)) * 255.0).round() as u8).collect();
        ColorImage::from_rgb([self.width, self.height], &px)
    }
}

/// `seed` grown into the 4-connected pixels `joins` accepts.
fn grow(seed: &[bool], w: usize, h: usize, joins: impl Fn(usize) -> bool) -> Vec<bool> {
    let mut out = seed.to_vec();
    let mut stack: Vec<usize> = (0..seed.len()).filter(|&i| seed[i]).collect();
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let mut visit = |j: usize| {
            if !out[j] && joins(j) {
                out[j] = true;
                stack.push(j);
            }
        };
        if x > 0 {
            visit(i - 1);
        }
        if x + 1 < w {
            visit(i + 1);
        }
        if y > 0 {
            visit(i - w);
        }
        if y + 1 < h {
            visit(i + w);
        }
    }
    out
}

/// `mask` grown by `r` pixels (a square neighbourhood), separably, in linear time.
fn dilate(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    // a pass along one axis: set where the window [i - r, i + r] holds any set pixel
    let pass = |src: &[bool], len: usize, lines: usize, at: &dyn Fn(usize, usize) -> usize| -> Vec<bool> {
        let mut out = vec![false; src.len()];
        let mut prefix = vec![0usize; len + 1];
        for l in 0..lines {
            for k in 0..len {
                prefix[k + 1] = prefix[k] + usize::from(src[at(l, k)]);
            }
            for k in 0..len {
                let (a, b) = (k.saturating_sub(r), (k + r + 1).min(len));
                out[at(l, k)] = prefix[b] > prefix[a];
            }
        }
        out
    };
    let rows = pass(mask, w, h, &|y, x| y * w + x);
    pass(&rows, h, w, &|x, y| y * w + x)
}

pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_inverts_and_downsamples() {
        // left half clear (base), right half dense
        let (w, h) = (8, 4);
        let data: Vec<u16> = (0..w * h).flat_map(|i| if i % w < w / 2 { [60000u16; 3] } else { [3000u16; 3] }).collect();
        let lin = Linear::from_rgb16(w, h, &data, 4).unwrap();
        assert_eq!((lin.width, lin.height), (4, 2));
        let img = lin.render(Look::Positive);
        let left = img.pixels[0].r();
        let right = img.pixels[3].r();
        assert!(left < 30 && right > 220, "{left} {right}");
    }

    /// A flatbed bed: mostly opaque holder, bare light past the strip end, and film. The film's own
    /// range sets the stretch, so the photo is not squashed into black.
    #[test]
    fn positive_ignores_holder_and_bare_light() {
        let (w, h) = (60, 10);
        let data: Vec<u16> = (0..w * h)
            .flat_map(|i| match i % w {
                0..=29 => [0u16; 3],      // holder
                30 => [30u16; 3],         // the holder's edge fading in
                31..=37 => [62000u16; 3], // bare light
                38..=48 => [28000u16; 3], // film base
                _ => [7000u16; 3],        // image density
            })
            .collect();
        let img = Linear::from_rgb16(w, h, &data, 64).unwrap().render(Look::Positive);
        let at = |x: usize| img.pixels[x].r();
        assert_eq!(at(0), HOLDER, "holder drawn dark");
        assert!(at(40) < 30, "film base is the black point: {}", at(40));
        assert!(at(55) > 220, "image density is bright: {}", at(55));
    }

    #[test]
    fn a_single_frame_keeps_its_base_and_shadows() {
        // a 3200 ppi frame crop: a wide holder border fading in over 12 px, then XP2 film whose
        // base is the clearest thing in it (no bare light) and an image only ~0.4 density deep
        let (w, h) = (600, 300);
        let border = 30.0f32;
        let data: Vec<u16> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let edge = x.min(y).min(w as f32 - 1.0 - x).min(h as f32 - 1.0 - y);
                let film = if x < 100.0 { 0.47 } else { 0.47 - (x - 100.0) / 500.0 * 0.30 };
                let t = if edge < border { 0.0005 } else { 0.0005 + (film - 0.0005) * ((edge - border) / 12.0).min(1.0) };
                [(t * 65535.0) as u16; 3]
            })
            .collect();
        let img = Linear::from_rgb16(w, h, &data, 1000).unwrap().render(Look::Positive);
        let at = |x: usize| img.pixels[150 * w + x].r();
        assert!(at(60) < 40, "the base is black: {}", at(60));
        assert!(at(330) > 30 && at(330) < 200, "mid-image is between: {}", at(330));
        assert!(at(560) > 200, "the densest image is bright: {}", at(560));
    }

    #[test]
    fn a_frame_box_keeps_padding_out_of_the_range() {
        // a thin negative (0.34..0.46) whose padding is 10% fading holder edge: measured over
        // everything, the edge sets the range and the picture is black; inside its box it is not
        let (w, h) = (200, 100);
        let data: Vec<u16> = (0..w * h)
            .flat_map(|i| {
                let x = i % w;
                let t = if x < 20 { 0.03 + x as f32 * 0.015 } else { 0.46 - (x - 20) as f32 / 180.0 * 0.12 };
                [(t * 65535.0) as u16; 3]
            })
            .collect();
        let lin = Linear::from_rgb16(w, h, &data, 1000).unwrap();
        let whole = lin.render(Look::Positive);
        let boxed = lin.render_within(Look::Positive, Some([20, 0, 200, 100]));
        let at = |img: &ColorImage, x: usize| img.pixels[50 * w + x].r();
        assert!(at(&whole, 190) < 60, "the padding squashes it: {}", at(&whole, 190));
        assert!(at(&boxed, 190) > 200 && at(&boxed, 25) < 30, "{} {}", at(&boxed, 190), at(&boxed, 25));
    }

    #[test]
    fn dust_does_not_set_the_range() {
        // a thin frame with 3% nearly opaque specks (dust, a hair) scattered through it
        let (w, h) = (200, 100);
        let data: Vec<u16> = (0..w * h)
            .flat_map(|i| {
                let x = i % w;
                let t = if i % 33 == 0 { 0.02 } else { 0.46 - x as f32 / 200.0 * 0.12 };
                [(t * 65535.0) as u16; 3]
            })
            .collect();
        let img = Linear::from_rgb16(w, h, &data, 1000).unwrap().render(Look::Positive);
        let at = |x: usize| img.pixels[50 * w + x].r();
        assert!(at(190) > 200 && at(5) < 30, "the picture spans the range: {} {}", at(190), at(5));
    }

    #[test]
    fn the_holders_shadow_does_not_set_the_range() {
        // a thin frame inside a holder whose shadow fades in over 25 px of dense film
        let (w, h) = (300, 60);
        let data: Vec<u16> = (0..w * h)
            .flat_map(|i| {
                let x = i % w;
                let t = match x {
                    0..10 => 0.001,                             // holder
                    10..35 => 0.02 + (x - 10) as f32 * 0.004,   // its shadow, 0.02..0.12
                    _ => 0.46 - (x - 35) as f32 / 265.0 * 0.12, // the picture, 0.46..0.34
                };
                [(t * 65535.0) as u16; 3]
            })
            .collect();
        let img = Linear::from_rgb16(w, h, &data, 1000).unwrap().render(Look::Positive);
        let at = |x: usize| img.pixels[30 * w + x].r();
        assert!(at(290) > 200 && at(40) < 30, "the picture spans the range: {} {}", at(290), at(40));
        assert_eq!(at(5), HOLDER);
        assert!(at(20) > 200, "the shadow still draws (as dense film): {}", at(20));
    }

    #[test]
    fn grow_follows_connected_pixels_only() {
        let (w, h) = (6, 3);
        let seed: Vec<bool> = (0..w * h).map(|i| i == 0).collect();
        // row 0 joins up to x = 3; an island at (5, 2) does not touch the seed
        let joins = |i: usize| (i % w <= 3 && i / w == 0) || i == 2 * w + 5;
        let g = grow(&seed, w, h, joins);
        assert_eq!((0..w * h).filter(|&i| g[i]).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn dilate_grows_by_r() {
        let (w, h) = (9, 7);
        let mut m = vec![false; w * h];
        m[3 * w + 4] = true;
        let d = dilate(&m, w, h, 2);
        for y in 0..h {
            for x in 0..w {
                assert_eq!(d[y * w + x], x.abs_diff(4) <= 2 && y.abs_diff(3) <= 2, "{x},{y}");
            }
        }
        assert_eq!(dilate(&m, w, h, 0), m);
    }

    #[test]
    fn hostile_sizes() {
        assert!(Linear::from_rgb16(0, 4, &[], 10).is_none());
        assert!(Linear::from_rgb16(4, 4, &[1, 2], 10).is_none(), "short buffer");
        let one = Linear::from_srgb8(1, 1, &[10, 20, 30], 0).unwrap();
        let _ = one.render(Look::Positive);
        let _ = one.render(Look::AsIs);
    }
}
