//! The composer: the text field (with input-method preedit), and under it
//! a paperclip for files to send along, the agent's model and send. Typing
//! a slash lists the commands and skills a message can start with. A tool
//! call waiting for approval sits on top of it, where the answer is one key
//! away.

use std::collections::HashMap;

use arboard::Clipboard;
use atroxabot_core::attach::media_type;
use atroxabot_core::{Agent, Ask, ConnectionKind, Verdict};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::menu::{Menu, effort_label, folder_name};
use super::question::{self, QuestionForm};
use super::{Account, Shell};
use crate::app::Action;
use crate::editor::Editor;
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::{Style, TextLayout};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, edit_key, icon, icon_button, id, move_line};

/// Height of a row in the list of commands a slash opens, and how many rows it shows.
const ROW: f32 = 38.0;
const SHOWN: usize = 7;
/// The composer grows up to this many lines, then scrolls.
const MAX_LINES: usize = 8;

/// The message being written.
#[derive(Default)]
pub(super) struct Composer {
    /// Its text.
    pub editor: Editor,
    /// Files that go with it, by path.
    pub files: Vec<String>,
    /// The command picked in the list a slash opens.
    slash: usize,
    /// Esc put that list away, until the slash is gone again.
    slash_off: bool,
    /// The file picked in the list an `@` opens.
    mention: usize,
    /// Esc put that list away, until the `@` word is left.
    mention_off: bool,
    /// Where the `@` word begins that the files were last listed for.
    listed_at: Option<usize>,
    /// The folder whose files that list offers, and each of them as it is
    /// written and in lowercase, to search.
    listing: (String, Vec<(String, String)>),
    /// Where the message box was drawn: the list hangs over it.
    card: Rect,
    /// Text an input method is composing at the caret, with its cursor.
    pub preedit: Option<(String, Option<(usize, usize)>)>,
    /// Where the caret was drawn, for the input method's window.
    pub caret: Option<Rect>,
    scroll: f32,
    selecting: bool,
    /// Last frame's layout and origin, for moving the caret by lines.
    layout: Option<(TextLayout, (f32, f32))>,
    /// Whose message is being written: a session's id, or [`next_of`] an
    /// agent; empty until the shell first says.
    owner: String,
    /// What was written for the others and not sent, by owner.
    drafts: HashMap<String, Draft>,
}

/// A message written for one session (or an agent's next one) and not
/// sent, put aside while another is open.
#[derive(Default)]
struct Draft {
    editor: Editor,
    files: Vec<String>,
}

/// The owner of what is written for `agent`'s next session, before it has one.
pub(super) fn next_of(agent: &str) -> String {
    // Session ids are file-name safe, so they never hold a colon.
    format!("new:{agent}")
}

impl Composer {
    /// An editing key (Enter and Escape are the shell's).
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<Clipboard>) {
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.editor.insert("\n"),
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) => {
                if let Some((layout, _)) = &self.layout {
                    move_line(&mut self.editor, layout, *key == NamedKey::ArrowUp, mods.shift_key());
                }
            }
            _ => {
                edit_key(&mut self.editor, event, mods, clipboard);
            }
        }
    }

    /// Text committed by an input method.
    pub fn commit(&mut self, text: &str) {
        self.preedit = None;
        self.editor.insert(text);
    }

    /// Shows the message being written for `owner`, putting aside the one
    /// shown until now (its text, caret and files) for when it is back.
    pub fn show(&mut self, owner: &str) {
        if self.owner == owner {
            return;
        }
        let previous = std::mem::replace(&mut self.owner, owner.to_owned());
        // Written before anyone owned it: it is the first owner's.
        if previous.is_empty() {
            return;
        }
        let draft = Draft { editor: std::mem::take(&mut self.editor), files: std::mem::take(&mut self.files) };
        if !draft.editor.text().is_empty() || !draft.files.is_empty() {
            self.drafts.insert(previous, draft);
        }
        let Draft { editor, files } = self.drafts.remove(owner).unwrap_or_default();
        (self.editor, self.files) = (editor, files);
        (self.preedit, self.layout, self.scroll, self.selecting) = (None, None, 0.0, false);
        (self.slash, self.slash_off, self.mention, self.mention_off, self.listed_at) = (0, false, 0, false, None);
    }

    /// Drops what was written for `owner`, a session that is gone.
    pub fn forget(&mut self, owner: &str) {
        self.drafts.remove(owner);
        if self.owner == owner {
            self.editor.take();
            self.files.clear();
        }
    }

    /// Puts a message that was not sent back, before what is being written.
    pub fn restore(&mut self, text: &str, files: Vec<String>) {
        let draft = self.editor.take();
        let joined = match (text.trim().is_empty(), draft.trim().is_empty()) {
            (_, true) => text.to_owned(),
            (true, false) => draft,
            (false, false) => format!("{text}\n\n{draft}"),
        };
        self.editor.insert(&joined);
        let files: Vec<String> = files.into_iter().filter(|file| !self.files.contains(file)).collect();
        self.files.splice(0..0, files);
    }
}

