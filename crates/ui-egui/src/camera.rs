//! The Camera tab: tethered camera scanning. Live view (shown as a positive, turned upright), the
//! film frame (detected, or set by dragging its edges), the camera's exposure, manual capture
//! (Space) and autocapture once each new frame has been advanced and has settled.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};
use silvercraft_engine::manifest::ManifestFrame;
use silvercraft_engine::rotation::Rotation;
use silvercraft_engine::tether::{AutoStatus, ExposureSetting, LiveImage, TetherCommand, TetherEvent, TetherWorker};
use silvercraft_ptp::autocapture::{AutoCaptureParams, AutoPhase, Rect as LiveRect};

use crate::display::{Linear, Look};
use crate::frame_drag::{FrameDrag, GRIP, Grab, cursor_for, dragged, grab_for};
use crate::icons::Icon;
use crate::theme::tokens;
use crate::widgets::{self, section};
use crate::{Services, Shared, Status};

/// Seconds after the last capture before the roll develops in the background (captures in quick
/// succession develop together).
pub const DEVELOP_DELAY: f64 = 15.0;
/// Smallest frame a drag leaves, in live-view pixels.
const MIN_FRAME: f64 = 16.0;

#[derive(Default)]
pub struct CameraTab {
    worker: Option<TetherWorker>,
    pub model: Option<String>,
    pub live: bool,
    pub auto: bool,
    live_tex: Option<TextureHandle>,
    live_size: (usize, usize),
    rect: Option<LiveRect>,
    pub auto_status: Option<AutoStatus>,
    pub roll: String,
    roll_sent: Option<(PathBuf, String)>,
    pub captured: Vec<ManifestFrame>,
    pub last_manifest: Option<PathBuf>,
    pub error: Option<String>,
    pub notes: Vec<String>,
    pub finished_roll: Option<PathBuf>,
    frames_seen: u64,
    /// The rotation the worker was last told.
    rotation_sent: Option<Rotation>,
    /// The frame set by hand, in live-view pixels as the camera sends them (not turned).
    pub manual: Option<LiveRect>,
    drag: Option<FrameDrag>,
    /// The camera's exposure settings (empty: it doesn't report them).
    pub exposure: Vec<ExposureSetting>,
    /// The roll to develop in the background, and when it was last captured to (egui time).
    develop_due: Option<(PathBuf, f64)>,
}

impl CameraTab {
    pub fn with_roll(roll: String) -> Self {
        Self { roll, ..Default::default() }
    }

    pub fn connect(&mut self, services: &Services, ctx: &egui::Context) {
        self.error = None;
        let c = ctx.clone();
        let measure = (services.frame_measure)();
        self.worker = Some((services.camera)(measure, AutoCaptureParams::app(), Arc::new(move || c.request_repaint())));
        self.roll_sent = None;
        self.rotation_sent = None;
        self.exposure.clear();
        if let Some(r) = self.manual {
            self.send(TetherCommand::SetFrame(Some(r)));
        }
    }

    pub fn disconnect(&mut self) {
        self.worker = None; // stops live view and closes the session
        self.model = None;
        self.live = false;
        self.auto = false;
        self.live_tex = None;
    }

    fn send(&self, c: TetherCommand) {
        if let Some(w) = &self.worker {
            w.send(c);
        }
    }

