//! What happens when an account's usage limit stops a turn (Settings,
//! Providers, "When an account runs out"): the session waits for the limit
//! to lift and goes on by itself, goes on at once with another account of
//! the same kind that has usage left, or stops there.
//!
//! A session goes on only while the user leaves it be. Whatever they do with
//! it first (send something, switch its provider, clear it, delete
//! it, or cancel the wait in the tray above the message box) is what happens
//! instead. A session that keeps running into the limit stops trying after
//! a few turns. The wait lives in memory: it ends with the app.

use atroxabot_core::{Agent, Connection, ConnectionKind, unix_now};

use super::{Account, Shell, until};
use crate::app::Action;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{Ui, icon, icon_button, id};

/// How long a session waits when the provider did not say when the limit lifts, in seconds.
const RETRY: u64 = 15 * 60;
/// How long after the limit lifts the session goes on: clocks differ a little.
const GRACE: u64 = 60;
/// Most turns in a row the limit may stop before the session stops trying.
const TRIES: u32 = 5;

/// What the agent is told when it goes on after the limit stopped it.
pub(super) const GO_ON: &str = "(A usage limit stopped you part way through. It has lifted now: carry on with \
    what you were doing, from where you left off.)";

/// What happens when an account's usage limit stops a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutOfLimits {
    /// The session goes on by itself once the limit lifts.
    Wait,
    /// It goes on at once with another account of the same kind that has usage left.
    Switch,
    /// It stops until the user sends something.
    Stop,
}

impl OutOfLimits {
    /// Every choice, the default first.
    pub const ALL: [Self; 3] = [Self::Wait, Self::Switch, Self::Stop];

    /// What the config calls it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Switch => "switch",
            Self::Stop => "stop",
        }
    }

    /// The one saved as `key`; waiting if none is.
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|choice| Some(choice.key()) == key).unwrap_or(Self::Wait)
    }

    /// Its name in the settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Wait => "Wait for limit refill",
            Self::Switch => "Continue using different account",
            Self::Stop => "Stop",
        }
    }

    /// What it does, in a line.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Wait => "The session goes on by itself once the limit lifts.",
            Self::Switch => "It goes on at once with an account that has usage left.",
            Self::Stop => "It stops until you send something.",
        }
    }
}

/// A session the usage limit stopped, waiting to go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resume {
    /// When it goes on, in Unix seconds.
    pub at: u64,
    /// No account of its kind had usage left to go on with at once.
    pub all_out: bool,
}

/// What the tray's row of a waiting session does when clicked.
enum Pick {
    Now,
    Cancel,
}

impl Shell {
    /// What the settings say happens when a usage limit stops a turn.
    pub(super) fn out_of_limits(&self) -> OutOfLimits {
        OutOfLimits::from_key(self.config.out_of_limits.as_deref())
    }

    /// Whether the providers `a` and `b` offer the same models: they are
    /// one, or two accounts (or Anthropic keys) of the same kind.
    pub(super) fn kin(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let kind = |id: &str| self.config.connection(id).map(|connection| connection.kind);
        matches!((kind(a), kind(b)), (Some(a), Some(b)) if a == b && a != ConnectionKind::OpenAi)
    }

    /// Gives `agent` the provider, model and effort `session` keeps, while
    /// its provider exists: a session thinks through its own.
    pub(super) fn apply_choice(&self, session: &atroxabot_core::Session, agent: &mut Agent) {
        session.apply_choice(agent, self.config.connection(&session.provider).is_some());
    }

    /// The agent of `session` as the session thinks with it.
    pub(super) fn agent_of(&self, session: &str) -> Option<Agent> {
        let thread = self.threads.get(session)?;
        let mut agent = self.agents.iter().find(|agent| agent.id == thread.session.agent)?.clone();
        self.apply_choice(&thread.session, &mut agent);
        Some(agent)
    }

    /// Whether the provider `connection` offers `model`, as far as is known:
    /// the default always, and any while its list is not in.
    fn offers(&self, connection: &str, model: &str) -> bool {
        let Some(connection) = self.config.connection(connection) else { return false };
        let choices = self.model_choices(&connection);
        model.is_empty() || choices.is_empty() || choices.iter().any(|choice| choice.value == model)
    }

