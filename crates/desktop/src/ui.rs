//! Per-frame input state, animation bookkeeping and shared widgets.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use arboard::Clipboard;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::editor::Editor;
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::TextLayout;
use crate::theme;

/// Input and bookkeeping shared by every widget during a frame.
#[derive(Default)]
pub struct Ui {
    /// Mouse position in logical pixels.
    pub mouse: (f32, f32),
    /// Where the left button last went down.
    pub press_pos: (f32, f32),
    /// Left button held.
    pub down: bool,
    /// Left button went down this frame.
    pub pressed: bool,
    /// Clicks in the current burst: 1 single, 2 double, 3 triple…
    pub clicks: u32,
    /// When the last press happened (for click bursts).
    last_press: f32,
    /// Left button went up this frame.
    pub released: bool,
    /// Accumulated wheel movement in logical pixels; positive scrolls down.
    pub scroll: f32,
    /// Accumulated sideways wheel movement; positive scrolls right.
    pub scroll_x: f32,
    /// Keyboard modifiers.
    pub mods: ModifiersState,
    /// Seconds since the previous frame.
    pub dt: f32,
    /// Seconds since startup.
    pub time: f32,
    /// Moment of the last text edit, used to restart the caret blink.
    pub last_edit: f32,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Cursor requested by whatever the mouse is over.
    pub cursor: CursorIcon,
    /// An overlay covering this area swallows hover for widgets beneath it.
    pub blocker: Option<Rect>,
    /// Set when something is mid-animation and needs another frame.
    pub animating: bool,
    /// Animated values with the frame they were last used in.
    anims: HashMap<u64, (f32, u64)>,
    frame: u64,
}

impl Ui {
    /// Prepares for a new frame.
    pub fn begin(&mut self, dt: f32, time: f32) {
        self.dt = dt;
        self.time = time;
        self.cursor = CursorIcon::Default;
        self.animating = false;
        // Forget widgets that were not drawn last frame so the map stays small.
        let frame = self.frame;
        self.anims.retain(|_, (_, used)| *used == frame);
        self.frame += 1;
    }

    /// Records a left-button press at the current mouse position, counting
    /// quick presses in the same spot as one burst (double/triple click).
    pub fn press(&mut self, now: f32) {
        let near = (self.mouse.0 - self.press_pos.0).abs() < 4.0 && (self.mouse.1 - self.press_pos.1).abs() < 4.0;
        self.clicks = if near && now - self.last_press < 0.4 { self.clicks + 1 } else { 1 };
        self.last_press = now;
        self.down = true;
        self.pressed = true;
        self.press_pos = self.mouse;
    }

    /// Clears one-shot input once a frame has consumed it.
    pub fn end(&mut self) {
        self.pressed = false;
        self.released = false;
        self.scroll = 0.0;
        self.scroll_x = 0.0;
    }

    /// Whether the mouse is over `rect` and not over an overlay.
    #[must_use]
    pub fn hovered(&self, rect: Rect) -> bool {
        rect.contains(self.mouse) && !self.blocker.is_some_and(|b| b.contains(self.mouse))
    }

    /// Whether `rect` was clicked (pressed and released inside) this frame.
    #[must_use]
    pub fn clicked(&self, rect: Rect) -> bool {
        self.released && self.hovered(rect) && rect.contains(self.press_pos)
    }

    /// Eases the value stored under `id` towards `target` and returns it.
    pub fn anim(&mut self, id: u64, target: f32) -> f32 {
        self.ease(id, target, target)
    }

    /// Eases from 0 to 1 from the first frame `id` is drawn in: something
    /// arriving fades in. Drawn again after a frame without it, it arrives
    /// again, so callers use it only for what is new.
    pub fn appear(&mut self, id: u64) -> f32 {
        self.ease(id, 1.0, 0.0)
    }

    /// Eases the value under `id` (starting at `initial`) towards `target`.
    fn ease(&mut self, id: u64, target: f32, initial: f32) -> f32 {
        let (value, used) = self.anims.entry(id).or_insert((initial, self.frame));
        *used = self.frame;
        let t = 1.0 - (-self.dt * 16.0).exp();
        *value += (target - *value) * t;
        if (target - *value).abs() < 0.002 {
            *value = target;
        } else {
            self.animating = true;
        }
        *value
    }

