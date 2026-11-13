//! Finding text in the open conversation (Ctrl+F, or a result of the
//! search window that was found in a session's messages).
//!
//! A small bar at the top of the conversation takes the words. Every place
//! they stand in what the conversation shows is washed, the one in hand
//! more strongly, and Enter goes to the next (Shift+Enter to the one
//! before), around the ends. What is folded away (reasoning, a tool's
//! detail) is searched once it is opened: the bar finds what is on the page.
//!
//! The places are those of the laid-out text, so a match is washed exactly
//! where it is drawn. They are looked for once per block and kept until the
//! block or the words change: a conversation that is being written costs
//! its newest block per frame, not all of it.

use std::collections::HashMap;
use std::ops::Range;

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use super::widgets::{Scroll, line_field};
use crate::doc::Doc;
use crate::editor::Editor;
use crate::paint::{Color, Painter, Rect, fade};
use crate::theme;
use crate::ui::{Ui, edit_key, icon, icon_button};

/// How much of the view stays free above and below a match it is brought to.
const MARGIN: f32 = 90.0;
/// The bar's size.
const BAR: (f32, f32) = (340.0, 40.0);

/// A block of the conversation, as its layouts are kept.
type Key2 = (usize, usize);
/// The places found in a block: each as its piece of the layout and the bytes there.
type Places = Vec<(usize, Range<usize>)>;

