//! LightCraft's widgets (its `crates/ui-egui/src/widgets.rs` and side-panel rows), drawn the same
//! way so SilverCraft looks like part of it: section headers, sidebar rows, bordered text buttons,
//! icon buttons, dropdowns, label/value rows and the empty-canvas message.

use egui::{Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::icons::{Icon, paint};
use crate::theme::tokens;

/// The panel title ("My Photos", "Edit"): semibold 15 at the panel's left inset.
pub fn panel_title(ui: &mut Ui, title: &str, left: f32) -> Rect {
    let t = tokens();
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    ui.painter().text(pos2(r.left() + left, r.center().y + 2.0), Align2::LEFT_CENTER, title, t.semibold(15.0), t.text);
    r
}

/// Collapsible section header of a right panel ("⌄ Light"). Click toggles.
pub fn section_header(ui: &mut Ui, title: &str, open: bool) -> Response {
    let t = tokens();
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 52.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, open, title));
    let p = ui.painter();
    if ui.rect_contains_pointer(r) {
        p.rect_filled(r, 0.0, t.chrome.gamma_multiply(1.06));
    }
    let chev = Rect::from_center_size(pos2(r.left() + 30.0, r.center().y), vec2(14.0, 14.0));
    paint(p, chev, if open { Icon::ChevronDown } else { Icon::ChevronRight }, t.text_label);
    p.text(pos2(r.left() + 44.0, r.center().y), Align2::LEFT_CENTER, title, t.semibold(14.0), t.text);
    resp
}

/// A collapsible right-panel section whose open state lives in egui memory under `id`.
pub fn section(ui: &mut Ui, id: &str, title: &str, default_open: bool, add: impl FnOnce(&mut Ui)) {
    let key = egui::Id::new(("section", id));
    let mut open = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(default_open);
    if section_header(ui, title, open).clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(key, open));
    }
    if open {
        padded(ui, add);
        ui.add_space(8.0);
    }
    divider(ui);
}

/// Content inset like LightCraft's right panel (24 left, 22 right).
pub fn padded(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 22, top: 2, bottom: 2 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        add(ui)
    });
}

/// Thin divider between groups.
pub fn divider(ui: &mut Ui) {
    let t = tokens();
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, t.divider);
}

/// "Label    value" in a panel (dim label, lighter value), like LightCraft's Info rows.
pub fn label_row(ui: &mut Ui, label: &str, value: &str) {
    let t = tokens();
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(86.0, 20.0), Sense::hover());
        ui.painter().text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, label, t.font(12.5), t.text_dim);
        ui.add(egui::Label::new(egui::RichText::new(value).size(12.5).color(t.text_label)).truncate());
    });
}

/// A small bordered text button (Auto, B&W, Reset…). `primary` fills it with the accent.
pub fn text_button(ui: &mut Ui, label: &str, enabled: bool, primary: bool) -> Response {
    let t = tokens();
    let font = t.semibold(11.5);
    let color = if enabled { t.text } else { t.text_disabled };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let size = vec2((galley.size().x + 18.0).max(37.0), 24.0);
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let (fill, border) = match (primary && enabled, resp.hovered() && enabled) {
        (true, false) => (t.accent, t.accent),
        (true, true) => (t.accent.gamma_multiply(1.2), t.accent),
        (false, true) => (t.hover, t.button_border),
        (false, false) => (t.button, t.button_border),
    };
    let p = ui.painter();
    p.rect(r, CornerRadius::same(4), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    p.galley(r.center() - galley.size() / 2.0, galley, color);
    resp
}

/// A segmented control: `items` share the width equally. Returns the clicked index.
pub fn segmented(ui: &mut Ui, items: &[&str], active: usize) -> Option<usize> {
    let t = tokens();
    let gap = 4.0;
    let n = items.len().max(1) as f32;
    let seg_w = ((ui.available_width() - gap * (n - 1.0)) / n).max(24.0);
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, label) in items.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(seg_w, 24.0), Sense::click());
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, active == i, *label));
            let fill = if active == i {
                t.pressed
            } else if resp.hovered() {
                t.hover
            } else {
                t.button
            };
            let p = ui.painter();
            p.rect(r, CornerRadius::same(4), fill, Stroke::new(1.0, t.button_border), StrokeKind::Inside);
            p.text(r.center(), Align2::CENTER_CENTER, *label, t.semibold(11.5), if active == i { t.text } else { t.text_label });
            if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// An icon-only button. `active` draws the selected background (tool strip).
