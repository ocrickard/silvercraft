//! SilverCraft's egui frontend, laid out like LightCraft: a top bar (menus, a status field in the
//! middle), the sidebar (capture devices, rolls, film stocks), the canvas (bed preview, live view,
//! a roll's frames), the right panel (the view's settings in collapsible sections) with its tool
//! strip, and the bottom bar (the view's actions).
//!
//! Pure egui: the app binary wraps it in eframe, tests drive it headless. Hardware comes in
//! through [`Services`] (real drivers, simulators or fakes), so nothing here names a driver.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod camera;
pub mod display;
mod frame_drag;
pub mod frames;
pub mod icons;
pub mod rolls;
pub mod scanner;
pub mod settings;
pub mod stocks;
pub mod theme;
pub mod titlebar;
pub mod viewer;
pub mod widgets;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use silvercraft_engine::hardware::{ScannerFactory, TetherFactory};
use silvercraft_engine::tether::FrameMeasure;
use silvercraft_epson::FrameBox;
use silvercraft_epson::session::Preview;
use silvercraft_film::profiles::ProfileRegistry;

pub use settings::AppSettings;

use crate::icons::{Icon, paint};
use crate::theme::tokens;
use crate::widgets::{icon_button, sidebar_row, sidebar_section};

/// Frame detection on a bed preview.
pub type DetectFrames = Arc<dyn Fn(&Preview) -> Vec<FrameBox> + Send + Sync>;

/// Hardware and platform hooks.
#[derive(Clone)]
pub struct Services {
    pub scanner: ScannerFactory,
    pub camera: TetherFactory,
    /// A fresh film-frame finder for live view (one per camera session).
    pub frame_measure: Arc<dyn Fn() -> FrameMeasure + Send + Sync>,
    /// Finds frames on a prescan; `None` = the user draws them.
    pub detect: Option<DetectFrames>,
    /// Where settings are saved; `None` = not saved (tests).
    pub settings_path: Option<PathBuf>,
}

/// State every view reads.
pub struct Shared {
    pub settings: AppSettings,
    /// Stock for new rolls.
    pub stock: String,
    pub registry: ProfileRegistry,
}

/// What the canvas shows (picked in the sidebar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Scanner,
    Camera,
    Rolls,
    Stocks,
}

/// What the right panel shows (picked in the tool strip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightPanel {
    /// The view's settings and controls.
    Edit,
    /// The view's activity log.
    Activity,
    /// Details: device, roll folder, stock source.
    Info,
    None,
}

/// A one-line status for the top bar's field: colour, text and an optional progress fraction.
pub struct Status {
    pub color: Color32,
    pub text: String,
    pub progress: Option<f32>,
}

pub struct SilverApp {
    pub tab: Tab,
    pub right: RightPanel,
    pub services: Services,
    pub shared: Shared,
    pub scanner: scanner::ScannerTab,
    pub camera: camera::CameraTab,
    pub rolls: rolls::RollsTab,
    pub stocks: stocks::StocksTab,
    /// Full-screen preview: the canvas alone on black (F; Esc leaves).
    pub fullscreen_preview: bool,
    /// The window is in the OS's full-screen mode (host reports it).
    pub window_fullscreen: bool,
    /// macOS: the top bar is the title bar (drag to move, double-click to zoom).
    pub integrated_titlebar: bool,
    /// The window buttons the app draws itself (Linux: no system title bar); `None` where the
    /// system draws them.
    pub window_controls: Option<titlebar::Controls>,
    /// The shortcuts popup is open.
    pub show_help: bool,
    /// The theme (fonts included) is installed; until then nothing is drawn.
    themed: bool,
    saved: AppSettings,
}

impl SilverApp {
    pub fn new(services: Services, settings: AppSettings, registry: ProfileRegistry) -> Self {
        let stock = if registry.get(&settings.default_stock).is_some() { settings.default_stock.clone() } else { "generic-c41".into() };
        let roll = next_roll_name(&settings.rolls_dir);
        let shared = Shared { stock, registry, settings: settings.clone() };
        Self {
            tab: Tab::Scanner,
            right: RightPanel::Edit,
            services,
            shared,
            scanner: scanner::ScannerTab::with_roll(roll.clone()),
            camera: camera::CameraTab::with_roll(roll),
            rolls: rolls::RollsTab::default(),
            stocks: stocks::StocksTab::default(),
            fullscreen_preview: false,
            window_fullscreen: false,
            integrated_titlebar: false,
            window_controls: None,
            show_help: false,
            themed: false,
            saved: settings,
        }
    }

