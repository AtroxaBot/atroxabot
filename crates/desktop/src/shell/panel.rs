//! The side panel, right of the session: what the session used and how full
//! its context is, the agent's task list and the turn's subagents, where
//! the Claude account's plan limits stand, and its folder's git repository
//! (the branch and what changed: committing and pushing are the agent's to
//! do). A changed file opens as a diff, a modal.

#![expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]

use atroxabot_core::git::{Change, GitFile, GitStatus, Job, Report};
use atroxabot_core::stats::Ledger;
use atroxabot_core::{ConnectionKind, Limits, TodoState};
use winit::window::CursorIcon;

use super::menu::Menu;
use super::thread::{Thread, agent_mark};
use super::widgets::{Scroll, dropdown, field_label, modal};
use super::{Modal, Shell, compact, money, until};
use crate::app::Action;
use crate::paint::{Color, Painter, Rect, fade, mix};
use crate::text::Style;
use crate::theme::{self, Palette};
use crate::ui::{Ui, icon, icon_button, id};

/// The panel's width.
pub(super) const WIDTH: f32 = 320.0;
/// Height of a changed file's row.
const FILE_H: f32 = 30.0;
/// Height of a line of a diff.
const LINE_H: f32 = 19.0;
/// What a tab in a diff's line is drawn as.
const TAB: &str = "    ";
/// Most tasks of the agent's list shown at once.
const TASKS_SHOWN: usize = 8;
/// Most subagents of a turn listed at once.
const AGENTS_SHOWN: usize = 8;
/// The diff's text.
const MONO: Style = Style::mono(12.5);

/// The repository of the folder the open session works in.
#[derive(Default)]
pub(super) struct GitView {
    /// The folder `status` is of.
    pub folder: String,
    /// `None` when the folder is in no repository (or was not read yet).
    pub status: Option<GitStatus>,
    /// The status is being read.
    loading: bool,
    scroll: Scroll,
}

/// A changed file's diff, open as a modal.
pub(super) struct DiffView {
    file: GitFile,
    /// `None` while git is asked.
    text: Option<Result<String, String>>,
    scroll: Scroll,
    /// How far its long lines are scrolled sideways.
    across: Scroll,
}

/// What a window of a plan's limits is called. Claude Code's own `/usage`
/// calls `seven_day_overage_included` the week's limit for Fable.
fn window_name(name: &str) -> String {
    match name {
        "five_hour" => "5-hour limit".to_owned(),
        "seven_day" => "Weekly limit".to_owned(),
        "seven_day_overage_included" => "Weekly, Fable".to_owned(),
        other => match other.strip_prefix("seven_day_") {
            // A model's own week: its name, capitalised.
            Some(model) => {
                let model = model.replace('_', " ");
                let mut letters = model.chars();
                let first: String = letters.next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                format!("Weekly, {first}{}", letters.as_str())
            }
            None => other.replace('_', " "),
        },
    }
}

/// A plan's limits as bars, one per window, each with how much is used
/// and when it starts over. A window that started over since it was last
/// reported is shown empty. Returns where they end.
pub(super) fn limit_bars(p: &mut Painter, limits: &Limits, (x, mut y, w): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    let now = atroxabot_core::unix_now();
    for window in &limits.windows {
        let over = window.resets > 0 && window.resets <= now;
        let full = if over { 0.0 } else { window.used.clamp(0.0, 1.0) as f32 };
        p.label(&window_name(&window.name), theme::SMALL, x, y, t.text_muted);
        let used = p.layout(&format!("{:.0}%", full * 100.0), theme::SMALL, None);
        p.text(&used, x + w - used.width(), y, t.text);
        y += 24.0;
        p.rect(Rect::new(x, y, w, 4.0), t.hover, 2.0);
        if full > 0.0 {
            let ink = mix(t.text_muted, t.danger, ((full - 0.7) / 0.3).max(0.0));
            p.rect(Rect::new(x, y, (w * full).max(4.0), 4.0), ink, 2.0);
        }
        y += 10.0;
        if window.resets > 0 {
            let when = if over {
                "Started over since".to_owned()
            } else {
                format!("Starts over {}", until(now, window.resets))
            };
            p.label(&when, theme::TINY, x, y, t.text_faint);
            y += 18.0;
        }
        y += 8.0;
    }
    y
}

