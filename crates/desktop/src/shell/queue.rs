//! Messages the user sends while the agent works. Each waits above the
//! message box until the agent takes it in, which it does at its next step:
//! the app hands it over while the agent's tools run, and the agent reads it
//! as soon as they are done (Claude Code queues it itself; the API engine
//! takes it from its `Control`). What is left when the turn ends is sent as
//! the next message. "Send now" stops the step in progress so the agent reads
//! it at once, with everything it did so far; Stop puts what waits back in the
//! message box. Until it is handed over a message can be edited or removed.

use atroxabot_core::{ConnectionKind, FollowUp, INTERRUPTING};

use super::Shell;
use crate::app::Action;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{Ui, icon, icon_button, id};

/// Height of a waiting message's row.
const ROW: f32 = 34.0;
/// Most waiting messages shown at once; the rest are counted.
const SHOWN: usize = 4;

/// A message the user sent while the agent worked, until it is taken in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Waiting {
    /// The message, with the id the engine reports it by.
    pub message: FollowUp,
    /// Handed to the agent, which reads it at its next step: it can no
    /// longer be edited or taken back.
    pub sent: bool,
}

/// Whether `text` is a command (`/compact`), which runs as a turn of its own.
fn is_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// What a row does when clicked.
enum Pick {
    Now,
    Edit(String),
    Remove(String),
}

