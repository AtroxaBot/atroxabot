//! Sessions in a git worktree of their own: the choice in the folder menu,
//! what the app knows of each worktree, when it has them cleaned up
//! (`atroxabot_core::worktree` says what goes and what stays), and the
//! settings page that lists what is on disk.

#![expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]

use std::time::{Duration, Instant};

use atroxabot_core::Worktree;
use atroxabot_core::worktree::{Job, Known, Report, State};

use super::menu::{Menu, folder_name};
use super::settings::Settings;
use super::widgets::{dropdown, row};
use super::{Modal, Shell};
use crate::app::Action;
use crate::log;
use crate::paint::{Painter, Rect};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, icon, icon_button};

/// How often, at most, coming back to the window has the worktrees looked at.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// When an unused worktree is cleaned up: what the settings store, what the
/// list calls it, and a line about it.
pub(super) const IDLE: [(&str, &str, &str); 5] = [
    ("1", "After a day", "A session unused for a day gives up its worktree."),
    ("3", "After 3 days", "A session unused for three days gives up its worktree."),
    ("7", "After a week", "A session unused for a week gives up its worktree."),
    ("30", "After a month", "A session unused for a month gives up its worktree."),
    ("never", "Never", "Only merged work and deleted sessions are cleaned up."),
];

/// The choice of [`IDLE`] the settings hold: three days unless said otherwise.
pub(super) fn idle_choice(setting: Option<&str>) -> (&'static str, &'static str, &'static str) {
    IDLE.into_iter().find(|(key, ..)| Some(*key) == setting).unwrap_or(IDLE[1])
}

/// A size on disk, short: "840 KB", "312 MB", "4.2 GB".
pub(super) fn bytes(size: u64) -> String {
    const KB: f64 = 1024.0;
    let size = size as f64;
    if size >= KB * KB * KB {
        format!("{:.1} GB", size / (KB * KB * KB))
    } else if size >= KB * KB {
        format!("{:.0} MB", size / (KB * KB))
    } else {
        format!("{:.0} KB", (size / KB).ceil())
    }
}