/// The colour a kind of change is written in.
fn change_ink(t: &Palette, change: Change) -> Color {
    match change {
        Change::Added => t.positive,
        Change::Deleted | Change::Conflict => t.danger,
        Change::Modified | Change::Renamed => t.text_muted,
    }
}

/// "+12" and "−3" ending at `right`, in the diff's colours. Returns where they start.
fn counts(p: &mut Painter, (added, removed): (u32, u32), right: f32, y: f32) -> f32 {
    let t = p.theme;
    let mut x = right;
    for (count, sign, ink) in [(removed, '−', t.danger), (added, '+', t.positive)] {
        if count > 0 {
            let text = p.layout(&format!("{sign}{count}"), theme::TINY, None);
            x -= text.width();
            p.text(&text, x, y, ink);
            x -= 6.0;
        }
    }
    x
}

impl DiffView {
    /// The diff of `file`, still to be read.
    pub fn new(file: GitFile) -> Self {
        Self { file, text: None, scroll: Scroll::default(), across: Scroll::default() }
    }

    /// Draws the diff over `view`. Returns whether it was closed.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect) -> bool {
        let t = p.theme;
        let (card, outside) = modal(p, ui, view, (1000.0, 800.0), "diff");
        let mut close = outside;
        if icon_button(p, ui, Rect::new(card.right() - 44.0, card.y + 14.0, 30.0, 30.0), icon::X, true) {
            close = true;
        }
        let letter = p.label(
            self.file.change.letter(),
            theme::HEADING,
            card.x + 24.0,
            card.y + 19.0,
            change_ink(&t, self.file.change),
        );
        let left = counts(p, (self.file.added, self.file.removed), card.right() - 58.0, card.y + 23.0);
        let mut path = p.layout(&self.file.path, theme::HEADING, None);
        path.truncate(p.fonts, left - card.x - 50.0 - letter);
        p.text(&path, card.x + 34.0 + letter, card.y + 19.0, t.text);
        p.rect(Rect::new(card.x + 1.0, card.y + 56.0, card.w - 2.0, 1.0), t.border, 0.0);
        let body = Rect::new(card.x + 1.0, card.y + 57.0, card.w - 2.0, card.h - 66.0);
        let note = match &self.text {
            None => Some("Reading the diff…".to_owned()),
            Some(Err(why)) => Some(why.clone()),
            Some(Ok(text)) if text.trim().is_empty() => {
                Some("Nothing to show: a binary file, or only its mode changed.".to_owned())
            }
            Some(Ok(_)) => None,
        };
        let (Some(Ok(text)), None) = (&self.text, &note) else {
            let note = p.layout(note.as_deref().unwrap_or_default(), theme::SMALL, Some(body.w - 48.0));
            p.text(&note, body.x + 24.0, body.y + 20.0, t.text_faint);
            return close;
        };
        let hovered = body.contains(ui.mouse);
        let offset = self.scroll.update(ui, hovered);
        // Sideways it scrolls the same way, fed the sideways wheel (Shift turns a plain one).
        std::mem::swap(&mut ui.scroll, &mut ui.scroll_x);
        let across = self.across.update(ui, hovered);
        std::mem::swap(&mut ui.scroll, &mut ui.scroll_x);
        // ponytail: columns are counted in characters of one width, so a long line of wide glyphs (CJK,
        // emoji) drifts when scrolled sideways. Lay the whole line out if that matters.
        let column = p.layout("0", MONO, None).width().max(1.0);
        let (skipped, columns) = ((across / column) as usize, (body.w / column) as usize + 2);
        let clip = p.push_clip(body);
        let first = (offset / LINE_H) as usize;
        let visible = (body.h / LINE_H) as usize + 2;
        // Only the lines in view are laid out.
        for (index, line) in text.lines().enumerate().skip(first).take(visible) {
            let y = body.y + 8.0 + index as f32 * LINE_H - offset;
            let (ink, wash) = if line.starts_with("+++")
                || line.starts_with("---")
                || line.starts_with("diff ")
                || line.starts_with("index ")
            {
                (t.text_faint, None)
            } else if line.starts_with('+') {
                (t.positive, Some(fade(t.positive, 0.1)))
            } else if line.starts_with('-') {
                (t.danger, Some(fade(t.danger, 0.1)))
            } else if line.starts_with("@@") {
                (t.text_faint, Some(t.code_bg))
            } else {
                (t.text_muted, None)
            };
            if let Some(wash) = wash {
                p.rect(Rect::new(body.x, y, body.w, LINE_H), wash, 0.0);
            }
            // Only the part of the line in view is laid out.
            let shown: String = line.replace('\t', TAB).chars().skip(skipped).take(columns).collect();
            p.label(&shown, MONO, body.x + 20.0 + skipped as f32 * column - across, y + 1.0, ink);
        }
        p.set_clip(clip);
        let content = text.lines().count() as f32 * LINE_H + 16.0;
        self.scroll.finish(ui, content, body.h);
        self.scroll.bar(p, body, content);
        let widest = text.lines().map(|line| line.chars().count() + (TAB.len() - 1) * line.matches('\t').count()).max();
        let wide = widest.unwrap_or(0) as f32 * column + 40.0;
        self.across.finish(ui, wide, body.w);
        if let Some(progress) = self.across.progress() {
            let track = body.w - 16.0;
            let thumb = super::widgets::thumb(track, body.w, wide);
            let at = Rect::new(body.x + 8.0 + (track - thumb) * progress, body.bottom() - 7.0, thumb, 3.0);
            p.rect(at, fade(t.text_faint, 0.35), 1.5);
        }
        close
    }
}

