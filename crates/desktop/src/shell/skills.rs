//! The skills settings page: the skills every agent has, and the
//! repositories they come from. A repository opens in the page's place,
//! with what it offers grouped by plugin and an Install button on each skill;
//! a plugin its marketplace lists from another repository opens on top of it.

#![expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]

use std::collections::{HashMap, HashSet};

use arboard::Clipboard;
use atroxabot_core::skills::{Catalog, Job, Record, Report, Skill, Source};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::Shell;
use super::widgets::row;
use crate::app::Action;
use crate::editor::Editor;
use crate::form::Fields;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, icon, icon_button, id};

/// A row's height, and the space between rows.
const ROW: f32 = 58.0;
const STEP: f32 = 66.0;
/// A repository offering more skills than this gets a filter.
const FILTER_FROM: usize = 8;

/// The library as the interface knows it.
#[derive(Default)]
pub(super) struct SkillsView {
    /// The installed skills, by name.
    pub installed: Vec<Skill>,
    /// Where they came from, and the repositories.
    pub record: Record,
    /// What each repository offers, as far as it was fetched since the app started.
    pub catalogs: HashMap<Source, Fetched>,
    /// Skills being installed or removed, by name.
    busy: HashSet<String>,
    /// Changes not yet reported back.
    pending: usize,
    /// The repositories installed skills came from were looked at for updates.
    checked: bool,
}

/// A repository being read, or what came of it.
pub(super) enum Fetched {
    Loading,
    Ready(Box<Catalog>),
    Failed(String),
}

impl SkillsView {
    /// Starts `job`, keeping track of a change until it is reported.
    pub fn start(&mut self, job: Job, actions: &mut Vec<Action>) {
        match &job {
            Job::Fetch(source) => {
                self.catalogs.insert(source.clone(), Fetched::Loading);
            }
            Job::Install(skill, _) => {
                self.busy.insert(skill.name.clone());
                self.pending += 1;
            }
            Job::Remove(name) => {
                self.busy.insert(name.clone());
                self.pending += 1;
            }
            Job::AddRepository(_) | Job::RemoveRepository(_) => self.pending += 1,
            Job::Load => {}
        }
        actions.push(Action::Skills(job));
    }

    /// Takes in what a job came to; returns a problem to tell the user about.
    pub fn report(&mut self, report: Report) -> Option<String> {
        match report {
            Report::Library { skills, record, problem } => {
                self.installed = skills;
                self.record = record;
                self.pending = self.pending.saturating_sub(1);
                if self.pending == 0 {
                    self.busy.clear();
                }
                problem
            }
            Report::Fetched { source, catalog } => {
                let fetched = match catalog {
                    Ok(catalog) => Fetched::Ready(catalog),
                    Err(e) => Fetched::Failed(e),
                };
                self.catalogs.insert(source, fetched);
                None
            }
        }
    }

    /// The newer version of the installed skill `name`, with where it comes
    /// from, when its repository was looked at and has one.
    fn update(&self, name: &str) -> Option<(&Skill, &Source)> {
        let origin = self.record.installed.get(name)?;
        match self.catalogs.get(&origin.source)? {
            Fetched::Ready(catalog) => Some((self.record.update(name, &origin.source, catalog)?, &origin.source)),
            _ => None,
        }
    }

    /// Fetches, once, the repositories installed skills came from, to see whether they have updates.
    fn check_updates(&mut self, actions: &mut Vec<Action>) {
        if std::mem::replace(&mut self.checked, true) {
            return;
        }
        let sources: HashSet<Source> = self.record.installed.values().map(|origin| origin.source.clone()).collect();
        for source in sources {
            if !self.catalogs.contains_key(&source) {
                self.start(Job::Fetch(source), actions);
            }
        }
    }
}

