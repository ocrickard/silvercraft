//! The integrated title bar's window gestures, as in LightCraft: on macOS the native title bar is
//! hidden and the top bar takes its place, so dragging its empty space moves the window and a
//! double-click zooms or restores it.
//!
//! [`command_for`] is the pure policy; [`window_handle`] wires it to the top bar.
//!
//! On Linux the app draws the whole title bar itself (GNOME's X11 frames follow the desktop's
//! light/dark setting, not the app's): no system decorations, the window buttons in the top bar
//! where the desktop's `button-layout` puts them ([`Controls`]), and resize grips along the
//! window's edges ([`resize_edges`]).

use egui::{Color32, CursorIcon, Rect, ResizeDirection, Sense, Stroke, ViewportCommand, pos2, vec2};

use crate::theme::tokens;

/// What the user did on the bar's empty space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// A primary-button drag began: the window follows the pointer.
    DragStarted,
    /// A primary-button double-click: zoom the window, or restore it when already zoomed.
    DoubleClicked,
}

/// The window as the host reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowState {
    /// Zoomed (macOS) / maximized (elsewhere).
    pub maximized: bool,
    /// In the OS's full-screen mode.
    pub fullscreen: bool,
}

/// The viewport command for `gesture`, if any. Full screen has no title bar to drag or zoom.
pub fn command_for(gesture: Gesture, window: WindowState) -> Option<ViewportCommand> {
    if window.fullscreen {
        return None;
    }
    match gesture {
        Gesture::DragStarted => Some(ViewportCommand::StartDrag),
        Gesture::DoubleClicked => Some(ViewportCommand::Maximized(!window.maximized)),
    }
}

/// Make the top bar's empty space (`content` plus its margins) the window's title bar. Called
/// before the bar's widgets so they win their clicks.
pub fn window_handle(ui: &mut egui::Ui, content: Rect, margin_left: f32, margin_right: f32, fullscreen: bool) {
    let bar = Rect::from_min_max(pos2(content.left() - margin_left, content.top()), pos2(content.right() + margin_right, content.bottom()));
    let handle = egui::Id::new("titlebar-handle");
    let resp = ui.interact(bar, handle, Sense::click_and_drag());
    // a press that began on a button belongs to it: dragging off it must not move the window
    let ctx = ui.ctx().clone();
    let memo = egui::Id::new("titlebar-press-on-widget");
    if resp.is_pointer_button_down_on() && ctx.input(|i| i.pointer.primary_pressed()) {
        let hovered = ctx.interaction_snapshot(|s| s.hovered.clone());
        let on_widget =
            hovered.iter().any(|id| *id != handle && ctx.read_response(*id).is_some_and(|r| r.sense.senses_click() || r.sense.senses_drag()));
        ctx.data_mut(|d| d.insert_temp(memo, on_widget));
    }
    let on_widget = ctx.data(|d| d.get_temp::<bool>(memo)).unwrap_or(false);
    let gesture = if resp.double_clicked_by(egui::PointerButton::Primary) {
        Some(Gesture::DoubleClicked)
    } else if resp.drag_started_by(egui::PointerButton::Primary) && !on_widget {
        Some(Gesture::DragStarted)
    } else {
        None
    };
    if let Some(g) = gesture {
        let window = WindowState { maximized: ui.input(|i| i.viewport().maximized).unwrap_or(false), fullscreen };
        if let Some(cmd) = command_for(g, window) {
            if cmd == ViewportCommand::StartDrag {
                handed_to_window_manager(ui.ctx());
            }
            ui.ctx().send_viewport_cmd(cmd);
        }
    }
}

/// A window button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Close,
    Minimize,
    Maximize,
}

/// Which window buttons sit at each end of the top bar, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controls {
    pub left: Vec<Control>,
    pub right: Vec<Control>,
}

impl Default for Controls {
    /// GNOME's default layout.
    fn default() -> Self {
        Controls { left: vec![], right: vec![Control::Minimize, Control::Maximize, Control::Close] }
    }
}

