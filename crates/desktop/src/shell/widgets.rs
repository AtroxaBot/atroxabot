//! The pieces the shell's surfaces are built from: smooth scrolling, the
//! modal frame, and the controls of forms (labels, dropdown buttons,
//! switches, option cards and provider tiles).

use winit::window::CursorIcon;

use crate::editor::Editor;
use crate::logo::Logo;
use crate::paint::{Color, Painter, Rect, fade, mix};
use crate::text::Style;
use crate::theme;
use crate::ui::{Ui, icon, id};

/// How fast a view catches up with where the wheel sent it.
const EASE: f32 = 22.0;

/// A scrolled view: where it is, where it is going, and how far it can go.
///
/// The wheel moves the target, never past the ends known from the last
/// frame, and the position eases after it: nothing is ever drawn beyond the
/// content and snapped back.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Scroll {
    pos: f32,
    target: f32,
    max: f32,
}

impl Scroll {
    /// Takes the wheel (when `hovered`) and returns the offset to draw at.
    pub fn update(&mut self, ui: &mut Ui, hovered: bool) -> f32 {
        if hovered && ui.scroll != 0.0 {
            self.target = (self.target + ui.scroll).clamp(0.0, self.max);
        }
        let gap = self.target - self.pos;
        if gap.abs() < 0.5 {
            self.pos = self.target;
        } else {
            self.pos += gap * (1.0 - (-ui.dt * EASE).exp());
            ui.animating = true;
        }
        self.pos.round()
    }

    /// Tells it how tall the content turned out to be in a view `view` tall.
    pub fn finish(&mut self, ui: &mut Ui, content: f32, view: f32) {
        self.max = (content - view).max(0.0);
        if self.target > self.max {
            self.target = self.max;
        }
        if self.pos > self.max {
            // Content shrank under the view: draw again where it ends.
            self.pos = self.max;
            ui.animating = true;
        }
    }

    /// Whether the view is headed for its end.
    pub fn at_end(&self) -> bool {
        self.target >= self.max - 1.0
    }

    /// Moves where the view is headed by `by`, never past its ends.
    pub fn nudge(&mut self, by: f32) {
        self.target = (self.target + by).clamp(0.0, self.max);
    }

    /// Sends the view to its end, as far as is known.
    pub fn follow_end(&mut self) {
        self.target = self.max;
    }

    /// Puts the view at its end at once, without easing there.
    pub fn jump_to_end(&mut self) {
        (self.pos, self.target) = (self.max, self.max);
    }

    /// Whether there is anything to scroll, and how far along the view is (`0..=1`).
    pub fn progress(&self) -> Option<f32> {
        (self.max > 0.0).then(|| (self.pos / self.max).clamp(0.0, 1.0))
    }

    /// Heads for where the rows from `top` to `bottom` (in content
    /// coordinates) are inside a view `view` tall.
    pub fn reveal(&mut self, top: f32, bottom: f32, view: f32) {
        // Not `clamp`, which panics when the rows are taller than the view: their top then wins.
        self.target = self.target.max((bottom - view).max(0.0)).min(top.max(0.0));
    }

    /// Back to the top, at once.
    pub fn reset(&mut self) {
        (self.pos, self.target) = (0.0, 0.0);
    }

    /// A thin bar at the right edge of `area` showing where the view is.
    pub fn bar(&self, p: &mut Painter, area: Rect, content: f32) {
        let Some(progress) = self.progress() else { return };
        let track = area.h - 16.0;
        let thumb = thumb(track, area.h, content);
        let at = area.y + 8.0 + (track - thumb) * progress;
        p.rect(Rect::new(area.right() - 7.0, at, 3.0, thumb), fade(p.theme.text_faint, 0.35), 1.5);
    }
}

/// How long a scroll bar's thumb is on a `track`, for a view `view` long
/// over `content`: 28 at least where the track has that, and nothing where a
/// window too small leaves the view no room (`clamp` would panic there).
pub(super) fn thumb(track: f32, view: f32, content: f32) -> f32 {
    (track * view / content.max(1.0)).max(28.0).min(track.max(0.0))
}