    /// Apply worker events and cross-view handoffs; called at the start of every frame.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.scanner.poll(&self.services, self.shared.settings.develop_after_capture);
        self.camera.poll(ctx, &self.shared);
        self.rolls.poll(ctx);
        self.rolls.refresh_if_stale(&self.shared, ctx);
        let finished = self.scanner.finished_roll.take().or_else(|| self.camera.finished_roll.take());
        if let Some(dir) = finished {
            self.tab = Tab::Rolls;
            if !self.rolls.running() {
                self.rolls.develop_dir(dir, &self.shared);
            }
        }
        // frames whose crop changed develop again
        self.rolls.develop_queued(&self.shared);
        // camera frames develop in the background as they come in (sent on when the roll is done)
        if self.shared.settings.develop_after_capture
            && !self.rolls.running()
            && let Some(dir) = self.camera.develop_due(ctx.input(|i| i.time)).map(Path::to_path_buf)
            && self.rolls.can_develop_in_background(&dir)
        {
            self.camera.develop_started();
            self.rolls.develop_in_background(dir, &self.shared);
        }
        if let Some(fs) = ctx.input(|i| i.viewport().fullscreen) {
            self.window_fullscreen = fs;
        }
        self.shared.settings.default_stock.clone_from(&self.shared.stock);
        if self.shared.settings != self.saved {
            if let Some(p) = &self.services.settings_path
                && let Err(e) = self.shared.settings.save(p)
            {
                log::warn!("settings not saved: {e}");
            }
            self.saved = self.shared.settings.clone();
        }
    }

    /// Keyboard shortcuts (LightCraft's where they overlap).
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.tab == Tab::Camera && !self.show_help {
            self.camera.keys(ctx, &mut self.shared);
        }
        let typing = ctx.egui_wants_keyboard_input();
        let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        ctx.input_mut(|i| {
            if i.consume_key(cmd_shift, egui::Key::L) {
                self.shared.settings.left_panel = !self.shared.settings.left_panel;
            }
            if i.consume_key(cmd_shift, egui::Key::F) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.window_fullscreen));
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Q) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if !typing && i.consume_key(egui::Modifiers::NONE, egui::Key::F) {
                self.fullscreen_preview = !self.fullscreen_preview;
            }
            // Esc closes the innermost thing open; left alone, it reaches the view (the frame viewer)
            if (self.show_help || self.fullscreen_preview || self.window_fullscreen) && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                if self.show_help {
                    self.show_help = false;
                } else if self.fullscreen_preview {
                    self.fullscreen_preview = false;
                } else if self.window_fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
            }
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.themed {
            // fonts take effect from the next frame: the semibold family must exist before use
            theme::apply(&ctx);
            // the window manager's dark variant, wherever it draws decorations that honour it
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
            self.themed = true;
            ctx.request_repaint();
            return;
        }
        self.logic(&ctx);
        self.shortcuts(&ctx);
        let t = tokens();
        if self.window_controls.is_some() {
            // after the frame's widgets (the panels' own resize handles are inside the window)
            let full = self.window_fullscreen;
            ctx.on_end_pass("window-resize-edges", std::sync::Arc::new(move |ctx| titlebar::resize_edges(ctx, full)));
        }
        if self.fullscreen_preview {
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(ui, |ui| self.canvas(ui));
            return;
        }
        // Order matters (as in LightCraft): the top bar spans the window; the tool strip, right
        // panel and sidebar run to the bottom; the bottom bar sits between them.
        self.top_bar(ui);
        self.tool_strip(ui);
        if self.right != RightPanel::None {
            let (min, _, max) = settings::RIGHT_WIDTH;
            let frame = egui::Frame::NONE.fill(t.chrome).stroke(Stroke::new(1.0, t.divider));
            let r = egui::Panel::right("right_panel")
                .frame(frame)
                .resizable(true)
                .size_range(min..=max)
                .default_size(self.shared.settings.right_width.clamp(min, max))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("right-scroll").auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        self.right_panel(ui);
                    });
                });
            remember_width(&mut self.shared.settings.right_width, r.response.rect.width());
        }
        if self.shared.settings.left_panel {
            let (min, _, max) = settings::LEFT_WIDTH;
            let frame = egui::Frame::NONE.fill(t.chrome).stroke(Stroke::new(1.0, t.divider));
            let r = egui::Panel::left("left_panel")
                .frame(frame)
                .resizable(true)
                .size_range(min..=max)
                .default_size(self.shared.settings.left_width.clamp(min, max))
                .show(ui, |ui| self.sidebar(ui));
            remember_width(&mut self.shared.settings.left_width, r.response.rect.width());
        }
        self.bottom_bar(ui);
        let bg = if self.tab == Tab::Rolls { t.grid_bg } else { t.canvas };
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg)).show(ui, |ui| self.canvas(ui));
        if self.show_help {
            help_window(&ctx, &mut self.show_help);
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        match self.tab {
            Tab::Scanner => self.scanner.canvas(ui, &self.shared),
            Tab::Camera => self.camera.canvas(ui, self.shared.settings.camera_rotation),
            Tab::Rolls => self.rolls.canvas(ui, &self.shared),
            Tab::Stocks => self.stocks.canvas(ui, &self.shared),
        }
    }

    /// The view's status for the top bar's field.
    pub fn status(&self) -> Status {
        match self.tab {
            Tab::Scanner => self.scanner.status(),
            Tab::Camera => self.camera.status(),
            Tab::Rolls => self.rolls.status(),
            Tab::Stocks => self.stocks.status(&self.shared),
        }
    }

    // ------------------------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let t = tokens();
        let controls_w = |c: &[titlebar::Control]| if c.is_empty() { 0.0 } else { c.len() as f32 * titlebar::CONTROL_W + 8.0 };
        let (cl, cr) = self.window_controls.as_ref().map_or((0.0, 0.0), |c| (controls_w(&c.left), controls_w(&c.right)));
        let left = if self.window_controls.is_some() {
            10 + cl as i8
        } else if self.integrated_titlebar {
            78
        } else {
            10
        };
        egui::Panel::top("top_bar")
            .exact_size(t.top_bar_h)
            .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin { left, right: 12, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                let full = ui.max_rect();
                if self.integrated_titlebar {
                    titlebar::window_handle(ui, full, left as f32, 12.0, self.window_fullscreen);
                }
                if let Some(c) = &self.window_controls {
                    let maximized = ui.input(|i| i.viewport().maximized).unwrap_or(false);
                    titlebar::draw_controls(ui, &c.left, full.left() - cl, full.center().y, maximized);
                    titlebar::draw_controls(ui, &c.right, full.right() - cr + 8.0, full.center().y, maximized);
                }
                let fw = 560.0f32.min(full.width() - 520.0).max(200.0);
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if icon_button(ui, Icon::Sidebar, vec2(30.0, 30.0), self.shared.settings.left_panel, true, "Show/hide sidebar (Ctrl+Shift+L)")
                        .clicked()
                    {
                        self.shared.settings.left_panel = !self.shared.settings.left_panel;
                    }
                    ui.add_space(10.0);
                    self.menus(ui);
                });
                self.status_field(ui, Rect::from_center_size(full.center(), vec2(fw, 28.0)));
                // right icons
                let mut x = full.right() - 18.0 - cr;
                let send = self.shared.settings.send_to_lightcraft;
                let icons: [(Icon, String, Color32, u8); 2] = [
                    (Icon::Help, "Keyboard shortcuts".into(), t.icon, 0),
                    (
                        Icon::Send,
                        if send {
                            "Developed rolls go to LightCraft (start it with --control 7980). Click to keep them here instead.".into()
                        } else {
                            "Developed rolls stay in their folders. Click to send them to LightCraft.".into()
                        },
                        if send { t.accent } else { t.icon },
                        1,
                    ),
                ];
                for (icon, tip, colour, what) in icons {
                    let r = Rect::from_center_size(pos2(x, full.center().y), vec2(28.0, 28.0));
                    let resp = ui.interact(r, egui::Id::new(("top-icon", what)), Sense::click()).on_hover_text(tip);
                    paint(ui.painter(), r.shrink(5.0), icon, if resp.hovered() { t.text } else { colour });
                    if resp.clicked() {
                        match what {
                            0 => self.show_help = !self.show_help,
                            _ => self.shared.settings.send_to_lightcraft = !send,
                        }
                    }
                    x -= 40.0;
                }
            });
    }

    /// The status field in the middle of the top bar (where LightCraft has its search field):
    /// device or job state, with progress drawn along its bottom edge.
    fn status_field(&self, ui: &mut egui::Ui, r: Rect) {
        let t = tokens();
        let s = self.status();
        ui.painter().rect(r, 4.0, t.field, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
        let g = widgets::one_line(ui.painter(), &s.text, t.font(13.5), t.text_label, r.width() - 48.0);
        let w = g.size().x + 16.0;
        let x0 = r.center().x - w / 2.0;
        ui.painter().circle_filled(pos2(x0 + 4.0, r.center().y), 4.0, s.color);
        ui.painter().galley(pos2(x0 + 16.0, r.center().y - g.size().y / 2.0), g, t.text_label);
        if let Some(f) = s.progress {
            let bar = Rect::from_min_max(pos2(r.left() + 3.0, r.bottom() - 4.0), pos2(r.right() - 3.0, r.bottom() - 2.0));
            ui.painter().rect_filled(bar, 1.0, t.inset);
            ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f.clamp(0.0, 1.0), bar.height())), 1.0, t.accent);
        }
    }

    /// In-window menus (LightCraft draws them in the top bar where there is no native menu bar).
    fn menus(&mut self, ui: &mut egui::Ui) {
        let t = tokens();
        let title =
            |ui: &mut egui::Ui, s: &str| ui.add(egui::Button::new(egui::RichText::new(s).font(t.font(13.0)).color(t.text_label)).frame(false));
        let file = title(ui, "File");
        egui::Popup::menu(&file).show(|ui| {
            ui.set_min_width(220.0);
            if ui.button("New Roll").clicked() {
                let name = next_roll_name(&self.shared.settings.rolls_dir);
                self.scanner.roll.clone_from(&name);
                self.camera.roll = name;
            }
            if ui.button("Refresh Rolls").clicked() {
                self.rolls.refresh(&self.shared);
            }
            ui.separator();
            if ui.button("Quit SilverCraft        Ctrl+Q").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        let capture = title(ui, "Capture");
        egui::Popup::menu(&capture).show(|ui| {
            ui.set_min_width(220.0);
            let ready = self.scanner.connected && self.scanner.busy.is_none();
            if ui.add_enabled(self.scanner.worker_absent(), egui::Button::new("Connect Scanner")).clicked() {
                let services = self.services.clone();
                self.scanner.connect(&services, &self.shared, ui.ctx());
                self.tab = Tab::Scanner;
            }
            if ui.add_enabled(ready, egui::Button::new("Prescan Bed")).clicked() {
                self.scanner.prescan();
                self.tab = Tab::Scanner;
            }
            if ui.add_enabled(self.scanner.busy.is_some(), egui::Button::new("Cancel Scan")).clicked() {
                self.scanner.cancel();
            }
            ui.separator();
            if ui.add_enabled(self.camera.worker_absent(), egui::Button::new("Connect Camera")).clicked() {
                let services = self.services.clone();
                self.camera.connect(&services, ui.ctx());
                self.tab = Tab::Camera;
            }
            if ui.add_enabled(self.camera.model.is_some(), egui::Button::new("Capture Frame")).clicked() {
                self.camera.capture();
            }
        });
        let view = title(ui, "View");
        egui::Popup::menu(&view).show(|ui| {
            ui.set_min_width(240.0);
            ui.checkbox(&mut self.shared.settings.left_panel, "Sidebar        Ctrl+Shift+L");
            ui.checkbox(&mut self.shared.settings.show_positive, "Negatives as Positives");
            ui.separator();
            if ui.button("Full Screen Preview        F").clicked() {
                self.fullscreen_preview = true;
            }
            if ui.button("Enter Full Screen        Ctrl+Shift+F").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.window_fullscreen));
            }
        });
        let help = title(ui, "Help");
        egui::Popup::menu(&help).show(|ui| {
            if ui.button("Keyboard Shortcuts").clicked() {
                self.show_help = true;
            }
        });
    }

    // ------------------------------------------------------------------------------- sidebar

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let t = tokens();
        ui.spacing_mut().item_spacing.y = 0.0;
        let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
        ui.painter().text(pos2(hr.left() + 18.0, hr.center().y), Align2::LEFT_CENTER, "Capture", t.semibold(15.0), t.text);
        egui::ScrollArea::vertical().id_salt("left-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let scanner_state = if self.scanner.busy.is_some() {
                Some("scanning".to_string())
            } else if self.scanner.connected {
                Some("ready".into())
            } else {
                None
            };
            if sidebar_row(ui, Icon::Scanner, "Flatbed Scanner", scanner_state, self.tab == Tab::Scanner, 0.0).clicked() {
                self.tab = Tab::Scanner;
            }
            let camera_state = if self.camera.live { Some("live".to_string()) } else { self.camera.model.as_ref().map(|_| "ready".to_string()) };
            if sidebar_row(ui, Icon::Camera, "Camera", camera_state, self.tab == Tab::Camera, 0.0).clicked() {
                self.tab = Tab::Camera;
            }
            ui.add_space(10.0);
            let (hr, open) = sidebar_section(ui, "rolls", "Rolls");
            let plus = Rect::from_center_size(pos2(hr.right() - 22.0, hr.center().y), vec2(26.0, 26.0));
            let presp = ui
                .interact(plus, egui::Id::new("roll-new"), Sense::click())
                .on_hover_text("New roll: the next free roll name for scanning and capture");
            widgets::paint_icon_button(ui, plus, &presp, Icon::Plus, false, true);
            if presp.clicked() {
                let name = next_roll_name(&self.shared.settings.rolls_dir);
                self.scanner.roll.clone_from(&name);
                self.camera.roll = name;
            }
            if open {
                if self.rolls.rolls.is_empty() {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                    ui.painter().text(pos2(r.left() + 42.0, r.center().y), Align2::LEFT_CENTER, "No rolls yet", t.font(12.5), t.text_dim);
                }
                let mut pick = None;
                for r in &self.rolls.rolls {
                    let sel = self.tab == Tab::Rolls && self.rolls.selected.as_deref() == Some(r.dir.as_path());
                    let busy = self.rolls.running() && self.rolls.job_dir() == Some(r.dir.as_path());
                    let count = if busy { "…".to_string() } else { r.manifest.frames.len().to_string() };
                    if sidebar_row(ui, Icon::Film, &r.manifest.roll, Some(count), sel, 0.0).clicked() {
                        pick = Some(r.dir.clone());
                    }
                }
                if let Some(d) = pick {
                    self.rolls.select(d);
                    self.tab = Tab::Rolls;
                }
            }
            ui.add_space(10.0);
            let (_, open) = sidebar_section(ui, "stocks", "Film Stocks");
            if open {
                let mut pick = None;
                for p in self.shared.registry.profiles() {
                    let sel = self.tab == Tab::Stocks && !self.stocks.show_config && self.stocks.selected.as_deref() == Some(p.id.as_str());
                    let mark = (p.id == self.shared.stock).then(|| "●".to_string());
                    if sidebar_row(ui, Icon::Album, &p.name, mark, sel, 0.0).on_hover_text(format!("{} · {}", p.kind.as_str(), p.process)).clicked()
                    {
                        pick = Some(p.id.clone());
                    }
                }
                if let Some(id) = pick {
                    self.stocks.selected = Some(id);
                    self.stocks.show_config = false;
                    self.tab = Tab::Stocks;
                }
                let sel = self.tab == Tab::Stocks && self.stocks.show_config;
                if sidebar_row(ui, Icon::Gear, "Pipeline Settings", None, sel, 0.0)
                    .on_hover_text("Every pipeline config key with its default")
                    .clicked()
                {
                    self.stocks.show_config = true;
                    self.tab = Tab::Stocks;
                }
            }
            ui.add_space(12.0);
        });
    }

    // ------------------------------------------------------------------------- right panel

    fn tool_strip(&mut self, ui: &mut egui::Ui) {
        let t = tokens();
        egui::Panel::right("tool_strip")
            .exact_size(t.strip_w)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(t.chrome)
                    .inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 8 })
                    .stroke(Stroke::new(1.0, t.divider)),
            )
            .show(ui, |ui| {
                let sz = vec2(t.strip_w, 40.0);
                let edit_tip = match self.tab {
                    Tab::Scanner => "Scan settings",
                    Tab::Camera => "Capture settings",
                    Tab::Rolls => "Develop",
                    Tab::Stocks => "Stock",
                };
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for (panel, icon, tip) in [(RightPanel::Edit, Icon::Sliders, edit_tip), (RightPanel::Activity, Icon::Activity, "Activity")] {
                        if icon_button(ui, icon, sz, self.right == panel, true, tip).clicked() {
                            self.right = if self.right == panel { RightPanel::None } else { panel };
                        }
                    }
                });
                let bottom = ui.max_rect().bottom();
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(Rect::from_min_max(pos2(ui.max_rect().left(), bottom - 48.0), ui.max_rect().right_bottom())),
                );
                child.vertical_centered(|ui| {
                    if icon_button(ui, Icon::Info, sz, self.right == RightPanel::Info, true, "Info").clicked() {
                        self.right = if self.right == RightPanel::Info { RightPanel::None } else { RightPanel::Info };
                    }
                });
            });
    }

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        let services = self.services.clone();
        match (self.right, self.tab) {
            (RightPanel::Edit, Tab::Scanner) => self.scanner.panel(ui, &services, &mut self.shared),
            (RightPanel::Edit, Tab::Camera) => self.camera.panel(ui, &services, &mut self.shared),
            (RightPanel::Edit, Tab::Rolls) => self.rolls.panel(ui, &mut self.shared),
            (RightPanel::Edit, Tab::Stocks) => self.stocks.panel(ui, &mut self.shared),
            (RightPanel::Activity, tab) => {
                widgets::panel_title(ui, "Activity", 24.0);
                let log: &[String] = match tab {
                    Tab::Scanner => &self.scanner.notes,
                    Tab::Camera => &self.camera.notes,
                    Tab::Rolls => &self.rolls.log,
                    Tab::Stocks => &[],
                };
                activity(ui, log);
            }
            (RightPanel::Info, Tab::Scanner) => self.scanner.info(ui, &self.shared),
            (RightPanel::Info, Tab::Camera) => self.camera.info(ui, &self.shared),
            (RightPanel::Info, Tab::Rolls) => self.rolls.info(ui, &self.shared),
            (RightPanel::Info, Tab::Stocks) => self.stocks.info(ui, &self.shared),
            (RightPanel::None, _) => {}
        }
    }

    // ------------------------------------------------------------------------- bottom bar

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let t = tokens();
        egui::Panel::bottom("bottom_bar")
            .exact_size(t.bottom_bar_h)
            .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin { left: 12, right: 12, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let pos = self.shared.settings.show_positive;
                    let tip = if pos {
                        "Showing negatives as positives (click: as scanned)"
                    } else {
                        "Showing captures as scanned (click: negatives as positives)"
                    };
                    if self.tab != Tab::Stocks && icon_button(ui, Icon::Invert, vec2(32.0, 32.0), pos, true, tip).clicked() {
                        self.shared.settings.show_positive = !pos;
                    }
                    if icon_button(ui, Icon::Single, vec2(32.0, 32.0), false, true, "Full screen preview (F)").clicked() {
                        self.fullscreen_preview = true;
                    }
                });
                // the view's actions, centred
                let services = self.services.clone();
                let mut centre = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(Rect::from_center_size(full.center(), vec2((full.width() - 200.0).max(200.0), full.height())))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                centre.spacing_mut().item_spacing.x = 8.0;
                // centred by last frame's width (one frame behind, no second layout pass)
                let key = egui::Id::new("bottom-actions-width");
                let width: f32 = ui.data(|d| d.get_temp(key)).unwrap_or(0.0);
                centre.add_space(((centre.available_width() - width) / 2.0).max(0.0));
                let start = centre.cursor().left();
                self.actions(&mut centre, &services);
                let used = centre.min_rect().right() - start;
                ui.data_mut(|d| d.insert_temp(key, used));
            });
    }

    fn actions(&mut self, ui: &mut egui::Ui, services: &Services) {
        match self.tab {
            Tab::Scanner => self.scanner.actions(ui, services, &self.shared),
            Tab::Camera => self.camera.actions(ui, services, &self.shared),
            Tab::Rolls => self.rolls.actions(ui, &self.shared),
            Tab::Stocks => self.stocks.actions(ui, &mut self.shared),
        }
    }
}