pub fn icon_button(ui: &mut Ui, icon: Icon, size: egui::Vec2, active: bool, enabled: bool, tooltip: &str) -> Response {
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, active, tooltip));
    paint_icon_button(ui, r, &resp, icon, active, enabled);
    if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) }
}

/// Draw an icon button's background and glyph in `r`.
pub fn paint_icon_button(ui: &Ui, r: Rect, resp: &Response, icon: Icon, active: bool, enabled: bool) {
    let t = tokens();
    let p = ui.painter();
    let sq = Rect::from_center_size(r.center(), vec2(32.0f32.min(r.width()), 32.0f32.min(r.height())));
    if active {
        p.rect_filled(sq, 4.0, t.tool_active);
    } else if resp.hovered() && enabled {
        p.rect_filled(sq, 4.0, t.hover.gamma_multiply(0.7));
    }
    let c = if !enabled {
        t.text_disabled
    } else if active {
        t.pick
    } else if resp.hovered() {
        t.text
    } else {
        t.icon
    };
    let glyph = (r.width().min(r.height()) * 0.62).min(22.0);
    paint(p, Rect::from_center_size(r.center(), vec2(glyph, glyph)), icon, c);
}

/// A borderless dropdown button: `text` followed by a painted chevron.
pub fn dropdown(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32) -> Response {
    let t = tokens();
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = vec2(galley.size().x + 20.0, galley.size().y.max(20.0));
    let (r, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, text));
    let c = if resp.hovered() { t.text } else { color };
    ui.painter().galley(pos2(r.left(), r.center().y - galley.size().y / 2.0), galley, c);
    paint(ui.painter(), Rect::from_center_size(pos2(r.right() - 7.0, r.center().y + 1.0), vec2(12.0, 12.0)), Icon::ChevronDown, c);
    resp
}

/// "Label  Value ⌄" with a popup of `choices`; returns the chosen index. LightCraft's "Profile
/// Color ⌄" row.
pub fn choice_row(ui: &mut Ui, label: &str, current: &str, choices: &[String]) -> Option<usize> {
    let t = tokens();
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).font(t.font(13.5)).color(t.text_dim));
        let r = dropdown(ui, current, t.font(13.5), t.text);
        egui::Popup::menu(&r).show(|ui| {
            ui.set_min_width(200.0);
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for (i, c) in choices.iter().enumerate() {
                    if ui.selectable_label(c == current, c).clicked() {
                        picked = Some(i);
                    }
                }
            });
        });
    });
    picked
}

