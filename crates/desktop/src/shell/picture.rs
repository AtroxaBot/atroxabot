//! A picture of the conversation, opened whole over the screen: as large as
//! fits and never larger than it is. A click beside it, the cross or Esc
//! puts it away.

use winit::window::CursorIcon;

use crate::image::Lookup;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{Ui, icon, id};

/// Space kept free around the picture.
const MARGIN: f32 = 56.0;

/// Draws the picture file at `path` over `view`. Returns whether it was closed.
pub(super) fn draw(p: &mut Painter, ui: &mut Ui, view: Rect, path: &str) -> bool {
    let t = p.theme;
    let appear = ui.appear(id(("picture", path)));
    p.rect(view, fade(t.scrim, appear), 0.0);
    // Under the title bar, whose buttons stay the window's.
    let top = view.y + theme::TITLE_HEIGHT;
    let room = Rect::new(view.x + MARGIN, top + MARGIN, view.w - 2.0 * MARGIN, view.bottom() - top - 2.0 * MARGIN);
    let shown = match p.picture(path, room, theme::RADIUS, t.bg) {
        Ok(shown) => Some(shown),
        Err(lookup) => {
            let note = if lookup == Lookup::Failed { "This picture cannot be shown." } else { "Opening…" };
            p.label_centered(note, theme::SMALL, room, t.on_scrim);
            None
        }
    };
    // The cross sits on a disc of its own, so it reads over whatever the scrim dims.
    let cross = Rect::new(view.right() - 48.0, top + 10.0, 32.0, 32.0);
    let over = ui.hovered(cross);
    let hover = ui.anim(id("picture-close"), f32::from(u8::from(over)));
    p.bordered(cross, mix(t.surface, t.hover, hover), 16.0, 1.0, t.border_strong);
    icon::draw(p, icon::X, (cross.x + 16.0, cross.y + 16.0), 15.0, mix(t.text_muted, t.text, hover));
    if over {
        ui.cursor = CursorIcon::Pointer;
    }
    // A click that began and ended beside the picture puts it away, as the cross does.
    let on_picture = |at: (f32, f32)| shown.is_some_and(|shown| shown.contains(at));
    ui.released && !on_picture(ui.mouse) && !on_picture(ui.press_pos)
}