/// Keep a resized panel's width in the settings (saved), ignoring sub-point jitter.
fn remember_width(stored: &mut f32, now: f32) {
    if (now - *stored).abs() >= 1.0 {
        *stored = now.round();
    }
}

/// The activity log, newest first.
pub fn activity(ui: &mut egui::Ui, log: &[String]) {
    let t = tokens();
    widgets::padded(ui, |ui| {
        if log.is_empty() {
            ui.label(egui::RichText::new("Nothing yet").size(12.5).color(t.text_dim));
        }
        for l in log.iter().rev().take(400) {
            ui.label(egui::RichText::new(l).size(12.0).color(t.text_label));
        }
    });
}

fn help_window(ctx: &egui::Context, open: &mut bool) {
    let t = tokens();
    egui::Window::new("Keyboard Shortcuts").open(open).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0)).show(
        ctx,
        |ui| {
            egui::Grid::new("shortcuts").num_columns(2).spacing(vec2(24.0, 6.0)).show(ui, |ui| {
                for (k, what) in [
                    ("Ctrl+Shift+L", "Show or hide the sidebar"),
                    ("F", "Full screen preview"),
                    ("Ctrl+Shift+F", "Full screen window"),
                    ("Esc", "Leave full screen"),
                    ("Delete", "Remove the selected frame (bed preview)"),
                    ("Space", "Capture (camera)"),
                    ("R / Shift+R", "Turn the live view right / left (camera)"),
                    ("← / →", "Previous / next frame (viewer)"),
                    ("Z", "Fit or 1:1 (viewer)"),
                    ("C", "Crop the frame; Enter applies, Esc cancels (viewer)"),
                    ("Ctrl+Q", "Quit"),
                ] {
                    ui.label(egui::RichText::new(k).font(t.semibold(12.5)).color(t.text));
                    ui.label(egui::RichText::new(what).size(12.5));
                    ui.end_row();
                }
            });
        },
    );
}