/// Dims `view` and draws a centred card of at most `size` over it: a modal.
/// Returns the card and whether a click landed outside it.
pub(super) fn modal(p: &mut Painter, ui: &mut Ui, view: Rect, size: (f32, f32), key: &str) -> (Rect, bool) {
    let t = p.theme;
    let appear = ui.appear(id(("modal", key)));
    p.rect(view, fade(t.scrim, appear), 0.0);
    let (w, h) = (size.0.min(view.w - 48.0), size.1.min(view.h - 64.0));
    let card = Rect::new(
        (view.x + (view.w - w) * 0.5).round(),
        (view.y + (view.h - h) * 0.5 + 10.0 * (1.0 - appear)).round(),
        w,
        h,
    );
    p.shadow(Rect::new(card.x, card.y + 16.0, card.w, card.h), fade(t.shadow, appear), theme::RADIUS_LG, 48.0);
    p.bordered(card, t.bg, theme::RADIUS_LG, 1.0, t.border_strong);
    let outside = ui.released && !card.contains(ui.mouse) && !card.contains(ui.press_pos);
    (card, outside)
}

/// One line of editable text in `rect`: the `placeholder` while it is
/// empty, a caret while `focused`. Text longer than the box slides so the
/// caret stays in view. A press in it places the caret (with Shift, or
/// dragged on, it marks text; twice, a word); giving it the keyboard is
/// the caller's.
pub(super) fn line_field(
    p: &mut Painter,
    ui: &mut Ui,
    rect: Rect,
    editor: &mut Editor,
    (placeholder, style): (&str, Style),
    focused: bool,
) {
    let t = p.theme;
    let mid = rect.y + rect.h * 0.5;
    if editor.text().is_empty() {
        let hint = p.layout(placeholder, style, None);
        let clip = p.push_clip(rect);
        p.text(&hint, rect.x, mid - hint.height() * 0.5, t.text_faint);
        p.set_clip(clip);
    }
    let layout = p.layout(editor.text(), style, None);
    let slide = |editor: &Editor| (layout.caret(editor.cursor()).0 - rect.w + 2.0).max(0.0);
    // Where the mouse is in the text as it was last drawn.
    let under = layout.hit(ui.mouse.0 - rect.x + slide(editor), 0.0);
    if ui.pressed && ui.hovered(rect) {
        editor.set_cursor(under, ui.mods.shift_key());
        if ui.clicks == 2 {
            editor.select_word();
        }
        ui.last_edit = ui.time;
    } else if ui.down && ui.clicks < 2 && rect.contains(ui.press_pos) && ui.mouse != ui.press_pos {
        editor.set_cursor(under, true);
    }
    let caret = layout.caret(editor.cursor()).0;
    let slide = slide(editor);
    let clip = p.push_clip(rect);
    let x = rect.x - slide;
    let selection = editor.selection();
    if focused && !selection.is_empty() {
        let (from, to) = (layout.caret(selection.start).0, layout.caret(selection.end).0);
        p.rect(Rect::new(x + from, mid - 9.0, to - from, 18.0), t.selection, 2.0);
    }
    p.text(&layout, x, mid - layout.height() * 0.5, t.text);
    if focused && ui.caret_visible() {
        p.rect(Rect::new(x + caret, mid - 8.0, 1.5, 16.0), t.accent, 0.0);
    }
    p.set_clip(clip);
}

/// A small caption over a control, with an icon before it.
pub(super) fn field_label(p: &mut Painter, glyph: char, text: &str, x: f32, y: f32) {
    let t = p.theme;
    icon::draw(p, glyph, (x + 7.0, y + 8.0), 13.0, t.text_faint);
    p.label(text, theme::CAPTION, x + 20.0, y, t.text_muted);
}

