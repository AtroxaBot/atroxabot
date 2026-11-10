//! Text fields for forms: a group of editors sharing focus, drawn as
//! bordered boxes, with mouse placement and selection and the usual keys.
//!
//! Enter in a single-line field moves to the next field; in a multi-line
//! one it starts a new line, unless its editor takes no line breaks (a list
//! that wraps, like tags). Tab cycles through the fields.

use arboard::Clipboard;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::editor::Editor;
use crate::paint::{Painter, Rect, mix};
use crate::text::TextLayout;
use crate::theme;
use crate::ui::{Ui, edit_key, id, move_line};

/// Space between a field's border and its text.
pub const FIELD_PAD: (f32, f32) = (12.0, 10.0);

/// Text fields, one of them focused.
pub struct Fields {
    editors: Vec<Editor>,
    multiline: Vec<bool>,
    focus: usize,
    /// The field a mouse drag is selecting in.
    selecting: Option<usize>,
    /// Each field's layout and text origin from the last frame, for the
    /// arrow keys.
    layouts: Vec<Option<(TextLayout, (f32, f32))>>,
    /// The focused field's caret, for placing the input method's window.
    caret: Option<Rect>,
    /// The field whose text is not shown (an API key), if any.
    secret: Option<usize>,
}

impl Fields {
    /// Fields holding `editors`, the first focused; `multiline` says which
    /// take several lines.
    #[must_use]
    pub fn new<const N: usize>(editors: [Editor; N], multiline: [bool; N]) -> Self {
        Self {
            editors: editors.into(),
            multiline: multiline.into(),
            focus: 0,
            selecting: None,
            layouts: vec![None; N],
            caret: None,
            secret: None,
        }
    }

    /// Hides what is typed into field `index`: it shows as stars.
    #[must_use]
    pub fn secret(mut self, index: usize) -> Self {
        self.secret = Some(index);
        self
    }

    /// The text of field `index`.
    #[must_use]
    pub fn text(&self, index: usize) -> &str {
        self.editors[index].text()
    }

    /// Replaces the text of field `index`.
    pub fn replace(&mut self, index: usize, text: &str) {
        let mut editor = Editor::default();
        editor.insert(text);
        self.editors[index] = editor;
    }

    /// The index of the field that has the keyboard.
    #[must_use]
    pub fn focused(&self) -> usize {
        self.focus
    }

    /// Gives field `index` the keyboard.
    pub fn focus(&mut self, index: usize) {
        self.focus = index.min(self.editors.len() - 1);
    }

    /// Text committed by an input method goes to the focused field.
    pub fn insert(&mut self, text: &str) {
        self.editors[self.focus].insert(text);
    }

    /// The focused field's caret, for the input method's window.
    #[must_use]
    pub fn caret(&self) -> Option<Rect> {
        self.caret
    }

