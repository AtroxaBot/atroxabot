//! The settings, a modal with a section per page: how the app looks, the
//! providers agents think through (Claude accounts and API providers), the
//! voice providers, the integrations (MCP servers, and what Claude
//! accounts bring of their own), and the skills (`skills.rs`).
//!
//! Adding something is always the same three steps: a button, a choice of
//! what to add, then only the fields that choice needs.

#![expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]

use arboard::Clipboard;
use atroxabot_core::{ClaudeMcpServer, Connection, ConnectionKind, McpServer, new_id};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::menu::Menu;
use super::skills::SkillsPage;
use super::widgets::{Scroll, chip, dropdown, line_field, logo_badge, modal, row, tile, toggle};
use super::{Account, Connectors, Shell};
use crate::app::Action;
use crate::editor::Editor;
use crate::form::Fields;
use crate::logo::Logo;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme::{self, Scheme};
use crate::tray::CloseAction;
use crate::ui::{ButtonStyle, Ui, button, icon, icon_button, id};

/// The modal's size.
const SIZE: (f32, f32) = (940.0, 660.0);
/// Width of the list of pages at its left.
const NAV: f32 = 196.0;
/// The modal's text fields, by index.
const NAME: usize = 0;
const URL: usize = 1;
const KEY: usize = 2;
const VARS: usize = 3;

/// A page of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    /// How the app looks, and where its data is.
    General,
    /// Claude accounts and API providers.
    Providers,
    /// Voice providers for calls.
    Voice,
    /// Integrations: MCP servers, and each Claude account's own.
    Integrations,
    /// The skills every agent has.
    Skills,
}

impl Tab {
    const ALL: [Self; 5] = [Self::General, Self::Providers, Self::Voice, Self::Integrations, Self::Skills];

    fn label(self) -> (&'static str, char) {
        match self {
            Self::General => ("General", icon::SETTINGS),
            Self::Providers => ("Providers", icon::PLUG),
            Self::Voice => ("Voice", icon::AUDIO),
            Self::Integrations => ("Integrations", icon::LINK),
            Self::Skills => ("Skills", icon::SPARKLES),
        }
    }
}

/// An API provider offered as a start: what it speaks, its name, its
/// address, and a line about it. The last one is the user's own.
const PRESETS: [(ConnectionKind, &str, &str, &str); 6] = [
    (ConnectionKind::ClaudeApi, "Anthropic", "https://api.anthropic.com/v1", "Claude, with an API key"),
    (ConnectionKind::OpenAi, "OpenRouter", "https://openrouter.ai/api/v1", "Many models, one key"),
    (ConnectionKind::OpenAi, "Inworld", "https://api.inworld.ai/v1", "Inworld's model router"),
    (ConnectionKind::OpenAi, "OpenAI", "https://api.openai.com/v1", "GPT models"),
    (ConnectionKind::OpenAi, "Ollama", "http://localhost:11434/v1", "Models on this computer"),
    (ConnectionKind::OpenAi, "Custom", "", "Any compatible address"),
];
/// The preset the user fills in themselves.
const CUSTOM: usize = PRESETS.len() - 1;

/// The voice providers: what the config calls them, their name, and a line about them.
const VOICE_PROVIDERS: [(&str, &str, &str); 2] =
    [("fish", "Fish Audio", "Expressive voices and clones"), ("elevenlabs", "ElevenLabs", "Fast, natural voices")];

/// What is being added, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Adding {
    Nothing,
    /// A Claude account: it only needs a name.
    ClaudeAccount,
    /// An API provider: which one is being chosen.
    PickProvider,
    /// The API provider with this preset: its key (and, for a custom one, its name and address).
    Provider(usize),
    /// A voice provider: which one is being chosen.
    PickVoice,
    /// The voice provider with this index: its key.
    Voice(usize),
    /// An MCP server, local (`false`) or remote (`true`).
    Mcp(bool),
}

/// The open settings.
pub(super) struct Settings {
    tab: Tab,
    fields: Fields,
    adding: Adding,
    /// What a custom API provider speaks.
    custom_kind: ConnectionKind,
    /// The provider being renamed (by id), and its name as typed so far.
    renaming: Option<(String, Editor)>,
    /// Enter was pressed while renaming: the name is kept on the next frame.
    renamed: bool,
    scroll: Scroll,
    /// The skills page's own state.
    skills: SkillsPage,
}

impl Settings {
    /// The settings, opened at `tab`. Saved keys are never shown: a key's
    /// field only exists while one is being added.
    pub fn new(tab: Tab) -> Self {
        let empty = Editor::default;
        Self {
            tab,
            fields: Fields::new([empty(), empty(), empty(), empty()], [false, false, false, true]).secret(KEY),
            adding: Adding::Nothing,
            custom_kind: ConnectionKind::OpenAi,
            renaming: None,
            renamed: false,
            scroll: Scroll::default(),
            skills: SkillsPage::new(),
        }
    }

    /// Starts (or with [`Adding::Nothing`] ends) adding something: its
    /// fields start empty, the first of them with the keyboard.
    pub(super) fn add(&mut self, adding: Adding) {
        self.adding = adding;
        for index in [NAME, URL, KEY, VARS] {
            self.fields.replace(index, "");
        }
        if let Some(first) = self.visible().first() {
            self.fields.focus(*first);
        }
    }

    /// Opens a repository on the skills page, as a click on it would.
    #[cfg(test)]
    pub(super) fn open_skills(&mut self, source: atroxabot_core::skills::Source) {
        self.skills.show(source);
    }