    /// `agent` on the provider `to`. Its model goes along where `to`
    /// offers it (from one Claude account to another), and its effort goes
    /// along always: also one the user never picked, which was its model's
    /// usual one, and also when `to` gives it another model. A model that
    /// does not take that effort thinks at its own ([`Shell::effort_in_use`]).
    pub(super) fn moved(&self, mut agent: Agent, to: &str) -> Agent {
        if agent.effort.is_empty()
            && let Some(from) = self.config.connection(&agent.connection)
            && !self.model_choices(&from).is_empty()
        {
            let model = self.model_in_use(&from, &agent.model);
            agent.effort = Self::effort_in_use(from.kind, &model, "");
        }
        let from = std::mem::replace(&mut agent.connection, to.to_owned());
        if !self.kin(&from, to) || !self.offers(to, &agent.model) {
            // Another provider has other models: its default, until one is picked.
            agent.model.clear();
        }
        agent
    }

    /// The provider menu of the top bar: the open session thinks through
    /// `to` from now on, and so do the sessions its agent starts next. The
    /// agent's other sessions stay where each was left.
    pub(super) fn swap_provider(&mut self, to: &str, actions: &mut Vec<Action>) {
        let Some(agent) = self.agent_in_use().filter(|agent| agent.connection != to) else { return };
        if self.loading() {
            return;
        }
        // The user chose where it goes on: it waits for no limit to lift any more.
        if let Some(session) = self.current.clone() {
            self.cancel_resume(&session);
        }
        let agent = self.moved(agent, to);
        if let Some(connection) = self.config.connection(to) {
            self.want_models(&connection, actions);
        }
        self.choose_thinking(agent, actions);
    }

    /// Moves `session` to the account `to` by itself, the one it ran on
    /// being out of usage. The sessions its agent starts next go there too,
    /// if they would have started on the account that ran out.
    fn take_over(&mut self, session: &str, to: &str, actions: &mut Vec<Action>) {
        let Some(agent) = self.agent_of(session) else { return };
        let from = agent.connection.clone();
        let moved = self.moved(agent, to);
        let Some(thread) = self.threads.get_mut(session) else { return };
        thread.session.keep_choice(&moved);
        actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        let own = self.agents.iter().find(|own| own.id == moved.id && own.connection == from).cloned();
        if let Some(own) = own {
            let own = self.moved(own, to);
            self.update_agent(own, actions);
        }
    }

    /// Until when the account `account` is out of usage, as far as is known:
    /// `None` while it has some, 0 when it ran out lately without saying until when.
    pub(super) fn out_until(&self, account: &str) -> Option<u64> {
        let now = unix_now();
        let limits = self.limits.get(account).filter(|limits| limits.status == "rejected")?;
        let full = limits.windows.iter().filter(|w| w.used >= 1.0 && w.resets > now).map(|w| w.resets).max();
        match (limits.resets, full) {
            (resets, full) if resets > now => Some(resets.max(full.unwrap_or(0))),
            // Refused without a time: for a while it counts as out.
            (0, None) => (now < limits.at + RETRY).then_some(0),
            // Lifted, unless a window is still full.
            (_, full) => full,
        }
    }

    /// Whether the account `connection` is known to be signed in.
    pub(super) fn signed_in(&self, connection: &Connection) -> bool {
        match connection.kind {
            ConnectionKind::ClaudeAgent => {
                matches!(self.claude.get(&connection.id), Some(Account::Ready(info)) if !info.account.email.is_empty())
            }
            ConnectionKind::Codex => self.codex_signed_in(&connection.id),
            ConnectionKind::ClaudeApi | ConnectionKind::OpenAi => false,
        }
    }

    /// The signed-in accounts of the same kind as `agent`'s.
    fn kin_accounts(&self, agent: &Agent) -> Vec<Connection> {
        let Some(kind) = self.config.connection(&agent.connection).map(|c| c.kind).filter(|kind| kind.is_account())
        else {
            return Vec::new();
        };
        let all = self.config.all_connections().into_iter();
        all.filter(|connection| connection.kind == kind && self.signed_in(connection)).collect()
    }

