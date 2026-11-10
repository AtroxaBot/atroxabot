//! The colour picker: the colour a character or persona wears (their name,
//! portrait ring, bubble and initial), picked from presets, on a shade
//! square and a hue strip, or typed as hex; or Auto, the colour picked from
//! their name. It opens as a popover under the button showing the colour,
//! and every change applies at once. A colour too dark or too light to read
//! on the scheme is shown lighter or deeper (see [`theme::legible`]).

use arboard::Clipboard;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::editor::Editor;
use crate::paint::{Color, Painter, Rect, fade};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, edit_key};

/// The colours offered at the top of the picker.
pub const PRESETS: [&str; 12] = [
    "#e5484d", "#f76b15", "#ffc53d", "#46a758", "#12a594", "#00a2c7", "#0090ff", "#3e63dd", "#8e4ec6", "#d6409f",
    "#a18072", "#8b8d98",
];
/// Width of the popover.
const WIDTH: f32 = 288.0;
/// Inner padding of the popover.
const PAD: f32 = 12.0;
/// Side of a preset's swatch.
const SWATCH: f32 = 18.0;
/// Height of the shade square.
const SHADE_H: f32 = 140.0;
/// Cells across and down the shade square: enough to look smooth.
const CELLS: (usize, usize) = (44, 24);
/// Height of the hue strip.
const HUE_H: f32 = 12.0;
/// Segments of the hue strip.
const HUE_STEPS: usize = 72;
/// Height of the row holding the preview, the hex field and Auto.
const ROW_H: f32 = 30.0;

/// Which control the mouse is dragging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drag {
    Shade,
    Hue,
}

/// What the picker asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Nothing changed.
    Stay,
    /// The colour is now this, as `#rrggbb`; empty for Auto.
    Set(String),
    /// It closes.
    Close,
}

/// An open colour picker.
pub struct ColorPicker {
    /// The colour picked, as hue (degrees), saturation and value (0–1).
    hue: f32,
    saturation: f32,
    value: f32,
    /// The hex field.
    hex: Editor,
    drag: Option<Drag>,
    /// Where it was drawn last frame: what is beneath it does not react.
    pub rect: Option<Rect>,
    /// Opened this frame: the click that opened it is not outside it.
    fresh: bool,
}

impl ColorPicker {
    /// A picker showing `color` (`#rrggbb`), or `auto` (the colour picked
    /// from the name) while there is none.
    #[must_use]
    pub fn new(color: &str, auto: Color) -> Self {
        let rgb = theme::parse_hex(color).unwrap_or([auto[0], auto[1], auto[2]]);
        let mut picker = Self {
            hue: 0.0,
            saturation: 0.0,
            value: 0.0,
            hex: Editor::restricted(7, |c| c.is_ascii_hexdigit() || c == '#'),
            drag: None,
            rect: None,
            fresh: true,
        };
        picker.show(rgb, true);
        picker
    }

    /// The colour picked, as `#rrggbb`.
    #[must_use]
    pub fn hex(&self) -> String {
        theme::to_hex(hsv_to_rgb(self.hue, self.saturation, self.value))
    }

    /// Moves the controls to `rgb`, and the hex field too when `field`.
    fn show(&mut self, rgb: [f32; 3], field: bool) {
        let (hue, saturation, value) = rgb_to_hsv(rgb);
        // Greys and black have no hue: the strip stays where it was.
        if saturation > 0.0 && value > 0.0 {
            self.hue = hue;
        }
        (self.saturation, self.value) = (saturation, value);
        if field {
            self.hex.take();
            self.hex.insert(&theme::to_hex(rgb));
        }
    }