impl Shell {
    /// Puts a message the user sent while the agent works in line; it goes
    /// over at once if the agent's tools are running.
    pub(super) fn follow_up(&mut self, session: &str, text: &str, files: Vec<String>, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session) else { return };
        let message = FollowUp { id: atroxabot_core::new_id(), text: text.to_owned(), files };
        thread.follow_ups.push(Waiting { message, sent: false });
        self.hand_over(session, actions);
    }

    /// Hands the agent what waits while its tools run, so that it reads it
    /// as soon as they are done. Commands wait for the turn to end.
    pub(super) fn hand_over(&mut self, session: &str, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session) else { return };
        if !thread.busy || thread.stopping || !thread.tools_running() {
            return;
        }
        for waiting in thread.follow_ups.iter_mut().filter(|w| !w.sent && !is_command(&w.message.text)) {
            waiting.sent = true;
            actions.push(Action::FollowUp { session: session.to_owned(), message: Box::new(waiting.message.clone()) });
        }
    }

    /// "Send now": the step in progress stops and the agent reads what
    /// waits right away, keeping what it did so far.
    pub(super) fn hurry(&mut self, session: &str, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session) else { return };
        if !thread.busy {
            self.send_follow_ups(session, false, actions);
            return;
        }
        if thread.stopping {
            return;
        }
        // Claude Code keeps what it is handed and answers it after the stop. Codex would drop it with the turn
        // it was put into: there it goes as the next message instead, once the turn has stopped.
        let kept =
            self.config.connection(&thread.sent_on).map(|connection| connection.kind) != Some(ConnectionKind::Codex);
        for waiting in thread.follow_ups.iter_mut().filter(|w| kept && !w.sent && !is_command(&w.message.text)) {
            waiting.sent = true;
            actions.push(Action::FollowUp { session: session.to_owned(), message: Box::new(waiting.message.clone()) });
        }
        (thread.hurry, thread.stopping) = (true, true);
        "Stopping to read your message…".clone_into(&mut thread.status);
        actions.push(Action::Interrupt(session.to_owned()));
    }

    /// Sends what waits in `session` as one message, now that its agent is
    /// free: up to the first command, or that command alone. `interrupted`
    /// when the agent was stopped for it. What cannot go yet stays in line.
    pub(super) fn send_follow_ups(&mut self, session: &str, interrupted: bool, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session).filter(|thread| !thread.busy) else { return };
        let Some(first) = thread.follow_ups.first() else { return };
        let count = if is_command(&first.message.text) {
            1
        } else {
            thread.follow_ups.iter().take_while(|w| !is_command(&w.message.text)).count()
        };
        let going: Vec<Waiting> = thread.follow_ups.drain(..count).collect();
        let text = going.iter().map(|w| w.message.text.trim()).filter(|text| !text.is_empty()).collect::<Vec<_>>();
        let files: Vec<String> = going.iter().flat_map(|w| w.message.files.iter().cloned()).collect();
        if !self.send_to(session, &text.join("\n\n"), &files, (false, interrupted), actions)
            && let Some(thread) = self.threads.get_mut(session)
        {
            // Not sent (a notice says why): it waits again, first in line.
            let unsent = going.into_iter().map(|w| Waiting { sent: false, ..w });
            thread.follow_ups.splice(0..0, unsent);
        }
    }

    /// The note before a message the agent was stopped for: an API agent is
    /// told; Claude Code marks the interruption itself.
    pub(super) fn interrupting_note(kind: ConnectionKind, text: &str) -> &'static str {
        if kind == ConnectionKind::ClaudeAgent || is_command(text) { "" } else { INTERRUPTING }
    }

    /// Puts what waits in `session` back in the message box, before
    /// whatever is being written there: the user stopped the agent.
    pub(super) fn restore_follow_ups(&mut self, session: &str) {
        if self.current.as_deref() != Some(session) {
            return;
        }
        self.show_draft();
        let Some(thread) = self.threads.get_mut(session) else { return };
        let waiting = std::mem::take(&mut thread.follow_ups);
        for waiting in waiting.into_iter().rev() {
            self.composer.restore(&waiting.message.text, waiting.message.files);
        }
    }

    /// The open session's waiting messages, as rows from `bottom` up, in
    /// the column at `x`, `width` wide. Returns where they begin.
    pub(super) fn draw_queue(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, bottom, width): (f32, f32, f32),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let Some(session) = self.current.clone() else { return bottom };
        let Some(thread) = self.threads.get(&session).filter(|thread| !thread.follow_ups.is_empty()) else {
            return bottom;
        };
        let shown = thread.follow_ups.len().min(SHOWN);
        let more = thread.follow_ups.len() - shown;
        let extra = if more > 0 { 22.0 } else { 0.0 };
        let tray = Rect::new(
            x + 12.0,
            bottom - shown as f32 * ROW - extra - 8.0,
            width - 24.0,
            shown as f32 * ROW + extra + 8.0,
        );
        p.shadow(
            Rect::new(tray.x + 6.0, tray.y + 6.0, tray.w - 12.0, tray.h),
            fade(t.shadow, 0.45),
            theme::RADIUS,
            16.0,
        );
        p.bordered(tray, t.surface, theme::RADIUS, 1.0, t.border);
        let mut pick = None;
        for (index, waiting) in thread.follow_ups.iter().take(SHOWN).enumerate() {
            let row = Rect::new(tray.x, tray.y + 4.0 + index as f32 * ROW, tray.w, ROW);
            let mid = row.y + row.h * 0.5;
            let appear = ui.appear(id(("waiting", &waiting.message.id)));
            let glyph = if waiting.sent { icon::CHECK } else { icon::HISTORY };
            icon::draw(p, glyph, (row.x + 18.0, mid), 13.0, fade(t.text_faint, appear));
            // Its controls at the right, in the same places on every row: what was
            // handed over can only be hurried.
            let mut right = row.right() - 8.0;
            for (glyph, what) in [(icon::X, Pick::Remove as fn(String) -> Pick), (icon::PEN, Pick::Edit)] {
                right -= 26.0;
                if !waiting.sent && icon_button(p, ui, Rect::new(right, mid - 13.0, 26.0, 26.0), glyph, true) {
                    pick = Some(what(waiting.message.id.clone()));
                }
            }
            let now = p.layout("Send now", theme::TINY, None);
            let button = Rect::new(right - now.width() - 16.0, mid - 12.0, now.width() + 12.0, 24.0);
            let hovered = ui.hovered(button);
            let hover = ui.anim(id(("send-now", &waiting.message.id)), f32::from(u8::from(hovered)));
            p.rect(button, fade(t.hover, hover), theme::RADIUS_SM);
            p.text(&now, button.x + 6.0, mid - now.height() * 0.5, mix(t.text_faint, t.text, hover));
            if hovered {
                ui.cursor = winit::window::CursorIcon::Pointer;
                if ui.clicked(button) {
                    pick = Some(Pick::Now);
                }
            }
            let first_line = waiting.message.text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();
            let files = waiting.message.files.len();
            let words = match (first_line.trim(), files) {
                ("", 1) => "1 file".to_owned(),
                ("", n) => format!("{n} files"),
                (line, _) => line.to_owned(),
            };
            let mut words = p.layout(&words, theme::SMALL, None);
            words.truncate(p.fonts, button.x - row.x - 44.0);
            p.text(&words, row.x + 34.0, mid - words.height() * 0.5, fade(t.text_muted, appear));
        }
        if more > 0 {
            let line = if more == 1 { "1 more waits".to_owned() } else { format!("{more} more wait") };
            p.label(&line, theme::TINY, tray.x + 34.0, tray.bottom() - 24.0, t.text_faint);
        }
        match pick {
            Some(Pick::Now) => self.hurry(&session, actions),
            Some(Pick::Edit(id)) => {
                if let Some(thread) = self.threads.get_mut(&session)
                    && let Some(at) = thread.follow_ups.iter().position(|w| w.message.id == id && !w.sent)
                {
                    let waiting = thread.follow_ups.remove(at);
                    self.composer.restore(&waiting.message.text, waiting.message.files);
                }
            }
            Some(Pick::Remove(id)) => {
                if let Some(thread) = self.threads.get_mut(&session) {
                    thread.follow_ups.retain(|w| w.message.id != id || w.sent);
                }
            }
            None => {}
        }
        tray.y - 8.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_wait_for_their_own_turn() {
        assert!(is_command("/compact") && is_command("  /clear"));
        assert!(!is_command("Also check /etc/hosts") && !is_command(""));
        assert_eq!(Shell::interrupting_note(ConnectionKind::ClaudeAgent, "Stop"), "");
        assert_eq!(Shell::interrupting_note(ConnectionKind::OpenAi, "/compact"), "");
        assert_eq!(Shell::interrupting_note(ConnectionKind::OpenAi, "Stop"), INTERRUPTING);
    }
}
