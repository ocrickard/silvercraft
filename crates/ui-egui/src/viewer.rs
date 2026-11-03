//! The frame viewer (LightCraft's loupe): one frame of a roll at full size, developed (as
//! LightCraft receives it) or as scanned. Fit or 1:1, scroll to zoom about the pointer, drag to
//! pan, ←/→ between frames, Esc back to the grid. Crop (C) shows the whole scan with the
//! frame's crop to drag; Apply (Enter) saves it and the frame develops again.
//!
//! Frames load on a background thread and arrive cut into tiles, so a 3200 ppi frame (taller
//! than many GPUs' largest texture) still shows at full resolution.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use egui::{Align2, Color32, ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2, pos2, vec2};

use crate::display::{Linear, Look};
use crate::frame_drag::{FrameDrag, Grab, cursor_for, dragged, grab_for};
use crate::theme::tokens;
use crate::widgets;

/// Which rendering of a frame to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The developed DNG.
    Developed,
    /// The scan as captured (shown inverted when the Invert toggle is on).
    Scan,
}

/// A frame the viewer can show.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewFrame {
    pub label: String,
    pub scan: PathBuf,
    /// The picture's place in the scan (`x0, y0, x1, y1`) and the scan size it was measured
    /// on; the scan's inverted view takes its range from inside it.
    pub frame_box: Option<([usize; 4], (usize, usize))>,
    /// Where its developed DNG goes (it may not exist yet).
    pub developed: Option<PathBuf>,
    /// How the scan is turned to stand upright (the developed DNG says so itself).
    pub rotation: silvercraft_engine::rotation::Rotation,
    /// The crop set by hand: fractions of the scan (not turned).
    pub crop: Option<[f64; 4]>,
}

impl ViewFrame {
    /// The crop the frame develops with: the one set by hand, else its frame box, else the whole
    /// scan (fractions of the scan, not turned).
    pub fn effective_crop(&self) -> [f64; 4] {
        if let Some(c) = self.crop {
            return c;
        }
        match self.frame_box {
            Some((b, (w, h))) if w > 0 && h > 0 && b[2] > b[0] && b[3] > b[1] => {
                let (w, h) = (w as f64, h as f64);
                [b[0] as f64 / w, b[1] as f64 / h, (b[2] as f64 / w).min(1.0), (b[3] as f64 / h).min(1.0)]
            }
            _ => [0.0, 0.0, 1.0, 1.0],
        }
    }
}

/// What a picture is a rendering of.
#[derive(Clone, Debug, PartialEq)]
struct Key {
    frame: ViewFrame,
    source: Source,
    positive: bool,
}

/// Tile side: within every GPU's texture limit.
const TILE: usize = 2048;
/// Closest zoom out and in, in screen points per image pixel.
const ZOOM_RANGE: (f32, f32) = (0.02, 16.0);
/// Smallest crop, as a fraction of the scan's shorter side.
const MIN_CROP: f64 = 0.05;

/// A full-size rendering cut into textures.
pub struct Picture {
    pub size: [usize; 2],
    /// Each tile's place in image pixels.
    tiles: Vec<(Rect, TextureHandle)>,
}

/// What the viewer asks of the roll view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    /// Back to the grid.
    Close,
    /// Save frame `index`'s crop (`None`: back to its frame box) and develop it again.
    Crop { index: usize, crop: Option<[f64; 4]> },
}

/// A crop being set: fractions of the scan (not turned).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropEdit {
    pub crop: [f64; 4],
    drag: Option<FrameDrag>,
}

pub struct Viewer {
    pub frames: Vec<ViewFrame>,
    pub index: usize,
    pub source: Source,
    /// `None` fits the frame to the canvas; `Some(k)` is `k` screen points per image pixel.
    pub zoom: Option<f32>,
    /// The image point at the canvas centre when zoomed.
    center: Pos2,
    picture: Option<(Key, Picture)>,
    loading: Option<(Key, Receiver<Result<Picture, String>>)>,
    error: Option<(Key, String)>,
    /// The crop being set on the current frame (the scan is shown whole).
    pub cropping: Option<CropEdit>,
}