/// The page's own state, while the settings are open.
pub(super) struct SkillsPage {
    /// The repositories opened, the one on screen last.
    open: Vec<Source>,
    /// A repository is being added.
    adding: bool,
    /// The address being typed.
    address: Fields,
    /// What the skills of the open repository are filtered by.
    filter: Fields,
    /// Which of the two has the keyboard.
    filtering: bool,
    /// Enter was pressed in the address: it is added on the next frame.
    submit: bool,
}

impl SkillsPage {
    pub fn new() -> Self {
        let field = || Fields::new([Editor::default()], [false]);
        Self { open: Vec::new(), adding: false, address: field(), filter: field(), filtering: false, submit: false }
    }

    /// Shows `source`'s page, as if it had been opened.
    #[cfg(test)]
    pub(super) fn show(&mut self, source: Source) {
        self.open.push(source);
    }

    /// Whether a field is on screen to type in.
    fn typing(&self) -> bool {
        if self.open.is_empty() { self.adding } else { self.filtering }
    }

    /// A key. Esc steps back (out of the field, then the repository);
    /// returns `true` when there is nothing left to step out of.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<Clipboard>) -> bool {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.open.pop().is_some() {
                    self.filter.replace(0, "");
                } else if self.adding {
                    self.adding = false;
                } else {
                    return true;
                }
            }
            _ if !self.typing() => {}
            Key::Named(NamedKey::Enter) if self.open.is_empty() => self.submit = true,
            _ if self.open.is_empty() => {
                self.address.key(event, mods, clipboard);
            }
            _ => {
                self.filter.key(event, mods, clipboard);
            }
        }
        false
    }

    /// Text committed by an input method.
    pub fn commit(&mut self, text: &str) {
        if !self.typing() {
        } else if self.open.is_empty() {
            self.address.insert(text);
        } else {
            self.filter.insert(text);
        }
    }

    /// Where the caret is, for the input method's window.
    pub fn caret(&self) -> Option<Rect> {
        if !self.typing() {
            None
        } else if self.open.is_empty() {
            self.address.caret()
        } else {
            self.filter.caret()
        }
    }

    /// Draws the page at `(x, y)`, `w` wide; rows outside `visible` (top
    /// and bottom) are only counted. Returns where it ends.
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        visible: (f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        shell.skills.check_updates(actions);
        match self.open.last().cloned() {
            Some(source) => self.repository(p, ui, (x, y, w), visible, &source, shell, actions),
            None => self.library(p, ui, (x, y, w), shell, actions),
        }
    }

    /// Opens `source`, reading it unless it was read since the app started.
    fn open(&mut self, source: Source, view: &mut SkillsView, actions: &mut Vec<Action>) {
        if !matches!(view.catalogs.get(&source), Some(Fetched::Ready(_) | Fetched::Loading)) {
            view.start(Job::Fetch(source.clone()), actions);
        }
        self.open.push(source);
        self.filter.replace(0, "");
        self.filtering = false;
    }

    /// The page itself: the installed skills, then the repositories.
    fn library(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let about = "Skills teach agents a kind of task: instructions, and the files they need, which an agent reads \
                     when a task calls for them. Every agent has the skills installed here, on every account and \
                     provider.";
        let mut y = title(p, (x, y, w), "Skills", about);
        p.label("Installed", theme::LABEL, x, y + 6.0, t.text);
        y += 40.0;
        let view = &mut shell.skills;
        let mut job = None;
        for skill in &view.installed {
            let rect = Rect::new(x, y, w, ROW);
            let update = view.update(&skill.name);
            let room = if update.is_some() { 150.0 } else { 60.0 };
            row(p, rect, (icon::SPARKLES, None), &skill.name, &skill.description, room);
            let busy = view.busy.contains(&skill.name);
            if busy {
                p.label("Working…", theme::SMALL, rect.right() - 92.0, rect.y + 20.0, t.text_faint);
            } else {
                if let Some((newer, source)) = update {
                    let at = Rect::new(rect.right() - 136.0, rect.y + 13.0, 84.0, 32.0);
                    if button(p, ui, at, "Update", ButtonStyle::Secondary, true) {
                        job = Some(Job::Install(Box::new(newer.clone()), source.clone()));
                    }
                }
                if icon_button(p, ui, Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0), icon::TRASH, true) {
                    job = Some(Job::Remove(skill.name.clone()));
                }
            }
            y += STEP;
        }
        if view.installed.is_empty() {
            let empty = Rect::new(x, y, w, ROW);
            p.bordered(empty, fade(t.surface, 0.5), theme::RADIUS, 1.0, t.border);
            p.label_centered("No skills yet. Install some from a repository.", theme::SMALL, empty, t.text_faint);
            y += STEP;
        }
        y += 18.0;

        // The repositories.
        if heading(p, ui, (x, y, w), "Repositories", (!self.adding).then_some("Add repository")) {
            self.adding = true;
            self.address.replace(0, "");
            self.address.focus(0);
        }
        y += 40.0;
        if self.adding {
            y = self.adding_card(p, ui, (x, y, w), view, &mut job);
        }
        let mut opened = None;
        for source in &view.record.repositories {
            let rect = Rect::new(x, y, w, ROW);
            let (name, detail) = match view.catalogs.get(source) {
                Some(Fetched::Ready(catalog)) => {
                    let count = catalog.skills().count();
                    let skills = if count == 1 { "1 skill".to_owned() } else { format!("{count} skills") };
                    (catalog.name.clone(), format!("{source} · {skills}"))
                }
                Some(Fetched::Loading) => (source.name(), format!("{source} · looking…")),
                Some(Fetched::Failed(_)) => (source.name(), format!("{source} · could not be read")),
                None => (source.name(), source.to_string()),
            };
            let trash = Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0);
            let hovered = ui.hovered(rect) && !ui.hovered(trash);
            let hover = ui.anim(id(("repository", rect.y.to_bits())), f32::from(u8::from(hovered)));
            row(p, rect, (source_icon(source), None), &name, &detail, 90.0);
            p.bordered(rect, fade(t.hover, hover * 0.6), theme::RADIUS, 1.0, fade(t.border_focus, hover));
            icon::draw(p, icon::CHEVRON_RIGHT, (rect.right() - 62.0, rect.y + 29.0), 15.0, t.text_muted);
            if icon_button(p, ui, trash, icon::TRASH, true) {
                job = Some(Job::RemoveRepository(source.clone()));
            } else if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(rect) {
                    opened = Some(source.clone());
                }
            }
            y += STEP;
        }
        if let Some(job) = job {
            view.start(job, actions);
        }
        if let Some(source) = opened {
            self.open(source, view, actions);
        }
        note(
            p,
            (x, y + 2.0, w),
            "A repository can be a Claude Code plugin marketplace or any repository with SKILL.md files; on GitHub \
             it needs no Git. Agents on a Claude account load the library as a plugin, so its skills are also \
             called library:<name> there. A change reaches each agent's next session.",
        )
    }

    /// The card a repository is added with. Returns where what follows starts.
    fn adding_card(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        view: &SkillsView,
        job: &mut Option<Job>,
    ) -> f32 {
        let t = p.theme;
        let card = Rect::new(x, y, w, 138.0);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_focus);
        let (cx, cw) = (card.x + 16.0, card.w - 32.0);
        let field = Rect::new(cx, card.y + 16.0, cw, 42.0);
        let layout = self.address.layout(p, 0, field.w);
        self.address.draw(0, p, ui, field, layout, "owner/repository, a Git address, or a folder", true);
        let typed = self.address.text(0).trim().to_owned();
        let parsed = Source::parse(&typed);
        let known = parsed.as_ref().is_ok_and(|source| view.record.repositories.contains(source));
        let problem = match &parsed {
            _ if typed.is_empty() => String::new(),
            Ok(_) if known => "That repository is on the list.".to_owned(),
            Ok(_) => String::new(),
            Err(e) => e.clone(),
        };
        p.label(&problem, theme::TINY, cx + 2.0, card.y + 66.0, t.text_faint);
        let at = card.y + card.h - 50.0;
        if button(p, ui, Rect::new(card.right() - 216.0, at, 84.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
            self.adding = false;
        } else if let Some(source) = parsed.ok().filter(|_| !known) {
            let add = Rect::new(card.right() - 124.0, at, 108.0, 34.0);
            if button(p, ui, add, "Add", ButtonStyle::Primary, true) || std::mem::take(&mut self.submit) {
                *job = Some(Job::AddRepository(source));
                self.adding = false;
            }
        } else {
            self.submit = false;
            button(p, ui, Rect::new(card.right() - 124.0, at, 108.0, 34.0), "Add", ButtonStyle::Primary, false);
        }
        y + card.h + 8.0
    }

    /// A repository's page: what it offers, to install.
    #[expect(clippy::too_many_arguments, reason = "the page's place, what is on screen, and what it shows")]
    fn repository(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        visible: (f32, f32),
        source: &Source,
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let view = &mut shell.skills;
        if icon_button(p, ui, Rect::new(x - 6.0, y + 2.0, 30.0, 30.0), icon::CHEVRON_LEFT, true) {
            self.open.pop();
            self.filter.replace(0, "");
            return y;
        }
        let name = match view.catalogs.get(source) {
            Some(Fetched::Ready(catalog)) => catalog.name.clone(),
            _ => source.name(),
        };
        let loading = matches!(view.catalogs.get(source), Some(Fetched::Loading));
        let place = source.to_string();
        let top = y;
        let y = title(p, (x + 32.0, y, w - 80.0), &name, &place);
        // Reading it again sits by where it is read from.
        let width = p.layout(&place, theme::SMALL, None).width().min(w - 120.0);
        if icon_button(p, ui, Rect::new(x + 38.0 + width, top + 31.0, 26.0, 26.0), icon::REFRESH, !loading) {
            view.start(Job::Fetch(source.clone()), actions);
        }
        let (mut job, mut opened) = (None, None);
        let end = match view.catalogs.get(source) {
            Some(Fetched::Ready(catalog)) => {
                self.offers(p, ui, (x, y, w), visible, (source, catalog), view, (&mut job, &mut opened))
            }
            Some(Fetched::Failed(problem)) => {
                let text = p.layout(problem, theme::SMALL, Some(w));
                p.text(&text, x, y, t.text_muted);
                let at = y + text.height() + 14.0;
                if button(p, ui, Rect::new(x, at, 108.0, 34.0), "Try again", ButtonStyle::Secondary, true) {
                    job = Some(Job::Fetch(source.clone()));
                }
                at + 34.0
            }
            _ => {
                p.label("Looking in the repository…", theme::SMALL, x, y, t.text_faint);
                y + 30.0
            }
        };
        if let Some(job) = job {
            view.start(job, actions);
        }
        if let Some(source) = opened {
            self.open(source, view, actions);
        }
        end
    }

    /// What a repository offers: its skills by plugin, then the plugins it
    /// lists from elsewhere. Sets the job a click asked for, or the
    /// repository to open. Returns where it ends.
    #[expect(clippy::too_many_arguments, reason = "the page's place, what is on screen, and what it shows")]
    fn offers(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, mut y, w): (f32, f32, f32),
        visible: (f32, f32),
        (source, catalog): (&Source, &Catalog),
        view: &SkillsView,
        (job, opened): (&mut Option<Job>, &mut Option<Source>),
    ) -> f32 {
        let t = p.theme;

        // A filter, once there is much to look through.
        let count = catalog.skills().count() + catalog.elsewhere.len();
        if count > FILTER_FROM {
            let field = Rect::new(x, y, w, 40.0);
            icon::draw(p, icon::SEARCH, (field.x + 18.0, field.y + 20.0), 15.0, t.text_faint);
            let inner = Rect::new(field.x + 26.0, field.y, field.w - 26.0, field.h);
            if ui.pressed {
                self.filtering = ui.hovered(field);
            }
            let layout = self.filter.layout(p, 0, inner.w);
            p.bordered(field, t.bg, theme::RADIUS_SM, 1.0, if self.filtering { t.border_focus } else { t.border });
            self.filter.draw(0, p, ui, inner, layout, "Filter by name or description", true);
            y += 56.0;
        }
        let wanted = self.filter.text(0).trim().to_lowercase();
        let fits = |name: &str, description: &str| {
            wanted.is_empty() || name.to_lowercase().contains(&wanted) || description.to_lowercase().contains(&wanted)
        };
        let shown = |y: f32| y + ROW >= visible.0 && y <= visible.1;
        let mut shown_any = false;
        for group in &catalog.groups {
            let skills: Vec<&Skill> =
                group.skills.iter().filter(|skill| fits(&skill.name, &skill.description)).collect();
            if skills.is_empty() {
                continue;
            }
            shown_any = true;
            if !group.name.is_empty() {
                y = group_heading(p, (x, y, w), &group.name, &group.description);
            }
            for skill in skills {
                if shown(y) {
                    let rect = Rect::new(x, y, w, ROW);
                    row(p, rect, (icon::SPARKLES, None), &skill.name, &skill.description, 130.0);
                    if let Some(wanted) = offered(p, ui, rect, skill, source, view) {
                        *job = Some(wanted);
                    }
                }
                y += STEP;
            }
            y += 10.0;
        }
        let elsewhere: Vec<_> =
            catalog.elsewhere.iter().filter(|plugin| fits(&plugin.name, &plugin.description)).collect();
        if !elsewhere.is_empty() {
            shown_any = true;
            y = group_heading(
                p,
                (x, y, w),
                "In other repositories",
                "Plugins this marketplace lists from elsewhere. Open one to see its skills.",
            );
            for plugin in elsewhere {
                if shown(y) {
                    let rect = Rect::new(x, y, w, ROW);
                    row(p, rect, (source_icon(&plugin.source), None), &plugin.name, &plugin.description, 100.0);
                    let at = Rect::new(rect.right() - 92.0, rect.y + 13.0, 80.0, 32.0);
                    if button(p, ui, at, "Open", ButtonStyle::Secondary, true) {
                        *opened = Some(plugin.source.clone());
                    }
                }
                y += STEP;
            }
        }
        if !shown_any {
            let text = if wanted.is_empty() { "No skills here." } else { "Nothing fits the filter." };
            p.label(text, theme::SMALL, x, y, t.text_faint);
            y += 30.0;
        }
        y
    }
}