/// `count` of something, with its noun in the right number: "1 change", "3 changes".
fn counted(count: u32, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// One line on how a worktree stands.
pub(super) fn standing(state: &State) -> String {
    if !state.exists {
        return match state.unmerged {
            0 => "cleaned up".to_owned(),
            ahead => format!("cleaned up · {} kept on its branch", counted(ahead, "commit")),
        };
    }
    match (state.changes, state.unmerged, state.landed) {
        (0, 0, true) => "merged".to_owned(),
        (0, 0, false) => "nothing to merge".to_owned(),
        (0, ahead, _) => format!("{} to merge", counted(ahead, "commit")),
        (changes, 0, _) => counted(changes, "uncommitted change"),
        (changes, ahead, _) => {
            format!("{} · {} to merge", counted(changes, "uncommitted change"), counted(ahead, "commit"))
        }
    }
}

impl Shell {
    /// The folder the user chose for the open session, or for the next one:
    /// for a session in a worktree, the folder of the repository it stands for.
    pub(super) fn origin_in_use(&self) -> String {
        self.thread().map_or_else(|| self.pending_folder.clone(), |thread| thread.session.origin())
    }

    /// The worktree the open session works in, once it is made.
    pub(super) fn worktree_in_use(&self) -> Option<&Worktree> {
        self.thread().and_then(|thread| thread.session.worktree.as_ref()).filter(|worktree| worktree.made())
    }

    /// Whether the open session works in a worktree of its own, or the next one will.
    pub(super) fn wants_worktree(&self) -> bool {
        self.thread().map_or(self.pending_worktree, |thread| thread.session.worktree.is_some())
    }

    /// Whether that can still be changed: until the worktree is made.
    pub(super) fn worktree_open(&self) -> bool {
        self.thread().is_none_or(|thread| {
            let asked = thread.session.worktree.is_some();
            !thread.busy && self.worktree_in_use().is_none() && (asked || thread.session.entries.is_empty())
        })
    }

    /// Turns the worktree of the open session (or the next one) on or off;
    /// the agent's next sessions start the same way.
    pub(super) fn toggle_worktree(&mut self, actions: &mut Vec<Action>) {
        if !self.worktree_open() || self.loading() {
            return;
        }
        let on = !self.wants_worktree();
        match self.thread_mut() {
            Some(thread) => {
                thread.session.worktree = on.then(Worktree::default);
                actions.push(Action::SaveSession(Box::new(thread.session.clone())));
            }
            None => self.pending_worktree = on,
        }
        if let Some(mut agent) = self.selected_agent().cloned().filter(|agent| agent.worktree != on) {
            agent.worktree = on;
            self.update_agent(agent, actions);
        }
    }

    /// The worktree of `session` is there: the session keeps where it is
    /// and works in it from now on.
    pub fn worktree_ready(&mut self, session: &str, worktree: &Worktree, workdir: &str, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session) else {
            // Its session was deleted while it was made: it goes again.
            let job = Job::Remove { session: session.to_owned(), worktree: worktree.clone(), discard: false };
            actions.push(Action::Worktree(job));
            return;
        };
        if thread.session.worktree.as_ref() != Some(worktree) || thread.session.workdir != workdir {
            thread.session.worktree = Some(worktree.clone());
            workdir.clone_into(&mut thread.session.workdir);
            actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        }
        self.worktrees.insert(session.to_owned(), State { exists: true, ..State::default() });
        // Its repository is one to tidy from now on.
        if !self.config.worktree_repos.contains(&worktree.repo) {
            self.config.worktree_repos.push(worktree.repo.clone());
            actions.push(Action::SaveConfig);
        }
        self.touch_summary(session);
        if self.current.as_deref() == Some(session) {
            self.refresh_git(actions);
        }
    }

    /// Has every worktree looked at and what is no longer needed taken
    /// away, unless that is under way.
    pub(super) fn sweep_worktrees(&mut self, actions: &mut Vec<Action>) {
        if self.sweeping {
            return;
        }
        let known: Vec<Known> = self
            .sessions
            .iter()
            .filter_map(|session| {
                let worktree = session.worktree.clone()?;
                Some(Known { session: session.id.clone(), worktree, updated: session.updated })
            })
            .collect();
        // Never used: nothing to look at.
        let kept = atroxabot_core::worktree::root().is_ok_and(|root| root.is_dir());
        if known.is_empty() && self.config.worktree_repos.is_empty() && !kept {
            return;
        }
        (self.sweeping, self.swept) = (true, Some(Instant::now()));
        let (idle, repos) = (self.config.worktree_idle_seconds(), self.config.worktree_repos.clone());
        actions.push(Action::Worktree(Job::Sweep { known, idle, repos }));
    }

    /// The same, when coming back to the window: what was merged meanwhile
    /// is found, though not more often than every minute.
    pub(super) fn sweep_worktrees_again(&mut self, actions: &mut Vec<Action>) {
        if self.swept.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
            self.sweep_worktrees(actions);
        }
    }

    /// Asks how the worktree of `session` stands, if it has one.
    pub(super) fn ask_worktree(&self, session: &str, actions: &mut Vec<Action>) {
        let worktree = self.sessions.iter().find(|known| known.id == session).and_then(|known| known.worktree.clone());
        if let Some(worktree) = worktree {
            actions.push(Action::Worktree(Job::State { session: session.to_owned(), worktree }));
        }
    }

    /// Has the folders the settings list measured.
    fn measure_worktrees(&self, actions: &mut Vec<Action>) {
        let of_sessions = self.sessions.iter().filter_map(|session| {
            let path = &session.worktree.as_ref()?.path;
            self.worktrees.get(&session.id).is_some_and(|state| state.exists).then(|| path.clone())
        });
        let paths: Vec<String> = of_sessions.chain(self.orphans.iter().map(|orphan| orphan.path.clone())).collect();
        if !paths.is_empty() {
            actions.push(Action::Worktree(Job::Measure(paths)));
        }
    }

    /// Whether the settings are open at the worktrees.
    fn lists_worktrees(&self) -> bool {
        matches!(&self.modal, Some(Modal::Settings(settings)) if settings.shows_worktrees())
    }

    /// Something asked about the worktrees is done.
    pub fn worktree_report(&mut self, report: Report, actions: &mut Vec<Action>) {
        match report {
            Report::Swept(swept) => {
                self.sweeping = false;
                for problem in &swept.problems {
                    log::error(format!("a worktree was not cleaned up: {problem}"));
                }
                let open = self.current.clone().filter(|session| swept.removed.contains(session));
                self.worktrees = swept.states.into_iter().collect();
                self.orphans = swept.orphans;
                self.sizes.retain(|path, _| std::path::Path::new(path).is_dir());
                // Those still holding something of the app's, and those of the sessions there are.
                let mut repos = swept.repos;
                let of_sessions = self.sessions.iter().filter_map(|session| session.worktree.as_ref());
                repos.extend(of_sessions.map(|worktree| worktree.repo.clone()));
                repos.sort_unstable();
                repos.dedup();
                if repos != self.config.worktree_repos {
                    self.config.worktree_repos = repos;
                    actions.push(Action::SaveConfig);
                }
                if open.is_some() {
                    self.refresh_git(actions);
                }
                if self.lists_worktrees() {
                    self.measure_worktrees(actions);
                }
            }
            Report::Removed { session, worktree, problem } => {
                self.worktrees.remove(&session);
                self.sizes.remove(&worktree.path);
                if let Some(problem) = problem {
                    // Kept rather than lost: it is one of the folders the settings list from now on.
                    self.notify(format!(
                        "The worktree {} was kept: {problem} It is listed under Settings, Worktrees.",
                        folder_name(&worktree.path)
                    ));
                    self.sweep_worktrees(actions);
                }
            }
            Report::OrphanRemoved { path, problem } => match problem {
                None => {
                    self.orphans.retain(|orphan| orphan.path != path);
                    self.sizes.remove(&path);
                }
                Some(problem) => self.notify(format!("{} could not be removed: {problem}", folder_name(&path))),
            },
            Report::State { session, state } => {
                self.worktrees.insert(session, state);
            }
            Report::Sizes(sizes) => self.sizes.extend(sizes),
        }
    }

    /// What the delete button of `session` says once armed: with a worktree
    /// that holds uncommitted changes, that they go with it.
    pub(super) fn delete_warning(&self, session: &str) -> String {
        match self.worktrees.get(session).map_or(0, |state| state.changes) {
            0 => "Click again to delete".to_owned(),
            changes => format!("Delete with {}", counted(changes, "uncommitted change")),
        }
    }

    /// The line about the open session's worktree, for the side panel:
    /// what it is of, and how it stands. `None` for a session without one.
    pub(super) fn worktree_line(&self) -> Option<String> {
        let worktree = self.worktree_in_use()?;
        let base = if worktree.base.len() == 40 && worktree.base.bytes().all(|b| b.is_ascii_hexdigit()) {
            // Started from a commit, not a branch: its first digits say enough.
            &worktree.base[..7]
        } else {
            worktree.base.as_str()
        };
        // What is uncommitted shows under it, as the changes: here, what is still to merge.
        let state = self.current.as_ref().and_then(|session| self.worktrees.get(session));
        let stands = match state.filter(|state| state.exists) {
            Some(state) if state.unmerged > 0 => format!(" · {} to merge", counted(state.unmerged, "commit")),
            Some(state) if state.landed => " · merged".to_owned(),
            _ => String::new(),
        };
        Some(format!("Own worktree, from {base}{stands}"))
    }
}