    /// A key while it is open: Enter or Esc closes it, anything else edits
    /// the hex field, which applies once it holds a colour.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> Pick {
        if let Key::Named(NamedKey::Enter | NamedKey::Escape) = &event.logical_key {
            return Pick::Close;
        }
        edit_key(&mut self.hex, event, mods, cb);
        self.typed()
    }

    /// Text committed by an input method, into the hex field.
    pub fn insert(&mut self, text: &str) -> Pick {
        self.hex.insert(text);
        self.typed()
    }

    /// The hex field changed: a whole colour in it is picked.
    fn typed(&mut self) -> Pick {
        match theme::parse_hex(self.hex.text()) {
            Some(rgb) => {
                self.show(rgb, false);
                Pick::Set(theme::to_hex(rgb))
            }
            None => Pick::Stay,
        }
    }

    /// Draws it under `anchor` (or over it, when the window is too short),
    /// for someone whose colour is `chosen` (`#rrggbb`; empty for Auto)
    /// and whose Auto colour is `auto`, and handles the mouse.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, anchor: Rect, chosen: &str, auto: Color) -> Pick {
        let t = p.theme;
        let view = p.clip();
        let picked = theme::parse_hex(chosen);
        let inner = WIDTH - 2.0 * PAD;
        let height = PAD + SWATCH + 12.0 + SHADE_H + 10.0 + HUE_H + 12.0 + ROW_H + PAD;
        let below = anchor.bottom() + 6.0 + height <= view.bottom() - 8.0 || anchor.y - 6.0 - height < view.y + 8.0;
        let y = if below { anchor.bottom() + 6.0 } else { anchor.y - 6.0 - height };
        // Its right edge on the button's: the buttons sit at the right of a form.
        let x = (anchor.right() - WIDTH).min(view.right() - WIDTH - 8.0).max(view.x + 8.0);
        let card = Rect::new(x, y, WIDTH, height);
        self.rect = Some(card);
        p.shadow(Rect::new(card.x, card.y + 8.0, card.w, card.h), t.shadow, theme::RADIUS + 2.0, 22.0);
        p.bordered(card, t.surface, theme::RADIUS + 2.0, 1.0, t.border_strong);
        let mut result = Pick::Stay;

        // The presets.
        let mut top = card.y + PAD;
        let gap = (inner - SWATCH * PRESETS.len() as f32) / (PRESETS.len() - 1) as f32;
        for (k, hex) in PRESETS.iter().enumerate() {
            let Some(rgb) = theme::parse_hex(hex) else { continue };
            let swatch = Rect::new(card.x + PAD + k as f32 * (SWATCH + gap), top, SWATCH, SWATCH);
            let hovered = ui.hovered(swatch);
            p.rect(swatch, [rgb[0], rgb[1], rgb[2], 1.0], SWATCH * 0.5);
            if picked.is_some_and(|c| theme::to_hex(c) == *hex) || hovered {
                let ring = Rect::new(swatch.x - 3.0, swatch.y - 3.0, SWATCH + 6.0, SWATCH + 6.0);
                p.bordered(ring, [0.0; 4], ring.w * 0.5, 1.5, if hovered { t.text_muted } else { t.text });
            }
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(swatch) {
                    self.show(rgb, true);
                    result = Pick::Set((*hex).to_owned());
                }
            }
        }
        top += SWATCH + 12.0;

        // The shade square: saturation across, value down, at this hue.
        let shade = Rect::new(card.x + PAD, top, inner, SHADE_H);
        let (cell_w, cell_h) = (shade.w / CELLS.0 as f32, shade.h / CELLS.1 as f32);
        for row in 0..CELLS.1 {
            for column in 0..CELLS.0 {
                let saturation = (column as f32 + 0.5) / CELLS.0 as f32;
                let value = 1.0 - (row as f32 + 0.5) / CELLS.1 as f32;
                let rgb = hsv_to_rgb(self.hue, saturation, value);
                // A hair wider than a cell, so no seams show between them.
                let cell = Rect::new(
                    shade.x + column as f32 * cell_w,
                    shade.y + row as f32 * cell_h,
                    cell_w + 0.5,
                    cell_h + 0.5,
                );
                p.rect(cell, [rgb[0], rgb[1], rgb[2], 1.0], 0.0);
            }
        }
        p.bordered(shade, [0.0; 4], 0.0, 1.0, fade(t.border_strong, 0.8));
        let marker = (shade.x + self.saturation * shade.w, shade.y + (1.0 - self.value) * shade.h);
        p.bordered(Rect::new(marker.0 - 7.0, marker.1 - 7.0, 14.0, 14.0), [0.0; 4], 7.0, 1.0, t.scrim);
        p.bordered(Rect::new(marker.0 - 6.0, marker.1 - 6.0, 12.0, 12.0), [0.0; 4], 6.0, 2.0, t.on_scrim);
        top += SHADE_H + 10.0;

        // The hue strip.
        let strip = Rect::new(card.x + PAD, top, inner, HUE_H);
        let step = strip.w / HUE_STEPS as f32;
        for k in 0..HUE_STEPS {
            let rgb = hsv_to_rgb((k as f32 + 0.5) / HUE_STEPS as f32 * 360.0, 1.0, 1.0);
            p.rect(
                Rect::new(strip.x + k as f32 * step, strip.y, step + 0.5, strip.h),
                [rgb[0], rgb[1], rgb[2], 1.0],
                0.0,
            );
        }
        let handle_x = strip.x + self.hue / 360.0 * strip.w;
        p.bordered(Rect::new(handle_x - 3.0, strip.y - 3.0, 6.0, strip.h + 6.0), t.on_scrim, 2.0, 1.0, t.scrim);
        top += HUE_H + 12.0;

        // Dragging the square or the strip picks as the mouse moves.
        if ui.pressed {
            let grab = |r: Rect| Rect::new(r.x - 4.0, r.y - 4.0, r.w + 8.0, r.h + 8.0).contains(ui.mouse);
            self.drag = if grab(shade) {
                Some(Drag::Shade)
            } else if grab(strip) {
                Some(Drag::Hue)
            } else {
                None
            };
        }
        if !ui.down {
            self.drag = None;
        }
        if let Some(drag) = self.drag {
            let across = ((ui.mouse.0 - shade.x) / shade.w).clamp(0.0, 1.0);
            match drag {
                Drag::Shade => {
                    self.saturation = across;
                    self.value = 1.0 - ((ui.mouse.1 - shade.y) / shade.h).clamp(0.0, 1.0);
                }
                Drag::Hue => self.hue = across * 360.0,
            }
            let hex = self.hex();
            self.hex.take();
            self.hex.insert(&hex);
            result = Pick::Set(hex);
            ui.cursor = CursorIcon::Crosshair;
            ui.animating = true;
        } else if ui.hovered(shade) || ui.hovered(strip) {
            ui.cursor = CursorIcon::Crosshair;
        }

        // The colour as shown, the hex field, and Auto.
        let shown = if chosen.is_empty() { auto } else { theme::agent_color("", chosen) };
        let preview = Rect::new(card.x + PAD, top + (ROW_H - 22.0) * 0.5, 22.0, 22.0);
        p.rect(preview, shown, 11.0);
        let field = Rect::new(preview.right() + 10.0, top, 112.0, ROW_H);
        p.bordered(field, t.surface, theme::RADIUS_SM, 1.0, t.border_focus);
        let mut text = p.layout(self.hex.text(), theme::SMALL, None);
        let caret_x = text.caret(self.hex.cursor()).0;
        text.truncate(p.fonts, field.w - 20.0);
        let origin = (field.x + 10.0, field.y + (ROW_H - text.height()) * 0.5);
        p.text(&text, origin.0, origin.1, if theme::parse_hex(self.hex.text()).is_some() { t.text } else { t.danger });
        if ui.caret_visible() {
            p.rect(Rect::new(origin.0 + caret_x - 0.5, origin.1 + 2.0, 1.5, text.line_height() - 4.0), t.accent, 0.0);
        }
        let auto_rect = Rect::new(card.right() - PAD - 72.0, top, 72.0, ROW_H);
        let auto_style = if chosen.is_empty() { ButtonStyle::Secondary } else { ButtonStyle::Ghost };
        if button(p, ui, auto_rect, if chosen.is_empty() { "✓ Auto" } else { "Auto" }, auto_style, true) {
            self.show([auto[0], auto[1], auto[2]], true);
            result = Pick::Set(String::new());
        }

        let outside = !card.contains(ui.press_pos) && !anchor.contains(ui.press_pos);
        if !self.fresh && ui.released && outside && self.drag.is_none() {
            result = Pick::Close;
        }
        self.fresh = false;
        result
    }
}