/// The control at the right of a skill a repository offers: Install, or
/// what stands in its way. Returns the job a click asked for.
fn offered(p: &mut Painter, ui: &mut Ui, rect: Rect, skill: &Skill, source: &Source, view: &SkillsView) -> Option<Job> {
    let t = p.theme;
    let origin = view.record.installed.get(&skill.name);
    let installed = view.installed.iter().any(|kept| kept.name == skill.name);
    let at = Rect::new(rect.right() - 108.0, rect.y + 13.0, 96.0, 32.0);
    let mine = origin.is_some_and(|origin| origin.source == *source && origin.path == skill.path);
    let quiet = |p: &mut Painter, glyph: Option<char>, text: &str| {
        let width = p.layout(text, theme::SMALL, None).width();
        let right = rect.right() - 16.0;
        p.label(text, theme::SMALL, right - width, rect.y + 20.0, t.text_faint);
        if let Some(glyph) = glyph {
            icon::draw(p, glyph, (right - width - 12.0, rect.y + 29.0), 14.0, t.text_faint);
        }
    };
    if view.busy.contains(&skill.name) {
        quiet(p, None, "Working…");
        None
    } else if installed && mine && origin.is_some_and(|origin| origin.digest == skill.digest) {
        quiet(p, Some(icon::CHECK), "Installed");
        None
    } else if installed && mine {
        button(p, ui, at, "Update", ButtonStyle::Secondary, true)
            .then(|| Job::Install(Box::new(skill.clone()), source.clone()))
    } else if installed {
        // Another skill has its name.
        quiet(p, None, "Name taken");
        None
    } else {
        button(p, ui, at, "Install", ButtonStyle::Secondary, true)
            .then(|| Job::Install(Box::new(skill.clone()), source.clone()))
    }
}