impl Viewer {
    /// Open on `frames[index]`: developed when it has been, else the scan.
    pub fn new(frames: Vec<ViewFrame>, index: usize) -> Self {
        let index = index.min(frames.len().saturating_sub(1));
        let developed = frames.get(index).and_then(|f| f.developed.as_ref()).is_some_and(|p| p.exists());
        Viewer {
            frames,
            index,
            source: if developed { Source::Developed } else { Source::Scan },
            zoom: None,
            center: Pos2::ZERO,
            picture: None,
            loading: None,
            error: None,
            cropping: None,
        }
    }

    /// Start setting the current frame's crop: the whole scan, upright and positive, fitted.
    pub fn start_crop(&mut self) {
        let Some(f) = self.frame() else { return };
        self.cropping = Some(CropEdit { crop: f.effective_crop(), drag: None });
        self.source = Source::Scan;
        self.zoom = None;
    }

    /// Finish setting the crop: the request that saves it (`None` when it is unchanged).
    pub fn apply_crop(&mut self) -> Option<Request> {
        let edit = self.cropping.take()?;
        let was = self.frame()?.effective_crop();
        self.source = Source::Developed;
        let same = edit.crop.iter().zip(was).all(|(a, b)| (a - b).abs() < 1e-6);
        (!same).then_some(Request::Crop { index: self.index, crop: Some(edit.crop) })
    }

    /// Drop the crop set by hand: the frame develops with its frame box again.
    pub fn reset_crop(&mut self) -> Option<Request> {
        self.cropping = None;
        let set = self.frame()?.crop.is_some();
        self.source = Source::Developed;
        set.then_some(Request::Crop { index: self.index, crop: None })
    }

    /// Forget the developed rendering on screen (the frame was developed again).
    pub fn reload_developed(&mut self) {
        if self.picture.as_ref().is_some_and(|(k, _)| k.source == Source::Developed) {
            self.picture = None;
        }
        if self.error.as_ref().is_some_and(|(k, _)| k.source == Source::Developed) {
            self.error = None;
        }
    }

    /// The current frame as a rendering's key knows it (the crop changes neither rendering: the
    /// scan is shown whole, and a developed frame is reloaded when it develops again).
    fn key_frame(&self) -> Option<ViewFrame> {
        self.frame().cloned().map(|f| ViewFrame { crop: None, ..f })
    }

    pub fn frame(&self) -> Option<&ViewFrame> {
        self.frames.get(self.index)
    }

    /// Show frame `index` (clamped), keeping the source and fit; a zoomed view starts centred.
    pub fn go(&mut self, index: usize) {
        if self.cropping.is_some() {
            return;
        }
        let index = index.min(self.frames.len().saturating_sub(1));
        if index != self.index {
            self.index = index;
            self.center = Pos2::ZERO;
        }
    }

    /// Whether the picture on screen is the current frame (not one still loading).
    pub fn shown(&self) -> bool {
        let f = self.key_frame();
        self.picture.as_ref().is_some_and(|(k, _)| Some(&k.frame) == f.as_ref() && k.source == self.source)
    }

    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// The current error, if the current rendering failed.
    pub fn error(&self) -> Option<&str> {
        let f = self.key_frame();
        self.error.as_ref().filter(|(k, _)| Some(&k.frame) == f.as_ref() && k.source == self.source).map(|(_, e)| e.as_str())
    }

