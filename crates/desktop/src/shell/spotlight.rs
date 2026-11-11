//! The spotlight: one field over everything that finds sessions (by title
//! at once, by what was said in them a moment later), agents and things to
//! do. Arrows move, Enter opens, Esc closes.

use std::sync::atomic::{AtomicU64, Ordering};

use arboard::Clipboard;
use atroxabot_core::SearchHit;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::Shell;
use super::widgets::{Scroll, line_field};
use crate::app::Action;
use crate::editor::Editor;
use crate::orb::{self, Mood};
use crate::paint::{Painter, Rect, fade};
use crate::theme;
use crate::ui::{Ui, edit_key, icon, id};

/// Height of a result's row.
const ROW: f32 = 46.0;
/// Rows shown before the list scrolls.
const VISIBLE: usize = 8;
/// Most results listed.
const MOST: usize = 40;
/// Sessions offered before anything is typed.
const RECENT: usize = 6;
/// Counts searches across every spotlight, so a late answer is never taken for a newer one's.
static SEARCHES: AtomicU64 = AtomicU64::new(0);

/// What a result opens or does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    /// A session of an agent.
    Session {
        /// The agent's id.
        agent: String,
        /// The session's id.
        session: String,
    },
    /// An agent, by id.
    Agent(String),
    /// Start a session with the selected agent.
    NewSession,
    /// Make an agent.
    NewAgent,
    /// The usage statistics.
    Stats,
    /// The settings.
    Settings,
    /// Show or hide the side panel.
    Panel,
}

/// How the spotlight was left.
pub(super) enum Outcome {
    /// With nothing chosen.
    Close,
    /// With this to open.
    Run(Target),
}

/// One row of results.
struct Entry {
    target: Target,
    /// Shown when the row is not an agent's.
    glyph: char,
    /// The agent whose face the row wears, by id.
    agent: Option<String>,
    title: String,
    detail: String,
    kind: &'static str,
}

/// The open spotlight.
#[derive(Default)]
pub(super) struct Spotlight {
    editor: Editor,
    /// The row Enter opens.
    selected: usize,
    /// Sessions whose messages hold the text, from the last search that came back.
    hits: Vec<SearchHit>,
    /// The search `hits` may come from; an earlier one's late answer is dropped.
    generation: u64,
    scroll: Scroll,
    /// Where the mouse was last frame: only a moving mouse selects.
    mouse: (f32, f32),
}

impl Spotlight {
    /// What matches what is typed: sessions, then agents, then things to do.
    fn entries(&self, shell: &Shell) -> Vec<Entry> {
        let needle = self.editor.text().trim().to_lowercase();
        let fits = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
        let name = |agent: &str| shell.agents.iter().find(|a| a.id == agent).map_or("", |a| a.name.as_str());
        let session = |agent: &str, session: &str, title: &str, said: &str| Entry {
            target: Target::Session { agent: agent.to_owned(), session: session.to_owned() },
            glyph: icon::MESSAGE,
            agent: Some(agent.to_owned()),
            title: if title.is_empty() { "New session".to_owned() } else { title.to_owned() },
            detail: if said.is_empty() { name(agent).to_owned() } else { format!("{} · {said}", name(agent)) },
            kind: "Session",
        };
        let most = if needle.is_empty() { RECENT } else { MOST };
        // The latest first, whatever order the sidebar has them in.
        let mut titled: Vec<_> = shell.sessions.iter().filter(|s| fits(&s.title)).collect();
        titled.sort_by_key(|s| std::cmp::Reverse(s.updated));
        let titled = titled.into_iter().take(most);
        let mut entries: Vec<Entry> = titled.map(|s| session(&s.agent, &s.id, &s.title, &s.preview)).collect();
        for hit in &self.hits {
            let listed = |e: &Entry| matches!(&e.target, Target::Session { session, .. } if *session == hit.session);
            if !entries.iter().any(listed) {
                entries.push(session(&hit.agent, &hit.session, &hit.title, &hit.snippet));
            }
        }
        entries.extend(shell.agents.iter().filter(|agent| fits(&agent.name)).map(|agent| Entry {
            target: Target::Agent(agent.id.clone()),
            glyph: icon::BOT,
            agent: Some(agent.id.clone()),
            title: agent.name.clone(),
            detail: agent.tagline.clone(),
            kind: "Agent",
        }));
        let actions = [
            (Target::NewSession, icon::PLUS, "New session"),
            (Target::NewAgent, icon::BOT, "New agent"),
            (Target::Stats, icon::CHART, "Usage statistics"),
            (Target::Settings, icon::SETTINGS, "Settings"),
            (Target::Panel, icon::PANEL, "Show or hide the side panel"),
        ];
        entries.extend(actions.into_iter().filter(|(_, _, title)| fits(title)).map(|(target, glyph, title)| Entry {
            target,
            glyph,
            agent: None,
            title: title.to_owned(),
            detail: String::new(),
            kind: "Action",
        }));
        entries.truncate(MOST);
        entries
    }

