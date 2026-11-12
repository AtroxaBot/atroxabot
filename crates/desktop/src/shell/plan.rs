//! Asking where an account's plan limits stand. A Claude account's replies
//! report the windows their turn counted against (`AgentEvent::Limits`);
//! asking Claude Code (its own `/usage`, through `atroxabot_core::claude_usage`)
//! gives every window at once: a model's week while another model is used,
//! and a limit reset on claude.ai. A Codex account is asked by reading it
//! anew, which brings its limits along.
//!
//! The app asks only about an account whose limits are on screen, and
//! seldom: when the side panel shows them while the window has the user's
//! attention and they were last asked about ten minutes ago or more; when
//! the user comes back to the window a minute or more after that; when the
//! statistics open; and when the refresh icon by them is clicked. Nothing
//! runs on a timer or while the window is in the background. Claude Code
//! answers from what it fetched in the last minute, so asking sooner gains
//! nothing.
//!
//! Anthropic calls the request experimental. A check that fails leaves the
//! numbers as the replies last reported them (and they say since when, once
//! they are old). One Claude Code refuses, or answers in a shape not known
//! here, is said under them and not made again by itself until the app
//! starts again: the app needs an update then.

use atroxabot_core::{ClaudeError, Connection, ConnectionKind, Limits, PlanCheck, unix_now};

use super::limits::OutOfLimits;
use super::{Shell, order_windows};
use crate::app::Action;

/// How old the last check of limits on screen may be before they are
/// checked again, in seconds.
pub(super) const RECHECK: u64 = 10 * 60;
/// How old it may be when the user comes back to the window or opens the
/// statistics: Claude Code answers from what it fetched in the last minute.
pub(super) const SOON: u64 = 60;

/// What is said under the limits once Claude Code no longer answers the
/// check as expected.
pub(super) const CHANGED: &str = "Not checked: Claude Code's /usage has changed.";

/// How asking about one account's plan limits goes.
#[derive(Debug, Clone, Default)]
pub(super) struct Check {
    /// When it was last asked, or answered with its limits unasked (Codex,
    /// when read), in Unix seconds; 0 never.
    asked: u64,
    /// An answer is awaited.
    running: bool,
    /// Why Claude Code's answer was not one the app knows: only a click
    /// asks it again.
    changed: Option<String>,
}

impl Check {
    /// Whether to ask again at `now`, the last time being at least `every`
    /// seconds ago (0: whenever). A check the user asks for `by_hand` is
    /// made even when Claude Code's answer was not understood last time.
    fn due(&self, now: u64, every: u64, by_hand: bool) -> bool {
        !self.running && (by_hand || self.changed.is_none()) && now.saturating_sub(self.asked) >= every
    }
}

impl Shell {
    /// Asks where the plan limits of the account `account` stand, when they
    /// were last asked about `every` seconds ago or more (0: at once). Not
    /// while an answer is awaited, of an account that cannot answer (not
    /// signed in, an API provider), or (unless asked `by_hand`) once Claude
    /// Code's answer was not understood.
    pub(super) fn check_limits(&mut self, account: &str, every: u64, by_hand: bool, actions: &mut Vec<Action>) {
        let now = unix_now();
        // Looked at on every frame the panel draws: the cheap part first.
        if !self.checks.get(account).is_none_or(|check| check.due(now, every, by_hand)) {
            return;
        }
        let Some(connection) = self.config.connection(account).filter(|connection| self.can_check(connection)) else {
            return;
        };
        let check = self.checks.entry(account.to_owned()).or_default();
        (check.running, check.asked) = (true, now);
        actions.push(match connection.kind {
            ConnectionKind::Codex => Action::ProbeCodex(Box::new(connection)),
            _ => Action::CheckLimits(Box::new(connection)),
        });
    }