/// The icon of where a repository is.
fn source_icon(source: &Source) -> char {
    match source {
        Source::Folder { .. } => icon::FOLDER,
        Source::GitHub { .. } | Source::Git { .. } => icon::BRANCH,
    }
}

/// A page's title and what it is for. Returns where what follows starts.
fn title(p: &mut Painter, (x, y, w): (f32, f32, f32), title: &str, about: &str) -> f32 {
    let t = p.theme;
    let mut heading = p.layout(title, theme::TITLE, None);
    heading.truncate(p.fonts, w);
    p.text(&heading, x, y, t.text);
    let about = p.layout(about, theme::SMALL, Some(w - 40.0));
    p.text(&about, x, y + 36.0, t.text_muted);
    y + 36.0 + about.height() + 26.0
}

/// A section's heading with, at its right, a button to add to it. Returns whether it was clicked.
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

/// A plugin's name over what it is for. Returns where its skills start.
fn group_heading(p: &mut Painter, (x, y, w): (f32, f32, f32), name: &str, description: &str) -> f32 {
    let t = p.theme;
    p.label(name, theme::LABEL, x, y + 4.0, t.text);
    let mut y = y + 26.0;
    if !description.is_empty() {
        let text = p.layout(description, theme::TINY, Some(w));
        p.text(&text, x, y, t.text_muted);
        y += text.height() + 4.0;
    }
    y + 8.0
}

