//! How long an account's process (Claude Code, Codex, Antigravity) outlives
//! the look at its session.
//!
//! A process that is up answers the next message at once; one that has to
//! start first takes a few seconds to come up and to read its conversation
//! again (Antigravity, which unpacks itself at every start, up to a minute). So a session's process is not closed the moment the user opens
//! another session: it stays for [`KEEP`] after they left it with nothing to
//! do, and of those that wait like this only the [`MOST`] left last are kept
//! (each holds a few hundred megabytes). One that is open, at work, or
//! running something in the background is never closed by this.
//!
//! Closing one costs time, not tokens: Claude Code and Codex take up a
//! conversation under the system prompt it began with
//! (`Session::account_prompt`), so the provider's cache still serves it.

//!
//! The messages of a session outlive its process, but not for good: an
//! opened session holds everything its agent and its subagents read and
//! wrote, tens of megabytes for a long one. One that is not looked at, has
//! no process and nothing waiting gives them back after [`HOLD`]; the
//! sidebar keeps listing it, and opening it reads them again.

use super::Shell;
use crate::app::Action;

/// How long a process is kept for a session that is not looked at and has nothing to do, in seconds.
const KEEP: u64 = 15 * 60;
/// How many such processes are kept at most.
const MOST: usize = 3;
/// How long the messages of a session that is not looked at and has nothing to do stay in memory, in seconds.
const HOLD: u64 = 30 * 60;

impl Shell {
    /// Closes the processes that waited long enough at `now` (Unix seconds),
    /// and the ones over [`MOST`], longest left first. Call it on every frame
    /// and timer: it also notes when the user leaves a session.
    pub(super) fn rest(&mut self, now: u64, actions: &mut Vec<Action>) {
        if self.looked != self.current {
            if let Some(thread) = self.looked.take().and_then(|left| self.threads.get_mut(&left)) {
                thread.left = now;
            }
            self.looked.clone_from(&self.current);
        }
        let mut resting = self.resting();
        resting.sort_unstable();
        let spare = resting.len().saturating_sub(MOST);
        for (index, (since, session)) in resting.into_iter().enumerate() {
            if index >= spare && since.saturating_add(KEEP) > now {
                continue;
            }
            if let Some(thread) = self.threads.get_mut(&session) {
                thread.live = false;
            }
            actions.push(Action::CloseLive(session));
        }
    }

    /// Lets go of the messages of the sessions that had nothing to do for [`HOLD`] at `now` (Unix
    /// seconds). What they hold is written first, whatever of it already was, and the writer lets
    /// go of it too. Call it on every frame and timer.
    pub(super) fn unload(&mut self, now: u64, actions: &mut Vec<Action>) {
        let due = |(since, _): &(u64, String)| since.saturating_add(HOLD) <= now;
        for (_, session) in self.idle().into_iter().filter(due) {
            if let Some(thread) = self.threads.remove(&session) {
                actions.push(Action::SaveSession(Box::new(thread.session)));
                actions.push(Action::ForgetSession(session));
            }
        }
    }

    /// When the next of them has had nothing to do for long enough, in Unix seconds: the app wakes for it.
    pub(super) fn next_unload(&self) -> Option<u64> {
        self.idle().into_iter().map(|(since, _)| since.saturating_add(HOLD)).min()
    }

    /// The sessions whose messages are in memory for nobody: not the open one or the one on a call,
    /// no process up, nothing at work, nothing asked and nothing waiting to be sent or to go on. Each
    /// with since when: it was left, or changed last after that.
    fn idle(&self) -> Vec<(u64, String)> {
        let open = self.current.as_deref();
        let called = self.call.as_ref().map(|call| call.session.as_str());
        let idle = |(id, thread): &(&String, &super::thread::Thread)| {
            let wanted = open == Some(id.as_str()) || called == Some(id.as_str());
            let waiting = !thread.asks.is_empty()
                || thread.queued.is_some()
                || !thread.follow_ups.is_empty()
                || thread.resume.is_some();
            thread.loaded && !wanted && !thread.live && !thread.at_work() && !waiting
        };
        let since =
            |(id, thread): (&String, &super::thread::Thread)| (thread.left.max(thread.session.updated), id.clone());
        self.threads.iter().filter(idle).map(since).collect()
    }

    /// When the next of them has waited long enough, in Unix seconds: the app wakes for it.
    pub(super) fn next_rest(&self) -> Option<u64> {
        self.resting().into_iter().map(|(since, _)| since.saturating_add(KEEP)).min()
    }

