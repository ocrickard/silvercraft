//! Dragging a frame box over a picture: its edges or corners resize it, its middle moves it,
//! and a drag elsewhere draws a new one. Shared by the live view and the frame viewer's crop.

use egui::{CursorIcon, Pos2, Rect};

/// How close (points) the pointer must be to a frame edge to drag it.
pub const GRIP: f32 = 8.0;

/// What a drag on a picture changes: edges of the frame (`-1` the left/top edge, `1` the
/// right/bottom, `0` neither), all of it (move), or a new frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Grab {
    Edges { x: i8, y: i8 },
    Move,
    New,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameDrag {
    pub grab: Grab,
    /// The frame and the pointer when the drag started, in upright image pixels.
    pub start: [f64; 4],
    pub from: Pos2,
}

/// What dragging at `p` changes on a frame drawn at `sr`.
pub fn grab_for(sr: Rect, p: Pos2) -> Option<Grab> {
    if !sr.expand(GRIP).contains(p) {
        return Some(Grab::New);
    }
    let side = |v: f32, lo: f32, hi: f32| -> i8 {
        if (v - lo).abs() <= GRIP {
            -1
        } else if (v - hi).abs() <= GRIP {
            1
        } else {
            0
        }
    };
    match (side(p.x, sr.left(), sr.right()), side(p.y, sr.top(), sr.bottom())) {
        (0, 0) => Some(Grab::Move),
        (x, y) => Some(Grab::Edges { x, y }),
    }
}

pub fn cursor_for(g: Option<Grab>) -> CursorIcon {
    match g {
        Some(Grab::Edges { x: 0, .. }) => CursorIcon::ResizeVertical,
        Some(Grab::Edges { y: 0, .. }) => CursorIcon::ResizeHorizontal,
        Some(Grab::Edges { x, y }) if x == y => CursorIcon::ResizeNwSe,
        Some(Grab::Edges { .. }) => CursorIcon::ResizeNeSw,
        Some(Grab::Move) => CursorIcon::Move,
        Some(Grab::New) => CursorIcon::Crosshair,
        None => CursorIcon::Default,
    }
}

/// The frame after dragging to `to` (upright image pixels), kept inside the `size` image and
/// at least `min` pixels on a side.
pub fn dragged(d: FrameDrag, to: (f64, f64), size: (f64, f64), min: f64) -> [f64; 4] {
    let (w, h) = size;
    let (tx, ty) = (to.0.clamp(0.0, w), to.1.clamp(0.0, h));
    let (dx, dy) = (to.0 - f64::from(d.from.x), to.1 - f64::from(d.from.y));
    let [x0, y0, x1, y1] = d.start;
    match d.grab {
        Grab::New => {
            let (fx, fy) = (f64::from(d.from.x).clamp(0.0, w), f64::from(d.from.y).clamp(0.0, h));
            [fx.min(tx), fy.min(ty), fx.max(tx), fy.max(ty)]
        }
        Grab::Move => {
            let dx = dx.clamp(-x0, w - x1);
            let dy = dy.clamp(-y0, h - y1);
            [x0 + dx, y0 + dy, x1 + dx, y1 + dy]
        }
        Grab::Edges { x, y } => {
            let mut b = [x0, y0, x1, y1];
            match x {
                -1 => b[0] = (x0 + dx).clamp(0.0, x1 - min),
                1 => b[2] = (x1 + dx).clamp(x0 + min, w),
                _ => {}
            }
            match y {
                -1 => b[1] = (y0 + dy).clamp(0.0, y1 - min),
                1 => b[3] = (y1 + dy).clamp(y0 + min, h),
                _ => {}
            }
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    #[test]
    fn edges_corners_and_middle_grab_what_they_should() {
        let sr = Rect::from_min_max(pos2(100.0, 100.0), pos2(300.0, 200.0));
        assert_eq!(grab_for(sr, pos2(102.0, 150.0)), Some(Grab::Edges { x: -1, y: 0 }));
        assert_eq!(grab_for(sr, pos2(297.0, 203.0)), Some(Grab::Edges { x: 1, y: 1 }));
        assert_eq!(grab_for(sr, pos2(200.0, 99.0)), Some(Grab::Edges { x: 0, y: -1 }));
        assert_eq!(grab_for(sr, pos2(200.0, 150.0)), Some(Grab::Move));
        assert_eq!(grab_for(sr, pos2(20.0, 20.0)), Some(Grab::New));
    }

    #[test]
    fn drags_resize_move_and_stay_on_the_image() {
        let d = |grab| FrameDrag { grab, start: [100.0, 100.0, 300.0, 200.0], from: pos2(300.0, 200.0) };
        let size = (400.0, 300.0);
        assert_eq!(dragged(d(Grab::Edges { x: 1, y: 1 }), (350.0, 260.0), size, 16.0), [100.0, 100.0, 350.0, 260.0]);
        assert_eq!(dragged(d(Grab::Edges { x: 1, y: 0 }), (999.0, 999.0), size, 16.0), [100.0, 100.0, 400.0, 200.0], "not past the image");
        assert_eq!(dragged(d(Grab::Edges { x: 1, y: 0 }), (0.0, 0.0), size, 16.0), [100.0, 100.0, 116.0, 200.0], "never smaller than the minimum");
        assert_eq!(dragged(d(Grab::Move), (400.0, 180.0), size, 16.0), [200.0, 80.0, 400.0, 180.0], "moves until the edge");
        assert_eq!(dragged(d(Grab::New), (250.0, 120.0), size, 16.0), [250.0, 120.0, 300.0, 200.0]);
    }
}