/// A small line of explanation. Returns where what follows starts.
fn note(p: &mut Painter, (x, y, w): (f32, f32, f32), text: &str) -> f32 {
    let note = p.layout(text, theme::TINY, Some(w));
    p.text(&note, x, y, p.theme.text_faint);
    y + note.height()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, digest: &str) -> Skill {
        Skill { name: name.into(), path: format!("skills/{name}"), digest: digest.into(), ..Skill::default() }
    }

    #[test]
    fn updates_follow_the_repository_a_skill_came_from() {
        let mut view = SkillsView::default();
        let source = Source::parse("me/skills").unwrap();
        let mut actions = Vec::new();
        view.start(Job::Install(Box::new(skill("pdf", "1")), source.clone()), &mut actions);
        assert!(view.busy.contains("pdf") && matches!(actions[..], [Action::Skills(Job::Install(..))]));
        let mut record = Record::default();
        let origin =
            atroxabot_core::skills::Origin { source: source.clone(), path: "skills/pdf".into(), digest: "1".into() };
        record.installed.insert("pdf".into(), origin);
        assert_eq!(view.report(Report::Library { skills: vec![skill("pdf", "1")], record, problem: None }), None);
        assert!(view.busy.is_empty(), "the change came back");
        assert!(view.update("pdf").is_none(), "its repository was not looked at");

        // Looking at the repositories of installed skills happens once.
        view.check_updates(&mut actions);
        view.check_updates(&mut actions);
        assert_eq!(actions.len(), 2);
        let newer = Catalog {
            groups: vec![atroxabot_core::skills::Group { skills: vec![skill("pdf", "2")], ..Default::default() }],
            ..Catalog::default()
        };
        view.report(Report::Fetched { source: source.clone(), catalog: Ok(Box::new(newer)) });
        assert_eq!(view.update("pdf").map(|(skill, from)| (skill.digest.as_str(), from)), Some(("2", &source)));
    }

    #[test]
    fn typing_goes_to_the_field_on_screen() {
        let mut page = SkillsPage::new();
        page.adding = true;
        assert!(page.typing());
        page.commit("me/skills");
        assert_eq!(page.address.text(0), "me/skills");
        page.open.push(Source::parse("me/skills").unwrap());
        assert!(!page.typing(), "the filter takes the keyboard only once clicked");
        page.commit("lost");
        assert_eq!(page.filter.text(0), "");
    }
}