/// The places `needle` (lowercase characters) stands in `text`, whatever
/// the case of either, as byte ranges that do not overlap.
pub(super) fn matches(text: &str, needle: &[char]) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    if needle.is_empty() {
        return found;
    }
    // Each character in lowercase, with where it begins and ends in the text: one may become several.
    let lowered: Vec<(char, usize, usize)> = text
        .char_indices()
        .flat_map(|(at, c)| c.to_lowercase().map(move |lower| (lower, at, at + c.len_utf8())))
        .collect();
    let mut at = 0;
    while at + needle.len() <= lowered.len() {
        let here = &lowered[at..at + needle.len()];
        // A match begins and ends with a whole character of the text.
        let whole = (at == 0 || lowered[at - 1].1 != here[0].1)
            && lowered.get(at + needle.len()).is_none_or(|next| next.1 != here[needle.len() - 1].1);
        if whole && here.iter().map(|(c, ..)| c).eq(needle) {
            found.push(here[0].1..here[needle.len() - 1].2);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

/// What the keyboard did to the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Typed {
    /// The key was the bar's.
    Taken,
    /// The bar was closed.
    Closed,
}

/// The search of one conversation.
#[derive(Default)]
pub(super) struct Find {
    /// The words looked for, as typed.
    pub editor: Editor,
    /// Whether the bar has the keyboard; the message box has it otherwise.
    pub focused: bool,
    /// The words the places below were looked up for, in lowercase.
    needle: Vec<char>,
    /// The places in each block: the length of the text it was laid out
    /// from, and each match as its piece of the layout and the bytes there.
    found: HashMap<Key2, (usize, Places)>,
    /// The match in hand, counted through the conversation as it is drawn.
    current: usize,
    /// How many were drawn so far this frame, and how many last frame.
    counted: usize,
    total: usize,
    /// The match in hand is to be brought into view.
    jump: bool,
    /// Where it was drawn this frame: its top and bottom on screen.
    seen: Option<(f32, f32)>,
}

impl Find {
    /// A search for `words`, to be brought to its first match; with the
    /// keyboard when `focused`.
    pub fn new(words: &str, focused: bool) -> Self {
        let mut find = Self { focused, jump: true, ..Self::default() };
        find.editor.insert(words);
        find.editor.select_all();
        find
    }

    /// The words looked for, in lowercase; nothing while they are only spaces.
    fn wanted(&self) -> Vec<char> {
        let typed = self.editor.text();
        if typed.trim().is_empty() { Vec::new() } else { typed.to_lowercase().chars().collect() }
    }

    /// What the search is of, for a fallback that looks through the
    /// messages themselves: the words in lowercase.
    pub fn words(&self) -> String {
        self.needle.iter().collect()
    }

    /// How many matches the conversation showed last frame.
    #[cfg(test)]
    pub fn total(&self) -> usize {
        self.total
    }

    /// Whether the search still has to bring its match into view.
    pub fn pending(&self) -> bool {
        self.jump
    }

    /// Goes to the next match (or the one before), around the ends.
    pub fn step(&mut self, back: bool) {
        if self.total > 0 {
            self.current =
                if back { (self.current + self.total - 1) % self.total } else { (self.current + 1) % self.total };
            self.jump = true;
        }
    }

    /// A key while the bar has the keyboard.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<arboard::Clipboard>) -> Typed {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => return Typed::Closed,
            Key::Named(NamedKey::Enter) => self.step(mods.shift_key()),
            Key::Named(NamedKey::ArrowDown) => self.step(false),
            Key::Named(NamedKey::ArrowUp) => self.step(true),
            _ => {
                edit_key(&mut self.editor, event, mods, clipboard);
            }
        }
        Typed::Taken
    }

    /// The conversation is about to be drawn: other words start over at the first match.
    pub fn begin(&mut self) {
        let wanted = self.wanted();
        if wanted != self.needle {
            self.needle = wanted;
            self.found.clear();
            (self.current, self.jump) = (0, true);
        }
        (self.counted, self.seen) = (0, None);
    }

    /// The block `key`, laid out as `doc` from a text `len` long, is about
    /// to be drawn at `origin`: has the matches in it washed in `ink`.
    pub fn block(&mut self, key: Key2, (doc, len): (&mut Doc, usize), origin: (f32, f32), ink: Color) {
        if self.needle.is_empty() {
            return;
        }
        let needle = &self.needle;
        let (_, places) = self
            .found
            .entry(key)
            .and_modify(|kept| {
                if kept.0 != len {
                    *kept = (len, places(doc, needle));
                }
            })
            .or_insert_with(|| (len, places(doc, needle)));
        for (piece, range) in places.iter() {
            let current = self.counted == self.current;
            self.counted += 1;
            let wash = fade(ink, if current { 0.42 } else { 0.16 });
            let drawn = doc.mark(origin, *piece, range.clone(), wash);
            if current {
                self.seen = drawn;
            }
        }
    }

    /// The conversation was drawn in `area`, scrolled by `offset`: counts
    /// what was found and heads for the match in hand when it is to be
    /// shown. Returns whether the view was sent somewhere.
    pub fn end(&mut self, scroll: &mut Scroll, area: Rect, offset: f32) -> bool {
        self.total = self.counted;
        if self.current >= self.total {
            // Matches went (a block was folded away): the first one is in hand again.
            self.current = 0;
        }
        if !self.jump || self.total == 0 {
            return false;
        }
        let Some((top, bottom)) = self.seen else {
            // The one in hand was only just chosen: the next frame draws it as such.
            return true;
        };
        self.jump = false;
        let (top, bottom) = (top - area.y + offset, bottom - area.y + offset);
        scroll.reveal(top - MARGIN, bottom + MARGIN, area.h);
        true
    }

    /// The search is over where nothing was found to show: it stops asking to be brought anywhere.
    pub fn settle(&mut self) {
        self.jump = false;
    }

    /// Where the bar floats: at the top right of the conversation's `area`.
    pub fn bar(area: Rect) -> Rect {
        let wide = BAR.0.min(area.w - 32.0);
        Rect::new(area.right() - 20.0 - wide, area.y + 10.0, wide, BAR.1)
    }

    /// Draws the bar over `area`. Returns whether it was closed.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect) -> bool {
        let t = p.theme;
        let bar = Self::bar(area);
        // A press gives the keyboard to what it lands on.
        if ui.pressed {
            self.focused = ui.hovered(bar);
        }
        p.shadow(Rect::new(bar.x, bar.y + 5.0, bar.w, bar.h), fade(t.shadow, 0.8), theme::RADIUS, 18.0);
        let edge = if self.focused { t.border_focus } else { t.border_strong };
        p.bordered(bar, t.surface, theme::RADIUS, 1.0, edge);
        let mid = bar.y + bar.h * 0.5;
        icon::draw(p, icon::SEARCH, (bar.x + 18.0, mid), 14.0, t.text_faint);
        let mut right = bar.right() - 6.0;
        let mut closed = false;
        for (glyph, does) in [(icon::X, 0), (icon::CHEVRON_DOWN, 1), (icon::CHEVRON_UP, 2)] {
            let button = Rect::new(right - 28.0, mid - 14.0, 28.0, 28.0);
            if icon_button(p, ui, button, glyph, does == 0 || self.total > 0) {
                match does {
                    0 => closed = true,
                    1 => self.step(false),
                    _ => self.step(true),
                }
            }
            right = button.x - 2.0;
        }
        // How far along it is, once there is something to look for.
        if !self.needle.is_empty() {
            let said = if self.total == 0 {
                "No results".to_owned()
            } else {
                format!("{} of {}", self.current.min(self.total - 1) + 1, self.total)
            };
            let count = p.layout(&said, theme::TINY, None);
            right -= count.width() + 6.0;
            p.text(&count, right, mid - count.height() * 0.5, t.text_faint);
        }
        let field = Rect::new(bar.x + 34.0, bar.y, (right - 10.0 - (bar.x + 34.0)).max(0.0), bar.h);
        if ui.hovered(field) {
            ui.cursor = winit::window::CursorIcon::Text;
        }
        line_field(p, ui, field, &mut self.editor, ("Find in this session", theme::SMALL), self.focused && ui.focused);
        closed
    }
}