    /// Handles an editing key. Returns `false` for keys fields do not use
    /// (Escape, shortcuts), so the caller may.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> bool {
        let focus = self.focus;
        match &event.logical_key {
            Key::Named(NamedKey::Tab) => {
                let n = self.editors.len();
                self.focus = if mods.shift_key() { (focus + n - 1) % n } else { (focus + 1) % n };
                true
            }
            Key::Named(NamedKey::Enter)
                if self.multiline[focus]
                    && self.editors[focus].accepts('\n')
                    && !mods.control_key()
                    && !mods.super_key() =>
            {
                self.editors[focus].insert("\n");
                true
            }
            Key::Named(NamedKey::Enter)
                if focus + 1 < self.editors.len() && !mods.control_key() && !mods.super_key() =>
            {
                self.focus += 1;
                true
            }
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) if self.multiline[focus] => {
                if let Some((layout, _)) = &self.layouts[focus] {
                    move_line(&mut self.editors[focus], layout, *key == NamedKey::ArrowUp, mods.shift_key());
                }
                true
            }
            _ => edit_key(&mut self.editors[focus], event, mods, cb),
        }
    }

    /// Lays out field `index` for a box `width` wide: wrapped when it is
    /// multi-line.
    #[must_use]
    pub fn layout(&self, p: &Painter, index: usize, width: f32) -> TextLayout {
        let wrap = self.multiline[index].then_some(width - 2.0 * FIELD_PAD.0);
        let text = self.editors[index].text();
        if self.secret == Some(index) {
            // A star per byte, so the caret and the mouse land where they would in the text itself.
            return p.layout(&"*".repeat(text.len()), theme::BODY, wrap);
        }
        p.layout(text, theme::BODY, wrap)
    }

    /// Draws field `index` as a text box in `rect` showing `layout`, and
    /// lets the mouse focus it, place the caret and select. `interactive`
    /// is false where the pointer is outside the visible area.
    #[allow(clippy::too_many_arguments, reason = "field geometry and content; a struct would only rename them")]
    pub fn draw(
        &mut self,
        index: usize,
        p: &mut Painter,
        ui: &mut Ui,
        rect: Rect,
        layout: TextLayout,
        placeholder: &str,
        interactive: bool,
    ) {
        let t = p.theme;
        // A single line longer than its box slides, so the caret stays in view.
        let inner = rect.w - 2.0 * FIELD_PAD.0;
        let slide =
            if self.multiline[index] { 0.0 } else { (layout.caret(self.editors[index].cursor()).0 - inner).max(0.0) };
        let origin = (rect.x + FIELD_PAD.0 - slide, rect.y + FIELD_PAD.1);
        let byte_at = |ui: &Ui| layout.hit(ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
        let hovered = interactive && ui.hovered(rect);
        if hovered {
            ui.cursor = CursorIcon::Text;
            if ui.pressed {
                self.focus = index;
                self.selecting = Some(index);
                let byte = byte_at(ui);
                let editor = &mut self.editors[index];
                editor.set_cursor(byte, ui.mods.shift_key());
                if ui.clicks == 2 {
                    editor.select_word();
                }
                ui.last_edit = ui.time;
            }
        }
        if self.selecting == Some(index) {
            if ui.down && ui.clicks < 2 {
                let byte = byte_at(ui);
                self.editors[index].set_cursor(byte, true);
            } else if !ui.down {
                self.selecting = None;
            }
        }

        let focused = self.focus == index;
        let focus = ui.anim(id(("field", rect.x.to_bits(), index)), f32::from(u8::from(focused)));
        p.bordered(rect, t.bg, theme::RADIUS_SM, 1.0, mix(t.border_strong, t.border_focus, focus));
        let clip = p.push_clip(Rect::new(rect.x + 6.0, rect.y + 1.0, rect.w - 12.0, rect.h - 2.0));
        let editor = &self.editors[index];
        let selection = editor.selection();
        let line_h = layout.line_height();
        if focused && !selection.is_empty() {
            // A selected line break shows as a small tail.
            for (x0, x1, y) in layout.selection_spans(selection.start, selection.end, false) {
                p.rect(Rect::new(origin.0 + x0, origin.1 + y, x1 - x0, line_h), t.selection, 2.0);
            }
        }
        if editor.text().is_empty() {
            let mut hint = p.layout(placeholder, theme::BODY, self.multiline[index].then_some(inner));
            if !self.multiline[index] {
                hint.truncate(p.fonts, inner);
            }
            p.text(&hint, origin.0, origin.1, t.text_faint);
        } else {
            p.text(&layout, origin.0, origin.1, t.text);
        }
        if focused {
            let (x, y) = layout.caret(editor.cursor());
            self.caret = Some(Rect::new(origin.0 + x, origin.1 + y, 2.0, line_h));
            if ui.caret_visible() && selection.is_empty() {
                p.rect(Rect::new(origin.0 + x - 1.0, origin.1 + y + 3.0, 2.0, line_h - 6.0), t.accent, 0.0);
            }
        }
        p.set_clip(clip);
        self.layouts[index] = Some((layout, origin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_moves_and_text_lands_in_the_focused_field() {
        let mut fields = Fields::new([Editor::default(), Editor::default()], [false, true]);
        fields.insert("Gale");
        fields.focus = 1;
        fields.insert("A hunter.");
        assert_eq!((fields.text(0), fields.text(1)), ("Gale", "A hunter."));
        assert!(fields.caret().is_none(), "no caret before the first draw");
    }
}