impl Shell {
    /// Draws the composer at the foot of `canvas`, with a pending approval
    /// above it. Returns the top edge of what it drew.
    pub(super) fn draw_composer(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        canvas: Rect,
        agent: &Agent,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let width = theme::COLUMN_WIDTH.min(canvas.w - 48.0);
        let x = canvas.x + ((canvas.w - width) * 0.5).round();
        let (pad, top_pad, toolbar_h) = (16.0, 14.0, 48.0);
        let text_w = width - 2.0 * pad;
        let elsewhere = self.renaming.is_some() || self.answering();
        let typing = ui.focused && self.menu.is_none() && self.modal.is_none() && !elsewhere;
        let composer = &mut self.composer;

        // With an input method composing, show its text at the caret.
        let cursor = composer.editor.cursor();
        let (shown, preedit) = match &composer.preedit {
            Some((text, _)) => {
                let mut shown = composer.editor.text().to_owned();
                shown.insert_str(cursor, text);
                (shown, Some(cursor..cursor + text.len()))
            }
            None => (composer.editor.text().to_owned(), None),
        };
        let layout = p.layout(&shown, theme::BODY, Some(text_w));
        let line_h = layout.line_height();
        let visible_h = layout.line_count().clamp(1, MAX_LINES) as f32 * line_h;
        let card_h = top_pad + visible_h + toolbar_h;
        let card = Rect::new(x, canvas.bottom() - 16.0 - card_h, width, card_h);
        composer.card = card;
        let text_area = Rect::new(card.x + pad, card.y + top_pad, text_w, visible_h);

        // Keep the caret inside the visible part of a tall draft.
        let caret_byte = preedit.as_ref().map_or(cursor, |range| {
            let within =
                composer.preedit.as_ref().and_then(|(_, c)| *c).map_or(range.end - range.start, |(_, end)| end);
            range.start + within
        });
        let (caret_x, caret_y) = layout.caret(caret_byte);
        composer.scroll = composer
            .scroll
            .clamp(caret_y + line_h - visible_h, caret_y)
            .clamp(0.0, (layout.height() - visible_h).max(0.0));
        let origin = (text_area.x, text_area.y - composer.scroll);
        composer.caret = Some(Rect::new(origin.0 + caret_x, origin.1 + caret_y, 2.0, line_h));

        // Mouse: click to place the caret, drag to select.
        let hit = |ui: &Ui| layout.hit(ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
        let text_zone = Rect::new(card.x, text_area.y - 4.0, card.w, text_area.h + 8.0);
        if ui.hovered(text_zone) {
            ui.cursor = CursorIcon::Text;
            if ui.pressed && preedit.is_none() {
                composer.editor.set_cursor(hit(ui), ui.mods.shift_key());
                if ui.clicks == 2 {
                    composer.editor.select_word();
                }
                composer.selecting = true;
                ui.last_edit = ui.time;
            }
        }
        if composer.selecting {
            if ui.down && ui.clicks < 2 {
                composer.editor.set_cursor(hit(ui), true);
            } else if !ui.down {
                composer.selecting = false;
            }
        }

        let focus = ui.anim(id("composer-focus"), f32::from(u8::from(typing)));
        let raised = Rect::new(card.x + 8.0, card.y + 10.0, card.w - 16.0, card.h);
        p.shadow(raised, fade(t.shadow, 0.8), theme::RADIUS_LG, 28.0);
        p.bordered(card, t.surface, theme::RADIUS_LG, 1.0, mix(t.border_strong, t.border_focus, focus * 0.6));

        let clip = p.push_clip(Rect::new(text_area.x - 2.0, text_area.y, text_area.w + 4.0, text_area.h));
        let selection = self.composer.editor.selection();
        if !selection.is_empty() && preedit.is_none() {
            for (x0, x1, y) in layout.selection_spans(selection.start, selection.end, false) {
                p.rect(Rect::new(origin.0 + x0, origin.1 + y, x1 - x0, line_h), t.selection, 2.0);
            }
        }
        if shown.is_empty() {
            p.label(&format!("Message {}", agent.name), theme::BODY, origin.0, origin.1, t.text_faint);
        } else {
            p.text(&layout, origin.0, origin.1, t.text);
        }
        // The input method's text is underlined until committed.
        if let Some(range) = &preedit {
            for (start, end, y) in layout.line_spans() {
                let (from, to) = (range.start.max(start), range.end.min(end));
                if from < to {
                    let (x0, x1) = (layout.caret(from).0, layout.caret(to).0);
                    p.rect(Rect::new(origin.0 + x0, origin.1 + y + line_h - 5.0, x1 - x0, 1.5), t.accent, 0.0);
                }
            }
        }
        if ui.caret_visible() && (selection.is_empty() || preedit.is_some()) && typing {
            p.rect(Rect::new(origin.0 + caret_x - 1.0, origin.1 + caret_y + 3.0, 2.0, line_h - 6.0), t.accent, 0.0);
        }
        p.set_clip(clip);
        self.composer.layout = if preedit.is_none() { Some((layout, origin)) } else { None };

        // Under the text: files to send along at the left, what the agent thinks with at the right.
        let item_y = card.bottom() - 9.0 - 30.0;
        let send = Rect::new(card.right() - 10.0 - 30.0, item_y, 30.0, 30.0);
        let mut room = send.x - 8.0;
        if let Some(connection) = self.connection(agent) {
            let model = self.model_in_use(&connection, &agent.model);
            // How hard it thinks rides on the model's chip, for models that take an effort.
            let effort = Self::effort_in_use(connection.kind, &model, &agent.effort);
            let label =
                if effort.is_empty() { model.label } else { format!("{} · {}", model.label, effort_label(&effort)) };
            let width = 58.0 + p.layout(&label, theme::SMALL, None).width().min(220.0);
            // The model wears its provider's mark.
            let model = self.toolbar_button(p, ui, (send.x - 6.0 - width, item_y), (None, &label), Menu::Model);
            self.provider_mark(p, &connection, (model.x + 17.0, model.y + 15.0), 15.0, t.text_muted);
            room = model.x - 8.0;
        }
        if icon_button(p, ui, Rect::new(card.x + 8.0, item_y, 30.0, 30.0), icon::PAPERCLIP, true) {
            actions.push(Action::PickFiles);
        }
        self.draw_attached(p, ui, (card.x + 44.0, item_y, room));

        // Send, or stop while the agent works. What is written then waits for its next step.
        let ready = !self.composer.editor.text().trim().is_empty() || !self.composer.files.is_empty();
        let busy = self.thread().is_some_and(|thread| thread.busy) && !ready;
        let hovered = ui.hovered(send) && (busy || ready);
        let hover = ui.anim(id("send"), f32::from(u8::from(hovered)));
        let center = (send.x + 15.0, send.y + 15.0);
        if busy {
            p.rect(send, mix(t.hover, t.active, hover), 15.0);
            p.rect(Rect::new(send.x + 10.0, send.y + 10.0, 10.0, 10.0), t.text, 2.5);
        } else if ready {
            p.rect(send, fade(t.accent, 1.0 - 0.12 * hover), 15.0);
            icon::draw(p, icon::ARROW_UP, center, 17.0, t.on_accent);
        } else {
            p.rect(send, t.hover, 15.0);
            icon::draw(p, icon::ARROW_UP, center, 17.0, t.text_faint);
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(send) {
                if busy {
                    self.stop(actions);
                } else {
                    let (text, files) = self.take_draft();
                    self.send(&text, false, files, actions);
                }
            }
        }

        // A session a usage limit stopped says when it goes on, right on top of the composer; what was sent while
        // the agent works waits above that, and a waiting approval on top of it all.
        let resuming = self.draw_resume(p, ui, (x, card.y - 8.0, width), actions);
        let waiting = self.draw_queue(p, ui, (x, resuming, width), actions);
        let top = if waiting < card.y - 8.0 { waiting } else { card.y };
        match self.thread().and_then(|thread| thread.asks.first()).cloned() {
            // A question is answered on a card of its own.
            Some(ask) if self.asks_question(&ask) => {
                let Some(mut form) = self.question.take() else { return top };
                let (top, verdict) = form.draw(p, ui, (x, waiting - 2.0, width), &ask.input, &agent.name);
                match verdict {
                    Some(verdict) => self.decide(verdict, actions),
                    None => self.question = Some(form),
                }
                top
            }
            Some(ask) => self.draw_ask(p, ui, (x, waiting - 2.0, width), &ask, &agent.name, actions),
            None => top,
        }
    }

    /// The files that go with the message, as pills from `x` to at most
    /// `right`: a click takes one off again.
    fn draw_attached(&mut self, p: &mut Painter, ui: &mut Ui, (mut x, y, right): (f32, f32, f32)) {
        let t = p.theme;
        let mut remove = None;
        for (index, file) in self.composer.files.iter().enumerate() {
            let mut name = p.layout(folder_name(file), theme::TINY, None);
            name.truncate(p.fonts, 130.0);
            // A picture shows itself, small, before its name.
            let picture = media_type(file).is_some();
            let lead = if picture { 22.0 } else { 0.0 };
            let pill = Rect::new(x, y + 3.0, name.width() + 34.0 + lead, 24.0);
            if pill.right() > right {
                // No room for the rest: how many more there are.
                let more = format!("+{}", self.composer.files.len() - index);
                p.label(&more, theme::TINY, x + 2.0, y + 8.0, t.text_faint);
                break;
            }
            let hovered = ui.hovered(pill);
            p.bordered(pill, if hovered { t.hover } else { t.bg }, 12.0, 1.0, t.border_strong);
            if picture {
                p.image(file, Rect::new(pill.x + 4.0, pill.y + 3.0, 18.0, 18.0), 6.0);
            }
            p.text(&name, pill.x + 10.0 + lead, pill.y + (pill.h - name.height()) * 0.5, t.text_muted);
            let cross = if hovered { t.text } else { t.text_faint };
            icon::draw(p, icon::X, (pill.right() - 13.0, pill.y + 12.0), 11.0, cross);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(pill) {
                    remove = Some(index);
                }
            }
            x = pill.right() + 6.0;
        }
        if let Some(index) = remove {
            self.composer.files.remove(index);
        }
    }