/// The settings page: when unused worktrees go, and what is on disk now.
/// `armed` is the folder whose remove button was clicked once. Returns
/// where the page ends, and the session to go to when the arrow of its
/// worktree was clicked.
pub(super) fn page(
    p: &mut Painter,
    ui: &mut Ui,
    (x, y, w): (f32, f32, f32),
    shell: &mut Shell,
    armed: &mut Option<String>,
    actions: &mut Vec<Action>,
) -> (f32, Option<String>) {
    let t = p.theme;
    let about = "A session can work in a git worktree of its own: a branch and a checkout apart from yours, chosen \
                 from the folder menu in its top bar. They are cleaned up by themselves.";
    let mut y = Settings::title(p, (x, y, w), "Worktrees", about);

    p.label("Cleaning up", theme::LABEL, x, y, t.text);
    y += 30.0;
    let (_, label, detail) = idle_choice(shell.config.worktree_idle.as_deref());
    let rect = Rect::new(x, y, w, 58.0);
    row(p, rect, (icon::HISTORY, None), "Unused worktrees go", detail, 216.0);
    let anchor = Rect::new(rect.right() - 212.0, rect.y + 9.0, 200.0, 40.0);
    if dropdown(p, ui, anchor, None, label, shell.menu_open(Menu::WorktreeIdle)) {
        shell.toggle_menu(Menu::WorktreeIdle, anchor);
    }
    y += 66.0;
    let rules = "A worktree goes at once when its work is merged into the branch it came from (squashed too) or its \
                 session is deleted, and is made again if the session is used after that. One with uncommitted \
                 changes is never cleaned up by itself. A branch stays for as long as it holds commits that are \
                 not merged.";
    y = Settings::note(p, (x, y + 2.0, w), rules) + 30.0;

    // What is on disk: the sessions' worktrees, then folders left without a session.
    let listed: Vec<(String, String, String, String)> = shell
        .sessions
        .iter()
        .filter_map(|session| {
            let worktree = session.worktree.as_ref()?;
            let state = shell.worktrees.get(&session.id).filter(|state| state.exists)?;
            let title = if session.title.is_empty() { "New session".to_owned() } else { session.title.clone() };
            let detail = format!("{} · {} · {}", worktree.repo_name(), worktree.branch, standing(state));
            Some((session.id.clone(), worktree.path.clone(), title, detail))
        })
        .collect();
    let total: u64 = listed
        .iter()
        .map(|(_, path, ..)| path)
        .chain(shell.orphans.iter().map(|orphan| &orphan.path))
        .filter_map(|path| shell.sizes.get(path))
        .sum();
    p.label("On this computer", theme::LABEL, x, y + 6.0, t.text);
    let clean = Rect::new(x + w - 118.0, y, 118.0, 30.0);
    if button(p, ui, clean, "Clean up now", ButtonStyle::Secondary, !shell.sweeping) {
        shell.sweep_worktrees(actions);
    }
    if total > 0 {
        let total = p.layout(&bytes(total), theme::SMALL, None);
        p.text(&total, clean.x - 12.0 - total.width(), y + 7.0, t.text_muted);
    }
    y += 40.0;
    if listed.is_empty() && shell.orphans.is_empty() {
        let none = if shell.sweeping { "Looking…" } else { "No worktrees on disk." };
        p.label(none, theme::SMALL, x, y, t.text_faint);
        return (y + 24.0, None);
    }
    let mut open = None;
    for (session, path, title, detail) in &listed {
        let rect = Rect::new(x, y, w, 58.0);
        row(p, rect, (icon::BRANCH, None), title, detail, 130.0);
        if let Some(size) = shell.sizes.get(path) {
            let size = p.layout(&bytes(*size), theme::SMALL, None);
            p.text(&size, rect.right() - 50.0 - size.width(), rect.y + 20.0, t.text_muted);
        }
        // To its session, where its work is merged or the session deleted.
        if icon_button(p, ui, Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0), icon::CHEVRON_RIGHT, true) {
            open = Some(session.clone());
        }
        y += 66.0;
    }
    let mut remove = None;
    for orphan in &shell.orphans {
        let rect = Rect::new(x, y, w, 58.0);
        let confirming = armed.as_deref() == Some(orphan.path.as_str());
        let what = match (orphan.repo.is_empty(), orphan.changes) {
            (true, _) => "its repository is gone".to_owned(),
            (false, changes) => format!("{} · {}", folder_name(&orphan.repo), counted(changes, "uncommitted change")),
        };
        let detail = if confirming {
            "Click again to delete it, with what is in it".to_owned()
        } else {
            format!("No session · {what}")
        };
        row(p, rect, (icon::FOLDER, None), folder_name(&orphan.path), &detail, 130.0);
        if let Some(size) = shell.sizes.get(&orphan.path) {
            let size = p.layout(&bytes(*size), theme::SMALL, None);
            p.text(&size, rect.right() - 50.0 - size.width(), rect.y + 20.0, t.text_muted);
        }
        let bin = Rect::new(rect.right() - 40.0, rect.y + 15.0, 28.0, 28.0);
        if icon_button(p, ui, bin, icon::TRASH, true) {
            remove = Some(orphan.path.clone());
        } else if confirming && ui.released && !bin.contains(ui.mouse) {
            // A click anywhere else disarms it.
            *armed = None;
        }
        if confirming {
            icon::draw(p, icon::TRASH, (bin.x + 14.0, bin.y + 14.0), 15.0, t.danger);
        }
        y += 66.0;
    }
    if let Some(path) = remove {
        if armed.as_deref() == Some(path.as_str()) {
            *armed = None;
            actions.push(Action::Worktree(Job::RemoveOrphan(path)));
        } else {
            *armed = Some(path);
        }
    }
    (y - 8.0, open)
}