impl Controls {
    /// Parse a GNOME `button-layout` (`"close,minimize,maximize:"`, `"appmenu:minimize,maximize,close"`;
    /// `gsettings` quotes it). Unknown names (appmenu, spacer, icon) are skipped; a layout with
    /// no close button gets one at the right, so the window can always be closed.
    pub fn parse(layout: &str) -> Controls {
        let layout = layout.trim().trim_matches('\'').trim_matches('"');
        let side = |s: &str| -> Vec<Control> {
            let mut out = Vec::new();
            for name in s.split(',') {
                let c = match name.trim() {
                    "close" => Control::Close,
                    "minimize" => Control::Minimize,
                    "maximize" => Control::Maximize,
                    _ => continue,
                };
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            out
        };
        let (l, r) = layout.split_once(':').unwrap_or(("", layout));
        let mut c = Controls { left: side(l), right: side(r) };
        c.right.retain(|x| !c.left.contains(x));
        if !c.left.contains(&Control::Close) && !c.right.contains(&Control::Close) {
            c.right.push(Control::Close);
        }
        c
    }
}

/// Width one button takes in the top bar.
pub const CONTROL_W: f32 = 30.0;

/// Draw `controls` left to right from `x`, centred on `y`, and act on clicks.
pub fn draw_controls(ui: &mut egui::Ui, controls: &[Control], x: f32, y: f32, maximized: bool) {
    let t = tokens();
    for (i, c) in controls.iter().enumerate() {
        let r = Rect::from_center_size(pos2(x + CONTROL_W * (i as f32 + 0.5), y), vec2(24.0, 24.0));
        let resp = ui.interact(r, egui::Id::new(("window-control", *c as u8)), Sense::click());
        let fill = if resp.is_pointer_button_down_on() {
            Color32::from_gray(80)
        } else if resp.hovered() {
            Color32::from_gray(68)
        } else {
            Color32::from_gray(56)
        };
        let p = ui.painter();
        p.circle_filled(r.center(), 12.0, fill);
        let stroke = Stroke::new(1.3, if resp.hovered() { t.text } else { t.text_label });
        let m = r.center();
        match c {
            Control::Close => {
                p.line_segment([m + vec2(-4.0, -4.0), m + vec2(4.0, 4.0)], stroke);
                p.line_segment([m + vec2(4.0, -4.0), m + vec2(-4.0, 4.0)], stroke);
            }
            Control::Minimize => {
                p.line_segment([m + vec2(-4.5, 3.5), m + vec2(4.5, 3.5)], stroke);
            }
            Control::Maximize if maximized => {
                p.rect_stroke(Rect::from_center_size(m + vec2(-1.0, 1.0), vec2(7.0, 7.0)), 1.0, stroke, egui::StrokeKind::Middle);
                p.line_segment([m + vec2(-1.5, -4.5), m + vec2(4.5, -4.5)], stroke);
                p.line_segment([m + vec2(4.5, -4.5), m + vec2(4.5, 1.5)], stroke);
            }
            Control::Maximize => {
                p.rect_stroke(Rect::from_center_size(m, vec2(9.0, 9.0)), 1.0, stroke, egui::StrokeKind::Middle);
            }
        }
        let tip = match c {
            Control::Close => "Close",
            Control::Minimize => "Minimize",
            Control::Maximize if maximized => "Restore",
            Control::Maximize => "Maximize",
        };
        if resp.on_hover_text(tip).clicked() {
            ui.ctx().send_viewport_cmd(match c {
                Control::Close => ViewportCommand::Close,
                Control::Minimize => ViewportCommand::Minimized(true),
                Control::Maximize => ViewportCommand::Maximized(!maximized),
            });
        }
    }
}

/// The resize direction for a pointer at `p` in a window `screen`, within `grip` of an edge.
pub fn edge_at(screen: Rect, p: egui::Pos2, grip: f32) -> Option<ResizeDirection> {
    if !screen.contains(p) {
        return None;
    }
    let corner = grip * 3.0;
    let (l, r) = (p.x - screen.left() < grip, screen.right() - p.x < grip);
    let (t, b) = (p.y - screen.top() < grip, screen.bottom() - p.y < grip);
    // corners are generous: a few pixels along either edge near the corner
    let (lc, rc) = (p.x - screen.left() < corner, screen.right() - p.x < corner);
    let (tc, bc) = (p.y - screen.top() < corner, screen.bottom() - p.y < corner);
    Some(match () {
        _ if (t && lc) || (l && tc) => ResizeDirection::NorthWest,
        _ if (t && rc) || (r && tc) => ResizeDirection::NorthEast,
        _ if (b && lc) || (l && bc) => ResizeDirection::SouthWest,
        _ if (b && rc) || (r && bc) => ResizeDirection::SouthEast,
        _ if t => ResizeDirection::North,
        _ if b => ResizeDirection::South,
        _ if l => ResizeDirection::West,
        _ if r => ResizeDirection::East,
        _ => return None,
    })
}

/// Resize grips along the window's edges for an undecorated window. Called after the frame's
/// widgets; a press on a grip starts the window manager's resize.
pub fn resize_edges(ctx: &egui::Context, fullscreen: bool) {
    let (maximized, screen, pos, pressed) =
        ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport_rect(), i.pointer.hover_pos(), i.pointer.primary_pressed()));
    if maximized || fullscreen {
        return;
    }
    let Some(dir) = pos.and_then(|p| edge_at(screen, p, 5.0)) else { return };
    ctx.set_cursor_icon(match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
    });
    if pressed {
        handed_to_window_manager(ctx);
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

const HANDED_OVER: &str = "titlebar-handed-to-window-manager";

/// The window manager takes the pointer for a move or resize and keeps the button's release, so
/// egui would think the button still held and ignore the next press on the bar. Remember where
/// it was taken; [`release_after_handover`] supplies the release.
fn handed_to_window_manager(ctx: &egui::Context) {
    if let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::new(HANDED_OVER), p));
    }
}