    /// Files were picked to go with the message (none when the picker was cancelled).
    pub fn files_picked(&mut self, files: Vec<String>) {
        self.show_draft();
        for file in files {
            if !self.composer.files.contains(&file) {
                self.composer.files.push(file);
            }
        }
    }

    /// Takes what was written and the files that go with it.
    pub(super) fn take_draft(&mut self) -> (String, Vec<String>) {
        (self.composer.editor.take(), std::mem::take(&mut self.composer.files))
    }

    /// Whether `ask` is a question of the agent's (not an approval), with
    /// a form ready for its answers.
    pub(super) fn asks_question(&mut self, ask: &Ask) -> bool {
        if ask.tool != question::TOOL {
            return false;
        }
        if self.question.as_ref().is_none_or(|form| form.ask != ask.id) {
            self.question = QuestionForm::new(ask);
        }
        self.question.is_some()
    }

    /// Whether a question of the agent's is waiting on the open session.
    pub(super) fn answering(&self) -> bool {
        let asked = self.thread().and_then(|thread| thread.asks.first());
        asked.is_some_and(|ask| self.question.as_ref().is_some_and(|form| form.ask == ask.id))
    }

    /// The commands and skills the message being written could start with:
    /// their names and a line on each. Empty unless the message is a slash
    /// and (so far) one word.
    pub(super) fn slash_matches(&self) -> Vec<(String, String)> {
        let typed = self.composer.editor.text();
        let Some(needle) = typed.strip_prefix('/').filter(|rest| !rest.contains(char::is_whitespace)) else {
            return Vec::new();
        };
        if self.composer.slash_off {
            return Vec::new();
        }
        let needle = needle.to_lowercase();
        let mut commands = vec![("clear".to_owned(), "Empty this session and start with a fresh context".to_owned())];
        let connection = self.selected_agent().and_then(|agent| self.connection(agent));
        // An API agent is summarised by the app, and has the library's skills.
        if connection.as_ref().is_some_and(|connection| !connection.kind.is_account()) {
            commands.push(("compact".to_owned(), "Summarise the conversation to free up its context".to_owned()));
            commands.extend(self.skills.installed.iter().map(|skill| {
                let line = skill.description.lines().next().unwrap_or_default();
                (skill.name.clone(), line.chars().take(160).collect())
            }));
        }
        if let Some(connection) = connection.filter(|connection| connection.kind == ConnectionKind::ClaudeAgent) {
            commands.push(("compact".to_owned(), "Summarise the conversation to free up its context".to_owned()));
            if let Some(Account::Ready(info)) = self.claude.get(&connection.id) {
                let own = |name: &str| commands.iter().any(|(known, _)| known == name);
                let listed: Vec<_> = info.commands.iter().filter(|command| !own(&command.name)).collect();
                commands.extend(listed.into_iter().map(|command| {
                    let line = command.description.lines().next().unwrap_or_default();
                    (command.name.clone(), line.chars().take(160).collect())
                }));
            }
        }
        let mut found: Vec<_> =
            commands.into_iter().filter(|(name, _)| name.to_lowercase().contains(&needle)).collect();
        // What starts with the typed letters comes before what only holds them.
        found.sort_by_key(|(name, _)| !name.to_lowercase().starts_with(&needle));
        found.truncate(60);
        found
    }