    /// Start loading what should be on screen, and pick up a finished load.
    pub fn poll(&mut self, ctx: &egui::Context, positive: bool) {
        if let Some((key, rx)) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(p)) => {
                    let key = key.clone();
                    self.picture = Some((key, p));
                    self.loading = None;
                }
                Ok(Err(e)) => {
                    self.error = Some((key.clone(), e));
                    self.loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.error = Some((key.clone(), "the viewer's loader stopped".into()));
                    self.loading = None;
                }
            }
        }
        let Some(frame) = self.key_frame() else { return };
        let positive = positive || self.source == Source::Developed || self.cropping.is_some();
        let want = Key { frame, source: self.source, positive };
        let have = |k: &Key| *k == want;
        if self.picture.as_ref().is_some_and(|(k, _)| have(k))
            || self.loading.as_ref().is_some_and(|(k, _)| have(k))
            || self.error.as_ref().is_some_and(|(k, _)| have(k))
        {
            return;
        }
        if self.loading.is_some() {
            // one load at a time: the current one finishes (and is cached) before the next starts
            return;
        }
        let (tx, rx) = channel();
        let (c, k) = (ctx.clone(), want.clone());
        std::thread::Builder::new()
            .name("viewer".into())
            .spawn(move || {
                let r = render(&c, &k);
                let _ = tx.send(r);
                c.request_repaint();
            })
            .map_err(|e| log::warn!("viewer thread: {e}"))
            .ok();
        self.loading = Some((want, rx));
    }

    /// Keyboard: ←/→ between frames, Z between fit and 1:1, C to crop, Esc back to the grid.
    /// While cropping: Enter applies, Esc (or C) leaves the crop as it was.
    pub fn keys(&mut self, ctx: &egui::Context) -> Option<Request> {
        if ctx.egui_wants_keyboard_input() {
            return None;
        }
        let mut out = None;
        if self.cropping.is_some() {
            let (apply, cancel) = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) || i.consume_key(egui::Modifiers::NONE, egui::Key::C),
                )
            });
            if apply {
                return self.apply_crop();
            }
            if cancel {
                self.cropping = None;
                self.source = Source::Developed;
            }
            return None;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::C)) {
            self.start_crop();
            return None;
        }
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                self.go(self.index + 1);
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                self.go(self.index.saturating_sub(1));
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Z) {
                self.zoom = if self.zoom.is_some() { None } else { Some(1.0) };
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) || i.consume_key(egui::Modifiers::NONE, egui::Key::G) {
                out = Some(Request::Close);
            }
        });
        out
    }

    /// The canvas: a header (back, frame, source, zoom) over the picture. `placeholder` is the
    /// frame's grid thumbnail, shown while the full size loads.
    pub fn canvas(&mut self, ui: &mut egui::Ui, roll: &str, placeholder: Option<&TextureHandle>) -> Option<Request> {
        let t = tokens();
        let rect = ui.max_rect();
        let mut out = None;
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), 52.0));
        ui.painter().rect_filled(header, 0.0, t.canvas);
        let mut bar =
            ui.new_child(egui::UiBuilder::new().max_rect(header.shrink2(vec2(16.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        if widgets::text_button(&mut bar, "‹ Grid", true, false).on_hover_text("Back to the roll (Esc)").clicked() {
            out = Some(Request::Close);
        }
        bar.add_space(12.0);
        let label = self.frame().map(|f| f.label.clone()).unwrap_or_default();
        bar.label(egui::RichText::new(format!("{roll} · {label}")).font(t.semibold(16.0)).color(t.text));
        bar.add_space(8.0);
        bar.label(egui::RichText::new(format!("{} of {}", self.index + 1, self.frames.len())).font(t.font(13.0)).color(t.text_dim));
        if widgets::text_button(&mut bar, "‹", self.index > 0, false).on_hover_text("Previous frame (←)").clicked() {
            self.go(self.index.saturating_sub(1));
        }
        if widgets::text_button(&mut bar, "›", self.index + 1 < self.frames.len(), false).on_hover_text("Next frame (→)").clicked() {
            self.go(self.index + 1);
        }
        if self.cropping.is_some() {
            bar.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::text_button(ui, "Apply", true, true).on_hover_text("Keep this crop and develop the frame again (Enter)").clicked() {
                    out = self.apply_crop();
                }
                if widgets::text_button(ui, "Cancel", true, false).on_hover_text("Leave the crop as it was (Esc)").clicked() {
                    self.cropping = None;
                    self.source = Source::Developed;
                }
                let set = self.frame().is_some_and(|f| f.crop.is_some());
                if widgets::text_button(ui, "Reset", set, false).on_hover_text("Back to the frame as it was found when captured").clicked() {
                    out = self.reset_crop();
                }
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Drag the edges, corners or middle").font(t.font(13.0)).color(t.text_dim));
            });
        } else {
            bar.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.view_controls(ui);
            });
        }

        let body = Rect::from_min_max(pos2(rect.left(), header.bottom()), rect.max);
        let resp = ui.interact(body, ui.id().with("viewer-body"), Sense::click_and_drag());
        let painter = ui.painter_at(body);
        let area = body.shrink(16.0);
        let current = self.shown().then(|| self.picture.as_ref().map(|(_, p)| p)).flatten();
        match current {
            Some(p) if self.cropping.is_some() => {
                let size = vec2(p.size[0] as f32, p.size[1] as f32);
                let k = (area.width() / size.x).min(area.height() / size.y).min(ZOOM_RANGE.1);
                let shown = Rect::from_center_size(area.center(), size * k);
                for (r, tex) in &p.tiles {
                    let sr = Rect::from_min_max(shown.min + r.min.to_vec2() * k, shown.min + r.max.to_vec2() * k);
                    painter.image(tex.id(), sr, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                }
                let rotation = self.frame().map(|f| f.rotation).unwrap_or_default();
                if let Some(edit) = &mut self.cropping {
                    crop_canvas(ui, &resp, &painter, edit, shown, k, (f64::from(size.x), f64::from(size.y)), rotation);
                }
            }
            _ => self.picture_canvas(ui, &resp, &painter, body, area, placeholder),
        }
        out
    }

    /// The header's zoom, source and Crop controls (right to left).
    fn view_controls(&mut self, ui: &mut egui::Ui) {
        {
            let zoom_label = match self.zoom {
                Some(k) => format!("{:.0}%", k * 100.0),
                None => "Fit".into(),
            };
            ui.allocate_ui(vec2(150.0, 24.0), |ui| {
                let active = if self.zoom.is_none() { 0 } else { 1 };
                let one = if self.zoom.is_some_and(|k| (k - 1.0).abs() > 1e-3) { zoom_label.as_str() } else { "1:1" };
                if let Some(i) = widgets::segmented(ui, &["Fit", one], active) {
                    self.zoom = if i == 0 { None } else { Some(1.0) };
                }
            });
            ui.add_space(12.0);
            ui.allocate_ui(vec2(190.0, 24.0), |ui| {
                let active = if self.source == Source::Developed { 0 } else { 1 };
                if let Some(i) = widgets::segmented(ui, &["Developed", "Scan"], active) {
                    self.source = if i == 0 { Source::Developed } else { Source::Scan };
                }
            });
            ui.add_space(12.0);
            if widgets::text_button(ui, "Crop", self.frame().is_some(), false).on_hover_text("Set the frame's crop (C)").clicked() {
                self.start_crop();
            }
        }
    }

    /// The picture, fitted or zoomed (scroll zooms, drag pans), or what stands in for it.
    fn picture_canvas(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        painter: &egui::Painter,
        body: Rect,
        area: Rect,
        placeholder: Option<&TextureHandle>,
    ) {
        let t = tokens();
        let current = self.shown().then(|| self.picture.as_ref().map(|(_, p)| p)).flatten();
        match current {
            Some(p) => {
                let size = vec2(p.size[0] as f32, p.size[1] as f32);
                let fit = (area.width() / size.x).min(area.height() / size.y).min(ZOOM_RANGE.1);
                let scale = self.zoom.unwrap_or(fit);
                if self.zoom.is_none() || self.center == Pos2::ZERO {
                    self.center = (size / 2.0).to_pos2();
                }
                // zoom about the pointer, pan by dragging, double-click toggles fit / 1:1 there
                let to_screen = |c: Pos2, k: f32, q: Pos2| area.center() + (q - c) * k;
                let to_image = |c: Pos2, k: f32, s: Pos2| c + (s - area.center()) / k;
                if let Some(hover) = resp.hover_pos() {
                    let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                    let factor = pinch * (scroll / 200.0).exp();
                    if (factor - 1.0).abs() > 1e-4 {
                        let k = (scale * factor).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
                        let under = to_image(self.center, scale, hover);
                        self.zoom = Some(k);
                        // keep the image point under the pointer where it is
                        self.center = under - (hover - area.center()) / k;
                    }
                }
                if resp.double_clicked()
                    && let Some(at) = resp.interact_pointer_pos()
                {
                    if self.zoom.is_some() {
                        self.zoom = None;
                    } else {
                        self.center = to_image(self.center, scale, at);
                        self.zoom = Some(1.0);
                    }
                }
                let scale = self.zoom.unwrap_or(fit);
                if self.zoom.is_some() && resp.dragged() {
                    self.center -= resp.drag_delta() / scale;
                }
                self.center = clamp_center(self.center, size, area.size() / scale);
                for (r, tex) in &p.tiles {
                    let sr = Rect::from_min_max(to_screen(self.center, scale, r.min), to_screen(self.center, scale, r.max));
                    if sr.intersects(body) {
                        painter.image(tex.id(), sr, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                    }
                }
                if self.zoom.is_some() {
                    ui.ctx().set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
                }
            }
            None => {
                if let Some(tex) = placeholder.filter(|_| self.source == Source::Developed) {
                    let s = tex.size_vec2();
                    let k = (area.width() / s.x.max(1.0)).min(area.height() / s.y.max(1.0));
                    let r = Rect::from_center_size(area.center(), s * k);
                    painter.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::from_gray(140));
                }
                let (title, body_text) = match self.error() {
                    Some(e) if self.source == Source::Developed && !self.frame().and_then(|f| f.developed.as_ref()).is_some_and(|p| p.exists()) => {
                        ("Not developed yet", format!("Develop the roll, or switch to Scan. ({e})"))
                    }
                    Some(e) => ("Can't show this frame", e.to_string()),
                    None => ("", "Loading full size…".to_string()),
                };
                if title.is_empty() {
                    painter.text(area.center(), Align2::CENTER_CENTER, body_text, t.font(14.0), t.text);
                } else {
                    widgets::empty_message(ui, area, title, &body_text);
                }
            }
        }
    }
}