    /// What was typed changed from `before`: the messages are searched off
    /// this thread, while titles match at once.
    fn changed(&mut self, before: &str, actions: &mut Vec<Action>) {
        let query = self.editor.text().trim().to_owned();
        if query == before.trim() {
            return;
        }
        self.generation = SEARCHES.fetch_add(1, Ordering::Relaxed) + 1;
        self.hits.clear();
        self.selected = 0;
        self.scroll.reset();
        if query.chars().count() >= 2 {
            actions.push(Action::Search { query, generation: self.generation });
        }
    }

    /// Sessions whose messages hold the searched text were found.
    pub fn found(&mut self, generation: u64, hits: Vec<SearchHit>) {
        if generation == self.generation {
            self.hits = hits;
        }
    }

    /// Text committed by an input method.
    pub fn insert(&mut self, text: &str, actions: &mut Vec<Action>) {
        let before = self.editor.text().to_owned();
        self.editor.insert(text);
        self.changed(&before, actions);
    }

    /// A key: arrows move, Enter opens the selected row, Esc leaves, the rest edits.
    pub fn key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<Clipboard>,
        shell: &Shell,
        actions: &mut Vec<Action>,
    ) -> Option<Outcome> {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => return Some(Outcome::Close),
            Key::Named(NamedKey::Enter) => {
                return self.entries(shell).into_iter().nth(self.selected).map(|entry| Outcome::Run(entry.target));
            }
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) => {
                let last = self.entries(shell).len().saturating_sub(1);
                self.selected = if *key == NamedKey::ArrowUp {
                    self.selected.saturating_sub(1)
                } else {
                    (self.selected + 1).min(last)
                };
                let top = self.selected as f32 * ROW;
                self.scroll.reveal(top, top + ROW + 12.0, VISIBLE as f32 * ROW + 12.0);
            }
            _ => {
                let before = self.editor.text().to_owned();
                edit_key(&mut self.editor, event, mods, clipboard);
                self.changed(&before, actions);
            }
        }
        None
    }

    /// Draws the spotlight over `view`. Returns how it was left, if it was.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, shell: &Shell) -> Option<Outcome> {
        let t = p.theme;
        let entries = self.entries(shell);
        self.selected = self.selected.min(entries.len().saturating_sub(1));
        let appear = ui.appear(id("spotlight"));
        p.rect(view, fade(t.scrim, appear), 0.0);
        let width = 640.0_f32.min(view.w - 48.0);
        let list_h = entries.len().clamp(1, VISIBLE) as f32 * ROW + 12.0;
        let card = Rect::new(
            (view.x + (view.w - width) * 0.5).round(),
            (view.y + (view.h * 0.16).max(64.0) - 8.0 * (1.0 - appear)).round(),
            width,
            55.0 + list_h + 34.0,
        );
        p.shadow(Rect::new(card.x, card.y + 16.0, card.w, card.h), fade(t.shadow, appear), theme::RADIUS_LG, 48.0);
        p.bordered(card, t.surface, theme::RADIUS_LG, 1.0, t.border_strong);
        let mut outcome = None;
        if ui.released && !card.contains(ui.mouse) && !card.contains(ui.press_pos) {
            outcome = Some(Outcome::Close);
        }

        icon::draw(p, icon::SEARCH, (card.x + 25.0, card.y + 27.0), 16.0, t.text_muted);
        let field = Rect::new(card.x + 46.0, card.y + 10.0, card.w - 64.0, 34.0);
        line_field(p, ui, field, &mut self.editor, ("Search sessions, agents and actions", theme::BODY), true);
        p.rect(Rect::new(card.x + 1.0, card.y + 54.0, card.w - 2.0, 1.0), t.border, 0.0);

        let list = Rect::new(card.x, card.y + 55.0, card.w, list_h);
        let moved = ui.mouse != self.mouse;
        self.mouse = ui.mouse;
        let offset = self.scroll.update(ui, list.contains(ui.mouse));
        let clip = p.push_clip(list);
        if entries.is_empty() {
            p.label("Nothing found", theme::SMALL, list.x + 24.0, list.y + 20.0, t.text_faint);
        }
        let mut y = list.y + 6.0 - offset;
        for (index, entry) in entries.iter().enumerate() {
            let row = Rect::new(list.x + 6.0, y, list.w - 12.0, ROW - 2.0);
            y += ROW;
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hovered = !ui.held && row.contains(ui.mouse) && list.contains(ui.mouse);
            if hovered && moved {
                self.selected = index;
            }
            if index == self.selected {
                p.rect(row, t.hover, theme::RADIUS_SM);
            }
            let mid = row.y + row.h * 0.5;
            match entry.agent.as_deref().and_then(|agent| shell.agents.iter().find(|a| a.id == agent)) {
                Some(agent) => {
                    orb::agent(p, &agent.portrait, (row.x + 22.0, mid), 24.0, Shell::color_of(agent), Mood::Idle, 0.0);
                }
                None => icon::draw(p, entry.glyph, (row.x + 22.0, mid), 16.0, t.text_muted),
            }
            let kind = p.layout(entry.kind, theme::TINY, None);
            p.text(&kind, row.right() - 12.0 - kind.width(), mid - kind.height() * 0.5, t.text_faint);
            let room = row.w - 68.0 - kind.width();
            let mut title = p.layout(&entry.title, theme::SMALL, None);
            title.truncate(p.fonts, room);
            if entry.detail.is_empty() {
                p.text(&title, row.x + 44.0, mid - title.height() * 0.5, t.text);
            } else {
                p.text(&title, row.x + 44.0, row.y + 5.0, t.text);
                let mut detail = p.layout(&entry.detail, theme::TINY, None);
                detail.truncate(p.fonts, room);
                p.text(&detail, row.x + 44.0, row.y + 24.0, t.text_faint);
            }
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.released && list.contains(ui.press_pos) {
                    outcome = Some(Outcome::Run(entry.target.clone()));
                }
            }
        }
        p.set_clip(clip);
        let content = entries.len() as f32 * ROW + 12.0;
        self.scroll.finish(ui, content, list.h);
        self.scroll.bar(p, ui, list, content);

        p.rect(Rect::new(card.x + 1.0, list.bottom(), card.w - 2.0, 1.0), t.border, 0.0);
        let keys = "↑↓ to move      ↵ to open      esc to close";
        p.label(keys, theme::TINY, card.x + 18.0, list.bottom() + 10.0, t.text_faint);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, Config, Session};

    use super::*;

    #[test]
    fn sessions_agents_and_actions_are_found() {
        let (scout, press) = (Agent::new("Scout"), Agent::new("Press"));
        let mut session = Session::new(&scout.id);
        session.prompt("Review the parser", "");
        let shell = Shell::new(Config::default(), vec![scout.clone(), press], vec![session.summary()]);
        let mut spotlight = Spotlight::default();
        let kinds = |spotlight: &Spotlight| spotlight.entries(&shell).iter().map(|e| e.kind).collect::<Vec<_>>();
        assert_eq!(kinds(&spotlight), ["Session", "Agent", "Agent", "Action", "Action", "Action", "Action", "Action"]);

        let mut actions = Vec::new();
        spotlight.insert("pars", &mut actions);
        assert!(
            matches!(&actions[..], [Action::Search { query, .. }] if query == "pars"),
            "the messages are searched too"
        );
        assert_eq!(kinds(&spotlight), ["Session"], "the title matches at once");
        // A session found by what was said in it joins the list, once; a late answer does not.
        let hit = |id: &str| SearchHit {
            session: id.into(),
            agent: scout.id.clone(),
            snippet: "parser".into(),
            ..SearchHit::default()
        };
        spotlight.found(spotlight.generation, vec![hit(&session.id), hit("other")]);
        assert_eq!(kinds(&spotlight), ["Session", "Session"]);
        spotlight.insert("x", &mut actions);
        spotlight.found(spotlight.generation - 1, vec![hit("late")]);
        assert!(spotlight.entries(&shell).is_empty());

        let mut typed = Spotlight::default();
        typed.insert("set", &mut actions);
        assert_eq!(typed.entries(&shell)[0].target, Target::Settings);
    }
}