    /// The sessions whose process is up with nothing to do while the user
    /// looks elsewhere, each with since when: it was left, or went quiet
    /// after that.
    fn resting(&self) -> Vec<(u64, String)> {
        let open = self.current.as_deref();
        let waits = |(id, thread): &(&String, &super::thread::Thread)| {
            thread.live && !thread.at_work() && open != Some(id.as_str())
        };
        let since =
            |(id, thread): (&String, &super::thread::Thread)| (thread.left.max(thread.session.updated), id.clone());
        self.threads.iter().filter(waits).map(since).collect()
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, AgentEvent, ClaudeInfo, Config, Connection};

    use super::*;

    /// A shell with `count` sessions on a Claude account, each answered once, made a second apart from
    /// the time 100 on: their ids, oldest first, and what was done meanwhile.
    fn answered(count: usize) -> (Shell, Vec<String>, Vec<Action>) {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        shell.claude_info(Connection::CLAUDE_AGENT, Ok(info));
        let mut sessions = Vec::new();
        for n in 0..count {
            shell.new_session();
            shell.send(&format!("Task {n}"), false, Vec::new(), &mut actions);
            let session = shell.current.clone().expect("a session");
            shell.rest(100 + n as u64, &mut actions);
            shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
            // Whenever the test runs, the turn ended long before the times it rests at.
            shell.threads.get_mut(&session).expect("open").session.updated = 0;
            sessions.push(session);
        }
        (shell, sessions, actions)
    }

    fn closed(actions: &[Action]) -> Vec<String> {
        let mut closed: Vec<String> =
            actions.iter().filter_map(|a| if let Action::CloseLive(s) = a { Some(s.clone()) } else { None }).collect();
        closed.sort();
        closed
    }

    fn unloaded(actions: &[Action]) -> Vec<String> {
        actions.iter().filter_map(|a| if let Action::ForgetSession(s) = a { Some(s.clone()) } else { None }).collect()
    }

    #[test]
    fn a_session_nobody_looks_at_gives_its_messages_back() {
        let (mut shell, sessions, mut actions) = answered(2);
        let (first, second) = (&sessions[0], &sessions[1]);
        shell.rest(1_000, &mut actions);
        shell.open_session(first, &mut actions);
        shell.rest(1_010, &mut actions);
        // While its process is up it is not let go of, however long that is.
        shell.unload(1_010 + HOLD, &mut actions);
        assert!(unloaded(&actions).is_empty() && shell.next_unload().is_none());
        shell.rest(1_010 + KEEP, &mut actions);
        assert_eq!(closed(&actions), std::slice::from_ref(second));
        actions.clear();
        // From then on it waits, and the app wakes when it has waited long enough.
        assert_eq!(shell.next_unload(), Some(1_010 + HOLD));
        shell.unload(1_010 + HOLD - 1, &mut actions);
        assert!(actions.is_empty() && shell.threads.contains_key(second));
        shell.unload(1_010 + HOLD, &mut actions);
        assert!(!shell.threads.contains_key(second), "its messages are let go of");
        let written = matches!(
            actions.as_slice(),
            [Action::SaveSession(saved), Action::ForgetSession(gone)] if saved.id == *second && gone == second
        );
        assert!(written, "written first, then forgotten by the writer too");
        assert!(shell.sessions.iter().any(|known| known.id == *second), "the sidebar still lists it");
        // The open one is never let go of.
        actions.clear();
        shell.unload(u64::MAX / 2, &mut actions);
        assert!(actions.is_empty() && shell.threads.contains_key(first) && shell.next_unload().is_none());
        // Opening it again reads its messages again.
        shell.open_session(second, &mut actions);
        assert!(matches!(actions.first(), Some(Action::LoadSession(id)) if id == second));
        assert!(shell.threads.get(second).is_some_and(|thread| !thread.loaded));
    }

    #[test]
    fn a_session_with_something_waiting_keeps_its_messages() {
        let (mut shell, sessions, mut actions) = answered(3);
        shell.rest(1_000, &mut actions);
        shell.open_session(&sessions[0], &mut actions);
        shell.rest(1_010, &mut actions);
        shell.rest(1_010 + KEEP, &mut actions);
        // One has a message to send once it can; the other runs something in the background.
        let waits = shell.threads.get_mut(&sessions[1]).expect("loaded");
        (waits.live, waits.queued) = (false, Some(("Later".into(), false, Vec::new())));
        let works = shell.threads.get_mut(&sessions[2]).expect("loaded");
        works.live = false;
        works.background_task("b1".into(), "cargo test".into(), false);
        actions.clear();
        shell.unload(u64::MAX / 2, &mut actions);
        assert!(unloaded(&actions).is_empty() && shell.threads.len() == 3, "{:?}", unloaded(&actions));
        // Both done, both go.
        shell.threads.get_mut(&sessions[1]).expect("loaded").queued = None;
        shell.threads.get_mut(&sessions[2]).expect("loaded").background_task("b1".into(), String::new(), true);
        shell.unload(u64::MAX / 2, &mut actions);
        let mut gone = unloaded(&actions);
        gone.sort();
        let mut expected = vec![sessions[1].clone(), sessions[2].clone()];
        expected.sort();
        assert_eq!(gone, expected);
    }