#[cfg(test)]
mod tests {
    use atroxabot_core::worktree::{Orphan, Swept};
    use atroxabot_core::{Agent, AgentEvent, Config, Session};

    use super::*;

    fn made(session: &str) -> Worktree {
        Worktree {
            repo: "/code/parser".into(),
            path: format!("/data/worktrees/parser-{session}"),
            branch: "atroxabot/fix-it-1a2b".into(),
            base: "main".into(),
            start: "abc".into(),
        }
    }

    fn jobs(actions: &[Action]) -> Vec<&Job> {
        actions.iter().filter_map(|a| if let Action::Worktree(job) = a { Some(job) } else { None }).collect()
    }

    #[test]
    fn sizes_and_standings_read_well() {
        assert_eq!(
            [bytes(1), bytes(900 * 1024), bytes(312 << 20), bytes(9 << 29)],
            ["1 KB", "900 KB", "312 MB", "4.5 GB"]
        );
        let state = |exists, changes, unmerged, landed| State { exists, changes, unmerged, landed, ..State::default() };
        let said = [
            state(true, 0, 0, false),
            state(true, 0, 0, true),
            state(true, 0, 2, false),
            state(true, 1, 0, false),
            state(true, 3, 1, false),
            state(false, 0, 0, false),
            state(false, 0, 2, false),
        ]
        .map(|state| standing(&state));
        assert_eq!(
            said,
            [
                "nothing to merge",
                "merged",
                "2 commits to merge",
                "1 uncommitted change",
                "3 uncommitted changes · 1 commit to merge",
                "cleaned up",
                "cleaned up · 2 commits kept on its branch",
            ]
        );
        assert_eq!(idle_choice(None).1, "After 3 days");
        assert_eq!((idle_choice(Some("never")).1, idle_choice(Some("soon")).1), ("Never", "After 3 days"));
    }