    /// Whether the plan limits of `connection` can be asked about: it is a
    /// Claude or Codex account that is signed in.
    pub(super) fn can_check(&self, connection: &Connection) -> bool {
        connection.kind.is_account() && self.signed_in(connection)
    }

    /// The window has the user's attention again: limits on screen that may
    /// have changed meanwhile (on claude.ai, say) are checked.
    pub(super) fn check_shown_limits(&mut self, actions: &mut Vec<Action>) {
        if self.config.panel
            && let Some(account) = self.limits_account()
        {
            self.check_limits(&account, SOON, false, actions);
        }
        if matches!(self.modal, Some(super::Modal::Stats(_))) {
            self.check_every_account(actions);
        }
    }

    /// The statistics show every account's limits: each is checked.
    pub(super) fn check_every_account(&mut self, actions: &mut Vec<Action>) {
        for connection in self.config.all_connections() {
            self.check_limits(&connection.id, SOON, false, actions);
        }
    }

    /// Whether the plan limits of `account` are being asked about.
    pub(super) fn checking(&self, account: &str) -> bool {
        self.checks.get(account).is_some_and(|check| check.running)
    }

    /// Why Claude Code's answer about the limits of `account` was not
    /// understood, if it was not.
    pub(super) fn check_changed(&self, account: &str) -> Option<&str> {
        self.checks.get(account).and_then(|check| check.changed.as_deref())
    }

    /// Claude Code answered (or could not) where the plan limits of the
    /// Claude account `account` stand.
    pub fn limits_checked(&mut self, account: &str, result: Result<PlanCheck, ClaudeError>, actions: &mut Vec<Action>) {
        let check = self.checks.entry(account.to_owned()).or_default();
        check.running = false;
        match result {
            Ok(PlanCheck::Limits { limits, complete }) => {
                check.changed = None;
                self.take_limits(account, limits, complete, actions);
            }
            // Answered as expected, with nothing to show: asked again later.
            Ok(PlanCheck::NoPlan | PlanCheck::Unavailable) => check.changed = None,
            Err(ClaudeError::Unsupported(why)) => check.changed = Some(why),
            // It could not be started or did not answer (the app logs why): asked again later.
            Err(_) => {}
        }
    }

    /// Codex was read for the account `account` (or could not be): its
    /// limits came along, if it had them.
    pub(super) fn codex_checked(&mut self, account: &str, limits: bool) {
        let check = self.checks.entry(account.to_owned()).or_default();
        check.running = false;
        if limits {
            check.asked = unix_now();
        }
    }

    /// Takes in what a check said of the limits of `account`. A limit that
    /// stopped it and is behind it now is over: its notice goes, and the
    /// sessions waiting to go on with it (on it, or with any account of its
    /// kind when another may take over) are looked at again at once.
    fn take_limits(&mut self, account: &str, limits: Limits, complete: bool, actions: &mut Vec<Action>) {
        let was_out = self.out_until(account).is_some();
        take_check(self.limits.entry(account.to_owned()).or_default(), limits, complete, unix_now());
        actions.push(Action::SaveLimits(self.limits.clone()));
        if !was_out || self.out_until(account).is_some() {
            return;
        }
        self.notices.retain(|notice| notice.account != account);
        let switching = self.out_of_limits() == OutOfLimits::Switch;
        let waiting: Vec<String> = (self.threads.iter())
            .filter(|(_, thread)| thread.resume.is_some())
            .map(|(id, _)| id.clone())
            .filter(|session| {
                self.agent_of(session).is_some_and(|agent| {
                    agent.connection == account || (switching && self.kin(&agent.connection, account))
                })
            })
            .collect();
        let now = unix_now();
        for session in waiting {
            if let Some(resume) = self.threads.get_mut(&session).and_then(|thread| thread.resume.as_mut()) {
                resume.at = resume.at.min(now);
            }
        }
    }
}