impl Shell {
    /// The folder the open session works in, or the next one will: the
    /// user's home when none was chosen.
    pub(super) fn work_folder(&self) -> String {
        let folder = self.folder_in_use();
        if folder.trim().is_empty() {
            std::env::home_dir().map(|home| home.display().to_string()).unwrap_or_default()
        } else {
            folder
        }
    }

    /// Asks git to do `job` in the session's folder.
    fn git_job(&mut self, job: Job, actions: &mut Vec<Action>) {
        self.git.folder = self.work_folder();
        self.git.loading = true;
        actions.push(Action::Git { folder: self.git.folder.clone(), job });
    }

    /// Reads the repository again, if the panel is there to show it.
    pub(super) fn refresh_git(&mut self, actions: &mut Vec<Action>) {
        if self.config.panel {
            self.git_job(Job::Refresh, actions);
        }
    }

    /// Git answered about `folder`.
    pub fn git_report(&mut self, folder: &str, report: Report) {
        if folder != self.git.folder {
            return;
        }
        match report {
            Report::Status { status, problem } => {
                (self.git.status, self.git.loading) = (status, false);
                if let Some(problem) = problem {
                    self.notify(problem);
                }
            }
            Report::Diff { path, text } => {
                if let Some(Modal::Diff(view)) = &mut self.modal
                    && view.file.path == path
                {
                    view.text = Some(text);
                }
            }
        }
    }

    /// Shows or hides the panel.
    pub(super) fn toggle_panel(&mut self, actions: &mut Vec<Action>) {
        self.config.panel = !self.config.panel;
        actions.push(Action::SaveConfig);
        self.refresh_git(actions);
    }

    /// Checks out `branch` in the session's folder.
    pub(super) fn switch_branch(&mut self, branch: &str, actions: &mut Vec<Action>) {
        if self.thread().is_some_and(|thread| thread.busy) {
            self.notify("Wait for the reply to finish, or stop it, before switching branch.".into());
        } else if self.git.status.as_ref().is_some_and(|status| status.branch != branch) {
            self.git_job(Job::Switch(branch.to_owned()), actions);
        }
    }