/// Before each frame's input reaches egui (eframe's `raw_input_hook`): after a move or resize,
/// the button release the window manager kept.
pub fn release_after_handover(ctx: &egui::Context, raw: &mut egui::RawInput) {
    let id = egui::Id::new(HANDED_OVER);
    let Some(pos) = ctx.data_mut(|d| d.remove_temp::<egui::Pos2>(id)) else { return };
    let release = egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() };
    raw.events.insert(0, release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_layouts_from_gnome() {
        use Control::*;
        assert_eq!(Controls::parse("'close,minimize,maximize:'"), Controls { left: vec![Close, Minimize, Maximize], right: vec![] });
        assert_eq!(Controls::parse("'appmenu:minimize,maximize,close'"), Controls { left: vec![], right: vec![Minimize, Maximize, Close] });
        assert_eq!(Controls::parse("appmenu:close"), Controls { left: vec![], right: vec![Close] });
        assert_eq!(Controls::parse(":"), Controls { left: vec![], right: vec![Close] }, "always a way to close");
        assert_eq!(Controls::parse("close,close,bogus:close"), Controls { left: vec![Close], right: vec![] }, "each button once");
        assert_eq!(Controls::parse(""), Controls { left: vec![], right: vec![Close] });
    }

    #[test]
    fn resize_grips_on_edges_and_corners() {
        let s = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        assert_eq!(edge_at(s, pos2(400.0, 300.0), 5.0), None);
        assert_eq!(edge_at(s, pos2(400.0, 2.0), 5.0), Some(ResizeDirection::North));
        assert_eq!(edge_at(s, pos2(798.0, 300.0), 5.0), Some(ResizeDirection::East));
        assert_eq!(edge_at(s, pos2(400.0, 599.0), 5.0), Some(ResizeDirection::South));
        assert_eq!(edge_at(s, pos2(1.0, 300.0), 5.0), Some(ResizeDirection::West));
        assert_eq!(edge_at(s, pos2(2.0, 10.0), 5.0), Some(ResizeDirection::NorthWest));
        assert_eq!(edge_at(s, pos2(790.0, 598.0), 5.0), Some(ResizeDirection::SouthEast));
        assert_eq!(edge_at(s, pos2(900.0, 300.0), 5.0), None, "outside the window");
    }

    const NORMAL: WindowState = WindowState { maximized: false, fullscreen: false };
    const ZOOMED: WindowState = WindowState { maximized: true, fullscreen: false };
    const FULL: WindowState = WindowState { maximized: false, fullscreen: true };

    #[test]
    fn drag_moves_and_double_click_zooms_or_restores() {
        assert_eq!(command_for(Gesture::DragStarted, NORMAL), Some(ViewportCommand::StartDrag));
        assert_eq!(command_for(Gesture::DragStarted, ZOOMED), Some(ViewportCommand::StartDrag));
        assert_eq!(command_for(Gesture::DoubleClicked, NORMAL), Some(ViewportCommand::Maximized(true)));
        assert_eq!(command_for(Gesture::DoubleClicked, ZOOMED), Some(ViewportCommand::Maximized(false)));
    }

    #[test]
    fn full_screen_ignores_both() {
        assert_eq!(command_for(Gesture::DragStarted, FULL), None);
        assert_eq!(command_for(Gesture::DoubleClicked, FULL), None);
    }
}