    #[test]
    fn a_session_left_keeps_its_process_for_a_while() {
        let (mut shell, sessions, mut actions) = answered(2);
        let (first, second) = (&sessions[0], &sessions[1]);
        // Opening another leaves its process be: coming back answers at once.
        shell.rest(1_000, &mut actions);
        shell.open_session(first, &mut actions);
        shell.rest(1_010, &mut actions);
        assert!(closed(&actions).is_empty(), "{:?}", closed(&actions));
        assert_eq!(shell.next_rest(), Some(1_010 + KEEP), "the app wakes when the one left has waited long enough");
        // Looked at again and left again, the wait starts over.
        shell.open_session(second, &mut actions);
        shell.rest(1_500, &mut actions);
        assert_eq!(shell.next_rest(), Some(1_500 + KEEP));
        shell.rest(1_500 + KEEP - 1, &mut actions);
        assert!(closed(&actions).is_empty());
        // Long enough: it is closed, once.
        shell.rest(1_500 + KEEP, &mut actions);
        assert_eq!(closed(&actions), std::slice::from_ref(first));
        actions.clear();
        shell.rest(1_500 + 2 * KEEP, &mut actions);
        assert!(closed(&actions).is_empty() && shell.next_rest().is_none(), "the open one is never closed by this");
        // Its next message starts a process again, which is then kept as before.
        shell.open_session(first, &mut actions);
        shell.rest(5_000, &mut actions);
        actions.clear();
        shell.send("More", false, Vec::new(), &mut actions);
        shell.agent_event(first, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(shell.threads[first].live);
    }

    #[test]
    fn only_a_few_wait_and_the_longest_left_goes_first() {
        // Each was left as the next began.
        let (mut shell, sessions, mut actions) = answered(MOST + 3);
        // The last is open, MOST wait, and the two left first were closed as the fifth and sixth began.
        let mut first_two = sessions[..2].to_vec();
        first_two.sort();
        assert_eq!(closed(&actions), first_two);
        let live = |shell: &Shell| sessions.iter().filter(|session| shell.threads[*session].live).count();
        assert_eq!(live(&shell), MOST + 1);
        // Going back to one that waits changes nothing but which one is open.
        actions.clear();
        shell.open_session(&sessions[3], &mut actions);
        shell.rest(200, &mut actions);
        assert!(closed(&actions).is_empty());
        assert_eq!(live(&shell), MOST + 1);
    }

    #[test]
    fn one_at_work_or_running_something_is_not_closed() {
        let (mut shell, sessions, mut actions) = answered(3);
        let (busy, watching, open) = (&sessions[0], &sessions[1], &sessions[2]);
        shell.open_session(busy, &mut actions);
        shell.send("Go on", false, Vec::new(), &mut actions);
        let watch = AgentEvent::Background { id: "b1".into(), what: "Watch the build".into(), ended: false };
        shell.agent_event(watching, watch, &mut actions);
        shell.open_session(open, &mut actions);
        actions.clear();
        shell.rest(10 * KEEP, &mut actions);
        assert!(closed(&actions).is_empty(), "{:?}", closed(&actions));
        // Done, each waits from then on, not from when it was left.
        shell.agent_event(busy, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.agent_event(
            watching,
            AgentEvent::Background { id: "b1".into(), what: String::new(), ended: true },
            &mut actions,
        );
        for session in [busy, watching] {
            shell.threads.get_mut(session).expect("open").session.updated = 10 * KEEP;
        }
        shell.rest(10 * KEEP + 1, &mut actions);
        assert!(closed(&actions).is_empty());
        shell.rest(11 * KEEP, &mut actions);
        assert_eq!(closed(&actions).len(), 2);
    }

    #[test]
    fn a_process_that_ended_is_not_waited_for() {
        let (mut shell, sessions, mut actions) = answered(2);
        shell.rest(1_000, &mut actions);
        shell.agent_event(&sessions[0], AgentEvent::Disconnected(None), &mut actions);
        actions.clear();
        assert_eq!(shell.next_rest(), None);
        shell.rest(1_000 + KEEP, &mut actions);
        assert!(closed(&actions).is_empty());
    }
}