/// A button that shows a choice and opens a list: `label` with an optional
/// icon before it and a chevron after. Returns whether it was clicked.
pub(super) fn dropdown(p: &mut Painter, ui: &mut Ui, rect: Rect, glyph: Option<char>, label: &str, open: bool) -> bool {
    let clicked = dropdown_frame(p, ui, rect, label, open, glyph.is_some());
    if let Some(glyph) = glyph {
        icon::draw(p, glyph, (rect.x + 19.0, rect.y + rect.h * 0.5), 15.0, p.theme.text_muted);
    }
    clicked
}

/// A [`dropdown`] without its icon: with `room`, space is left before the
/// label for a mark the caller draws there.
pub(super) fn dropdown_frame(p: &mut Painter, ui: &mut Ui, rect: Rect, label: &str, open: bool, room: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let hover = ui.anim(id(("dropdown", rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(hovered || open)));
    p.bordered(
        rect,
        mix(t.bg, t.hover, hover * 0.6),
        theme::RADIUS_SM,
        1.0,
        mix(t.border_strong, t.border_focus, hover),
    );
    let mid = rect.y + rect.h * 0.5;
    let x = rect.x + if room { 36.0 } else { 12.0 };
    let mut text = p.layout(label, theme::BODY, None);
    text.truncate(p.fonts, rect.right() - 30.0 - x);
    p.text(&text, x, mid - text.height() * 0.5, t.text);
    icon::draw(p, icon::CHEVRON_DOWN, (rect.right() - 17.0, mid), 14.0, t.text_faint);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(rect)
}

/// A switch, `on` or off, in a 38 by 22 box at `(x, y)`. Returns whether it was clicked.
pub(super) fn toggle(p: &mut Painter, ui: &mut Ui, (x, y): (f32, f32), on: bool, key: &str) -> bool {
    let t = p.theme;
    let rect = Rect::new(x, y, 38.0, 22.0);
    let slide = ui.anim(id(("toggle", key)), f32::from(u8::from(on)));
    p.rect(rect, mix(t.border_strong, t.accent, slide), 11.0);
    let knob = Rect::new(x + 3.0 + 16.0 * slide, y + 3.0, 16.0, 16.0);
    p.rect(knob, mix(t.text_muted, t.on_accent, slide), 8.0);
    if ui.hovered(rect) {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(rect)
}

/// A square of colour with an icon in it: what a row is about, at a glance.
pub(super) fn badge(p: &mut Painter, rect: Rect, glyph: char, tint: Option<Color>) {
    let t = p.theme;
    let fill = tint.map_or(t.surface, |tint| fade(tint, 0.16));
    p.bordered(rect, fill, rect.w * 0.28, 1.0, tint.map_or(t.border, |tint| fade(tint, 0.35)));
    icon::draw(p, glyph, (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5), rect.w * 0.46, tint.unwrap_or(t.text));
}

/// A [`badge`] wearing a platform's mark instead of an icon, over whatever stood in `rect`.
pub(super) fn logo_badge(p: &mut Painter, rect: Rect, logo: Logo, tint: Option<Color>) {
    let t = p.theme;
    let fill = tint.map_or(t.surface, |tint| mix(t.surface, tint, 0.16));
    p.bordered(rect, fill, rect.w * 0.28, 1.0, tint.map_or(t.border, |tint| mix(t.border, tint, 0.35)));
    let center = (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5);
    p.logo(logo, center, (rect.w * 0.56).round(), tint.unwrap_or(t.text));
}

/// A row of a settings list: a card `rect` with a badge, a title and a
/// line under it. What goes at its right is the caller's.
pub(super) fn row(
    p: &mut Painter,
    rect: Rect,
    (glyph, tint): (char, Option<Color>),
    title: &str,
    detail: &str,
    room: f32,
) {
    let t = p.theme;
    p.bordered(rect, t.surface, theme::RADIUS, 1.0, t.border);
    badge(p, Rect::new(rect.x + 12.0, rect.y + (rect.h - 34.0) * 0.5, 34.0, 34.0), glyph, tint);
    let width = rect.w - 60.0 - room;
    let mut title = p.layout(title, theme::LABEL, None);
    title.truncate(p.fonts, width);
    let mut detail = p.layout(detail, theme::TINY, None);
    detail.truncate(p.fonts, width);
    if detail.width() > 0.0 {
        p.text(&title, rect.x + 58.0, rect.y + rect.h * 0.5 - 17.0, t.text);
        p.text(&detail, rect.x + 58.0, rect.y + rect.h * 0.5 + 3.0, t.text_faint);
    } else {
        p.text(&title, rect.x + 58.0, rect.y + (rect.h - title.height()) * 0.5, t.text);
    }
}

/// A tile to pick a provider with: a badge (an icon, in a colour when it
/// has one) over its name. Returns whether it was clicked.
pub(super) fn tile(
    p: &mut Painter,
    ui: &mut Ui,
    rect: Rect,
    (glyph, tint): (char, Option<Color>),
    title: &str,
    detail: &str,
) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let hover = ui.anim(id(("tile", title, rect.x.to_bits())), f32::from(u8::from(hovered)));
    p.bordered(rect, mix(t.surface, t.hover, hover), theme::RADIUS, 1.0, mix(t.border, t.border_focus, hover));
    badge(p, Rect::new(rect.x + 14.0, rect.y + 14.0, 30.0, 30.0), glyph, tint);
    p.label(title, theme::LABEL, rect.x + 14.0, rect.y + 54.0, t.text);
    let mut detail = p.layout(detail, theme::TINY, None);
    detail.truncate(p.fonts, rect.w - 28.0);
    p.text(&detail, rect.x + 14.0, rect.y + 73.0, t.text_faint);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(rect)
}

/// A pill the mouse can pick: `label` in `rect`, filled when `selected`.
/// Returns whether it was clicked.
pub(super) fn chip(p: &mut Painter, ui: &mut Ui, rect: Rect, label: &str, selected: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let hover = ui.anim(id(("chip", label, rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(hovered)));
    let fill = if selected { t.accent } else { mix(t.surface, t.hover, hover) };
    p.bordered(rect, fill, rect.h * 0.5, 1.0, if selected { t.accent } else { t.border_strong });
    p.label_centered(label, theme::SMALL, rect, if selected { t.on_accent } else { mix(t.text_muted, t.text, hover) });
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_stays_inside_the_content_and_eases() {
        let mut ui = Ui::default();
        let mut scroll = Scroll::default();
        ui.begin(0.016, 0.0);
        scroll.finish(&mut ui, 1000.0, 400.0);
        // A big spin of the wheel cannot go past the end.
        ui.scroll = 5000.0;
        let first = scroll.update(&mut ui, true);
        assert!(first > 0.0 && first < 600.0 && ui.animating, "eases towards the end: {first}");
        ui.scroll = 0.0;
        for _ in 0..200 {
            scroll.update(&mut ui, true);
        }
        assert!((scroll.update(&mut ui, true) - 600.0).abs() < f32::EPSILON && scroll.at_end());
        assert_eq!(scroll.progress(), Some(1.0));
        // Content that shrinks takes the view with it; the wheel elsewhere is ignored.
        scroll.finish(&mut ui, 500.0, 400.0);
        assert!((scroll.update(&mut ui, false) - 100.0).abs() < f32::EPSILON);
        ui.scroll = -5000.0;
        assert!((scroll.update(&mut ui, false) - 100.0).abs() < f32::EPSILON, "not hovered");
        scroll.finish(&mut ui, 100.0, 400.0);
        assert_eq!(scroll.progress(), None);
        // A view with no room at all (a window squeezed small) is no reason to panic.
        assert_eq!(
            (thumb(-184.0, -168.0, 500.0), thumb(10.0, 26.0, 500.0), thumb(400.0, 416.0, 832.0)),
            (0.0, 10.0, 200.0)
        );
        scroll.reveal(40.0, 400.0, 100.0);
        assert!((scroll.target - 40.0).abs() < f32::EPSILON, "rows taller than the view show their top");
    }
}