    /// Whether the caret should be drawn right now.
    #[must_use]
    pub fn caret_visible(&self) -> bool {
        self.focused && (self.time - self.last_edit).rem_euclid(1.0) < 0.55
    }

    /// Seconds until the caret blink state next flips.
    #[must_use]
    pub fn next_blink(&self) -> f32 {
        let phase = (self.time - self.last_edit).rem_euclid(1.0);
        if phase < 0.55 { 0.55 - phase } else { 1.0 - phase }
    }
}

/// Stable widget identity from any hashable key.
#[must_use]
pub fn id(key: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// Glyphs of the icon font ([`theme::icon`]), Lucide's, by name, and
/// [`icon::draw`] to draw one.
pub mod icon {
    use crate::paint::{Color, Painter};
    use crate::theme;

    /// Draws `glyph`, `size` pixels square, centred on `(cx, cy)`.
    pub fn draw(p: &mut Painter, glyph: char, (cx, cy): (f32, f32), size: f32, color: Color) {
        let mut bytes = [0; 4];
        let layout = p.layout(glyph.encode_utf8(&mut bytes), theme::icon(size), None);
        // Lucide draws an em up from the baseline, which the layout puts
        // 0.85 em down: the glyph's middle is 0.35 em below the top.
        p.text(&layout, (cx - layout.width() * 0.5).round(), (cy - size * 0.35).round(), color);
    }

    /// Arrow pointing up: send.
    pub const ARROW_UP: char = '\u{e04a}';
    /// A brain: reasoning.
    pub const BRAIN: char = '\u{e3c6}';
    /// A tick.
    pub const CHECK: char = '\u{e06c}';
    /// Chevron pointing down.
    pub const CHEVRON_DOWN: char = '\u{e06d}';
    /// Chevron pointing left: back.
    pub const CHEVRON_LEFT: char = '\u{e06e}';
    /// Chevron pointing right.
    pub const CHEVRON_RIGHT: char = '\u{e06f}';
    /// A pen on a square: edit.
    pub const PEN: char = '\u{e172}';
    /// A bin: delete.
    pub const TRASH: char = '\u{e18e}';
    /// Two sheets: copy.
    pub const COPY: char = '\u{e09e}';
    /// A globe: worlds.
    pub const GLOBE: char = '\u{e0e8}';
    /// A plus: new.
    pub const PLUS: char = '\u{e13d}';
    /// A magnifier: search.
    pub const SEARCH: char = '\u{e151}';
    /// A cog: settings.
    pub const SETTINGS: char = '\u{e154}';
    /// A cross: close.
    pub const X: char = '\u{e1b2}';
    /// A picture.
    pub const IMAGE: char = '\u{e0f6}';
    /// An eye: looking only.
    pub const EYE: char = '\u{e0ba}';
    /// A server.
    pub const SERVER: char = '\u{e153}';
    /// A key: an API key.
    pub const KEY: char = '\u{e0fd}';
    /// Arrows in a circle: ask again.
    pub const REFRESH: char = '\u{e145}';
    /// A chain link: an address.
    pub const LINK: char = '\u{e102}';
    /// A clock run back: memory.
    pub const HISTORY: char = '\u{e1f5}';
    /// A handset: start a call.
    pub const PHONE: char = '\u{e133}';
    /// A struck handset: end the call.
    pub const PHONE_OFF: char = '\u{e138}';
    /// A microphone.
    pub const MIC: char = '\u{e118}';
    /// A struck microphone: muted.
    pub const MIC_OFF: char = '\u{e119}';
    /// A prompt: a command.
    pub const TERMINAL: char = '\u{e181}';
    /// A page of text: a file.
    pub const FILE: char = '\u{e0cc}';
    /// A folder.
    pub const FOLDER: char = '\u{e0d7}';
    /// A robot: a helper agent.
    pub const BOT: char = '\u{e1bb}';
    /// Sparkles: the model.
    pub const SPARKLES: char = '\u{e412}';
    /// A shield: an approval.
    pub const SHIELD: char = '\u{e158}';
    /// A bolt: autonomy.
    pub const ZAP: char = '\u{e1b4}';
    /// A plug: connections.
    pub const PLUG: char = '\u{e37f}';
    /// Sound bars: voice.
    pub const AUDIO: char = '\u{e55a}';
    /// A list.
    pub const LIST: char = '\u{e106}';
    /// A wrench: a tool.
    pub const WRENCH: char = '\u{e1b1}';
    /// A circled exclamation mark: a problem.
    pub const ALERT: char = '\u{e077}';
    /// A slash in a square: a command.
    pub const SLASH: char = '\u{e174}';
    /// A paperclip: attach files.
    pub const PAPERCLIP: char = '\u{e12d}';
    /// A panel at the right: the side panel.
    pub const PANEL: char = '\u{e431}';
    /// A bar chart: statistics.
    pub const CHART: char = '\u{e2a3}';
    /// A branch: git.
    pub const BRANCH: char = '\u{e0e2}';
    /// A route: a gateway to many models.
    pub const ROUTE: char = '\u{e53e}';
    /// A hexagon: a model maker's own API.
    pub const HEXAGON: char = '\u{e0f3}';
    /// A flame: a streak.
    pub const FLAME: char = '\u{e0d2}';
    /// A speech bubble: a session.
    pub const MESSAGE: char = '\u{e117}';
    /// Layers: the context, or a cache.
    pub const LAYERS: char = '\u{e529}';
    /// A pulse line: activity.
    pub const ACTIVITY: char = '\u{e038}';
}

/// A square ghost button showing `glyph`, faint until hovered. Returns
/// whether it was clicked.
pub fn icon_button(p: &mut Painter, ui: &mut Ui, rect: Rect, glyph: char, enabled: bool) -> bool {
    let t = p.theme;
    let hovered = enabled && ui.hovered(rect);
    let hover = ui.anim(id(("icon-button", rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(hovered)));
    p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
    let color = if enabled { mix(t.text_muted, t.text, hover) } else { fade(t.text_faint, 0.6) };
    icon::draw(p, glyph, (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5), 15.0, color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    hovered && ui.clicked(rect)
}

/// Visual weight of a button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Accent-filled call to action.
    Primary,
    /// Outlined neutral.
    Secondary,
    /// Text only until hovered.
    Ghost,
    /// Outlined in the danger colour, for destructive actions.
    Danger,
}

/// Draws a button and returns whether it was clicked.
pub fn button(p: &mut Painter, ui: &mut Ui, rect: Rect, label: &str, style: ButtonStyle, enabled: bool) -> bool {
    let t = p.theme;
    let hovered = enabled && ui.hovered(rect);
    let hover = ui.anim(id((label, "button", rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(hovered)));
    let radius = theme::RADIUS_SM;
    let text_color = match style {
        _ if !enabled => t.text_faint,
        ButtonStyle::Primary => t.on_accent,
        ButtonStyle::Secondary => t.text,
        ButtonStyle::Ghost => mix(t.text_muted, t.text, hover),
        ButtonStyle::Danger => t.danger,
    };
    match style {
        ButtonStyle::Primary if enabled => p.rect(rect, fade(t.accent, 1.0 - 0.12 * hover), radius),
        ButtonStyle::Primary => p.rect(rect, t.hover, radius),
        ButtonStyle::Secondary => p.bordered(rect, mix(t.surface, t.hover, hover), radius, 1.0, t.border_strong),
        ButtonStyle::Ghost => p.rect(rect, fade(t.hover, hover), radius),
        ButtonStyle::Danger => {
            p.bordered(rect, fade(t.danger, 0.1 * hover), radius, 1.0, fade(t.danger, 0.45 + 0.3 * hover));
        }
    }
    p.label_centered(label, theme::LABEL, rect, text_color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    enabled && ui.clicked(rect)
}

/// Opens the clipboard lazily; `None` when the platform has none.
fn clipboard(slot: &mut Option<Clipboard>) -> Option<&mut Clipboard> {
    if slot.is_none() {
        *slot = Clipboard::new().ok();
    }
    slot.as_mut()
}

/// Copies `text` to the system clipboard, ignoring failures.
pub fn copy(slot: &mut Option<Clipboard>, text: &str) {
    if let Some(cb) = clipboard(slot) {
        let _ = cb.set_text(text);
    }
}

/// Moves `editor`'s caret one visual line up or down in `layout` (its text
/// as drawn), to the start or end past the first or last line.
pub fn move_line(editor: &mut Editor, layout: &TextLayout, up: bool, select: bool) {
    let (x, y) = layout.caret(editor.cursor());
    let line = layout.line_height();
    let target = if up { y - line * 0.5 } else { y + line * 1.5 };
    let byte = if target < 0.0 {
        0
    } else if target > layout.height() {
        editor.text().len()
    } else {
        layout.hit(x, target)
    };
    editor.set_cursor(byte, select);
}

/// Applies standard text-editing keys to `editor`. Returns `false` when the
/// key is not an editing key, so the caller may handle it.
///
/// Enter is never handled here; callers decide between submit and newline.
pub fn edit_key(editor: &mut Editor, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> bool {
    let mac = cfg!(target_os = "macos");
    let primary = if mac { mods.super_key() } else { mods.control_key() };
    let word = if mac { mods.alt_key() } else { mods.control_key() };
    let shift = mods.shift_key();
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => return false,
        Key::Named(NamedKey::Backspace) => editor.backspace(word),
        Key::Named(NamedKey::Delete) => editor.delete(word),
        Key::Named(NamedKey::ArrowLeft) if mac && primary => editor.home(shift),
        Key::Named(NamedKey::ArrowRight) if mac && primary => editor.end(shift),
        Key::Named(NamedKey::ArrowLeft) => editor.left(word, shift),
        Key::Named(NamedKey::ArrowRight) => editor.right(word, shift),
        Key::Named(NamedKey::Home) => editor.home(shift),
        Key::Named(NamedKey::End) => editor.end(shift),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("a") => editor.select_all(),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("c") => copy(cb, editor.selected_text()),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("x") => {
            copy(cb, editor.selected_text());
            editor.insert("");
        }
        Key::Character(c) if primary && c.eq_ignore_ascii_case("v") => {
            if let Some(text) = clipboard(cb).and_then(|cb| cb.get_text().ok()) {
                editor.insert(&text);
            }
        }
        _ => match &event.text {
            // Ctrl+Alt is AltGr on Windows/Linux and produces real characters.
            Some(text) if !primary || mods.alt_key() => editor.insert(text),
            _ => return false,
        },
    }
    true
}

#[cfg(test)]
mod tests {
    use super::icon;
    use crate::text::{FontId, Fonts, TextLayout};
    use crate::theme;

    #[test]
    fn icons_are_square_glyphs_of_the_icon_font() {
        let fonts = Fonts::load();
        let all = [
            icon::ARROW_UP,
            icon::BRAIN,
            icon::CHECK,
            icon::CHEVRON_DOWN,
            icon::CHEVRON_LEFT,
            icon::CHEVRON_RIGHT,
            icon::PEN,
            icon::TRASH,
            icon::COPY,
            icon::GLOBE,
            icon::PLUS,
            icon::SEARCH,
            icon::SETTINGS,
            icon::X,
            icon::IMAGE,
            icon::EYE,
            icon::SERVER,
            icon::KEY,
            icon::REFRESH,
            icon::LINK,
            icon::HISTORY,
            icon::PHONE,
            icon::PHONE_OFF,
            icon::MIC,
            icon::MIC_OFF,
            icon::TERMINAL,
            icon::FILE,
            icon::FOLDER,
            icon::BOT,
            icon::SPARKLES,
            icon::SHIELD,
            icon::ZAP,
            icon::PLUG,
            icon::AUDIO,
            icon::LIST,
            icon::WRENCH,
            icon::ALERT,
            icon::SLASH,
            icon::PAPERCLIP,
            icon::PANEL,
            icon::CHART,
            icon::BRANCH,
            icon::ROUTE,
            icon::HEXAGON,
            icon::FLAME,
            icon::MESSAGE,
            icon::LAYERS,
            icon::ACTIVITY,
        ];
        for glyph in all {
            assert_eq!(fonts.resolve(FontId::ICON, glyph, None).0, FontId::ICON, "{:x}", u32::from(glyph));
            // `icon::draw` centres on the advance: it must be the em.
            let layout = TextLayout::new(&fonts, &glyph.to_string(), theme::icon(16.0), None, 1.0);
            assert!((layout.width() - 16.0).abs() < 0.5, "{:x}: {}", u32::from(glyph), layout.width());
        }
    }
}