    pub fn set_live(&mut self, on: bool) {
        self.live = on;
        self.send(TetherCommand::LiveView(on));
    }

    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
        self.send(TetherCommand::AutoCapture(on));
    }

    pub fn capture(&mut self) {
        self.send(TetherCommand::Capture);
    }

    /// The live view's size on screen (turned upright), once a frame has arrived.
    pub fn live_view_size(&self) -> Option<[usize; 2]> {
        self.live_tex.as_ref().map(|t| t.size())
    }

    /// Whether a capture can be taken now.
    pub fn can_capture(&self) -> bool {
        self.model.is_some() && !self.roll.trim().is_empty()
    }

    /// Set the frame by hand (`None`: detect it again).
    pub fn set_frame(&mut self, r: Option<LiveRect>) {
        self.manual = r;
        self.send(TetherCommand::SetFrame(r));
        if r.is_none() {
            self.rect = None;
        }
    }

    /// The roll to develop now: captured to, and quiet for [`DEVELOP_DELAY`].
    pub fn develop_due(&self, now: f64) -> Option<&std::path::Path> {
        let (dir, at) = self.develop_due.as_ref()?;
        (now - at >= DEVELOP_DELAY || now < *at).then_some(dir.as_path())
    }

    /// The due develop has started.
    pub fn develop_started(&mut self) {
        self.develop_due = None;
    }

    /// Camera keys (when nothing is being typed): Space captures, R turns the view clockwise,
    /// Shift+R counter-clockwise.
    pub fn keys(&mut self, ctx: &egui::Context, shared: &mut Shared) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, cw, ccw) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::Space),
                i.consume_key(egui::Modifiers::NONE, egui::Key::R),
                i.consume_key(egui::Modifiers::SHIFT, egui::Key::R),
            )
        });
        if space && self.can_capture() {
            self.capture();
        }
        if cw || ccw {
            shared.settings.camera_rotation = shared.settings.camera_rotation.turned(cw);
        }
    }

    /// Keep the worker's roll folder in step with the roll name and stock.
    fn sync_roll(&mut self, shared: &Shared) {
        if self.worker.is_none() || self.roll.trim().is_empty() {
            return;
        }
        let dir = shared.settings.rolls_dir.join(crate::safe_name(&self.roll));
        let key = (dir.clone(), shared.stock.clone());
        if self.roll_sent.as_ref() != Some(&key) {
            self.send(TetherCommand::SetRoll { dir, roll: self.roll.clone(), stock: shared.stock.clone() });
            self.roll_sent = Some(key);
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context, shared: &Shared) {
        self.sync_roll(shared);
        let rotation = shared.settings.camera_rotation;
        if self.worker.is_some() && self.rotation_sent != Some(rotation) {
            self.send(TetherCommand::SetRotation(rotation));
            self.rotation_sent = Some(rotation);
            self.live_tex = None; // the next live frame comes turned
        }
        let now = ctx.input(|i| i.time);
        if self.develop_due.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        let Some(w) = &self.worker else { return };
        let mut latest: Option<Arc<LiveImage>> = None;
        for e in w.poll() {
            match e {
                TetherEvent::Connected { model } => {
                    self.notes.push(format!("{model} connected"));
                    self.model = Some(model);
                }
                TetherEvent::Live { image, rect, auto } => {
                    latest = Some(image);
                    self.rect = rect;
                    self.auto_status = auto;
                    self.frames_seen += 1;
                }
                TetherEvent::Captured { frame, manifest, .. } => {
                    self.notes.push(format!("captured {}", frame.label));
                    self.captured.push(frame);
                    self.develop_due = manifest.parent().map(|d| (d.to_path_buf(), now));
                    self.last_manifest = Some(manifest);
                }
                TetherEvent::Exposure(s) => self.exposure = s,
                TetherEvent::Warning(m) => self.notes.push(m),
                TetherEvent::Failed(m) => self.error = Some(m),
                TetherEvent::Disconnected => {
                    self.model = None;
                    self.live = false;
                }
            }
        }
        // only the newest live frame is uploaded
        if let Some(img) = latest {
            let look = if shared.settings.show_positive { Look::Positive } else { Look::AsIs };
            if let Some(lin) = Linear::from_srgb8(img.width, img.height, &img.rgb, 1600) {
                // the positive's range from inside the frame: the holder around it is not film
                let (sx, sy) = (lin.width as f64 / img.width as f64, lin.height as f64 / img.height as f64);
                let frame = self.rect.filter(|r| r.width > 0 && r.height > 0).map(|r| {
                    let s = |v: i32, k: f64, max: usize| ((f64::from(v.max(0)) * k).round() as usize).min(max);
                    [s(r.x, sx, lin.width), s(r.y, sy, lin.height), s(r.x + r.width, sx, lin.width), s(r.y + r.height, sy, lin.height)]
                });
                let ci = lin.render_within(look, frame);
                let (w, h, px) = rotation.pixels(ci.size[0], ci.size[1], 1, &ci.pixels);
                let ci = egui::ColorImage::new([w, h], px);
                self.live_size = (img.width, img.height);
                match &mut self.live_tex {
                    Some(t) => t.set(ci, TextureOptions::LINEAR),
                    None => self.live_tex = Some(ctx.load_texture("live-view", ci, TextureOptions::LINEAR)),
                }
            }
        }
        if self.worker.as_ref().is_some_and(|w| !w.alive()) {
            // The tether thread ended (open failure, or a crash after connecting):
            // never leave the tab showing a live camera that is not there.
            self.worker = None;
            let was_connected = self.model.is_some();
            self.model = None;
            self.live = false;
            if self.error.is_none() {
                self.error = Some(if was_connected {
                    "the camera worker stopped; connect again".into()
                } else {
                    "the camera worker stopped before connecting".into()
                });
            }
        }
    }

    /// No worker yet (not connected or connecting).
    pub fn worker_absent(&self) -> bool {
        self.worker.is_none()
    }

    /// The top bar's status.
    pub fn status(&self) -> Status {
        let t = tokens();
        if let Some(e) = &self.error {
            return Status { color: t.error, text: e.clone(), progress: None };
        }
        match (&self.worker, &self.model) {
            (None, _) => Status { color: t.text_disabled, text: "Camera not connected".into(), progress: None },
            (Some(_), None) => Status { color: t.caution, text: "Connecting to the camera…".into(), progress: None },
            (Some(_), Some(m)) => {
                let what = match &self.auto_status {
                    Some(s) if self.auto => s.message.clone(),
                    _ if self.live => "live view".into(),
                    _ => "ready".into(),
                };
                Status { color: t.ok, text: format!("{m} · {what} · {} captured", self.captured.len()), progress: None }
            }
        }
    }

    /// The right panel: camera, roll and capture settings.
    pub fn panel(&mut self, ui: &mut egui::Ui, services: &Services, shared: &mut Shared) {
        let t = tokens();
        widgets::panel_title(ui, "Capture", 24.0);
        section(ui, "camera", "Camera", true, |ui| {
            if let Some(e) = &self.error {
                widgets::status_line(ui, t.error, e);
            }
            match (&self.worker, &self.model) {
                (None, _) => widgets::status_line(ui, t.text_disabled, "Not connected"),
                (Some(_), None) => widgets::status_line(ui, t.caution, "Connecting…"),
                (Some(_), Some(m)) => widgets::status_line(ui, t.ok, m),
            }
            ui.horizontal(|ui| {
                if self.worker.is_none() {
                    if widgets::text_button(ui, "Connect", true, true).clicked() {
                        self.connect(services, ui.ctx());
                    }
                } else if widgets::text_button(ui, "Disconnect", true, false).clicked() {
                    self.disconnect();
                }
            });
            ui.label(RichText::new("Sony cameras: set USB Connection to PC Remote.").size(12.0).color(t.text_dim));
        });
        section(ui, "roll", "Roll", true, |ui| {
            crate::roll_fields(ui, &mut self.roll, shared);
        });
        let ready = self.model.is_some();
        section(ui, "capture", "Capture", true, |ui| {
            ui.add_enabled_ui(ready, |ui| {
                let mut live = self.live;
                if ui.checkbox(&mut live, "Live view").changed() {
                    self.set_live(live);
                }
                let mut auto = self.auto;
                if ui.add_enabled(self.live, egui::Checkbox::new(&mut auto, "Autocapture each new frame")).changed() {
                    self.set_auto(auto);
                }
                if self.auto
                    && widgets::text_button(ui, "New Frame", true, false).on_hover_text("This is a new frame: clear the duplicate guard").clicked()
                {
                    self.send(TetherCommand::NewFrame);
                }
            });
            ui.checkbox(&mut shared.settings.develop_after_capture, "Develop as frames come in");
            if let Some(s) = &self.auto_status {
                let color = match s.phase {
                    AutoPhase::Searching => t.text_dim,
                    AutoPhase::Settling => t.caution,
                    AutoPhase::WaitAdvance => t.ok,
                };
                widgets::status_line(ui, if s.stuck { t.caution } else { color }, &s.message);
            }
        });
        section(ui, "framing", "Framing", true, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Rotate").font(t.font(13.5)).color(t.text_dim));
                ui.add_space(8.0);
                let r = &mut shared.settings.camera_rotation;
                if widgets::icon_button(ui, Icon::Rotate, vec2(26.0, 24.0), false, true, "Turn left (Shift+R)").clicked() {
                    *r = r.turned(false);
                }
                if widgets::icon_button(ui, Icon::RotateRight, vec2(26.0, 24.0), false, true, "Turn right (R)").clicked() {
                    *r = r.turned(true);
                }
                ui.label(RichText::new(format!("{}°", r.degrees())).size(12.5).color(t.text_label));
            });
            match self.manual {
                Some(_) => {
                    widgets::status_line(ui, t.caution, "Frame set by hand");
                    if widgets::text_button(ui, "Detect Again", true, false).on_hover_text("Find the frame automatically again").clicked() {
                        self.set_frame(None);
                    }
                }
                None => {
                    ui.label(RichText::new("Drag the frame's edges or corners in live view to set it by hand.").size(12.0).color(t.text_dim));
                }
            }
        });
        if ready {
            section(ui, "exposure", "Exposure", true, |ui| {
                if self.exposure.is_empty() {
                    ui.label(RichText::new("Set the exposure on the camera: it isn't reporting it over USB.").size(12.0).color(t.text_dim));
                }
                let mut set = None;
                for s in &self.exposure {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(96.0, 22.0), Sense::hover());
                        ui.painter().text(pos2(r.left(), r.center().y), egui::Align2::LEFT_CENTER, s.control.name(), t.font(12.5), t.text_dim);
                        if s.settable {
                            // the camera lists choices darkest-last or brightest-last by control; ‹ › follow its list
                            if widgets::text_button(ui, "‹", s.step(-1).is_some(), false).clicked() {
                                set = s.step(-1).map(|v| (s.control, v));
                            }
                            let (vr, _) = ui.allocate_exact_size(vec2(78.0, 22.0), Sense::hover());
                            ui.painter().text(vr.center(), egui::Align2::CENTER_CENTER, &s.label, t.semibold(12.5), t.text);
                            if widgets::text_button(ui, "›", s.step(1).is_some(), false).clicked() {
                                set = s.step(1).map(|v| (s.control, v));
                            }
                        } else {
                            ui.label(RichText::new(&s.label).size(12.5).color(t.text_label));
                        }
                    });
                }
                if let Some((control, value)) = set {
                    self.send(TetherCommand::SetExposure(control, value));
                    if let Some(s) = self.exposure.iter_mut().find(|s| s.control == control) {
                        s.value = value;
                        s.label = silvercraft_engine::tether::exposure_label(control, value);
                    }
                }
            });
        }
        if !self.captured.is_empty() {
            section(ui, "captured", "Captured", true, |ui| {
                for f in self.captured.iter().rev().take(12) {
                    let name = f.capture.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    widgets::label_row(ui, &f.label, &name);
                }
                if let Some(m) = &self.last_manifest
                    && widgets::text_button(ui, "Develop This Roll", true, false).clicked()
                {
                    self.finished_roll = m.parent().map(PathBuf::from);
                }
            });
        }
    }

    /// The Info panel.
    pub fn info(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        widgets::panel_title(ui, "Info", 24.0);
        widgets::padded(ui, |ui| {
            widgets::label_row(ui, "Camera", self.model.as_deref().unwrap_or("not connected"));
            widgets::label_row(ui, "Roll folder", &shared.settings.rolls_dir.join(crate::safe_name(&self.roll)).display().to_string());
            if self.live_tex.is_some() {
                widgets::label_row(ui, "Live view", &format!("{} × {} px", self.live_size.0, self.live_size.1));
            }
            widgets::label_row(ui, "Captured", &self.captured.len().to_string());
        });
    }

    /// The bottom bar's actions.
    pub fn actions(&mut self, ui: &mut egui::Ui, services: &Services, _shared: &Shared) {
        if self.worker.is_none() {
            if widgets::pill_button(ui, "Connect Camera", true, true).clicked() {
                self.connect(services, ui.ctx());
            }
            return;
        }
        let ready = self.model.is_some();
        if widgets::pill_button(ui, if self.live { "Stop Live View" } else { "Live View" }, ready, false).clicked() {
            let on = !self.live;
            self.set_live(on);
        }
        if widgets::pill_button(ui, if self.auto { "Stop Autocapture" } else { "Autocapture" }, ready && self.live, false).clicked() {
            let on = !self.auto;
            self.set_auto(on);
        }
        if widgets::pill_button(ui, "Capture", ready && !self.roll.trim().is_empty(), true).clicked() {
            self.capture();
        }
    }

    /// The canvas: live view (turned upright) with the film frame; drag its edges, corners or
    /// middle to set it by hand, or drag out a new one where none was found.
    pub fn canvas(&mut self, ui: &mut egui::Ui, rotation: Rotation) {
        let t = tokens();
        let area = ui.max_rect().shrink(24.0);
        let Some(tex) = &self.live_tex else {
            let (title, body) = match (&self.worker, self.model.is_some()) {
                (None, _) => ("Tethered camera", "Connect a Sony camera (PC Remote) to scan with it"),
                (Some(_), false) => ("Connecting…", "Waiting for the camera"),
                (Some(_), true) => ("Live view is off", "Turn on live view to frame the film"),
            };
            widgets::empty_message(ui, ui.max_rect(), title, body);
            return;
        };
        let tex_id = tex.id();
        let (lw, lh) = (self.live_size.0.max(1) as f64, self.live_size.1.max(1) as f64);
        let (dw, dh) = rotation.size(self.live_size.0.max(1), self.live_size.1.max(1));
        let (dw, dh) = (dw as f64, dh as f64);
        let k = (area.width() / dw as f32).min(area.height() / dh as f32).max(0.01);
        let rect = Rect::from_center_size(area.center(), vec2(dw as f32 * k, dh as f32 * k));
        let to_screen = |x: f64, y: f64| rect.min + vec2(x as f32 * k, y as f32 * k);
        let to_image = |p: Pos2| (((p.x - rect.min.x) / k) as f64, ((p.y - rect.min.y) / k) as f64);
        let painter = ui.painter_at(ui.max_rect());
        painter.image(tex_id, rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), egui::Color32::WHITE);

        // the frame, upright
        let shown = self.manual.or(self.rect).filter(|r| r.width > 0 && r.height > 0);
        let upright = shown.map(|r| rotation.bbox([f64::from(r.x), f64::from(r.y), f64::from(r.x + r.width), f64::from(r.y + r.height)], lw, lh));
        let screen = upright.map(|b| Rect::from_min_max(to_screen(b[0], b[1]), to_screen(b[2], b[3])));
        let resp = ui.interact(rect.expand(GRIP), ui.id().with("live-frame"), Sense::drag());
        let grab_at = |p: Pos2| match screen {
            Some(sr) => grab_for(sr, p),
            None => Some(Grab::New),
        };
        if self.drag.is_none()
            && let Some(p) = resp.hover_pos()
        {
            ui.ctx().set_cursor_icon(cursor_for(grab_at(p)));
        }
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
            && let Some(grab) = grab_at(p)
        {
            let (x, y) = to_image(p);
            self.drag = Some(FrameDrag { grab, start: upright.unwrap_or([x, y, x, y]), from: pos2(x as f32, y as f32) });
        }
        if let (Some(d), Some(p)) = (self.drag, resp.interact_pointer_pos())
            && resp.dragged()
        {
            ui.ctx().set_cursor_icon(cursor_for(Some(d.grab)));
            let (x, y) = to_image(p);
            let b = dragged(d, (x, y), (dw, dh), MIN_FRAME);
            let s = rotation.inverse().bbox(b, dw, dh);
            let r = LiveRect::new(s[0].round() as i32, s[1].round() as i32, (s[2] - s[0]).round() as i32, (s[3] - s[1]).round() as i32);
            if b[2] - b[0] >= MIN_FRAME && b[3] - b[1] >= MIN_FRAME && self.manual != Some(r) {
                self.set_frame(Some(r));
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }

        if let Some(sr) = screen {
            let color = match self.auto_status.as_ref().map(|s| s.phase) {
                _ if self.manual.is_some() => t.frame_selected,
                Some(AutoPhase::WaitAdvance) => t.ok,
                Some(AutoPhase::Settling) => t.caution,
                _ => t.frame_box,
            };
            painter.rect_stroke(sr, 2.0, Stroke::new(2.0, color), StrokeKind::Middle);
            // corner handles
            for c in [sr.left_top(), sr.right_top(), sr.left_bottom(), sr.right_bottom()] {
                painter.rect_filled(Rect::from_center_size(c, vec2(7.0, 7.0)), 1.0, color);
            }
        }
    }
}