/// The crop over a scan shown whole at `shown` (`k` points per pixel, `size` upright pixels):
/// the outside dimmed, the edges, corners and middle dragged.
#[allow(clippy::too_many_arguments)]
fn crop_canvas(
    ui: &egui::Ui,
    resp: &egui::Response,
    painter: &egui::Painter,
    edit: &mut CropEdit,
    shown: Rect,
    k: f32,
    size: (f64, f64),
    rotation: silvercraft_engine::rotation::Rotation,
) {
    let t = tokens();
    let upright = crop_upright(edit.crop, size, rotation);
    let to_screen = |x: f64, y: f64| shown.min + vec2(x as f32 * k, y as f32 * k);
    let to_image = |p: Pos2| (f64::from((p.x - shown.min.x) / k), f64::from((p.y - shown.min.y) / k));
    let sr = Rect::from_min_max(to_screen(upright[0], upright[1]), to_screen(upright[2], upright[3]));
    if edit.drag.is_none()
        && let Some(p) = resp.hover_pos()
    {
        ui.ctx().set_cursor_icon(cursor_for(grab_for(sr, p)));
    }
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
        && let Some(grab) = grab_for(sr, p)
    {
        let (x, y) = to_image(p);
        let start = if grab == Grab::New { [x, y, x, y] } else { upright };
        edit.drag = Some(FrameDrag { grab, start, from: pos2(x as f32, y as f32) });
    }
    if let (Some(d), Some(p)) = (edit.drag, resp.interact_pointer_pos())
        && resp.dragged()
    {
        ui.ctx().set_cursor_icon(cursor_for(Some(d.grab)));
        let min = MIN_CROP * size.0.min(size.1);
        let b = dragged(d, to_image(p), size, min);
        if b[2] - b[0] >= min && b[3] - b[1] >= min {
            edit.crop = crop_of_upright(b, size, rotation);
        }
    }
    if resp.drag_stopped() {
        edit.drag = None;
    }
    let upright = crop_upright(edit.crop, size, rotation);
    let sr = Rect::from_min_max(to_screen(upright[0], upright[1]), to_screen(upright[2], upright[3]));
    // dim what's cut away
    let dim = Color32::from_black_alpha(150);
    for r in [
        Rect::from_min_max(shown.min, pos2(shown.max.x, sr.min.y)),
        Rect::from_min_max(pos2(shown.min.x, sr.max.y), shown.max),
        Rect::from_min_max(pos2(shown.min.x, sr.min.y), pos2(sr.min.x, sr.max.y)),
        Rect::from_min_max(pos2(sr.max.x, sr.min.y), pos2(shown.max.x, sr.max.y)),
    ] {
        painter.rect_filled(r, 0.0, dim);
    }
    painter.rect_stroke(sr, 0.0, egui::Stroke::new(2.0, t.frame_selected), egui::StrokeKind::Middle);
    for c in [sr.left_top(), sr.right_top(), sr.left_bottom(), sr.right_bottom()] {
        painter.rect_filled(Rect::from_center_size(c, vec2(8.0, 8.0)), 1.0, t.frame_selected);
    }
}