    /// What the open session used, as rows of a name and a number. Returns where they end.
    fn draw_usage(&self, p: &mut Painter, (x, mut y, w): (f32, f32, f32)) -> f32 {
        let t = p.theme;
        let Some(thread) = self.thread().filter(|thread| thread.session.tokens() > 0) else {
            p.label("Nothing used yet", theme::SMALL, x, y, t.text_faint);
            return y + 26.0;
        };
        let total = Ledger::collect(std::slice::from_ref(&thread.session)).summary(0).total;
        let context = thread.session.context_tokens();
        // How much the model takes: what its engine said, or what its provider lists.
        let listed = || {
            let agent = self.selected_agent()?;
            let connection = self.connection(agent)?;
            self.model_info(&connection.id, &agent.model).map(|model| model.context_window)
        };
        let window = if thread.session.window > 0 { thread.session.window } else { listed().unwrap_or(0) };
        let filled = if window > 0 { format!("{} of {}", compact(context), compact(window)) } else { compact(context) };
        let mut rows = vec![
            ("Context", filled),
            ("Tokens", format!("{} in · {} out", compact(total.input), compact(total.output))),
            ("From cache", format!("{:.0}%", total.cache_rate() * 100.0)),
            ("Tool calls", total.tools.to_string()),
        ];
        if total.cost > 0.0 {
            rows.push(("Cost", money(total.cost)));
        }
        for (index, (name, value)) in rows.into_iter().enumerate() {
            p.label(name, theme::SMALL, x, y, t.text_muted);
            let value = p.layout(&value, theme::SMALL, None);
            p.text(&value, x + w - value.width(), y, t.text);
            y += 24.0;
            if index == 0 && window > 0 {
                let full = (context as f32 / window as f32).clamp(0.0, 1.0);
                p.rect(Rect::new(x, y, w, 4.0), t.hover, 2.0);
                p.rect(
                    Rect::new(x, y, (w * full).max(4.0), 4.0),
                    mix(t.text_muted, t.danger, ((full - 0.7) / 0.3).max(0.0)),
                    2.0,
                );
                y += 14.0;
            }
        }
        y
    }

    /// The agent's task list, when it keeps one: what is done, what it is
    /// doing now and what is still to do. With more than fit, the ones
    /// around the task in hand are shown. Returns where it ends (`y` itself
    /// when there is no list).
    fn draw_todos(&self, p: &mut Painter, (x, mut y, w): (f32, f32, f32)) -> f32 {
        let t = p.theme;
        let todos = self.thread().map(|thread| thread.session.todos()).unwrap_or_default();
        if todos.is_empty() {
            return y;
        }
        p.rect(Rect::new(x - 16.0, y, w + 32.0, 1.0), t.border, 0.0);
        y += 18.0;
        field_label(p, icon::LIST, "TASKS", x, y);
        let done = todos.iter().filter(|todo| todo.state == TodoState::Done).count();
        let count = p.layout(&format!("{done} of {}", todos.len()), theme::TINY, None);
        p.text(&count, x + w - count.width(), y + 1.0, t.text_faint);
        y += 26.0;
        let current = todos.iter().position(|todo| todo.state != TodoState::Done).unwrap_or(0);
        let first = current.saturating_sub(2).min(todos.len().saturating_sub(TASKS_SHOWN));
        for todo in todos.iter().skip(first).take(TASKS_SHOWN) {
            let mark = Rect::new(x + 2.0, y + 4.0, 10.0, 10.0);
            let ink = match todo.state {
                TodoState::Done => {
                    icon::draw(p, icon::CHECK, (x + 7.0, y + 9.0), 13.0, t.text_faint);
                    t.text_faint
                }
                TodoState::Active => {
                    p.rect(mark, t.text, 5.0);
                    t.text
                }
                TodoState::Pending => {
                    p.bordered(mark, fade(t.text, 0.0), 5.0, 1.0, t.text_faint);
                    t.text_muted
                }
            };
            let mut subject = p.layout(&todo.subject, theme::SMALL, None);
            subject.truncate(p.fonts, w - 22.0);
            p.text(&subject, x + 22.0, y, ink);
            y += 24.0;
        }
        let hidden = todos.len().saturating_sub(first + TASKS_SHOWN);
        if hidden + first > 0 {
            p.label(&format!("{} more", hidden + first), theme::TINY, x + 22.0, y, t.text_faint);
            y += 20.0;
        }
        y + 6.0
    }