/// sRGB (0–1 each) of hue `hue` (degrees), saturation and value (0–1).
#[must_use]
pub fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> [f32; 3] {
    let channel = |n: f32| {
        let k = (n + hue.rem_euclid(360.0) / 60.0).rem_euclid(6.0);
        value - value * saturation * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [channel(5.0), channel(3.0), channel(1.0)]
}

/// Hue (degrees), saturation and value (0–1) of an sRGB colour.
#[must_use]
pub fn rgb_to_hsv([red, green, blue]: [f32; 3]) -> (f32, f32, f32) {
    let max = red.max(green).max(blue);
    let spread = max - red.min(green).min(blue);
    let hue = if spread <= 0.0 {
        0.0
    } else if (max - red).abs() < f32::EPSILON {
        60.0 * ((green - blue) / spread).rem_euclid(6.0)
    } else if (max - green).abs() < f32::EPSILON {
        60.0 * ((blue - red) / spread + 2.0)
    } else {
        60.0 * ((red - green) / spread + 4.0)
    };
    (hue, if max > 0.0 { spread / max } else { 0.0 }, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_round_trips() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [1.0, 0.0, 0.0]);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), [0.0, 1.0, 0.0]);
        assert_eq!(hsv_to_rgb(240.0, 1.0, 0.5), [0.0, 0.0, 0.5]);
        assert_eq!(hsv_to_rgb(300.0, 0.0, 0.25), [0.25, 0.25, 0.25], "no saturation: grey");
        for hex in PRESETS.iter().chain(&["#000000", "#ffffff", "#808080"]) {
            let (h, s, v) = rgb_to_hsv(theme::parse_hex(hex).unwrap());
            assert_eq!(&theme::to_hex(hsv_to_rgb(h, s, v)), hex);
        }
    }

    #[test]
    fn the_hex_field_picks_once_it_holds_a_colour() {
        let mut picker = ColorPicker::new("", [0.5, 0.5, 0.5, 1.0]);
        assert_eq!(picker.hex(), "#808080", "Auto starts at the name's colour");
        picker.hex.take();
        assert_eq!(picker.insert("#3e6"), Pick::Set("#33ee66".into()), "short hex");
        picker.hex.take();
        assert_eq!(picker.insert("#3e63d"), Pick::Stay, "not a colour yet");
        assert_eq!(picker.insert("d"), Pick::Set("#3e63dd".into()));
        assert_eq!(picker.hex(), "#3e63dd", "the controls follow");
        // A grey keeps the hue the strip was at.
        let hue = picker.hue;
        picker.show([0.5, 0.5, 0.5], true);
        assert!((picker.hue - hue).abs() < f32::EPSILON && picker.hex.text() == "#808080");
        assert_eq!(ColorPicker::new("#e8a55e", [0.0; 4]).hex(), "#e8a55e");
    }
}