    #[test]
    fn a_session_asks_for_a_worktree_and_keeps_the_one_it_gets() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let mut info = atroxabot_core::ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        shell.claude_info(atroxabot_core::Connection::CLAUDE_AGENT, Ok(info));
        shell.set_folder("/code/parser", &mut actions);
        assert!(!shell.wants_worktree() && shell.worktree_open());
        shell.toggle_worktree(&mut actions);
        assert!(shell.wants_worktree() && shell.selected_agent().unwrap().worktree, "its next sessions too");
        actions.clear();
        shell.send("Fix it", false, Vec::new(), &mut actions);
        let session = shell.current.clone().expect("a session");
        let asked = actions.iter().find_map(|a| match a {
            Action::Send { session, .. } => Some(session.worktree.clone()),
            _ => None,
        });
        assert_eq!(asked, Some(Some(Worktree::default())), "the message goes with the wish for one");
        assert!(shell.sessions[0].worktree.is_none(), "nothing to clean up before it is made");

        // The app made it: the session works in it from now on, and shows the folder it stands for.
        let worktree = made(&session);
        actions.clear();
        shell.worktree_ready(&session, &worktree, &format!("{}/src", worktree.path), &mut actions);
        let thread = &shell.threads[&session];
        assert_eq!(thread.session.worktree.as_ref(), Some(&worktree));
        assert!(thread.session.workdir.ends_with("/src") && shell.origin_in_use().ends_with("src"));
        assert_eq!(shell.sessions[0].worktree.as_ref(), Some(&worktree));
        assert_eq!(shell.config.worktree_repos, ["/code/parser"]);
        assert!(actions.iter().any(|a| matches!(a, Action::SaveSession(saved) if saved.worktree.is_some())));
        assert!(!shell.worktree_open(), "made: it is the session's for good");
        shell.toggle_worktree(&mut actions);
        assert!(shell.wants_worktree());
        // It cannot move to another folder; the notice says why.
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.set_folder("/elsewhere", &mut actions);
        assert!(shell.threads[&session].session.workdir.ends_with("/src") && !shell.notices.is_empty());
        assert_eq!(shell.config.recent_folders, ["/code/parser"], "the folder it stands for, never the worktree's");

