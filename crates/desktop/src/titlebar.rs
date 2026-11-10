//! The window's own title bar, so the chrome wears the scheme: a strip
//! along the top to drag the window by (twice to maximize it), and on
//! Windows and Linux its minimize, maximize and close buttons and the edges
//! to resize it by. macOS keeps its traffic lights over a transparent bar.

use winit::window::{CursorIcon, ResizeDirection};

use crate::paint::{Painter, Rect, fade};
use crate::theme;
use crate::ui::{Ui, icon};

/// Whether the app draws the window's buttons and edges itself: everywhere
/// but macOS, whose own sit over its transparent title bar.
pub const CUSTOM: bool = !cfg!(target_os = "macos");

/// Width of one window button.
const BUTTON_W: f32 = 46.0;
/// How far in from the window's edge a press resizes it.
const EDGE: f32 = 5.0;

/// What the title bar asks of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Minimize it.
    Minimize,
    /// Maximize it, or restore it when maximized.
    Maximize,
    /// Close the app.
    Close,
}

/// What a press at a point takes hold of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    /// The strip: the window moves with the mouse.
    Move,
    /// An edge or corner: the window resizes that way.
    Resize(ResizeDirection),
}

/// What a press at `(x, y)` in `view` (the window, in logical pixels) takes
/// hold of, if anything; the window buttons are left to [`draw`].
#[must_use]
pub fn grab(view: Rect, (x, y): (f32, f32), maximized: bool) -> Option<Grab> {
    if !CUSTOM || !view.contains((x, y)) {
        return None;
    }
    if !maximized {
        let (west, east) = (x < view.x + EDGE, x >= view.right() - EDGE);
        let (north, south) = (y < view.y + EDGE, y >= view.bottom() - EDGE);
        let direction = match (north, south, west, east) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (true, _, _, true) => Some(ResizeDirection::NorthEast),
            (_, true, true, _) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, ..) => Some(ResizeDirection::North),
            (_, true, ..) => Some(ResizeDirection::South),
            (.., true, _) => Some(ResizeDirection::West),
            (.., true) => Some(ResizeDirection::East),
            _ => None,
        };
        if let Some(direction) = direction {
            return Some(Grab::Resize(direction));
        }
    }
    let strip = y < view.y + theme::TITLE_HEIGHT && x < view.right() - 3.0 * BUTTON_W;
    strip.then_some(Grab::Move)
}

/// Draws the window buttons at the top right of `view` and the resize
/// cursor over an edge. Returns the button clicked.
pub fn draw(p: &mut Painter, ui: &mut Ui, view: Rect, maximized: bool) -> Option<Command> {
    if !CUSTOM {
        return None;
    }
    let t = p.theme;
    if let Some(Grab::Resize(direction)) = grab(view, ui.mouse, maximized) {
        ui.cursor = resize_cursor(direction);
    }
    let mut clicked = None;
    let buttons = [Command::Minimize, Command::Maximize, Command::Close];
    for (i, command) in buttons.into_iter().enumerate() {
        let rect = Rect::new(view.right() - (3 - i) as f32 * BUTTON_W, view.y, BUTTON_W, theme::TITLE_HEIGHT);
        // Over every overlay: the window's buttons always answer.
        let over = rect.contains(ui.mouse) && grab(view, ui.mouse, maximized).is_none();
        let hover = ui.anim(crate::ui::id(("window-button", i)), f32::from(u8::from(over)));
        let ink = if command == Command::Close {
            p.rect(rect, fade(t.danger, hover), 0.0);
            crate::paint::mix(t.text_muted, t.on_scrim, hover)
        } else {
            p.rect(rect, fade(t.hover, hover), 0.0);
            crate::paint::mix(t.text_muted, t.text, hover)
        };
        let (cx, cy) = ((rect.x + rect.w * 0.5).round(), (rect.y + rect.h * 0.5).round());
        match command {
            Command::Minimize => p.rect(Rect::new(cx - 5.0, cy, 10.0, 1.0), ink, 0.0),
            Command::Maximize if maximized => {
                // Restore: a window behind a window.
                p.bordered(Rect::new(cx - 3.0, cy - 5.0, 8.0, 8.0), [0.0; 4], 1.5, 1.0, ink);
                p.rect(Rect::new(cx - 5.0, cy - 3.0, 8.0, 8.0), t.panel, 1.5);
                p.bordered(Rect::new(cx - 5.0, cy - 3.0, 8.0, 8.0), [0.0; 4], 1.5, 1.0, ink);
            }
            Command::Maximize => p.bordered(Rect::new(cx - 5.0, cy - 5.0, 10.0, 10.0), [0.0; 4], 1.5, 1.0, ink),
            Command::Close => icon::draw(p, icon::X, (cx, cy), 16.0, ink),
        }
        if over {
            ui.cursor = CursorIcon::Default;
            if ui.released && rect.contains(ui.press_pos) {
                clicked = Some(command);
            }
        }
    }
    clicked
}

/// The cursor that shows which way an edge resizes.
fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_take_hold_of_edges_then_the_strip() {
        if !CUSTOM {
            return;
        }
        let view = Rect::new(0.0, 0.0, 1000.0, 700.0);
        assert_eq!(grab(view, (2.0, 2.0), false), Some(Grab::Resize(ResizeDirection::NorthWest)));
        assert_eq!(grab(view, (500.0, 2.0), false), Some(Grab::Resize(ResizeDirection::North)));
        assert_eq!(grab(view, (998.0, 400.0), false), Some(Grab::Resize(ResizeDirection::East)));
        assert_eq!(grab(view, (500.0, 699.0), false), Some(Grab::Resize(ResizeDirection::South)));
        assert_eq!(grab(view, (500.0, 20.0), false), Some(Grab::Move));
        assert_eq!(grab(view, (500.0, 2.0), true), Some(Grab::Move), "a maximized window has no edges");
        assert_eq!(grab(view, (980.0, 20.0), false), None, "the buttons answer for themselves");
        assert_eq!(grab(view, (500.0, 300.0), false), None, "the page below");
    }
}