/// A crop (fractions of the scan, not turned) in pixels of the upright `size` scan.
fn crop_upright(c: [f64; 4], size: (f64, f64), rotation: silvercraft_engine::rotation::Rotation) -> [f64; 4] {
    let (w, h) = rotation.size(size.0 as usize, size.1 as usize);
    let (w, h) = (w as f64, h as f64);
    rotation.bbox([c[0] * w, c[1] * h, c[2] * w, c[3] * h], w, h)
}

/// The crop (fractions of the scan, not turned) for box `b` in pixels of the upright `size` scan.
fn crop_of_upright(b: [f64; 4], size: (f64, f64), rotation: silvercraft_engine::rotation::Rotation) -> [f64; 4] {
    let s = rotation.inverse().bbox(b, size.0, size.1);
    let (w, h) = rotation.size(size.0 as usize, size.1 as usize);
    let (w, h) = (w as f64, h as f64);
    [(s[0] / w).clamp(0.0, 1.0), (s[1] / h).clamp(0.0, 1.0), (s[2] / w).clamp(0.0, 1.0), (s[3] / h).clamp(0.0, 1.0)]
}

/// Keep the view on the image: a centre at most half a view outside it on each axis.
fn clamp_center(c: Pos2, image: Vec2, view: Vec2) -> Pos2 {
    let axis = |c: f32, img: f32, v: f32| if v >= img { img / 2.0 } else { c.clamp(v / 2.0, img - v / 2.0) };
    pos2(axis(c.x, image.x, view.x), axis(c.y, image.y, view.y))
}