    /// The fields on screen now, in order: Tab moves among these.
    fn visible(&self) -> Vec<usize> {
        match (self.tab, self.adding) {
            (Tab::Providers, Adding::ClaudeAccount) => vec![NAME],
            (Tab::Providers, Adding::Provider(CUSTOM)) => vec![NAME, URL, KEY],
            (Tab::Providers, Adding::Provider(_)) | (Tab::Voice, Adding::Voice(_)) => vec![KEY],
            (Tab::Integrations, Adding::Mcp(_)) => vec![NAME, URL, VARS],
            _ => Vec::new(),
        }
    }

    /// A key: Esc steps back out of what is being added, then leaves
    /// (returns `true`); Tab moves between the fields shown; the rest edits.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<Clipboard>) -> bool {
        // A provider's name being changed takes the keys: Enter keeps it, Esc drops it.
        if let Some((_, editor)) = &mut self.renaming {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => self.renaming = None,
                Key::Named(NamedKey::Enter) => self.renamed = true,
                _ => {
                    crate::ui::edit_key(editor, event, mods, clipboard);
                }
            }
            return false;
        }
        if self.tab == Tab::Skills {
            return self.skills.key(event, mods, clipboard);
        }
        let visible = self.visible();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) if self.adding != Adding::Nothing => self.add(Adding::Nothing),
            Key::Named(NamedKey::Escape) => return true,
            Key::Named(NamedKey::Tab) => {
                let at = visible.iter().position(|index| *index == self.fields.focused()).unwrap_or(0);
                let step = if mods.shift_key() { visible.len().saturating_sub(1) } else { 1 };
                if let Some(next) = visible.get((at + step) % visible.len().max(1)) {
                    self.fields.focus(*next);
                }
            }
            // Only a field on screen takes what is typed.
            _ if visible.contains(&self.fields.focused()) => {
                self.fields.key(event, mods, clipboard);
            }
            _ => {}
        }
        false
    }

    /// Text committed by an input method.
    pub fn commit(&mut self, text: &str) {
        if let Some((_, editor)) = &mut self.renaming {
            editor.insert(text);
        } else if self.tab == Tab::Skills {
            self.skills.commit(text);
        } else if self.visible().contains(&self.fields.focused()) {
            self.fields.insert(text);
        }
    }

    /// Where the caret is, for the input method's window.
    pub fn caret(&self) -> Option<Rect> {
        if self.tab == Tab::Skills {
            return self.skills.caret();
        }
        self.fields.caret()
    }

    /// The name of the provider `id` in its row `rect`, which leaves `room`
    /// at its right: a click on the name turns it into a field. Returns
    /// the new name once one was entered (Enter, or a click elsewhere).
    fn name_field(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        rect: Rect,
        (id, name): (&str, &str),
        room: f32,
    ) -> Option<String> {
        let t = p.theme;
        let typed = match &mut self.renaming {
            Some((renamed, editor)) if renamed == id => {
                let field = Rect::new(rect.x + 50.0, rect.y + 6.0, (rect.w - 62.0 - room).min(280.0), 26.0);
                p.bordered(field, t.bg, theme::RADIUS_SM, 1.0, t.border_focus);
                let inner = Rect::new(field.x + 8.0, field.y, field.w - 16.0, field.h);
                line_field(p, ui, inner, editor, ("Name", theme::LABEL), true);
                let done = self.renamed || (ui.pressed && !ui.hovered(field));
                done.then(|| editor.text().split_whitespace().collect::<Vec<_>>().join(" "))
            }
            _ => {
                let width = p.layout(name, theme::LABEL, None).width().min(rect.w - 60.0 - room);
                let hit = Rect::new(rect.x + 54.0, rect.y + 9.0, width + 8.0, 22.0);
                if ui.hovered(hit) {
                    ui.cursor = winit::window::CursorIcon::Text;
                    if ui.clicked(hit) {
                        let mut editor = Editor::default();
                        editor.insert(name);
                        editor.select_all();
                        self.renaming = Some((id.to_owned(), editor));
                        ui.last_edit = ui.time;
                    }
                }
                None
            }
        };
        let typed = typed?;
        (self.renaming, self.renamed) = (None, false);
        Some(typed).filter(|typed| !typed.is_empty() && typed != name)
    }

    /// Whether the provider `id` is being renamed: its row then leaves its name to the field.
    fn is_renaming(&self, id: &str) -> bool {
        self.renaming.as_ref().is_some_and(|(renamed, _)| renamed == id)
    }

    /// A text field `index` in `rect`.
    fn field(&mut self, p: &mut Painter, ui: &mut Ui, index: usize, rect: Rect, hint: &str) {
        let layout = self.fields.layout(p, index, rect.w);
        self.fields.draw(index, p, ui, rect, layout, hint, true);
    }

    /// Draws the settings over `view`. Returns whether they were left.
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        view: Rect,
        shell: &mut Shell,
        scheme: Scheme,
        actions: &mut Vec<Action>,
    ) -> bool {
        let t = p.theme;
        let (card, outside) = modal(p, ui, view, SIZE, "settings");
        let mut leave = outside && shell.menu.is_none();
        if icon_button(p, ui, Rect::new(card.right() - 44.0, card.y + 14.0, 30.0, 30.0), icon::X, true) {
            leave = true;
        }

        // The pages, down the left.
        p.label("Settings", theme::HEADING, card.x + 24.0, card.y + 20.0, t.text);
        let mut y = card.y + 62.0;
        for tab in Tab::ALL {
            let (label, glyph) = tab.label();
            let item = Rect::new(card.x + 12.0, y, NAV - 24.0, 34.0);
            let hovered = ui.hovered(item);
            let hover = ui.anim(id(("settings-tab", label)), f32::from(u8::from(hovered)));
            let selected = self.tab == tab;
            p.rect(item, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS_SM);
            let ink = mix(t.text_muted, t.text, if selected { 1.0 } else { hover });
            icon::draw(p, glyph, (item.x + 18.0, item.y + 17.0), 15.0, ink);
            p.label(label, theme::SMALL, item.x + 34.0, item.y + 8.0, ink);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(item) && !selected {
                    self.tab = tab;
                    self.add(Adding::Nothing);
                    self.skills = SkillsPage::new();
                    self.scroll.reset();
                }
            }
            y += 38.0;
        }
        p.rect(Rect::new(card.x + NAV, card.y + 1.0, 1.0, card.h - 2.0), t.border, 0.0);

        // The page, scrolling beside them.
        let area = Rect::new(card.x + NAV + 1.0, card.y + 1.0, card.w - NAV - 2.0, card.h - 2.0);
        let (x, w) = (area.x + 32.0, area.w - 64.0);
        let offset = self.scroll.update(ui, ui.hovered(area));
        let clip = p.push_clip(area);
        let top = area.y + 22.0 - offset;
        let end = match self.tab {
            Tab::General => Self::general(p, ui, (x, top, w), shell, scheme, actions),
            Tab::Providers => self.providers(p, ui, (x, top, w), shell, actions),
            Tab::Voice => self.voice(p, ui, (x, top, w), shell, actions),
            Tab::Integrations => self.integrations(p, ui, (x, top, w), shell, actions),
            Tab::Skills => self.skills.draw(p, ui, (x, top, w), (area.y, area.bottom()), shell, actions),
        };
        p.set_clip(clip);
        let content = end + offset - area.y + 24.0;
        self.scroll.finish(ui, content, area.h);
        self.scroll.bar(p, area, content);
        leave
    }

    /// A page's title and what it is for. Returns where what follows starts.
    fn title(p: &mut Painter, (x, y, w): (f32, f32, f32), title: &str, about: &str) -> f32 {
        let t = p.theme;
        p.label(title, theme::TITLE, x, y, t.text);
        let about = p.layout(about, theme::SMALL, Some(w - 40.0));
        p.text(&about, x, y + 36.0, t.text_muted);
        y + 36.0 + about.height() + 26.0
    }

    /// A section's heading with, at its right, a button to add to it.
    /// Returns whether the button was clicked.
    fn heading(p: &mut Painter, ui: &mut Ui, (x, y, w): (f32, f32, f32), title: &str, add: Option<&str>) -> bool {
        let t = p.theme;
        p.label(title, theme::LABEL, x, y + 6.0, t.text);
        let Some(add) = add else { return false };
        let width = p.layout(add, theme::LABEL, None).width() + 44.0;
        let rect = Rect::new(x + w - width, y, width, 30.0);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id(("add", title)), f32::from(u8::from(hovered)));
        p.bordered(rect, mix(t.bg, t.hover, hover), theme::RADIUS_SM, 1.0, t.border_strong);
        icon::draw(p, icon::PLUS, (rect.x + 17.0, rect.y + 15.0), 14.0, t.text);
        p.label(add, theme::LABEL, rect.x + 30.0, rect.y + 6.0, t.text);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
        }
        ui.clicked(rect)
    }

    /// A small line of explanation. Returns where what follows starts.
    fn note(p: &mut Painter, (x, y, w): (f32, f32, f32), text: &str) -> f32 {
        let note = p.layout(text, theme::TINY, Some(w));
        p.text(&note, x, y, p.theme.text_faint);
        y + note.height()
    }

    fn general(
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        shell: &mut Shell,
        scheme: Scheme,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let mut y = Self::title(p, (x, y, w), "General", "How AtroxaBot looks, and where it keeps what you make.");
        p.label("Theme", theme::LABEL, x, y, t.text);
        y += 30.0;
        // Each theme as a small picture of the app in it.
        let tile_w = ((w - 24.0) / 3.0).min(190.0);
        for (index, option) in Scheme::ALL.into_iter().enumerate() {
            let colors = option.palette();
            let rect = Rect::new(x + index as f32 * (tile_w + 12.0), y, tile_w, 104.0);
            let hovered = ui.hovered(rect);
            let selected = option == scheme;
            let lit = ui.anim(id(("theme", option.key())), f32::from(u8::from(selected || hovered)));
            p.bordered(
                rect,
                colors.panel,
                theme::RADIUS,
                if selected { 2.0 } else { 1.0 },
                mix(t.border_strong, t.text, if selected { 1.0 } else { lit * 0.4 }),
            );
            let canvas = Rect::new(rect.x + 44.0, rect.y + 12.0, rect.w - 56.0, 56.0);
            p.rect(canvas, colors.bg, 6.0);
            for (line, width) in [(0.0, 26.0), (9.0, 20.0), (18.0, 23.0)] {
                p.rect(Rect::new(rect.x + 12.0, rect.y + 16.0 + line, width, 4.0), fade(colors.text_muted, 0.6), 2.0);
            }
            p.rect(Rect::new(canvas.x + 10.0, canvas.y + 12.0, canvas.w * 0.6, 5.0), colors.text, 2.5);
            p.rect(Rect::new(canvas.x + 10.0, canvas.y + 24.0, canvas.w * 0.4, 5.0), colors.text_muted, 2.5);
            p.bordered(
                Rect::new(canvas.x + 8.0, canvas.bottom() - 16.0, canvas.w - 16.0, 10.0),
                colors.surface,
                5.0,
                1.0,
                colors.border_strong,
            );
            p.label(option.label(), theme::SMALL, rect.x + 12.0, rect.bottom() - 26.0, colors.text);
            if selected {
                icon::draw(p, icon::CHECK, (rect.right() - 18.0, rect.bottom() - 17.0), 14.0, colors.text);
            }
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(rect) && !selected {
                    actions.push(Action::SetTheme(option));
                }
            }
        }
        y += 104.0 + 34.0;

        // What the close button does: by default the app keeps running.
        p.label("Window", theme::LABEL, x, y, t.text);
        y += 30.0;
        let closing = CloseAction::from_key(shell.config.close.as_deref());
        let rect = Rect::new(x, y, w, 58.0);
        row(p, rect, (icon::X, None), "When you close the window", closing.detail(), 256.0);
        let anchor = Rect::new(rect.right() - 252.0, rect.y + 9.0, 240.0, 40.0);
        if dropdown(p, ui, anchor, None, closing.label(), shell.menu_open(Menu::Close)) {
            shell.toggle_menu(Menu::Close, anchor);
        }
        y += 58.0 + 34.0;

        p.label("On this computer", theme::LABEL, x, y, t.text);
        y += 30.0;
        let installed = shell.claude.values().find_map(|account| match account {
            Account::Ready(info) => Some(format!("Claude Code {}", info.version)),
            _ => None,
        });
        let missing = shell.claude.values().any(|account| matches!(account, Account::Missing));
        let (title, about) = match (installed, missing) {
            (Some(version), _) => (version, "Claude accounts run the Claude Code installed here, as it is."),
            (None, true) => (
                "Claude Code is not installed".to_owned(),
                "Install it to use Claude accounts. API providers work without it.",
            ),
            (None, false) => ("Looking for Claude Code…".to_owned(), ""),
        };
        row(p, Rect::new(x, y, w, 58.0), (icon::SPARKLES, theme::provider_color("claude")), &title, about, 0.0);
        logo_badge(p, Rect::new(x + 12.0, y + 12.0, 34.0, 34.0), Logo::Claude, theme::provider_color("claude"));
        y += 66.0;
        let data = Rect::new(x, y, w, 58.0);
        row(
            p,
            data,
            (icon::FOLDER, None),
            "Your data",
            "Agents, sessions, notes and settings are files in one folder.",
            130.0,
        );
        if button(
            p,
            ui,
            Rect::new(data.right() - 122.0, data.y + 13.0, 110.0, 32.0),
            "Open folder",
            ButtonStyle::Secondary,
            true,
        ) {
            actions.push(Action::OpenDataDir);
        }
        y + 58.0
    }

    fn providers(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let about = "What your agents think through. Swap an agent's provider from the top bar of its session.";
        let mut y = Self::title(p, (x, y, w), "Providers", about);
        let mut changed = false;

        // Claude accounts: each a Claude Code profile of its own.
        if Self::heading(p, ui, (x, y, w), "Claude accounts", Some("Add account")) {
            self.add(Adding::ClaudeAccount);
        }
        y += 40.0;
        let mut remove = None;
        let accounts: Vec<Connection> =
            shell.config.all_connections().into_iter().filter(|c| c.kind == ConnectionKind::ClaudeAgent).collect();
        for account in &accounts {
            let rect = Rect::new(x, y, w, 58.0);
            let state = shell.claude.get(&account.id);
            let signed_in = matches!(state, Some(Account::Ready(info)) if !info.account.email.is_empty());
            let can_sign_in = matches!(state, Some(Account::Ready(_) | Account::Failed(_)));
            let mark = (icon::SPARKLES, Shell::provider_tint(account));
            let title = if self.is_renaming(&account.id) { "" } else { account.name.as_str() };
            row(p, rect, mark, title, &shell.provider_detail(account), 190.0);
            if let Some(name) = self.name_field(p, ui, rect, (&account.id, &account.name), 190.0) {
                shell.config.rename_connection(&account.id, &name);
                changed = true;
            }
            shell.provider_picture(p, account, Rect::new(rect.x + 12.0, rect.y + 12.0, 34.0, 34.0));
            let mut right = rect.right() - 12.0;
            right -= 28.0;
            if icon_button(p, ui, Rect::new(right, rect.y + 15.0, 28.0, 28.0), icon::TRASH, true) {
                remove = Some(account.id.clone());
            }
            right -= 30.0;
            if icon_button(p, ui, Rect::new(right, rect.y + 15.0, 28.0, 28.0), icon::REFRESH, true) {
                shell.claude.insert(account.id.clone(), Account::Checking);
                actions.push(Action::ProbeClaude(Box::new(account.clone())));
            }
            if signed_in {
                icon::draw(p, icon::CHECK, (right - 16.0, rect.y + 29.0), 15.0, t.text_muted);
            } else if can_sign_in {
                let sign_in = Rect::new(right - 92.0, rect.y + 13.0, 84.0, 32.0);
                if button(p, ui, sign_in, "Sign in", ButtonStyle::Primary, true) {
                    actions.push(Action::ClaudeLogin(Box::new(account.clone())));
                }
            }
            y += 66.0;
        }
        if let Some(id) = remove {
            // Claude Code's own sign-in is not the app's to delete: it only leaves the list.
            shell.config.hide_claude_agent |= id == Connection::CLAUDE_AGENT;
            shell.config.connections.retain(|c| c.id != id);
            shell.claude.remove(&id);
            changed = true;
        }
        if shell.config.hide_claude_agent {
            // Off the list, it can be put back.
            let rect = Rect::new(x, y, w, 58.0);
            let about = "Whoever `claude` is signed in as in your terminal, with its skills, plugins and settings.";
            row(p, rect, (icon::TERMINAL, None), "Claude Code's own sign-in", about, 100.0);
            if button(
                p,
                ui,
                Rect::new(rect.right() - 92.0, rect.y + 13.0, 80.0, 32.0),
                "Use it",
                ButtonStyle::Secondary,
                true,
            ) {
                shell.config.hide_claude_agent = false;
                if let Some(account) = shell.config.connection(Connection::CLAUDE_AGENT) {
                    shell.claude.insert(account.id.clone(), Account::Checking);
                    actions.push(Action::ProbeClaude(Box::new(account)));
                }
                changed = true;
            }
            y += 66.0;
        }
        if self.adding == Adding::ClaudeAccount {
            let card = Rect::new(x, y, w, 108.0);
            p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_focus);
            p.label("A name for the account", theme::CAPTION, card.x + 16.0, card.y + 14.0, t.text_muted);
            self.field(
                p,
                ui,
                NAME,
                Rect::new(card.x + 16.0, card.y + 38.0, card.w - 32.0 - 250.0, 42.0),
                "Work, Personal …",
            );
            let name = self.fields.text(NAME).trim().to_owned();
            if button(
                p,
                ui,
                Rect::new(card.right() - 242.0, card.y + 38.0, 84.0, 42.0),
                "Cancel",
                ButtonStyle::Ghost,
                true,
            ) {
                self.add(Adding::Nothing);
            }
            let add = Rect::new(card.right() - 150.0, card.y + 38.0, 134.0, 42.0);
            if button(p, ui, add, "Add and sign in", ButtonStyle::Primary, !name.is_empty()) {
                match Connection::claude_account(&name) {
                    Ok(account) => {
                        shell.claude.insert(account.id.clone(), Account::Checking);
                        actions.push(Action::ClaudeLogin(Box::new(account.clone())));
                        shell.config.connections.push(account);
                        changed = true;
                    }
                    Err(e) => shell.notify(format!("The account could not be added: {e}")),
                }
                self.add(Adding::Nothing);
            }
            y += 116.0;
        }
        let note = "\"Claude\" is Claude Code as you use it in your terminal: whoever `claude` is signed in as there, \
                    with the skills, plugins and settings you have there. Each account you add is a Claude Code \
                    profile of its own, with its own sign-in, settings and history, and starts without those. \
                    Signing in opens Claude Code in a terminal; AtroxaBot never sees your credentials, and usage \
                    is billed to the account you sign in to.";
        y = Self::note(p, (x, y + 2.0, w), note) + 30.0;

        // API providers.
        let adding_provider = matches!(self.adding, Adding::PickProvider | Adding::Provider(_));
        if Self::heading(p, ui, (x, y, w), "API providers", (!adding_provider).then_some("Add provider")) {
            self.add(Adding::PickProvider);
        }
        y += 40.0;
        let (mut remove, mut rename) = (None, None);
        for connection in shell.config.connections.iter().filter(|c| c.kind != ConnectionKind::ClaudeAgent) {
            let rect = Rect::new(x, y, w, 58.0);
            let key = if connection.api_key.is_some() { "key saved" } else { "no key" };
            let detail = format!("{} · {key}", connection.host());
            let mark = (Shell::provider_glyph(connection), Shell::provider_tint(connection));
            let title = if self.is_renaming(&connection.id) { "" } else { connection.name.as_str() };
            row(p, rect, mark, title, &detail, 50.0);
            if let Some(name) = self.name_field(p, ui, rect, (&connection.id, &connection.name), 50.0) {
                rename = Some((connection.id.clone(), name));
            }
            shell.provider_picture(p, connection, Rect::new(rect.x + 12.0, rect.y + 12.0, 34.0, 34.0));
            if icon_button(p, ui, Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0), icon::TRASH, true) {
                remove = Some(connection.id.clone());
            }
            y += 66.0;
        }
        if let Some((id, name)) = rename {
            shell.config.rename_connection(&id, &name);
            changed = true;
        }
        if let Some(id) = remove {
            shell.config.connections.retain(|c| c.id != id);
            shell.models.remove(&id);
            changed = true;
        }
        match self.adding {
            Adding::PickProvider => {
                // Which one: a tile each.
                let tile_w = (w - 20.0) / 3.0;
                for (index, (_, name, _, about)) in PRESETS.iter().enumerate() {
                    let rect = Rect::new(
                        x + (index % 3) as f32 * (tile_w + 10.0),
                        y + (index / 3) as f32 * 108.0,
                        tile_w,
                        98.0,
                    );
                    let glyph = if index == CUSTOM { icon::LINK } else { Shell::preset_glyph(PRESETS[index].0, name) };
                    if tile(p, ui, rect, (glyph, theme::provider_color(name)), name, about) {
                        self.add(Adding::Provider(index));
                    }
                    if let Some(logo) = Logo::named(name).filter(|_| index != CUSTOM) {
                        let badge = Rect::new(rect.x + 14.0, rect.y + 14.0, 30.0, 30.0);
                        logo_badge(p, badge, logo, theme::provider_color(name));
                    }
                }
                y += 2.0 * 108.0;
                if button(p, ui, Rect::new(x, y, 84.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
                    self.add(Adding::Nothing);
                }
                y += 34.0;
            }
            Adding::Provider(preset) => {
                let (kind, name, url, _) = PRESETS[preset.min(CUSTOM)];
                let custom = preset == CUSTOM;
                let card = Rect::new(x, y, w, if custom { 248.0 } else { 148.0 });
                p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_focus);
                let (cx, cw) = (card.x + 16.0, card.w - 32.0);
                p.label(if custom { "A provider of your own" } else { name }, theme::LABEL, cx, card.y + 14.0, t.text);
                let mut cy = card.y + 42.0;
                if custom {
                    let half = (cw - 10.0) * 0.5;
                    self.field(p, ui, NAME, Rect::new(cx, cy, half, 42.0), "Name");
                    self.field(
                        p,
                        ui,
                        URL,
                        Rect::new(cx + half + 10.0, cy, half, 42.0),
                        "Address, e.g. https://host/v1",
                    );
                    cy += 52.0;
                    p.label("It speaks", theme::SMALL, cx, cy + 5.0, t.text_muted);
                    let mut chip_x = cx + 72.0;
                    for (speaks, label) in
                        [(ConnectionKind::OpenAi, "OpenAI's API"), (ConnectionKind::ClaudeApi, "Anthropic's API")]
                    {
                        let width = p.layout(label, theme::SMALL, None).width() + 26.0;
                        if chip(p, ui, Rect::new(chip_x, cy, width, 30.0), label, self.custom_kind == speaks) {
                            self.custom_kind = speaks;
                        }
                        chip_x += width + 8.0;
                    }
                    cy += 44.0;
                } else {
                    p.label(
                        url,
                        theme::TINY,
                        cx + p.layout(name, theme::LABEL, None).width() + 10.0,
                        card.y + 17.0,
                        t.text_faint,
                    );
                }
                let hint =
                    if url.contains("localhost") { "API key (not needed for a local server)" } else { "API key" };
                self.field(p, ui, KEY, Rect::new(cx, cy, cw, 42.0), hint);
                cy += 54.0;
                let address = if custom { self.fields.text(URL).trim().to_owned() } else { url.to_owned() };
                let valid = address.starts_with("https://") || address.starts_with("http://");
                if button(p, ui, Rect::new(card.right() - 216.0, cy, 84.0, 34.0), "Back", ButtonStyle::Ghost, true) {
                    self.add(Adding::PickProvider);
                } else if button(
                    p,
                    ui,
                    Rect::new(card.right() - 124.0, cy, 108.0, 34.0),
                    "Add provider",
                    ButtonStyle::Primary,
                    valid,
                ) {
                    let typed = self.fields.text(NAME).trim();
                    let key = self.fields.text(KEY).trim();
                    let mut connection = Connection {
                        id: new_id(),
                        kind: if custom { self.custom_kind } else { kind },
                        name: if custom { typed.to_owned() } else { name.to_owned() },
                        base_url: address,
                        api_key: (!key.is_empty()).then(|| key.to_owned()),
                        ..Connection::default()
                    };
                    if connection.name.is_empty() {
                        let host = connection.host().to_owned();
                        connection.name = host;
                    }
                    shell.want_models(&connection, actions);
                    shell.config.connections.push(connection);
                    changed = true;
                    self.add(Adding::Nothing);
                }
                y += card.h;
            }
            _ => {}
        }
        if changed {
            actions.push(Action::SaveConfig);
        }
        y
    }

    fn voice(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let about = "Call an agent with the phone in its top bar. A voice provider hears you and speaks for it.";
        let mut y = Self::title(p, (x, y, w), "Voice", about);
        let mut changed = false;
        let has_key = |shell: &Shell, index: usize| match index {
            0 => shell.config.voice.fish_key.is_some(),
            _ => shell.config.voice.elevenlabs_key.is_some(),
        };
        let missing: Vec<usize> = (0..VOICE_PROVIDERS.len()).filter(|index| !has_key(shell, *index)).collect();
        let adding = matches!(self.adding, Adding::PickVoice | Adding::Voice(_));
        let offer = (!missing.is_empty() && !adding).then_some("Add voice provider");
        if Self::heading(p, ui, (x, y, w), "Voice providers", offer) {
            // With one left to add there is nothing to choose.
            self.add(if missing.len() == 1 { Adding::Voice(missing[0]) } else { Adding::PickVoice });
        }
        y += 40.0;
        let in_use = usize::from(shell.config.voice.provider == "elevenlabs");
        let only = (0..VOICE_PROVIDERS.len()).filter(|index| has_key(shell, *index)).count() == 1;
        for (index, (key, name, _)) in VOICE_PROVIDERS.iter().enumerate() {
            if !has_key(shell, index) {
                continue;
            }
            let rect = Rect::new(x, y, w, 58.0);
            let default = index == in_use || only;
            row(p, rect, (icon::AUDIO, None), name, if default { "Used for calls" } else { "Key saved" }, 170.0);
            if icon_button(p, ui, Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0), icon::TRASH, true) {
                match index {
                    0 => shell.config.voice.fish_key = None,
                    _ => shell.config.voice.elevenlabs_key = None,
                }
                changed = true;
            }
            if !default
                && button(
                    p,
                    ui,
                    Rect::new(rect.right() - 150.0, rect.y + 13.0, 100.0, 32.0),
                    "Use for calls",
                    ButtonStyle::Secondary,
                    true,
                )
            {
                (*key).clone_into(&mut shell.config.voice.provider);
                changed = true;
            }
            y += 66.0;
        }
        if missing.len() == VOICE_PROVIDERS.len() && !adding {
            let empty = Rect::new(x, y, w, 58.0);
            p.bordered(empty, fade(t.surface, 0.5), theme::RADIUS, 1.0, t.border);
            p.label_centered("No voice provider yet. Add one to call your agents.", theme::SMALL, empty, t.text_faint);
            y += 66.0;
        }
        match self.adding {
            Adding::PickVoice => {
                let tile_w = (w - 20.0) / 3.0;
                for (slot, index) in missing.iter().enumerate() {
                    let (_, name, about) = VOICE_PROVIDERS[*index];
                    let rect = Rect::new(x + slot as f32 * (tile_w + 10.0), y, tile_w, 98.0);
                    if tile(p, ui, rect, (icon::AUDIO, None), name, about) {
                        self.add(Adding::Voice(*index));
                    }
                }
                y += 108.0;
                if button(p, ui, Rect::new(x, y, 84.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
                    self.add(Adding::Nothing);
                }
                y += 42.0;
            }
            Adding::Voice(index) => {
                let (key, name, _) = VOICE_PROVIDERS[index.min(VOICE_PROVIDERS.len() - 1)];
                let card = Rect::new(x, y, w, 148.0);
                p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_focus);
                p.label(name, theme::LABEL, card.x + 16.0, card.y + 14.0, t.text);
                self.field(p, ui, KEY, Rect::new(card.x + 16.0, card.y + 42.0, card.w - 32.0, 42.0), "API key");
                let typed = self.fields.text(KEY).trim().to_owned();
                let by = card.y + 96.0;
                if button(p, ui, Rect::new(card.right() - 216.0, by, 84.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
                    self.add(Adding::Nothing);
                } else if button(
                    p,
                    ui,
                    Rect::new(card.right() - 124.0, by, 108.0, 34.0),
                    "Add",
                    ButtonStyle::Primary,
                    !typed.is_empty(),
                ) {
                    let voice = &mut shell.config.voice;
                    match index {
                        0 => voice.fish_key = Some(typed),
                        _ => voice.elevenlabs_key = Some(typed),
                    }
                    // The first one added is the one calls use.
                    if only || missing.len() == VOICE_PROVIDERS.len() {
                        key.clone_into(&mut voice.provider);
                    }
                    changed = true;
                    self.add(Adding::Nothing);
                }
                y += 156.0;
            }
            _ => {}
        }

        y = Self::note(p, (x, y + 20.0, w), "Use headphones on a call: with speakers, the agent hears itself.");
        if changed {
            actions.push(Action::SaveConfig);
        }
        y
    }

    fn integrations(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let about = "What agents can reach besides their folder: your files, your tracker, your mail. Each comes \
                     from an MCP server.";
        let mut y = Self::title(p, (x, y, w), "Integrations", about);
        let mut changed = false;
        let adding = matches!(self.adding, Adding::Mcp(_));
        if Self::heading(p, ui, (x, y, w), "MCP servers", (!adding).then_some("Add server")) {
            self.add(Adding::Mcp(false));
        }
        y += 40.0;
        let mut remove = None;
        for (index, server) in shell.config.mcp.iter_mut().enumerate() {
            let rect = Rect::new(x, y, w, 58.0);
            let (glyph, target) =
                if server.url.is_empty() { (icon::TERMINAL, &server.command) } else { (icon::GLOBE, &server.url) };
            row(p, rect, (glyph, None), &server.name, target, 110.0);
            if toggle(p, ui, (rect.right() - 92.0, rect.y + 18.0), server.enabled, &server.name) {
                server.enabled = !server.enabled;
                changed = true;
            }
            if icon_button(p, ui, Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0), icon::TRASH, true) {
                remove = Some(index);
            }
            y += 66.0;
        }
        if let Some(index) = remove {
            shell.config.mcp.remove(index);
            changed = true;
        }
        if shell.config.mcp.is_empty() && !adding {
            let empty = Rect::new(x, y, w, 58.0);
            p.bordered(empty, fade(t.surface, 0.5), theme::RADIUS, 1.0, t.border);
            p.label_centered("No servers yet.", theme::SMALL, empty, t.text_faint);
            y += 66.0;
        }
        if let Adding::Mcp(remote) = self.adding {
            let card = Rect::new(x, y, w, 330.0);
            p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_focus);
            let (cx, cw) = (card.x + 16.0, card.w - 32.0);
            let mut chip_x = cx;
            for (is_remote, label) in [(false, "Runs on this computer"), (true, "At an address")] {
                let width = p.layout(label, theme::SMALL, None).width() + 26.0;
                if chip(p, ui, Rect::new(chip_x, card.y + 16.0, width, 30.0), label, remote == is_remote) {
                    self.adding = Adding::Mcp(is_remote);
                }
                chip_x += width + 8.0;
            }
            let mut cy = card.y + 60.0;
            self.field(p, ui, NAME, Rect::new(cx, cy, cw, 42.0), "Name, e.g. files");
            cy += 52.0;
            let hint = if remote {
                "Address, e.g. https://mcp.example.com/mcp"
            } else {
                "Command, e.g. npx -y @modelcontextprotocol/server-filesystem C:\\notes"
            };
            self.field(p, ui, URL, Rect::new(cx, cy, cw, 42.0), hint);
            cy += 52.0;
            let hint = if remote {
                "Headers, one NAME=value per line (optional)"
            } else {
                "Environment, one NAME=value per line (optional)"
            };
            self.field(p, ui, VARS, Rect::new(cx, cy, cw, 96.0), hint);
            cy += 108.0;
            let (name, target) = (self.fields.text(NAME).trim().to_owned(), self.fields.text(URL).trim().to_owned());
            let taken = shell.config.mcp.iter().any(|server| server.name == name);
            let valid =
                !name.is_empty() && !taken && if remote { target.starts_with("http") } else { !target.is_empty() };
            if button(p, ui, Rect::new(card.right() - 216.0, cy, 84.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
                self.add(Adding::Nothing);
            } else if button(
                p,
                ui,
                Rect::new(card.right() - 124.0, cy, 108.0, 34.0),
                "Add server",
                ButtonStyle::Primary,
                valid,
            ) {
                let vars = self.fields.text(VARS).trim().to_owned();
                let server = if remote {
                    McpServer { name, url: target, vars, enabled: true, ..McpServer::default() }
                } else {
                    McpServer { name, command: target, vars, enabled: true, ..McpServer::default() }
                };
                shell.config.mcp.push(server);
                changed = true;
                self.add(Adding::Nothing);
            }
            y += 338.0;
        }
        y = Self::note(
            p,
            (x, y + 2.0, w),
            "Every agent gets these, on every provider. A change here applies to the next message.",
        );
        if changed {
            actions.push(Action::SaveConfig);
        }
        Self::claude_servers(p, ui, (x, y + 30.0, w), shell, actions)
    }

    /// What Claude Code gives each Claude account's sessions besides the
    /// servers above: the connectors added on claude.ai and the servers set
    /// up in Claude Code itself. Only shown, never changed: they are the
    /// account's. Claude Code is asked once the page is first shown.
    fn claude_servers(
        p: &mut Painter,
        ui: &mut Ui,
        (x, mut y, w): (f32, f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        const MANAGE: &str = "https://claude.ai/customize/connectors";
        let t = p.theme;
        let accounts: Vec<Connection> = shell
            .config
            .all_connections()
            .into_iter()
            .filter(|connection| connection.kind == ConnectionKind::ClaudeAgent)
            .collect();
        if accounts.is_empty() {
            return y;
        }
        p.label("From your Claude accounts", theme::LABEL, x, y + 6.0, t.text);
        let loading = shell.connectors.values().any(|connectors| matches!(connectors, Connectors::Loading));
        if icon_button(p, ui, Rect::new(x + w - 30.0, y + 1.0, 28.0, 28.0), icon::REFRESH, !loading) {
            shell.connectors.clear();
        }
        y += 34.0;
        y = Self::note(
            p,
            (x, y, w),
            "Agents on a Claude account also get what that account has: the connectors added on claude.ai, and the \
             servers set up in Claude Code itself. They are the account's own, so each account has its own.",
        ) + 14.0;
        for connection in accounts {
            let known = shell.connectors.get(&connection.id).cloned();
            if known.is_none() {
                shell.connectors.insert(connection.id.clone(), Connectors::Loading);
                actions.push(Action::ProbeConnectors(Box::new(connection.clone())));
            }
            p.label(&connection.name, theme::SMALL, x, y, t.text_muted);
            y += 26.0;
            let servers = match known {
                Some(Connectors::Ready(servers)) => servers,
                Some(Connectors::Failed(problem)) => {
                    y = Self::note(p, (x, y, w), &problem) + 16.0;
                    continue;
                }
                _ => {
                    p.label("Asking Claude Code…", theme::SMALL, x, y, t.text_faint);
                    y += 34.0;
                    continue;
                }
            };
            // What agents can use, or are kept from by a failure, gets a row; the rest a line.
            let (listed, waiting): (Vec<&ClaudeMcpServer>, Vec<&ClaudeMcpServer>) =
                servers.iter().partition(|server| matches!(server.status.as_str(), "connected" | "pending" | "failed"));
            for server in &listed {
                let connector = server.source == "claudeai";
                let name = server.name.strip_prefix("claude.ai ").unwrap_or(&server.name);
                let kind = if connector { "claude.ai connector" } else { "Claude Code" };
                let (state, detail) = match server.status.as_str() {
                    "connected" => {
                        let tools =
                            if server.tools == 1 { "1 tool".to_owned() } else { format!("{} tools", server.tools) };
                        ("Connected", format!("{kind} · {tools}"))
                    }
                    "pending" => ("Connecting…", kind.to_owned()),
                    _ if server.error.is_empty() => ("Failed", kind.to_owned()),
                    _ => ("Failed", format!("{kind} · {}", server.error.trim())),
                };
                let rect = Rect::new(x, y, w, 58.0);
                let glyph = if connector { icon::GLOBE } else { icon::SERVER };
                row(p, rect, (glyph, None), name, &detail, 110.0);
                let width = p.layout(state, theme::SMALL, None).width();
                p.label(state, theme::SMALL, rect.right() - 16.0 - width, rect.y + 20.0, t.text_faint);
                y += 66.0;
            }
            let names = |status: &str| -> Vec<&str> {
                waiting
                    .iter()
                    .filter(|server| server.status == status)
                    .map(|server| server.name.strip_prefix("claude.ai ").unwrap_or(&server.name))
                    .collect()
            };
            let (sign_in, off) = (names("needs-auth"), names("disabled"));
            let mut lines = Vec::new();
            if !sign_in.is_empty() {
                lines.push(format!("Not connected yet: {}.", sign_in.join(", ")));
            }
            if !off.is_empty() {
                lines.push(format!("Turned off: {}.", off.join(", ")));
            }
            if listed.is_empty() && lines.is_empty() {
                lines.push("No connectors or servers.".to_owned());
            }
            if !lines.is_empty() {
                // The line sits level with the button beside it.
                let top = y + 2.0;
                let mut bottom = Self::note(p, (x, top + 9.0, w - 172.0), &lines.join(" "));
                if !sign_in.is_empty() {
                    let at = Rect::new(x + w - 156.0, top, 156.0, 32.0);
                    if button(p, ui, at, "Connect on claude.ai", ButtonStyle::Secondary, true) {
                        actions.push(Action::OpenLink(MANAGE.to_owned()));
                    }
                    bottom = bottom.max(at.bottom());
                }
                y = bottom + 6.0;
            }
            y += 18.0;
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_fields_on_screen_take_the_keyboard() {
        let mut settings = Settings::new(Tab::Providers);
        assert!(settings.visible().is_empty());
        settings.commit("lost");
        settings.add(Adding::Provider(1));
        assert_eq!(settings.visible(), [KEY], "a known provider only needs its key");
        settings.commit("sk-or");
        assert_eq!((settings.fields.text(KEY), settings.fields.text(NAME)), ("sk-or", ""));
        settings.add(Adding::Provider(CUSTOM));
        assert_eq!(settings.visible(), [NAME, URL, KEY]);
        assert_eq!(settings.fields.text(KEY), "", "what was typed for another provider is gone");
        settings.tab = Tab::Integrations;
        settings.add(Adding::Mcp(true));
        assert_eq!((settings.visible(), settings.fields.focused()), (vec![NAME, URL, VARS], NAME));
    }
}
