//! The frame editor on the bed preview: shows the detected frames, lets the user add one (drag
//! on empty film), select (click), move (drag a selected frame), delete (Delete/Backspace) and
//! renumber them.

use egui::{Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2, pos2, vec2};
use silvercraft_epson::{FrameBox, PixelRect};

use crate::theme::tokens;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    New { start: Pos2 },
    Move { index: usize, grab: Pos2, orig: PixelRect },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameEditor {
    pub frames: Vec<FrameBox>,
    pub selected: Option<usize>,
    drag: Option<Drag>,
}

/// Smallest frame a drag creates, in preview pixels.
const MIN_SIDE: f32 = 8.0;

impl FrameEditor {
    pub fn set_frames(&mut self, frames: Vec<FrameBox>) {
        self.frames = frames;
        self.selected = None;
        self.drag = None;
    }

    pub fn delete_selected(&mut self) {
        if let Some(i) = self.selected.take()
            && i < self.frames.len()
        {
            self.frames.remove(i);
        }
    }

    /// Order frames strip by strip (left to right), top to bottom within a strip, and label them
    /// 01, 02, …
    pub fn renumber(&mut self) {
        let mut f = std::mem::take(&mut self.frames);
        // a frame belongs to the strip of the first frame whose column overlaps it by half
        f.sort_by_key(|b| b.bounds.x);
        let mut strips: Vec<Vec<FrameBox>> = Vec::new();
        for b in f {
            let cx = b.bounds.x + b.bounds.width / 2;
            match strips.iter_mut().find(|s| s.first().is_some_and(|h| cx >= h.bounds.x && cx < h.bounds.x + h.bounds.width)) {
                Some(s) => s.push(b),
                None => strips.push(vec![b]),
            }
        }
        for s in &mut strips {
            s.sort_by_key(|b| b.bounds.y);
        }
        self.frames = strips
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, mut b)| {
                b.label = format!("{:02}", i + 1);
                b
            })
            .collect();
        self.selected = None;
    }

    fn next_label(&self) -> String {
        (1..).map(|i| format!("{i:02}")).find(|l| self.frames.iter().all(|f| &f.label != l)).unwrap_or_default()
    }

    fn hit(&self, p: Pos2) -> Option<usize> {
        // topmost (last drawn) first
        self.frames.iter().enumerate().rev().find(|(_, f)| px_rect(f.bounds).contains(p)).map(|(i, _)| i)
    }

    /// Draw `tex` (showing an image of `size` preview pixels) fitted into the available space,
    /// with the frames over it, and handle editing.
    pub fn show(&mut self, ui: &mut egui::Ui, tex: &TextureHandle, size: (u32, u32)) -> egui::Response {
        let t = tokens();
        let avail = ui.available_size();
        let (iw, ih) = (size.0.max(1) as f32, size.1.max(1) as f32);
        let scale = (avail.x / iw).min(avail.y / ih).max(0.01);
        let (rect, resp) = ui.allocate_exact_size(vec2(iw * scale, ih * scale), Sense::click_and_drag());
        let to_screen = |p: Pos2| rect.min + (p.to_vec2() * scale);
        let to_image = |p: Pos2| ((p - rect.min) / scale).to_pos2();
        let painter = ui.painter_at(rect);
        painter.image(tex.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), egui::Color32::WHITE);

        // interaction
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos().map(to_image)
        {
            self.drag = match self.hit(p) {
                Some(i) => {
                    self.selected = Some(i);
                    Some(Drag::Move { index: i, grab: p, orig: self.frames[i].bounds })
                }
                None => {
                    self.selected = None;
                    Some(Drag::New { start: p })
                }
            };
        }
        let current = resp.interact_pointer_pos().map(to_image);
        if let (Some(Drag::Move { index, grab, orig }), Some(p)) = (self.drag, current)
            && resp.dragged()
            && let Some(f) = self.frames.get_mut(index)
        {
            let d: Vec2 = p - grab;
            let nx = (orig.x as f32 + d.x).clamp(0.0, (iw - orig.width as f32).max(0.0));
            let ny = (orig.y as f32 + d.y).clamp(0.0, (ih - orig.height as f32).max(0.0));
            f.bounds.x = nx.round() as u32;
            f.bounds.y = ny.round() as u32;
        }
        if resp.drag_stopped() {
            if let (Some(Drag::New { start }), Some(p)) = (self.drag, current) {
                let r = Rect::from_two_pos(start, p).intersect(Rect::from_min_size(Pos2::ZERO, vec2(iw, ih)));
                if r.width() >= MIN_SIDE && r.height() >= MIN_SIDE {
                    let label = self.next_label();
                    self.frames.push(FrameBox {
                        bounds: PixelRect { x: r.min.x as u32, y: r.min.y as u32, width: r.width() as u32, height: r.height() as u32 },
                        label,
                    });
                    self.selected = Some(self.frames.len() - 1);
                }
            }
            self.drag = None;
        }
        if resp.clicked()
            && let Some(p) = current
        {
            self.selected = self.hit(p);
        }
        if resp.hovered() && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            self.delete_selected();
        }

        // drawing
        for (i, f) in self.frames.iter().enumerate() {
            let r = px_rect(f.bounds);
            let sr = Rect::from_min_max(to_screen(r.min), to_screen(r.max));
            let sel = self.selected == Some(i);
            let color = if sel { t.frame_selected } else { t.frame_box };
            painter.rect_stroke(sr, 2.0, Stroke::new(if sel { 2.5 } else { 1.5 }, color), StrokeKind::Middle);
            painter.text(sr.min + vec2(4.0, 3.0), egui::Align2::LEFT_TOP, &f.label, egui::FontId::proportional(12.0), color);
        }
        if let (Some(Drag::New { start }), Some(p)) = (self.drag, current) {
            let sr = Rect::from_two_pos(to_screen(start), to_screen(p));
            painter.rect_stroke(sr, 0.0, Stroke::new(1.0, t.text), StrokeKind::Middle);
        }
        resp
    }
}

fn px_rect(b: PixelRect) -> Rect {
    Rect::from_min_size(pos2(b.x as f32, b.y as f32), vec2(b.width as f32, b.height as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(x: u32, y: u32, label: &str) -> FrameBox {
        FrameBox { bounds: PixelRect { x, y, width: 100, height: 60 }, label: label.into() }
    }

    #[test]
    fn renumber_goes_strip_by_strip_top_to_bottom() {
        let mut e = FrameEditor::default();
        e.set_frames(vec![fb(300, 10, "a"), fb(10, 200, "b"), fb(12, 10, "c"), fb(305, 120, "d")]);
        e.renumber();
        let order: Vec<(u32, u32, &str)> = e.frames.iter().map(|f| (f.bounds.x, f.bounds.y, f.label.as_str())).collect();
        assert_eq!(order, [(12, 10, "01"), (10, 200, "02"), (300, 10, "03"), (305, 120, "04")]);
    }

    #[test]
    fn delete_and_next_label() {
        let mut e = FrameEditor::default();
        e.set_frames(vec![fb(0, 0, "01"), fb(0, 100, "02")]);
        e.selected = Some(0);
        e.delete_selected();
        assert_eq!(e.frames.len(), 1);
        assert_eq!(e.next_label(), "01");
        e.selected = Some(7);
        e.delete_selected(); // out of range: nothing happens
        assert_eq!(e.frames.len(), 1);
    }
}