    /// The account `agent` can go on with now: its own while it has usage
    /// left, else another of the same kind that has, the least used first.
    fn fresh_account(&self, agent: &Agent) -> Option<String> {
        let now = unix_now();
        // How full its fullest window is that has not started over yet.
        let used = |id: &str| {
            let windows = self.limits.get(id).map(|limits| limits.windows.as_slice()).unwrap_or_default();
            windows.iter().filter(|w| w.resets == 0 || w.resets > now).map(|w| w.used).fold(0.0, f64::max)
        };
        let mut fresh: Vec<Connection> =
            self.kin_accounts(agent).into_iter().filter(|account| self.out_until(&account.id).is_none()).collect();
        if fresh.iter().any(|account| account.id == agent.connection) {
            return Some(agent.connection.clone());
        }
        fresh.sort_by(|a, b| used(&a.id).total_cmp(&used(&b.id)));
        fresh.into_iter().next().map(|account| account.id)
    }

    /// The usage limit of the account it ran on stopped the turn of
    /// `session`, which has ended: what the settings say happens next.
    pub(super) fn limit_stopped(&mut self, session: &str, actions: &mut Vec<Action>) {
        let now = unix_now();
        let mode = self.out_of_limits();
        let Some(thread) = self.threads.get_mut(session) else { return };
        thread.tries += 1;
        let (tries, account) = (thread.tries, thread.session.account.clone());
        // The account counts as out from now, even where its provider said nothing (more) of it.
        if self.out_until(&account).is_none() {
            let limits = self.limits.entry(account.clone()).or_default();
            (limits.status, limits.resets, limits.at) = ("rejected".to_owned(), 0, now);
        }
        if mode == OutOfLimits::Stop || account.is_empty() {
            return;
        }
        if tries > TRIES {
            let title = self.threads.get(session).map(|thread| thread.session.title.clone()).unwrap_or_default();
            self.notify(format!("\"{title}\" kept running into usage limits, so it no longer goes on by itself."));
            return;
        }
        let Some(agent) = self.agent_of(session) else { return };
        // It was moved to another provider while the turn ran: the user chose where it goes on.
        if mode == OutOfLimits::Wait && agent.connection != account {
            return;
        }
        if mode == OutOfLimits::Switch {
            if self.fresh_account(&agent).is_some() {
                self.go_on(session, actions);
                return;
            }
            // None has usage left: it goes on with the first one whose limit lifts.
            let lifts = self.kin_accounts(&agent).iter().filter_map(|account| self.out_until(&account.id)).collect();
            self.wait(session, lifts, true);
            return;
        }
        let lifts = self.out_until(&account).into_iter().collect();
        self.wait(session, lifts, false);
    }

    /// Has `session` go on at the earliest of `lifts` (Unix seconds; 0 for
    /// a limit that did not say), a little after it.
    fn wait(&mut self, session: &str, lifts: Vec<u64>, all_out: bool) {
        let now = unix_now();
        let at = lifts.into_iter().map(|lifts| if lifts > now { lifts + GRACE } else { now + RETRY }).min();
        if let Some(thread) = self.threads.get_mut(session) {
            thread.resume = Some(Resume { at: at.unwrap_or(now + RETRY), all_out });
        }
    }

    /// The limit that stopped `session` is behind it, or another account
    /// takes over: it goes on, with what the user sent meanwhile if anything.
    /// With "Continue using different account" it moves to an account that
    /// has usage left. Returns whether it went.
    pub(super) fn go_on(&mut self, session: &str, actions: &mut Vec<Action>) -> bool {
        let Some(thread) = self.threads.get_mut(session).filter(|thread| thread.loaded && !thread.busy) else {
            return false;
        };
        thread.resume = None;
        let Some(agent) = self.agent_of(session) else { return false };
        if self.out_of_limits() == OutOfLimits::Switch
            && let Some(fresh) = self.fresh_account(&agent)
            && fresh != agent.connection
        {
            self.take_over(session, &fresh, actions);
            // The other sessions waiting for the account it left go on with another too.
            let mut waiting: Vec<String> = (self.threads.iter())
                .filter(|(id, thread)| id.as_str() != session && thread.resume.is_some())
                .filter(|(id, _)| self.agent_of(id).is_some_and(|other| other.connection == agent.connection))
                .map(|(id, _)| id.clone())
                .collect();
            waiting.sort();
            for other in waiting {
                self.go_on(&other, actions);
            }
        }
        // What the user sent meanwhile goes with it; the agent is told why it goes on either way.
        self.going_on = true;
        let went = if self.threads.get(session).is_some_and(|thread| !thread.follow_ups.is_empty()) {
            self.send_follow_ups(session, false, actions);
            self.threads.get(session).is_some_and(|thread| thread.busy)
        } else {
            self.send_to(session, "", &[], (false, false), actions)
        };
        self.going_on = false;
        went
    }