/// `b` measured on an image of size `from`, in an image of size `to`.
fn scale_box(b: [usize; 4], from: (usize, usize), to: (usize, usize)) -> [usize; 4] {
    let sx = |v: usize| (v * to.0).checked_div(from.0).unwrap_or(v).min(to.0);
    let sy = |v: usize| (v * to.1).checked_div(from.1).unwrap_or(v).min(to.1);
    [sx(b[0]), sy(b[1]), sx(b[2]), sy(b[3])]
}

/// Render `key` at full size and upload it as tiles (any thread: egui's context is shared).
fn render(ctx: &egui::Context, key: &Key) -> Result<Picture, String> {
    let image = match key.source {
        Source::Developed => {
            let path = key.frame.developed.as_ref().ok_or("no developed file for this frame")?;
            if !path.exists() {
                return Err(format!("{} does not exist", path.display()));
            }
            let t = silvercraft_engine::view::render_developed(path).map_err(|e| e.to_string())?;
            ColorImage::from_rgb([t.width, t.height], &t.rgb)
        }
        Source::Scan => {
            let s = silvercraft_engine::view::load_scan(&key.frame.scan).map_err(|e| e.to_string())?;
            let frame = key.frame.frame_box.map(|(b, space)| scale_box(b, space, (s.width, s.height)));
            let lin = Linear { width: s.width, height: s.height, rgb: s.rgb };
            let img = lin.render_within(if key.positive { Look::Positive } else { Look::AsIs }, frame);
            let (w, h, px) = key.frame.rotation.pixels(img.size[0], img.size[1], 1, &img.pixels);
            ColorImage::new([w, h], px)
        }
    };
    let name = format!("view-{}-{:?}", key.frame.scan.display(), key.source);
    Ok(tiles(ctx, &name, &image))
}