    /// The subagents of the turn, when it has any: how each is doing and
    /// what it is for. A click opens one in the conversation's place, a
    /// click on the open one goes back. With more than fit, the ones from
    /// the first still at work are shown. Returns where the list ends (`y`
    /// itself when there are none).
    fn draw_subagents(&mut self, p: &mut Painter, ui: &mut Ui, (x, mut y, w): (f32, f32, f32)) -> f32 {
        let t = p.theme;
        let agents = self.thread().map(Thread::subagents).unwrap_or_default();
        if agents.is_empty() {
            return y;
        }
        p.rect(Rect::new(x - 16.0, y, w + 32.0, 1.0), t.border, 0.0);
        y += 18.0;
        field_label(p, icon::BOT, "SUBAGENTS", x, y);
        let done = agents.iter().filter(|agent| !agent.running).count();
        let count = p.layout(&format!("{done} of {}", agents.len()), theme::TINY, None);
        p.text(&count, x + w - count.width(), y + 1.0, t.text_faint);
        y += 26.0;
        let working = agents.iter().position(|agent| agent.running).unwrap_or(agents.len());
        let first = working.min(agents.len().saturating_sub(AGENTS_SHOWN));
        let mut pick = None;
        for agent in agents.iter().skip(first).take(AGENTS_SHOWN) {
            let row = Rect::new(x - 8.0, y - 4.0, w + 16.0, 26.0);
            let hovered = ui.hovered(row);
            let hover = ui.anim(id(("subagent", &agent.id)), f32::from(u8::from(hovered || agent.open)));
            p.rect(row, fade(t.hover, hover), theme::RADIUS_SM);
            let ink = mix(if agent.running { t.text } else { t.text_muted }, t.text, hover);
            agent_mark(p, ui, (x + 7.0, y + 9.0), (agent.running, agent.failed), ink);
            let mut label = p.layout(&agent.label, theme::SMALL, None);
            label.truncate(p.fonts, w - 22.0);
            p.text(&label, x + 22.0, y, ink);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    pick = Some((!agent.open).then(|| agent.id.clone()));
                }
            }
            y += 26.0;
        }
        let hidden = agents.len().saturating_sub(AGENTS_SHOWN);
        if hidden > 0 {
            p.label(&format!("{hidden} more"), theme::TINY, x + 22.0, y, t.text_faint);
            y += 20.0;
        }
        if let Some(pick) = pick
            && let Some(thread) = self.thread_mut()
        {
            // It shows with the next frame.
            thread.open_agent(pick);
            ui.animating = true;
        }
        y + 6.0
    }

    /// The Claude account whose plan limits the panel shows: the one the top
    /// bar names, which the next message goes to (whatever account the open
    /// session ran on before). `None` when that is an API provider.
    pub(super) fn limits_account(&self) -> Option<String> {
        let connection = self.selected_agent().and_then(|agent| self.connection(agent))?;
        (connection.kind == ConnectionKind::ClaudeAgent).then_some(connection.id)
    }

    /// Where the plan limits of the Claude account the top bar names stand,
    /// as Claude Code last reported them: a bar per window and when it
    /// starts over, or a line saying they come with its next reply. Who is
    /// signed in to it is not put on screen here. Returns where they end
    /// (`y` itself for an API provider).
    fn draw_limits(&self, p: &mut Painter, (x, mut y, w): (f32, f32, f32)) -> f32 {
        let t = p.theme;
        let Some(account) = self.limits_account() else { return y };
        p.rect(Rect::new(x - 16.0, y, w + 32.0, 1.0), t.border, 0.0);
        y += 18.0;
        field_label(p, icon::ACTIVITY, "PLAN USAGE", x, y);
        y += 28.0;
        if let Some(limits) = self.limits.get(&account).filter(|limits| !limits.windows.is_empty()) {
            return limit_bars(p, limits, (x, y, w));
        }
        let note = p.layout("Known after this account's next reply.", theme::SMALL, Some(w));
        p.text(&note, x, y, t.text_faint);
        y + note.height() + 16.0
    }

    /// Draws the panel in `rect`.
    pub(super) fn draw_panel(&mut self, p: &mut Painter, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        p.shadow(Rect::new(rect.x, rect.y + 2.0, rect.w, rect.h), fade(t.shadow, 0.35), theme::CANVAS_RADIUS, 18.0);
        p.rect(rect, t.bg, theme::CANVAS_RADIUS);
        // Another session, another folder: its repository is read.
        if self.work_folder() != self.git.folder {
            self.refresh_git(actions);
        }
        let clip = p.push_clip(rect);
        let (x, w) = (rect.x + 16.0, rect.w - 32.0);
        field_label(p, icon::LAYERS, "SESSION", x, rect.y + 20.0);
        let mut y = self.draw_usage(p, (x, rect.y + 48.0, w)) + 8.0;
        y = self.draw_todos(p, (x, y, w));
        y = self.draw_subagents(p, ui, (x, y, w));
        y = self.draw_limits(p, (x, y, w));
        p.rect(Rect::new(rect.x, y, rect.w, 1.0), t.border, 0.0);
        y += 18.0;
        field_label(p, icon::BRANCH, "GIT", x, y);
        if icon_button(p, ui, Rect::new(x + w - 26.0, y - 6.0, 26.0, 26.0), icon::REFRESH, !self.git.loading) {
            self.refresh_git(actions);
        }
        y += 30.0;
        // Taken out while it is drawn, as the menus and the notices need the shell.
        if let Some(status) = self.git.status.take() {
            self.draw_git(p, ui, (x, y, w), rect.bottom(), &status, actions);
            self.git.status.get_or_insert(status);
        } else {
            let note = if self.git.loading { "Reading…" } else { "This folder is not in a git repository." };
            let note = p.layout(note, theme::SMALL, Some(w));
            p.text(&note, x, y, t.text_faint);
        }
        p.set_clip(clip);
    }

    /// The repository: its branch and what changed.
    fn draw_git(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, mut y, w): (f32, f32, f32),
        bottom: f32,
        status: &GitStatus,
        actions: &mut Vec<Action>,
    ) {
        let t = p.theme;
        let branch = Rect::new(x, y, w, 36.0);
        if dropdown(p, ui, branch, Some(icon::BRANCH), &status.branch, self.menu_open(Menu::Branch)) {
            self.toggle_menu(Menu::Branch, branch);
        }
        y += 44.0;
        let sync = match (status.upstream.is_empty(), status.ahead, status.behind) {
            (true, ..) => "Not pushed anywhere yet".to_owned(),
            (false, 0, 0) => format!("Up to date with {}", status.upstream),
            (false, ahead, 0) => format!("{ahead} to push to {}", status.upstream),
            (false, 0, behind) => format!("{behind} to pull from {}", status.upstream),
            (false, ahead, behind) => format!("{ahead} to push · {behind} to pull"),
        };
        let mut sync = p.layout(&sync, theme::TINY, None);
        sync.truncate(p.fonts, w);
        p.text(&sync, x, y, t.text_faint);
        y += 28.0;

        let changes =
            if status.files.is_empty() { "CHANGES".to_owned() } else { format!("CHANGES  {}", status.files.len()) };
        p.label(&changes, theme::CAPTION, x, y, t.text_muted);
        let total = status.files.iter().fold((0, 0), |sum, file| (sum.0 + file.added, sum.1 + file.removed));
        counts(p, total, x + w, y);
        y += 24.0;

        // The files scroll in what is left of the panel.
        let list = Rect::new(x - 8.0, y, w + 16.0, (bottom - y - 10.0).max(0.0));
        let offset = self.git.scroll.update(ui, ui.hovered(list));
        let clip = p.push_clip(list);
        if status.files.is_empty() {
            p.label("Nothing changed", theme::SMALL, x, y + 4.0, t.text_faint);
        }
        let mut row_y = list.y - offset;
        let mut open = None;
        for file in &status.files {
            let row = Rect::new(list.x, row_y, list.w, FILE_H - 2.0);
            row_y += FILE_H;
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hovered = ui.hovered(row) && list.contains(ui.mouse);
            let hover = ui.anim(id(("changed", &file.path)), f32::from(u8::from(hovered)));
            p.rect(row, fade(t.hover, hover), theme::RADIUS_SM);
            let mid = row.y + row.h * 0.5;
            p.label(file.change.letter(), theme::CAPTION, row.x + 9.0, mid - 8.0, change_ink(&t, file.change));
            let left = counts(p, (file.added, file.removed), row.right() - 8.0, mid - 7.0);
            // Its name, then faintly where it is.
            let (folder, name) = file.path.rsplit_once('/').unwrap_or(("", &file.path));
            let room = left - row.x - 34.0;
            let mut name = p.layout(name, theme::SMALL, None);
            name.truncate(p.fonts, room);
            p.text(&name, row.x + 26.0, mid - name.height() * 0.5, mix(t.text_muted, t.text, 0.5 + 0.5 * hover));
            let mut folder = p.layout(folder, theme::TINY, None);
            folder.truncate(p.fonts, room - name.width() - 8.0);
            p.text(&folder, row.x + 34.0 + name.width(), mid - folder.height() * 0.5 + 0.5, t.text_faint);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    open = Some(file.clone());
                }
            }
        }
        p.set_clip(clip);
        self.git.scroll.finish(ui, row_y + offset - list.y, list.h);
        if let Some(file) = open {
            actions.push(Action::Git { folder: self.git.folder.clone(), job: Job::Diff(file.clone()) });
            self.modal = Some(Modal::Diff(Box::new(DiffView::new(file))));
        }
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, Config};

    use super::*;

    #[test]
    fn limit_windows_are_named_as_claude_code_names_them() {
        let names =
            ["five_hour", "seven_day", "seven_day_overage_included", "seven_day_opus", "monthly_cap"].map(window_name);
        assert_eq!(names, ["5-hour limit", "Weekly limit", "Weekly, Fable", "Weekly, Opus", "monthly cap"]);
    }

    #[test]
    fn the_panel_follows_git() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.refresh_git(&mut actions);
        assert!(actions.is_empty(), "nothing is read while the panel is closed");
        shell.pending_folder = "/work".into();
        shell.toggle_panel(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Git { folder, job: Job::Refresh } if folder == "/work")));

        let status = GitStatus { branch: "main".into(), ..GitStatus::default() };
        let report = |status: &GitStatus| Report::Status { status: Some(status.clone()), problem: None };
        shell.git_report("/elsewhere", report(&status));
        assert!(shell.git.status.is_none(), "an answer about another folder is dropped");
        shell.git_report("/work", report(&status));
        assert_eq!(shell.git.status.as_ref().map(|s| s.branch.as_str()), Some("main"));
        assert!(!shell.git.loading);

        actions.clear();
        shell.switch_branch("main", &mut actions);
        assert!(actions.is_empty(), "already there");
        shell.switch_branch("side", &mut actions);
        assert!(matches!(&actions[..], [Action::Git { job: Job::Switch(branch), .. }] if branch == "side"));
    }
}