        // The next session starts in the same folder of the repository, in a worktree again.
        shell.new_session();
        assert!(shell.pending_folder.ends_with("src") && shell.pending_folder.starts_with("/code/parser"));
        assert!(shell.wants_worktree() && shell.worktree_open());
        shell.toggle_worktree(&mut actions);
        assert!(!shell.wants_worktree() && !shell.selected_agent().unwrap().worktree);
    }

    #[test]
    fn worktrees_are_swept_and_go_with_their_session() {
        let agent = Agent::new("Scout");
        let mut session = Session::new(&agent.id);
        session.worktree = Some(made("s1"));
        session.workdir = made("s1").path;
        let id = session.id.clone();
        let mut shell = Shell::new(Config::default(), vec![agent], vec![session.summary()]);
        let mut actions = Vec::new();
        shell.start(&mut actions);
        // At the start every worktree is looked at; one look at a time.
        let swept = jobs(&actions);
        assert!(matches!(&swept[..], [Job::Sweep { known, idle: Some(259_200), .. }] if known.len() == 1));
        actions.clear();
        shell.sweep_worktrees(&mut actions);
        assert!(jobs(&actions).is_empty(), "one is under way");

        let dirty = State { exists: true, changes: 2, ..State::default() };
        let orphan =
            Orphan { path: "/data/worktrees/old".into(), repo: "/code/old".into(), changes: 1, ..Orphan::default() };
        let report = Swept {
            states: vec![(id.clone(), dirty)],
            orphans: vec![orphan],
            repos: vec!["/code/old".into()],
            ..Swept::default()
        };
        shell.worktree_report(Report::Swept(report), &mut actions);
        assert_eq!(shell.config.worktree_repos, ["/code/old", "/code/parser"], "its sessions' repositories stay known");
        assert_eq!(shell.orphans.len(), 1);
        shell.sweep_worktrees_again(&mut actions);
        assert!(jobs(&actions).is_empty(), "not again within the minute");

        // Deleting the session: the button says what goes with it, and then it does.
        assert_eq!(shell.delete_warning(&id), "Delete with 2 uncommitted changes");
        assert_eq!(shell.delete_warning("other"), "Click again to delete");
        actions.clear();
        shell.delete_session(&id, true, &mut actions);
        assert!(matches!(&jobs(&actions)[..], [Job::Remove { session, discard: true, .. }] if *session == id));
        // What could not go is said, and looked for again.
        actions.clear();
        let kept = Report::Removed { session: id, worktree: made("s1"), problem: Some("It is in use.".into()) };
        shell.worktree_report(kept, &mut actions);
        assert!(shell.notices.last().is_some_and(|notice| notice.text.contains("was kept: It is in use.")));
        shell
            .worktree_report(Report::OrphanRemoved { path: "/data/worktrees/old".into(), problem: None }, &mut actions);
        assert!(shell.orphans.is_empty());
    }
}
