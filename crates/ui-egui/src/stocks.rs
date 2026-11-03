//! Film stocks (listed in the sidebar): the canvas shows the selected profile's file, the right
//! panel what it is and where it came from, with "Use for New Rolls". "Pipeline Settings" lists
//! every config key with its meaning and default.

use egui::RichText;

use crate::theme::tokens;
use crate::widgets::{self, section};
use crate::{Shared, Status};

#[derive(Default)]
pub struct StocksTab {
    pub selected: Option<String>,
    pub filter: String,
    pub show_config: bool,
}

impl StocksTab {
    fn profile<'a>(&self, shared: &'a Shared) -> Option<(usize, &'a silvercraft_film::FilmProfile)> {
        let id = self.selected.as_deref()?;
        shared.registry.profiles().iter().enumerate().find(|(_, p)| p.id == id)
    }

    pub fn status(&self, shared: &Shared) -> Status {
        let t = tokens();
        let current = shared.registry.get(&shared.stock).map(|p| p.name.as_str()).unwrap_or("?");
        Status { color: t.ok, text: format!("{} stocks · new rolls use {current}", shared.registry.profiles().len()), progress: None }
    }

    pub fn panel(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let t = tokens();
        if self.show_config {
            widgets::panel_title(ui, "Pipeline Settings", 24.0);
            widgets::padded(ui, |ui| {
                ui.label(RichText::new("Filter").size(12.5).color(t.text_dim));
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("key or words").desired_width(f32::INFINITY));
                ui.label(
                    RichText::new("Set keys in ~/.config/silvercraft/config.toml, or per roll in Develop ▸ Pipeline Overrides.")
                        .size(12.0)
                        .color(t.text_dim),
                );
            });
            return;
        }
        widgets::panel_title(ui, "Stock", 24.0);
        let Some((_, p)) = self.profile(shared) else {
            widgets::padded(ui, |ui| {
                ui.label(
                    RichText::new("Select a stock in the sidebar. Add your own as TOML files in the profiles folder.").size(12.5).color(t.text_dim),
                );
            });
            return;
        };
        let (id, name) = (p.id.clone(), p.name.clone());
        let rows = [("Kind", p.kind.as_str().to_string()), ("Process", p.process.clone()), ("Maker", p.manufacturer.clone())];
        let current = shared.stock == id;
        section(ui, "stock", &name, true, |ui| {
            for (k, v) in &rows {
                widgets::label_row(ui, k, v);
            }
            ui.add_space(4.0);
            if widgets::text_button(ui, if current { "Used for New Rolls" } else { "Use for New Rolls" }, !current, !current).clicked() {
                shared.stock = id.clone();
                shared.settings.default_stock = id.clone();
            }
        });
    }

    pub fn info(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        widgets::panel_title(ui, "Info", 24.0);
        let Some((i, p)) = self.profile(shared) else { return };
        widgets::padded(ui, |ui| {
            widgets::label_row(ui, "Id", &p.id);
            if let Some(src) = shared.registry.source_of(i) {
                widgets::label_row(ui, "From", src);
            }
        });
    }

    pub fn actions(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let Some((_, p)) = self.profile(shared).filter(|_| !self.show_config) else { return };
        let id = p.id.clone();
        let current = shared.stock == id;
        if widgets::pill_button(ui, if current { "Used for New Rolls" } else { "Use for New Rolls" }, !current, true).clicked() {
            shared.stock = id.clone();
            shared.settings.default_stock = id;
        }
    }

    pub fn canvas(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let t = tokens();
        let rect = ui.max_rect();
        if self.show_config {
            config_keys(ui, &self.filter);
            return;
        }
        let Some((_, p)) = self.profile(shared) else {
            widgets::empty_message(ui, rect, "Film stocks", "Select a stock in the sidebar");
            return;
        };
        let mut text = p.to_toml();
        egui::Frame::NONE.inner_margin(24).show(ui, |ui| {
            ui.label(RichText::new(&p.name).font(t.semibold(18.0)).color(t.text));
            ui.add_space(10.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut text).code_editor().interactive(false).frame(egui::Frame::NONE).desired_width(f32::INFINITY));
            });
        });
    }
}

fn config_keys(ui: &mut egui::Ui, filter: &str) {
    let t = tokens();
    let f = filter.to_lowercase();
    egui::Frame::NONE.inner_margin(24).show(ui, |ui| {
        ui.label(RichText::new("Pipeline Settings").font(t.semibold(18.0)).color(t.text));
        ui.add_space(10.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("config-docs").striped(true).num_columns(3).spacing(egui::vec2(16.0, 6.0)).show(ui, |ui| {
                for d in silvercraft_config::docs::all() {
                    if !f.is_empty() && !d.path.contains(&f) && !d.doc.to_lowercase().contains(&f) {
                        continue;
                    }
                    ui.label(RichText::new(&d.path).monospace().color(t.text));
                    ui.label(RichText::new(&d.default).monospace().color(t.text_dim));
                    ui.add(egui::Label::new(RichText::new(&d.doc).size(12.5)).wrap());
                    ui.end_row();
                }
            });
        });
    });
}