/// Cut `image` into [`TILE`]-sized textures.
pub fn tiles(ctx: &egui::Context, name: &str, image: &ColorImage) -> Picture {
    let [w, h] = image.size;
    let options = TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    };
    let mut tiles = Vec::new();
    for ty in (0..h).step_by(TILE) {
        for tx in (0..w).step_by(TILE) {
            let (tw, th) = (TILE.min(w - tx), TILE.min(h - ty));
            let mut px = Vec::with_capacity(tw * th);
            for y in ty..ty + th {
                px.extend_from_slice(&image.pixels[y * w + tx..y * w + tx + tw]);
            }
            let tex = ctx.load_texture(format!("{name}-{tx}-{ty}"), ColorImage::new([tw, th], px), options);
            tiles.push((Rect::from_min_size(pos2(tx as f32, ty as f32), vec2(tw as f32, th as f32)), tex));
        }
    }
    Picture { size: [w, h], tiles }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_the_image_exactly() {
        let ctx = egui::Context::default();
        let (w, h) = (TILE * 2 + 5, TILE + 1);
        let img = ColorImage::new([w, h], vec![Color32::from_gray(7); w * h]);
        let p = tiles(&ctx, "t", &img);
        assert_eq!(p.size, [w, h]);
        assert_eq!(p.tiles.len(), 3 * 2);
        let area: f32 = p.tiles.iter().map(|(r, _)| r.area()).sum();
        assert_eq!(area, (w * h) as f32, "no gaps, no overlap");
        assert!(p.tiles.iter().all(|(r, tex)| tex.size() == [r.width() as usize, r.height() as usize] && r.width() as usize <= TILE));
    }

    #[test]
    fn frame_boxes_follow_the_scan_size() {
        assert_eq!(scale_box([10, 20, 30, 40], (100, 100), (100, 100)), [10, 20, 30, 40]);
        assert_eq!(scale_box([10, 20, 30, 40], (100, 100), (200, 50)), [20, 10, 60, 20]);
        assert_eq!(scale_box([10, 20, 300, 400], (100, 100), (100, 100)), [10, 20, 100, 100], "never past the image");
        assert_eq!(scale_box([1, 2, 3, 4], (0, 0), (10, 10)), [1, 2, 3, 4], "no size: as given");
    }

    #[test]
    fn crops_turn_with_the_scan_and_back() {
        use silvercraft_engine::rotation::Rotation;
        // a 400 × 300 scan turned a quarter clockwise shows as 300 × 400
        let r = Rotation::from_degrees(90);
        let c = [0.1, 0.2, 0.5, 0.6];
        let up = crop_upright(c, (300.0, 400.0), r);
        // across becomes down: x 40..200 of the scan is y 40..200 on screen
        assert_eq!(up, [300.0 - 180.0, 40.0, 300.0 - 60.0, 200.0]);
        let back = crop_of_upright(up, (300.0, 400.0), r);
        assert!(back.iter().zip(c).all(|(a, b)| (a - b).abs() < 1e-9), "{back:?}");
        assert_eq!(crop_upright(c, (400.0, 300.0), Rotation::NONE), [40.0, 60.0, 200.0, 180.0]);
    }

    #[test]
    fn a_crop_starts_from_the_one_set_else_the_frame_box() {
        let mut f = ViewFrame {
            label: "1".into(),
            scan: "1.arw".into(),
            frame_box: Some(([100, 50, 300, 150], (400, 200))),
            developed: None,
            rotation: Default::default(),
            crop: None,
        };
        assert_eq!(f.effective_crop(), [0.25, 0.25, 0.75, 0.75]);
        f.crop = Some([0.0, 0.0, 0.5, 0.5]);
        assert_eq!(f.effective_crop(), [0.0, 0.0, 0.5, 0.5]);
        f.crop = None;
        f.frame_box = None;
        assert_eq!(f.effective_crop(), [0.0, 0.0, 1.0, 1.0]);

        let mut v = Viewer::new(vec![f.clone(), ViewFrame { label: "2".into(), ..f }], 0);
        v.start_crop();
        assert_eq!(v.source, Source::Scan);
        v.go(1);
        assert_eq!(v.index, 0, "no stepping away mid-crop");
        assert_eq!(v.apply_crop(), None, "unchanged: nothing to save");
        v.start_crop();
        v.cropping.as_mut().unwrap().crop = [0.1, 0.1, 0.9, 0.9];
        assert_eq!(v.apply_crop(), Some(Request::Crop { index: 0, crop: Some([0.1, 0.1, 0.9, 0.9]) }));
        assert_eq!((v.source, v.cropping), (Source::Developed, None));
        assert_eq!(v.reset_crop(), None, "nothing set by hand to drop");
    }

    #[test]
    fn the_view_stays_on_the_image() {
        let img = vec2(1000.0, 3000.0);
        // a view larger than the image centres it
        assert_eq!(clamp_center(pos2(0.0, 0.0), img, vec2(2000.0, 4000.0)), pos2(500.0, 1500.0));
        // a smaller one can reach each edge but not past it
        assert_eq!(clamp_center(pos2(-50.0, 9999.0), img, vec2(200.0, 600.0)), pos2(100.0, 2700.0));
        assert_eq!(clamp_center(pos2(400.0, 1000.0), img, vec2(200.0, 600.0)), pos2(400.0, 1000.0));
    }

    #[test]
    fn opens_developed_when_there_is_a_dng_and_steps_within_the_roll() {
        let dir = std::env::temp_dir().join(format!("sc-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dng = dir.join("01.dng");
        std::fs::write(&dng, b"x").unwrap();
        let f = |l: &str, d: Option<PathBuf>| ViewFrame {
            label: l.into(),
            scan: dir.join(format!("{l}.tif")),
            frame_box: None,
            developed: d,
            rotation: Default::default(),
            crop: None,
        };
        let v = Viewer::new(vec![f("01", Some(dng)), f("02", Some(dir.join("missing.dng")))], 0);
        assert_eq!(v.source, Source::Developed);
        let mut v2 = Viewer::new(v.frames.clone(), 7);
        assert_eq!((v2.index, v2.source), (1, Source::Scan), "clamped; nothing developed there yet");
        v2.go(0);
        v2.go(usize::MAX);
        assert_eq!(v2.index, 1);
    }
}