    /// A key while the list of commands is up: arrows move, Tab or Enter
    /// takes the picked one, Esc puts the list away. Returns whether the
    /// key was the list's.
    pub(super) fn slash_key(&mut self, event: &KeyEvent) -> bool {
        let typed = self.composer.editor.text().to_owned();
        if !typed.starts_with('/') {
            self.composer.slash_off = false;
        }
        let found = self.slash_matches();
        if found.is_empty() {
            return false;
        }
        let picked = self.composer.slash.min(found.len() - 1);
        match &event.logical_key {
            Key::Named(NamedKey::ArrowUp) => self.composer.slash = picked.saturating_sub(1),
            Key::Named(NamedKey::ArrowDown) => self.composer.slash = (picked + 1).min(found.len() - 1),
            Key::Named(NamedKey::Escape) => self.composer.slash_off = true,
            // The command already written out in full is sent as it is.
            Key::Named(NamedKey::Enter) if typed[1..] == found[picked].0 => return false,
            Key::Named(NamedKey::Enter | NamedKey::Tab) => self.take_command(&found[picked].0),
            _ => {
                self.composer.slash = 0;
                return false;
            }
        }
        true
    }

    /// Writes the command `name` into the message box, ready for what it takes.
    fn take_command(&mut self, name: &str) {
        self.composer.editor.take();
        self.composer.editor.insert(&format!("/{name} "));
        self.composer.slash = 0;
    }