/// A sidebar row (LightCraft's "All Photos 24"): icon, name, count; selected rows sit on the canvas
/// colour, hovered ones on a faint hover.
pub fn sidebar_row(ui: &mut Ui, icon: Icon, label: &str, count: Option<String>, selected: bool, indent: f32) -> Response {
    let t = tokens();
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 29.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label));
    let inner = Rect::from_min_max(r.min + vec2(8.0, 0.0), pos2((r.right() - 8.0).max(r.left() + 8.0), r.bottom()));
    if selected {
        ui.painter().rect_filled(inner, 4.0, t.canvas);
    } else if resp.hovered() {
        ui.painter().rect_filled(inner, 4.0, t.hover.gamma_multiply(0.6));
    }
    paint(
        ui.painter(),
        Rect::from_min_size(pos2(r.left() + 18.0 + indent, r.center().y - 8.0), vec2(16.0, 16.0)),
        icon,
        if selected { t.text } else { t.icon },
    );
    let color = if selected { t.text } else { t.text_label };
    let count_galley = count.map(|n| ui.painter().layout_no_wrap(n, t.font(12.5), t.text_dim));
    let label_left = r.left() + 42.0 + indent;
    let count_left = count_galley.as_ref().map_or(r.right() - 18.0, |g| r.right() - 18.0 - g.size().x);
    let g = one_line(ui.painter(), label, t.font(13.5), color, count_left - 8.0 - label_left);
    let elided = g.elided;
    ui.painter().galley(pos2(label_left, r.center().y - g.size().y / 2.0), g, color);
    if let Some(g) = count_galley {
        ui.painter().galley(pos2(r.right() - 18.0 - g.size().x, r.center().y - g.size().y / 2.0), g, t.text_dim);
    }
    if elided { resp.on_hover_text(label) } else { resp }
}

/// A sidebar section header (Albums, By Date): the bold title with a disclosure triangle after it.
/// The open state lives in egui memory under `id`; returns the header rect and whether it is open.
pub fn sidebar_section(ui: &mut Ui, id: &str, title: &str) -> (Rect, bool) {
    let t = tokens();
    let key = egui::Id::new(("sidebar-section", id));
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let mut open = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(true);
    if resp.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(key, open));
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, open, title));
    let text = ui.painter().text(pos2(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, title, t.semibold(13.5), t.text_label);
    let c = pos2(text.right() + 10.0, r.center().y);
    let col = if resp.hovered() { t.text } else { t.text_dim };
    let pts = if open {
        vec![c + vec2(-4.0, -2.0), c + vec2(4.0, -2.0), c + vec2(0.0, 3.0)]
    } else {
        vec![c + vec2(-2.0, -4.0), c + vec2(3.0, 0.0), c + vec2(-2.0, 4.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    (r, open)
}

/// The two-line message in the middle of an empty canvas or panel.
pub fn empty_message(ui: &Ui, rect: Rect, title: &str, body: &str) {
    let t = tokens();
    let p = ui.painter();
    p.text(rect.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, title, t.semibold(18.0), t.text_label);
    p.text(rect.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, body, t.font(13.0), t.text_dim);
}

/// A rounded "pill" button in the bottom bar (LightCraft's Copy Edit Settings).
pub fn pill_button(ui: &mut Ui, label: &str, enabled: bool, primary: bool) -> Response {
    let t = tokens();
    let font = t.font(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, t.text);
    let (r, resp) = ui.allocate_exact_size(vec2((galley.size().x + 28.0).max(96.0), 30.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let fill = match (primary && enabled, resp.hovered() && enabled) {
        (true, h) => t.accent.gamma_multiply(if h { 1.2 } else { 1.0 }),
        (false, true) => t.hover,
        (false, false) => t.canvas,
    };
    ui.painter().rect_filled(r, 15.0, fill);
    let color = if !enabled {
        t.text_disabled
    } else if primary {
        Color32::WHITE
    } else {
        t.text_label
    };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, t.font(13.0), color);
    resp
}

/// A thin progress bar with a caption above it.
pub fn progress(ui: &mut Ui, fraction: f32, caption: &str) {
    let t = tokens();
    ui.label(egui::RichText::new(caption).size(12.0).color(t.text_label));
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(r, 2.0, t.inset);
    let f = fraction.clamp(0.0, 1.0);
    ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * f, r.height())), 2.0, t.accent);
}

/// A status line: a coloured dot and text.
pub fn status_line(ui: &mut Ui, color: Color32, text: &str) {
    let t = tokens();
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, color);
        ui.label(egui::RichText::new(text).size(12.5).color(t.text_label));
    });
}

/// `text` laid out on one line, cut short with … when wider than `max`.
pub fn one_line(painter: &egui::Painter, text: &str, font: egui::FontId, color: Color32, max: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max.max(1.0));
    painter.layout_job(job)
}