/// Roll name and stock pickers shared by the capture views (inside a panel section).
pub fn roll_fields(ui: &mut egui::Ui, roll: &mut String, shared: &mut Shared) {
    let t = tokens();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Roll").font(t.font(13.5)).color(t.text_dim));
        ui.add(egui::TextEdit::singleline(roll).desired_width(ui.available_width() - 56.0));
        if widgets::text_button(ui, "New", true, false).on_hover_text("Next free roll name").clicked() {
            *roll = next_roll_name(&shared.settings.rolls_dir);
        }
    });
    let current = shared.registry.get(&shared.stock).map(|p| p.name.clone()).unwrap_or_else(|| shared.stock.clone());
    let names: Vec<String> = shared.registry.profiles().iter().map(|p| p.name.clone()).collect();
    if let Some(i) = widgets::choice_row(ui, "Stock", &current, &names)
        && let Some(p) = shared.registry.profiles().get(i)
    {
        shared.stock = p.id.clone();
    }
}

/// A folder name for a roll: letters, digits, space, `-`, `_` and `.`; anything else becomes `-`.
pub fn safe_name(s: &str) -> String {
    let t: String = s.trim().chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '-' }).collect();
    let t = t.trim_matches('.').trim().to_string();
    if t.is_empty() { "roll".into() } else { t }
}

/// The first `R-NNN` with no folder in `rolls_dir`.
pub fn next_roll_name(rolls_dir: &Path) -> String {
    (1..10_000).map(|i| format!("R-{i:03}")).find(|n| !rolls_dir.join(n).exists()).unwrap_or_else(|| "R-new".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name(" Portra / Paris 2025 "), "Portra - Paris 2025");
        assert_eq!(safe_name("../.."), "-", "never a path that climbs out");
        assert_eq!(safe_name(""), "roll");
        assert_eq!(safe_name(".."), "roll");
    }

    #[test]
    fn next_roll_skips_existing() {
        let d = std::env::temp_dir().join(format!("sc-rollname-{}", std::process::id()));
        std::fs::create_dir_all(d.join("R-001")).unwrap();
        assert_eq!(next_roll_name(&d), "R-002");
    }

    #[test]
    fn panel_widths_ignore_jitter() {
        let mut w = 270.0;
        remember_width(&mut w, 270.4);
        assert_eq!(w, 270.0);
        remember_width(&mut w, 301.6);
        assert_eq!(w, 302.0);
    }
}