    /// The `@` word being written at the caret: where it starts and what follows the `@`.
    fn mention(&self) -> Option<(usize, &str)> {
        let editor = &self.composer.editor;
        mention_at(editor.text(), editor.cursor()).filter(|_| editor.selection().is_empty())
    }

    /// The message changed: the files of the session's folder are listed
    /// once for each `@` word (anew each time, as files come and go).
    pub(super) fn mention_typed(&mut self, actions: &mut Vec<Action>) {
        let Some(start) = self.mention().map(|(start, _)| start) else {
            (self.composer.mention_off, self.composer.listed_at) = (false, None);
            return;
        };
        let folder = self.work_folder();
        let elsewhere = self.composer.listing.0 != folder;
        if elsewhere {
            self.composer.listing = (folder.clone(), Vec::new());
        }
        if elsewhere || self.composer.listed_at != Some(start) {
            self.composer.listed_at = Some(start);
            actions.push(Action::ListFiles(folder));
        }
    }

    /// The files of `folder` were listed.
    pub fn files_listed(&mut self, folder: &str, files: Vec<String>) {
        if folder == self.composer.listing.0 {
            let searchable = |path: String| {
                let lower = path.to_lowercase();
                (path, lower)
            };
            self.composer.listing.1 = files.into_iter().map(searchable).collect();
        }
    }