    /// The user takes `session` back: it no longer goes on by itself.
    pub(super) fn cancel_resume(&mut self, session: &str) {
        if let Some(thread) = self.threads.get_mut(session) {
            thread.resume = None;
            thread.tries = 0;
        }
    }

    /// Does what is due: sessions whose limit lifted go on, and processes
    /// left long enough are closed (`warm.rs`). Call it on every frame and timer.
    pub fn tick(&mut self, actions: &mut Vec<Action>) {
        let now = unix_now();
        self.rest(now, actions);
        let mut due: Vec<String> = (self.threads.iter())
            .filter(|(_, thread)| thread.resume.as_ref().is_some_and(|resume| resume.at <= now))
            .map(|(id, _)| id.clone())
            .collect();
        due.sort();
        for session in due {
            self.resume_due(&session, actions);
        }
    }

    /// The time `session` waited for has come: it goes on, unless what is
    /// known now says its account is still out for longer.
    fn resume_due(&mut self, session: &str, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session) else { return };
        let Some(resume) = thread.resume.take() else { return };
        // The user started something there: it is theirs.
        if thread.busy {
            return;
        }
        let Some(agent) = self.agent_of(session) else { return };
        let switching = self.out_of_limits() == OutOfLimits::Switch;
        if switching && self.fresh_account(&agent).is_some() {
            self.go_on(session, actions);
            return;
        }
        match self.out_until(&agent.connection) {
            Some(later) if later > unix_now() => {
                let lifts = if switching {
                    self.kin_accounts(&agent).iter().filter_map(|account| self.out_until(&account.id)).collect()
                } else {
                    vec![later]
                };
                self.wait(session, lifts, resume.all_out);
            }
            _ => {
                self.go_on(session, actions);
            }
        }
    }

    /// When something is due next, in Unix seconds: a session going on,
    /// a process left long enough, or the open session's cache running
    /// out. The app wakes for it.
    pub fn next_wake(&self) -> Option<u64> {
        let resumes = self.threads.values().filter_map(|thread| thread.resume.as_ref().map(|resume| resume.at));
        resumes.chain(self.next_rest()).chain(self.cache_turns()).min()
    }

    /// The open session's row in the tray above the message box while it
    /// waits for a limit to lift, ending at `bottom` in the column at `x`,
    /// `width` wide. Returns where it begins.
    pub(super) fn draw_resume(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, bottom, width): (f32, f32, f32),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let Some(session) = self.current.clone() else { return bottom };
        let Some(resume) = self.threads.get(&session).and_then(|thread| thread.resume.clone()) else { return bottom };
        let tray = Rect::new(x + 12.0, bottom - 42.0, width - 24.0, 42.0);
        p.shadow(
            Rect::new(tray.x + 6.0, tray.y + 6.0, tray.w - 12.0, tray.h),
            fade(t.shadow, 0.45),
            theme::RADIUS,
            16.0,
        );
        p.bordered(tray, t.surface, theme::RADIUS, 1.0, t.border);
        let mid = tray.y + tray.h * 0.5;
        icon::draw(p, icon::HISTORY, (tray.x + 18.0, mid), 13.0, t.text_faint);
        let mut pick = None;
        let close = Rect::new(tray.right() - 34.0, mid - 13.0, 26.0, 26.0);
        if icon_button(p, ui, close, icon::X, true) {
            pick = Some(Pick::Cancel);
        }
        let now_label = p.layout("Continue now", theme::TINY, None);
        let button = Rect::new(close.x - now_label.width() - 16.0, mid - 12.0, now_label.width() + 12.0, 24.0);
        let hovered = ui.hovered(button);
        let hover = ui.anim(id(("resume-now", &session)), f32::from(u8::from(hovered)));
        p.rect(button, fade(t.hover, hover), theme::RADIUS_SM);
        p.text(&now_label, button.x + 6.0, mid - now_label.height() * 0.5, mix(t.text_faint, t.text, hover));
        if hovered {
            ui.cursor = winit::window::CursorIcon::Pointer;
            if ui.clicked(button) {
                pick = Some(Pick::Now);
            }
        }
        // When the limit lifts; it goes on a moment after.
        let when = until(unix_now(), resume.at.saturating_sub(GRACE));
        let said = if resume.all_out {
            format!("No account has usage left · goes on {when}")
        } else {
            format!("Goes on when the limit lifts · {when}")
        };
        let mut words = p.layout(&said, theme::SMALL, None);
        words.truncate(p.fonts, button.x - tray.x - 44.0);
        p.text(&words, tray.x + 34.0, mid - words.height() * 0.5, t.text_muted);
        match pick {
            Some(Pick::Now) => {
                self.go_on(&session, actions);
            }
            Some(Pick::Cancel) => self.cancel_resume(&session),
            None => {}
        }
        tray.y - 8.0
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{AgentEvent, ClaudeInfo, Config, Entry, LimitWindow, Limits};

    use super::*;
    use crate::shell::menu::Menu;

    #[test]
    fn the_choice_reads_back_and_waits_by_default() {
        for choice in OutOfLimits::ALL {
            assert_eq!(OutOfLimits::from_key(Some(choice.key())), choice);
        }
        assert_eq!(OutOfLimits::from_key(None), OutOfLimits::Wait);
        assert_eq!(OutOfLimits::from_key(Some("later")), OutOfLimits::Wait);
    }

    /// Two Claude accounts, both signed in, and a session at work on the
    /// first (with Opus at high effort) when the settings say `out`.
    fn two_accounts(out: OutOfLimits) -> (Shell, String, Vec<Action>) {
        let mut config = Config::default();
        config.connections.push(Connection {
            id: "work".into(),
            name: "Work".into(),
            config_dir: "/accounts/work".into(),
            ..Connection::claude_agent()
        });
        config.out_of_limits = Some(out.key().to_owned());
        let mut agent = Agent::new("Scout");
        (agent.model, agent.effort) = ("opus".into(), "high".into());
        let mut shell = Shell::new(config, vec![agent], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        for (id, email) in [(Connection::CLAUDE_AGENT, "me@example.com"), ("work", "me@work.example")] {
            let mut info = ClaudeInfo::default();
            info.account.email = email.into();
            shell.claude_info(id, Ok(info));
        }
        shell.send("Fix the bug", false, Vec::new(), &mut actions);
        let session = shell.current.clone().expect("a session");
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), &mut actions);
        actions.clear();
        (shell, session, actions)
    }

    /// The account the turn runs on is refused until `resets`, and Claude Code ends the turn on it.
    fn run_out(shell: &mut Shell, session: &str, resets: u64, actions: &mut Vec<Action>) {
        let window = LimitWindow { name: "five_hour".into(), used: 1.0, resets, label: String::new() };
        let limits = Limits { at: unix_now(), status: "rejected".into(), resets, windows: vec![window] };
        shell.agent_event(session, AgentEvent::Limits(limits), actions);
        shell.agent_event(session, AgentEvent::ReplyStarted, actions);
        shell.agent_event(session, AgentEvent::Text("You've hit your limit · resets 7pm".into()), actions);
        shell.agent_event(session, AgentEvent::LimitReached, actions);
        let problem = Some("You've hit your limit · resets 7pm".to_owned());
        shell.agent_event(session, AgentEvent::TurnEnded { problem }, actions);
    }

    /// The texts of the messages sent.
    fn sent(actions: &[Action]) -> Vec<String> {
        let text = |action: &Action| match action {
            Action::Send { text, .. } => Some(text.clone()),
            _ => None,
        };
        actions.iter().filter_map(text).collect()
    }

    /// Time passes until the limit of `account` has lifted and the session's wait is over.
    fn lift(shell: &mut Shell, session: &str, account: &str) {
        let past = unix_now() - 10;
        let limits = shell.limits.get_mut(account).expect("limits");
        limits.resets = past;
        for window in &mut limits.windows {
            window.resets = past;
        }
        shell.threads.get_mut(session).and_then(|thread| thread.resume.as_mut()).expect("waiting").at = past;
    }

    #[test]
    fn a_session_waits_for_the_limit_and_goes_on_by_itself() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        let resets = unix_now() + 3_600;
        run_out(&mut shell, &session, resets, &mut actions);
        assert!(sent(&actions).is_empty(), "nothing goes while the account is out");
        let resume = shell.threads[&session].resume.clone().expect("it waits");
        assert_eq!((resume.at, resume.all_out), (resets + GRACE, false));
        // The conversation and the tray say it: no card is put over the session for it.
        assert!(shell.notices.is_empty());
        assert_eq!(shell.next_wake(), Some(resets + GRACE), "the app wakes when it goes on");
        shell.tick(&mut actions);
        assert!(sent(&actions).is_empty(), "not yet");

        lift(&mut shell, &session, Connection::CLAUDE_AGENT);
        let before = shell.threads[&session].session.entries.len();
        shell.tick(&mut actions);
        let thread = &shell.threads[&session];
        assert_eq!(sent(&actions), [GO_ON], "the same account is only told to go on");
        assert!(thread.busy && thread.resume.is_none());
        // A line says so; nothing is put in the user's mouth.
        assert!(matches!(&thread.session.entries[before], Entry::Note { text } if text.contains("lifted")));
        assert!(!thread.session.entries.iter().skip(before).any(|entry| matches!(entry, Entry::User { .. })));
        assert_eq!(shell.next_wake(), None);
    }

    #[test]
    fn what_the_user_does_first_is_what_happens() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        run_out(&mut shell, &session, unix_now() + 3_600, &mut actions);
        shell.send("Never mind, look at the tests", false, Vec::new(), &mut actions);
        assert_eq!(sent(&actions), ["Never mind, look at the tests"]);
        assert!(shell.threads[&session].resume.is_none());

        // Switching the agent's provider by hand takes the session back too.
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        run_out(&mut shell, &session, unix_now() + 3_600, &mut actions);
        assert!(shell.threads[&session].resume.is_some());
        shell.choose(Menu::Provider, "work", &mut actions);
        assert!(shell.threads[&session].resume.is_none(), "the user chose where it goes on");
        assert!(shell.notices.is_empty(), "no card says what the conversation says");
        // Its model and effort stay: the other account has the same ones.
        let agent = shell.agent_in_use().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "opus", "high"));
    }

    /// The models Claude Code lists for an account, by the name each is chosen by.
    fn lists(shell: &mut Shell, account: &str, models: &[&str]) {
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        info.models = models
            .iter()
            .map(|value| atroxabot_core::ClaudeModel {
                value: (*value).to_owned(),
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                ..atroxabot_core::ClaudeModel::default()
            })
            .collect();
        shell.claude_info(account, Ok(info));
    }

    #[test]
    fn the_effort_goes_along_to_another_account() {
        // The other account does not list the session's model (a smaller plan): it gets that account's
        // own, at the effort the user picked.
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Switch);
        lists(&mut shell, Connection::CLAUDE_AGENT, &["default", "opus"]);
        lists(&mut shell, "work", &["default", "haiku"]);
        run_out(&mut shell, &session, unix_now() + 3_600, &mut actions);
        let agent = shell.agent_in_use().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "", "high"));
        // By hand it is the same, back to where its model is listed.
        shell.choose(Menu::Model, "haiku", &mut actions);
        shell.choose(Menu::Provider, Connection::CLAUDE_AGENT, &mut actions);
        let agent = shell.agent_in_use().expect("agent");
        assert_eq!((agent.model.as_str(), agent.effort.as_str()), ("", "high"));

        // An effort never picked is its model's usual one (Sonnet's is high): it stays what it was, also
        // on a model whose usual one is another.
        let (mut shell, _, mut actions) = two_accounts(OutOfLimits::Wait);
        lists(&mut shell, Connection::CLAUDE_AGENT, &["sonnet", "opus"]);
        lists(&mut shell, "work", &["opus"]);
        let mut scout = shell.agent_in_use().expect("agent");
        (scout.model, scout.effort) = ("sonnet".into(), String::new());
        shell.choose_thinking(scout, &mut actions);
        shell.choose(Menu::Provider, "work", &mut actions);
        let agent = shell.agent_in_use().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "", "high"));
        // Before an account's models are known nothing is guessed: the effort stays unpicked.
        let (shell, _, _) = two_accounts(OutOfLimits::Wait);
        let unpicked = Agent { effort: String::new(), ..shell.agent_in_use().expect("agent") };
        assert_eq!(shell.moved(unpicked, "work").effort, "");
    }

    #[test]
    fn what_was_sent_meanwhile_waits_and_goes_with_it() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        shell.send("Also update the changelog", false, Vec::new(), &mut actions);
        run_out(&mut shell, &session, unix_now() + 600, &mut actions);
        assert!(sent(&actions).is_empty(), "it would only be refused");
        assert_eq!(shell.threads[&session].follow_ups.len(), 1, "it waits in the tray");
        lift(&mut shell, &session, Connection::CLAUDE_AGENT);
        shell.tick(&mut actions);
        let texts = sent(&actions);
        assert!(matches!(&texts[..], [text] if text.starts_with(GO_ON) && text.ends_with("Also update the changelog")));
        assert!(shell.threads[&session].follow_ups.is_empty());
    }

    #[test]
    fn another_account_takes_over_with_the_same_model() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Switch);
        run_out(&mut shell, &session, unix_now() + 3_600, &mut actions);
        let agent = shell.agent_in_use().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "opus", "high"));
        // The new account gets the gist of the conversation in a process of its own, and is told to go on.
        let closed = actions.iter().position(|a| matches!(a, Action::CloseLive(closed) if *closed == session));
        let send = actions.iter().position(|a| matches!(a, Action::Send { session, .. } if session.account == "work"));
        assert!(matches!((closed, send), (Some(closed), Some(send)) if closed < send));
        assert!(matches!(&sent(&actions)[..], [text] if text.contains("User: Fix the bug") && text.ends_with(GO_ON)));
        let thread = &shell.threads[&session];
        assert!(thread.session.entries.iter().any(
            |entry| matches!(entry, Entry::Note { text } if text == "Claude is out of usage · going on with Work")
        ));
        // The agent's next sessions start where there is usage too.
        assert_eq!(shell.selected_agent().map(|agent| agent.connection.as_str()), Some("work"));
    }

    #[test]
    fn only_the_session_that_ran_out_moves() {
        let (mut shell, first, mut actions) = two_accounts(OutOfLimits::Switch);
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);
        // A second session of the agent, on the same account, is the one that runs out.
        shell.new_session();
        shell.send("Something else", false, Vec::new(), &mut actions);
        let second = shell.current.clone().expect("a session");
        shell.agent_event(&second, AgentEvent::Remote("r2".into()), &mut actions);
        run_out(&mut shell, &second, unix_now() + 3_600, &mut actions);
        assert_eq!(shell.agent_of(&second).map(|agent| agent.connection).as_deref(), Some("work"));
        // The first was not stopped by anything: it is where it was left, with its conversation.
        let kept = shell.agent_of(&first).expect("agent");
        assert_eq!((kept.connection.as_str(), kept.model.as_str()), (Connection::CLAUDE_AGENT, "opus"));
        let thread = &shell.threads[&first];
        assert_eq!((thread.session.account.as_str(), thread.session.remote.as_str()), (Connection::CLAUDE_AGENT, "r1"));
    }

    #[test]
    fn with_every_account_out_it_waits_for_the_first_to_lift() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Switch);
        let soon = unix_now() + 900;
        let window = LimitWindow { name: "five_hour".into(), used: 1.0, resets: soon, label: String::new() };
        let work = Limits { at: unix_now(), status: "rejected".into(), resets: soon, windows: vec![window] };
        shell.limits.insert("work".into(), work);
        run_out(&mut shell, &session, unix_now() + 7_200, &mut actions);
        assert!(sent(&actions).is_empty());
        let resume = shell.threads[&session].resume.clone().expect("it waits");
        assert_eq!((resume.at, resume.all_out), (soon + GRACE, true), "for the account that lifts first");
        // That one lifts first, and takes over.
        lift(&mut shell, &session, "work");
        shell.tick(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Send { session, .. } if session.account == "work")));
        assert_eq!(shell.agent_in_use().map(|agent| agent.connection).as_deref(), Some("work"));
    }

    #[test]
    fn stop_stops_and_a_session_that_keeps_running_out_gives_up() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Stop);
        run_out(&mut shell, &session, unix_now() + 600, &mut actions);
        assert!(shell.threads[&session].resume.is_none() && sent(&actions).is_empty());

        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        if let Some(thread) = shell.threads.get_mut(&session) {
            thread.tries = TRIES;
        }
        run_out(&mut shell, &session, unix_now() + 600, &mut actions);
        assert!(shell.threads[&session].resume.is_none());
        assert!(shell.notices.iter().any(|notice| notice.text.contains("no longer goes on by itself")));
        // A turn that ends well starts the count over.
        shell.send("Try again", false, Vec::new(), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert_eq!(shell.threads[&session].tries, 0);
    }

    #[test]
    fn a_turn_that_was_refused_but_went_on_is_not_stopped() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        // Refused for the plan, carried on extra usage: the turn ends well.
        let limits =
            Limits { at: unix_now(), status: "rejected".into(), resets: unix_now() + 600, windows: Vec::new() };
        shell.agent_event(&session, AgentEvent::Limits(limits), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(shell.threads[&session].resume.is_none());
        // Stopped by the user while refused: that is what they asked for.
        shell.send("Go on", false, Vec::new(), &mut actions);
        shell.agent_event(&session, AgentEvent::LimitReached, &mut actions);
        if let Some(thread) = shell.threads.get_mut(&session) {
            thread.stopping = true;
        }
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("x".into()) }, &mut actions);
        assert!(shell.threads[&session].resume.is_none());
    }

    #[test]
    fn a_limit_without_a_time_is_tried_again_later() {
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Wait);
        shell.agent_event(&session, AgentEvent::LimitReached, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("Rejected (429)".into()) }, &mut actions);
        let at = shell.threads[&session].resume.as_ref().map(|resume| resume.at).expect("it waits");
        assert!(at > unix_now() + RETRY - 5 && at <= unix_now() + RETRY);
        assert_eq!(shell.out_until(Connection::CLAUDE_AGENT), Some(0), "the account counts as out meanwhile");

        // A refusal long since lifted does not make the account look fresh to the next one.
        let (mut shell, session, mut actions) = two_accounts(OutOfLimits::Switch);
        let stale = Limits { at: 1, status: "rejected".into(), resets: 2, windows: Vec::new() };
        shell.limits.insert("work".into(), stale.clone());
        shell.limits.insert(Connection::CLAUDE_AGENT.into(), stale);
        shell.agent_event(&session, AgentEvent::LimitReached, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("Rejected".into()) }, &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Send { session, .. } if session.account == "work")));
        assert_eq!(shell.out_until(Connection::CLAUDE_AGENT), Some(0));
    }

    #[test]
    fn only_kin_providers_keep_the_model() {
        let mut config = Config::default();
        config.connections.push(Connection {
            id: "work".into(),
            config_dir: "/w".into(),
            ..Connection::claude_agent()
        });
        config.connections.push(Connection { id: "or".into(), kind: ConnectionKind::OpenAi, ..Connection::default() });
        let mut agent = Agent::new("Scout");
        (agent.model, agent.effort) = ("opus".into(), "high".into());
        let mut shell = Shell::new(config, vec![agent.clone()], Vec::new());
        assert!(shell.kin(Connection::CLAUDE_AGENT, "work") && shell.kin("or", "or"));
        assert!(!shell.kin(Connection::CLAUDE_AGENT, "or") && !shell.kin("work", "gone"));

        // The agent editor moves it to another Claude account: the model and effort stay.
        let mut actions = Vec::new();
        let edited = Agent { connection: "work".into(), model: String::new(), effort: String::new(), ..agent };
        shell.save_agent(edited, &mut actions);
        let agent = shell.selected_agent().cloned().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "opus", "high"));
        // To another kind of provider the model means nothing; how hard it thinks still does.
        shell.choose(Menu::Provider, "or", &mut actions);
        let agent = shell.selected_agent().cloned().expect("agent");
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("or", "", "high"));
    }
}