/// Takes what a check said of an account's limits (`new`) into what is
/// `known` of them, at `now`. A `complete` check names every window, so one
/// it does not name is gone; otherwise a window it does not name (a model's
/// week, when Claude Code answered from what it kept) keeps what was last
/// said of it. A check says no status: a limit that was hit stands while a
/// window is still full, and is over once none is.
fn take_check(known: &mut Limits, new: Limits, complete: bool, now: u64) {
    if complete {
        known.windows = new.windows;
    } else {
        for window in new.windows {
            match known.windows.iter_mut().find(|old| old.name == window.name) {
                Some(old) => *old = window,
                None => known.windows.push(window),
            }
        }
    }
    order_windows(&mut known.windows);
    known.at = new.at;
    let full = known.windows.iter().any(|window| window.used >= 1.0 && (window.resets == 0 || window.resets > now));
    if known.status == "rejected" && !full {
        (known.status, known.resets) = ("allowed".to_owned(), 0);
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, AgentEvent, ClaudeInfo, Config, LimitWindow};

    use super::*;
    use crate::shell::limits::Resume;

    fn window(name: &str, used: f64, resets: u64) -> LimitWindow {
        LimitWindow { name: name.into(), used, resets, label: String::new() }
    }

    fn names(limits: &Limits) -> Vec<(&str, f64)> {
        limits.windows.iter().map(|window| (window.name.as_str(), window.used)).collect()
    }

    #[test]
    fn a_check_says_every_window_or_only_those_it_knows() {
        let fable = || LimitWindow { label: "Fable".into(), ..window("seven_day_overage_included", 1.0, 900) };
        let mut known = Limits {
            at: 1,
            status: "rejected".into(),
            resets: 900,
            windows: vec![window("five_hour", 0.2, 50), window("seven_day", 0.5, 900), fable(), window("odd", 0.1, 0)],
        };
        // From what Claude Code kept: the plan's windows, nothing of Fable's week, which is still full.
        let kept = Limits {
            at: 2,
            windows: vec![window("five_hour", 0.3, 50), window("seven_day", 0.6, 900)],
            ..Limits::default()
        };
        take_check(&mut known, kept, false, 10);
        assert_eq!(
            names(&known),
            [("five_hour", 0.3), ("seven_day", 0.6), ("odd", 0.1), ("seven_day_overage_included", 1.0)]
        );
        assert_eq!((known.at, known.status.as_str(), known.resets), (2, "rejected", 900), "the limit still stands");
        // Reset on claude.ai: a complete check shows Fable's week empty, and what it does not name is gone.
        let reset = Limits {
            at: 3,
            windows: vec![
                window("five_hour", 0.3, 50),
                window("seven_day", 0.6, 900),
                LimitWindow { used: 0.0, ..fable() },
            ],
            ..Limits::default()
        };
        take_check(&mut known, reset, true, 10);
        assert_eq!(names(&known), [("five_hour", 0.3), ("seven_day", 0.6), ("seven_day_overage_included", 0.0)]);
        assert_eq!(known.windows[2].label, "Fable");
        assert_eq!((known.at, known.status.as_str(), known.resets), (3, "allowed", 0), "the limit is over");
        // A full window that has started over by now is no limit either.
        let mut known = Limits { status: "rejected".into(), resets: 5, ..Limits::default() };
        take_check(&mut known, Limits { windows: vec![window("five_hour", 1.0, 5)], ..Limits::default() }, true, 10);
        assert_eq!(known.status, "allowed");
    }

    #[test]
    fn checks_are_made_seldom_and_never_twice_at_once() {
        let check = Check { asked: 100, ..Check::default() };
        assert!(!check.due(100 + RECHECK - 1, RECHECK, false) && check.due(100 + RECHECK, RECHECK, false));
        assert!(check.due(100, 0, false), "a click asks at once");
        let running = Check { running: true, ..Check::default() };
        assert!(!running.due(10_000, 0, true), "one at a time");
        let changed = Check { changed: Some("refused".into()), ..Check::default() };
        assert!(!changed.due(10_000, RECHECK, false) && changed.due(10_000, 0, true), "only a click asks again");
    }

    #[test]
    fn a_limit_reset_elsewhere_is_seen_on_the_next_check() {
        let mut config = Config::default();
        let work = Connection { id: "work".into(), config_dir: "/accounts/work".into(), ..Connection::claude_agent() };
        config.connections.push(work);
        let mut shell = Shell::new(config, vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let account = Connection::CLAUDE_AGENT;
        // Not signed in yet: nothing to ask.
        shell.check_limits(account, 0, true, &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::CheckLimits(_))));
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        shell.claude_info(account, Ok(info));
        actions.clear();
        shell.check_limits(account, RECHECK, false, &mut actions);
        assert!(matches!(&actions[..], [Action::CheckLimits(asked)] if asked.id == account));
        actions.clear();
        shell.check_limits(account, 0, true, &mut actions);
        assert!(actions.is_empty() && shell.checking(account), "the answer is awaited");

        // A turn on Fable ran into its week; the session waits for it to lift, days from now.
        shell.send("Hello", false, Vec::new(), &mut actions);
        let session = shell.current.clone().unwrap();
        let resets = unix_now() + 3 * 86_400;
        let fable = LimitWindow { label: "Fable".into(), ..window("seven_day_overage_included", 1.0, resets) };
        let refused = Limits { at: unix_now(), status: "rejected".into(), resets, windows: vec![fable.clone()] };
        shell.agent_event(&session, AgentEvent::Limits(refused), &mut actions);
        assert!(shell.notices.iter().any(|notice| notice.account == account));
        shell.threads.get_mut(&session).unwrap().resume = Some(Resume { at: resets + 60, all_out: false });
        // Another session waits for the other account's limit.
        shell.new_session(&mut actions);
        shell.send("Elsewhere", false, Vec::new(), &mut actions);
        let elsewhere = shell.current.clone().unwrap();
        let other = shell.threads.get_mut(&elsewhere).unwrap();
        other.session.provider = "work".into();
        other.resume = Some(Resume { at: resets + 60, all_out: false });

        // The user reset it on claude.ai; the check says so: the notice goes and the session looks at once.
        let now = unix_now();
        let windows = vec![window("five_hour", 0.1, now + 600), LimitWindow { used: 0.0, ..fable }];
        let check = PlanCheck::Limits { limits: Limits { at: now, windows, ..Limits::default() }, complete: true };
        actions.clear();
        shell.limits_checked(account, Ok(check), &mut actions);
        assert!(!shell.checking(account) && actions.iter().any(|a| matches!(a, Action::SaveLimits(_))));
        assert_eq!(shell.out_until(account), None);
        assert!(!shell.notices.iter().any(|notice| notice.account == account), "its notice is gone");
        assert!(shell.threads[&session].resume.as_ref().is_some_and(|resume| resume.at <= unix_now()));
        let theirs = shell.threads[&elsewhere].resume.as_ref().map(|resume| resume.at);
        assert_eq!(theirs, Some(resets + 60), "a wait for another account's limit stays as it was");
        assert_eq!(shell.limits[account].windows[1].label, "Fable");

        // Claude Code no longer answers as expected: said once, and asked again only by hand.
        shell.limits_checked(account, Err(ClaudeError::Unsupported("refused".into())), &mut actions);
        assert_eq!(shell.check_changed(account), Some("refused"));
        actions.clear();
        shell.check_limits(account, 0, false, &mut actions);
        assert!(actions.is_empty());
        shell.check_limits(account, 0, true, &mut actions);
        assert!(matches!(&actions[..], [Action::CheckLimits(_)]));
        shell.limits_checked(account, Ok(PlanCheck::Unavailable), &mut actions);
        assert_eq!(shell.check_changed(account), None, "answered as expected again");
    }
}