    /// The files the `@` word being written could name: those whose name
    /// starts with it, then those whose name holds it, then those whose
    /// path does. Empty when no such word is being written.
    fn mention_matches(&self) -> Vec<String> {
        let Some((_, needle)) = self.mention().filter(|_| !self.composer.mention_off) else { return Vec::new() };
        let needle = needle.to_lowercase();
        let mut found: Vec<_> = self.composer.listing.1.iter().filter(|(_, lower)| lower.contains(&needle)).collect();
        found.sort_by_cached_key(|(path, lower)| {
            let name = lower.rsplit('/').next().unwrap_or(lower);
            (!name.starts_with(&needle), !name.contains(&needle), path.len())
        });
        found.into_iter().take(60).map(|(path, _)| path.clone()).collect()
    }

    /// A key while the list of files is up: arrows move, Tab or Enter takes
    /// the picked one, Esc puts the list away. Returns whether the key was
    /// the list's.
    pub(super) fn mention_key(&mut self, event: &KeyEvent) -> bool {
        let found = self.mention_matches();
        if found.is_empty() {
            return false;
        }
        let picked = self.composer.mention.min(found.len() - 1);
        match &event.logical_key {
            Key::Named(NamedKey::ArrowUp) => self.composer.mention = picked.saturating_sub(1),
            Key::Named(NamedKey::ArrowDown) => self.composer.mention = (picked + 1).min(found.len() - 1),
            Key::Named(NamedKey::Escape) => self.composer.mention_off = true,
            Key::Named(NamedKey::Enter | NamedKey::Tab) => self.take_mention(&found[picked]),
            _ => {
                self.composer.mention = 0;
                return false;
            }
        }
        true
    }

    /// Writes the file `path` where the `@` word is, in quotes when it holds a space.
    fn take_mention(&mut self, path: &str) {
        let Some((start, _)) = self.mention() else { return };
        let editor = &mut self.composer.editor;
        let end = editor.cursor();
        editor.set_cursor(start, false);
        editor.set_cursor(end, true);
        editor.insert(&if path.contains(char::is_whitespace) { format!("@\"{path}\" ") } else { format!("@{path} ") });
        self.composer.mention = 0;
    }

    /// The list of commands and skills, over the message box, while a slash
    /// is being typed, or of files while an `@` word is.
    pub(super) fn draw_slash(&mut self, p: &mut Painter, ui: &mut Ui) {
        if self.modal.is_some() {
            return;
        }
        let commands = self.slash_matches();
        if !commands.is_empty() {
            if let Some(index) = self.draw_list(p, ui, &commands, self.composer.slash, icon::SLASH) {
                self.take_command(&commands[index].0);
            }
            return;
        }
        // A file's name, then faintly where it is.
        let files = self.mention_matches();
        let rows: Vec<_> = files
            .iter()
            .map(|path| {
                path.rsplit_once('/').map_or((path.clone(), String::new()), |(dir, name)| (name.into(), dir.into()))
            })
            .collect();
        if !rows.is_empty()
            && let Some(index) = self.draw_list(p, ui, &rows, self.composer.mention, icon::FILE)
        {
            self.take_mention(&files[index]);
        }
    }