/// The places `needle` stands in the laid-out text of `doc`.
fn places(doc: &Doc, needle: &[char]) -> Places {
    let found = |(index, piece): (usize, &crate::doc::TextPiece)| {
        matches(&piece.text, needle).into_iter().map(move |range| (index, range))
    };
    doc.texts.iter().enumerate().flat_map(found).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
        let needle: Vec<char> = needle.to_lowercase().chars().collect();
        matches(text, &needle).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn words_are_found_whatever_their_case() {
        assert_eq!(found("The parser. the PARSER; Parsers", "parser"), ["parser", "PARSER", "Parser"]);
        assert_eq!(found("aaaa", "aa"), ["aa", "aa"], "matches do not overlap");
        assert_eq!(found("Čeština a ČEŠTINA", "češ"), ["Češ", "ČEŠ"]);
        assert_eq!(found("日本語のテキスト", "テキ"), ["テキ"]);
        assert!(found("anything", "").is_empty() && found("", "x").is_empty() && found("ab", "abc").is_empty());
        // A capital whose lowercase is two characters is matched whole or not at all.
        assert_eq!(found("İstanbul", "i\u{307}s"), ["İs"]);
        assert!(found("İstanbul", "\u{307}s").is_empty(), "no match from the middle of a character");
        assert_eq!(found("fn main() { a.b(); }", "A.B("), ["a.b("]);
    }

    #[test]
    fn the_match_in_hand_goes_around_the_ends() {
        let mut find = Find::new("x", true);
        find.step(false);
        assert_eq!(find.current, 0, "nothing found, nowhere to go");
        find.total = 3;
        find.step(true);
        assert_eq!(find.current, 2);
        find.step(false);
        assert_eq!((find.current, find.jump), (0, true));
        // Other words start at the first match again; only spaces look for nothing.
        find.current = 2;
        find.begin();
        assert_eq!((find.current, find.words().as_str()), (0, "x"));
        find.editor.take();
        find.editor.insert("  ");
        find.begin();
        assert!(find.needle.is_empty());
    }

    #[test]
    fn matches_that_went_leave_the_first_in_hand() {
        let mut find = Find::new("x", false);
        find.begin();
        (find.current, find.counted) = (5, 2);
        let mut scroll = Scroll::default();
        find.seen = Some((300.0, 320.0));
        assert!(find.end(&mut scroll, Rect::new(0.0, 0.0, 800.0, 600.0), 0.0));
        assert_eq!((find.current, find.total(), find.pending()), (0, 2, false));
        // Nothing found: nothing is headed for.
        find.begin();
        find.jump = true;
        assert!(!find.end(&mut scroll, Rect::new(0.0, 0.0, 800.0, 600.0), 0.0));
        assert_eq!(find.total(), 0);
    }
}