    /// A list over the message box: each row a name and a line on it, the
    /// `picked` one lit. Returns the row that was clicked.
    fn draw_list(
        &self,
        p: &mut Painter,
        ui: &mut Ui,
        found: &[(String, String)],
        picked: usize,
        glyph: char,
    ) -> Option<usize> {
        let t = p.theme;
        let picked = picked.min(found.len().saturating_sub(1));
        // The picked row stays in view: the window of rows moves with it.
        let first = picked.saturating_sub(SHOWN - 1).min(found.len().saturating_sub(SHOWN));
        let rows = found.len().min(SHOWN);
        let card = self.composer.card;
        let list =
            Rect::new(card.x, card.y - 10.0 - (rows as f32 * ROW + 12.0), card.w.min(600.0), rows as f32 * ROW + 12.0);
        p.shadow(Rect::new(list.x, list.y + 8.0, list.w, list.h), t.shadow, theme::RADIUS, 24.0);
        p.bordered(list, t.surface, theme::RADIUS, 1.0, t.border_strong);
        let mut take = None;
        for (index, (name, about)) in found.iter().enumerate().skip(first).take(SHOWN) {
            let row = Rect::new(list.x + 6.0, list.y + 6.0 + (index - first) as f32 * ROW, list.w - 12.0, ROW - 2.0);
            let hovered = row.contains(ui.mouse);
            if index == picked || hovered {
                p.rect(row, if index == picked { t.hover } else { fade(t.hover, 0.5) }, theme::RADIUS_SM);
            }
            let mid = row.y + row.h * 0.5;
            icon::draw(p, glyph, (row.x + 18.0, mid), 14.0, t.text_muted);
            let mut command = p.layout(name, theme::LABEL, None);
            command.truncate(p.fonts, row.w * 0.5);
            p.text(&command, row.x + 34.0, mid - command.height() * 0.5, t.text);
            let mut line = p.layout(about, theme::TINY, None);
            line.truncate(p.fonts, row.w - command.width() - 62.0);
            p.text(&line, row.x + 46.0 + command.width(), mid - line.height() * 0.5 + 0.5, t.text_faint);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.released && row.contains(ui.press_pos) {
                    take = Some(index);
                }
            }
        }
        take
    }

    /// A ghost button at `(x, y)` with an icon (or room for a mark the
    /// caller draws), a label and a chevron, that toggles `menu`. Returns
    /// its rect.
    pub(super) fn toolbar_button(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y): (f32, f32),
        (glyph, label): (Option<char>, &str),
        menu: Menu,
    ) -> Rect {
        let t = p.theme;
        let mut text = p.layout(label, theme::SMALL, None);
        text.truncate(p.fonts, 220.0);
        let rect = Rect::new(x, y, 32.0 + text.width() + 26.0, 30.0);
        let open = self.menu_open(menu);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id(("toolbar", menu)), f32::from(u8::from(hovered || open)));
        p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
        let color = mix(t.text_muted, t.text, hover);
        let mid = y + 15.0;
        if let Some(glyph) = glyph {
            icon::draw(p, glyph, (rect.x + 17.0, mid), 15.0, color);
        }
        p.text(&text, rect.x + 32.0, mid - text.height() * 0.5, color);
        icon::draw(p, icon::CHEVRON_DOWN, (rect.right() - 14.0, mid), 13.0, fade(color, 0.7));
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                self.toggle_menu(menu, rect);
            }
        }
        rect
    }

    /// The approval card ending at `bottom`: what the agent wants to do and
    /// the three answers. Returns its top edge.
    fn draw_ask(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, bottom, width): (f32, f32, f32),
        ask: &Ask,
        agent: &str,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let about = if ask.summary.is_empty() { ask.input.as_str() } else { ask.summary.as_str() };
        let about = p.layout(about, Style::mono(12.5), Some(width - 64.0));
        let about_h = about.height().min(4.0 * about.line_height());
        let card = Rect::new(x, bottom - (58.0 + about_h + 52.0), width, 58.0 + about_h + 52.0);
        let appear = ui.appear(id(("ask", &ask.id)));
        let raised = Rect::new(card.x + 8.0, card.y + 8.0, card.w - 16.0, card.h);
        p.shadow(raised, fade(t.shadow, 0.7), theme::RADIUS_LG, 22.0);
        p.bordered(card, t.surface, theme::RADIUS_LG, 1.0, mix(t.border_strong, t.border_focus, appear));
        icon::draw(p, icon::SHIELD, (card.x + 24.0, card.y + 25.0), 16.0, t.text);
        let title = format!("{agent} wants to use {}", ask.tool);
        p.label(&title, theme::LABEL, card.x + 42.0, card.y + 16.0, t.text);
        if !ask.reason.is_empty() {
            let mut reason = p.layout(&ask.reason, theme::TINY, None);
            reason.truncate(p.fonts, width - 64.0);
            p.text(&reason, card.x + 42.0, card.y + 35.0, t.text_faint);
        }
        let frame = Rect::new(card.x + 16.0, card.y + 54.0, width - 32.0, about_h + 12.0);
        p.bordered(frame, t.code_bg, theme::RADIUS_SM, 1.0, t.border);
        let clip = p.push_clip(frame);
        p.text(&about, frame.x + 10.0, frame.y + 6.0, t.text_muted);
        p.set_clip(clip);

        let y = card.bottom() - 42.0;
        let mut right = card.right() - 16.0;
        let mut verdict = None;
        for (label, w, style, answer) in [
            ("Allow  ↵", 92.0, ButtonStyle::Primary, Verdict::Allow),
            ("Always allow", 112.0, ButtonStyle::Secondary, Verdict::AllowAlways),
            ("Deny  esc", 92.0, ButtonStyle::Ghost, Verdict::Deny(String::new())),
        ] {
            right -= w;
            if button(p, ui, Rect::new(right, y, w, 30.0), label, style, true) {
                verdict = Some(answer);
            }
            right -= 8.0;
        }
        if let Some(verdict) = verdict {
            self.decide(verdict, actions);
        }
        card.y - 6.0
    }
}

/// The `@` word that ends at `cursor` in `text`, if one is being written
/// there: where its `@` is and what follows it. An `@` only opens one at
/// the start of the text or after a space, so an email address is left alone.
fn mention_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let start = before.rfind('@')?;
    let needle = &before[start + 1..];
    let opens = before[..start].chars().next_back().is_none_or(char::is_whitespace);
    (opens && !needle.contains(char::is_whitespace)).then_some((start, needle))
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, Config};

    use super::*;

    #[test]
    fn each_session_keeps_what_was_written_for_it() {
        let mut composer = Composer::default();
        composer.editor.insert("typed before anything opened");
        composer.show("a");
        assert_eq!(composer.editor.text(), "typed before anything opened", "the first owner keeps it");
        composer.files.push("notes.md".into());
        composer.show("b");
        assert!(composer.editor.text().is_empty() && composer.files.is_empty(), "another session starts empty");
        composer.editor.insert("for b");
        composer.show(&next_of("scout"));
        assert!(composer.editor.text().is_empty());
        composer.show("a");
        assert_eq!(
            (composer.editor.text(), composer.files.as_slice()),
            ("typed before anything opened", &["notes.md".to_owned()][..])
        );
        composer.forget("b");
        composer.show("b");
        assert!(composer.editor.text().is_empty(), "a forgotten draft is gone");
        // What was sent leaves nothing behind to come back.
        composer.show("a");
        composer.editor.take();
        composer.files.clear();
        composer.show("c");
        assert!(!composer.drafts.contains_key("a"));
    }

    #[test]
    fn an_at_word_is_found_at_the_caret() {
        assert_eq!(mention_at("@", 1), Some((0, "")));
        assert_eq!(mention_at("see @src/ma and", 11), Some((4, "src/ma")));
        assert_eq!(mention_at("see @src/ma and", 15), None, "the caret left the word");
        assert_eq!(mention_at("mail me@example.com", 19), None, "not after a space");
        assert_eq!(mention_at("é@x", 1), None, "a caret inside a character finds nothing");
        assert_eq!(mention_at("no at here", 5), None);
    }

    #[test]
    fn a_file_is_picked_from_the_list_an_at_opens() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        shell.pending_folder = "/work".into();
        let mut actions = Vec::new();
        shell.composer.editor.insert("fix @");
        shell.mention_typed(&mut actions);
        assert!(matches!(&actions[..], [Action::ListFiles(folder)] if folder == "/work"));
        shell.files_listed("/elsewhere", vec!["lost.rs".into()]);
        assert!(shell.mention_matches().is_empty(), "another folder's files are dropped");
        shell.files_listed("/work", vec!["src/main.rs".into(), "docs/my main notes.md".into(), "README.md".into()]);
        assert_eq!(shell.mention_matches().len(), 3);
        shell.composer.editor.insert("MAIN");
        // The name that starts with what was typed comes first.
        assert_eq!(shell.mention_matches(), ["src/main.rs", "docs/my main notes.md"]);
        shell.take_mention("src/main.rs");
        assert_eq!(shell.composer.editor.text(), "fix @src/main.rs ");
        assert!(shell.mention_matches().is_empty());
        shell.composer.editor.insert("@notes");
        shell.take_mention("docs/my main notes.md");
        assert_eq!(shell.composer.editor.text(), "fix @src/main.rs @\"docs/my main notes.md\" ");
    }
}
