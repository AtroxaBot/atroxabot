//! The sidebar: the agents, each with its face, and under the selected one
//! its sessions, in the order the user drags them into (a new one starts
//! on top, and none moves by itself); the search and a new agent are icons
//! over them, the settings and the statistics icons at the foot.

use atroxabot_core::{Agent, unix_now};
use winit::window::CursorIcon;

use super::agent_form::AgentForm;
use super::settings::{Settings, Tab};
use super::{Modal, Shell, ago};
use crate::app::Action;
use crate::orb::{self, Mood};
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{Ui, icon, icon_button, id};

/// Height of an agent's row.
const AGENT_H: f32 = 54.0;
/// Height of a session's row.
const SESSION_H: f32 = 30.0;
/// Space between the sidebar's edge and its rows.
const INSET: f32 = theme::GAP;
/// How far a held session moves before it is dragged and not clicked.
const DRAG_SLACK: f32 = 4.0;

impl Shell {
    pub(super) fn draw_sidebar(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let top = area.y + 2.0;
        p.label("AGENTS", theme::CAPTION, area.x + INSET + 10.0, top + 7.0, t.text_faint);
        let add = Rect::new(area.right() - INSET - 29.0, top, 28.0, 28.0);
        if icon_button(p, ui, add, icon::PLUS, true) {
            self.modal = Some(Modal::Agent(Box::new(AgentForm::new(None))));
        }
        if icon_button(p, ui, Rect::new(add.x - 30.0, top, 28.0, 28.0), icon::SEARCH, true) {
            self.modal = Some(Modal::Spotlight(Box::default()));
        }
        // The app's own things, as icons at the foot.
        let foot = area.bottom() - INSET - 30.0;
        let settings = Rect::new(area.x + INSET, foot, 30.0, 30.0);
        if icon_button(p, ui, settings, icon::SETTINGS, true) {
            self.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::General))));
        }
        if icon_button(p, ui, Rect::new(settings.right() + 2.0, foot, 30.0, 30.0), icon::CHART, true) {
            self.open_stats(actions);
        }
        let list = Rect::new(area.x, top + 32.0, area.w, foot - 6.0 - top - 32.0);
        self.draw_agents(p, ui, list, actions);
    }

    /// The agents, and under the selected one its sessions.
    fn draw_agents(&mut self, p: &mut Painter, ui: &mut Ui, list: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let now = unix_now();
        let width = list.w - 2.0 * INSET;
        let offset = self.sidebar_scroll.update(ui, ui.hovered(list));
        let clip = p.push_clip(list);
        let mut y = list.y - offset;
        let (mut select, mut open, mut delete, mut fresh, mut edit) = (None, None, None, false, None::<Agent>);
        // The session being dragged, the one a press just took hold of, and the place the drag is over.
        let dragged = self.drag.as_ref().filter(|(_, moved)| *moved).map(|(session, _)| session.clone());
        let (mut held, mut place) = (None, None);
        for agent in &self.agents {
            let selected = self.agent.as_deref() == Some(agent.id.as_str());
            let row = Rect::new(list.x + INSET, y, width, AGENT_H - 2.0);
            y += AGENT_H;
            let sessions = self.sessions.iter().filter(|s| s.agent == agent.id);
            let busy = self.threads.values().any(|thread| thread.busy && thread.session.agent == agent.id);
            if row.bottom() >= list.y && row.y <= list.bottom() {
                let hovered = ui.hovered(row) && list.contains(ui.mouse);
                let hover = ui.anim(id(("agent", &agent.id)), f32::from(u8::from(hovered)));
                p.rect(row, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS);
                let mood = if busy { Mood::Thinking } else { Mood::Idle };
                let face = (row.x + 27.0, row.y + row.h * 0.5);
                orb::agent(p, &agent.portrait, face, 32.0, Self::color_of(agent), mood, ui.time);
                let latest = self.latest_session(&agent.id);
                // Right: how long ago, or an edit button while hovered.
                let pen = Rect::new(row.right() - 30.0, row.y + 6.0, 24.0, 24.0);
                let on_pen = hovered && pen.contains(ui.mouse);
                let right = if hovered {
                    icon::draw(
                        p,
                        icon::PEN,
                        (pen.x + 12.0, pen.y + 12.0),
                        13.0,
                        if on_pen { t.text } else { t.text_faint },
                    );
                    if on_pen && ui.clicked(pen) {
                        edit = Some(agent.clone());
                    }
                    30.0
                } else {
                    let age = latest.map(|s| ago(now, s.updated)).unwrap_or_default();
                    let age = p.layout(&age, theme::TINY, None);
                    p.text(&age, row.right() - 10.0 - age.width(), row.y + 11.0, t.text_faint);
                    age.width() + 14.0
                };
                let mut name = p.layout(&agent.name, theme::LABEL, None);
                name.truncate(p.fonts, row.w - 62.0 - right);
                let bright = if selected { 1.0 } else { 0.4 + 0.6 * hover };
                p.text(&name, row.x + 52.0, row.y + 8.0, mix(t.text_muted, t.text, bright));
                // Under the name: what was said last, or what the agent is for.
                let about = latest.map(|s| s.preview.as_str()).filter(|preview| !preview.is_empty());
                let about = about.unwrap_or(if agent.tagline.is_empty() { "No sessions yet" } else { &agent.tagline });
                let mut about = p.layout(about, theme::TINY, None);
                about.truncate(p.fonts, row.w - 62.0);
                p.text(&about, row.x + 52.0, row.y + 28.0, t.text_faint);
                if hovered {
                    ui.cursor = CursorIcon::Pointer;
                    if !on_pen && ui.clicked(row) {
                        select = Some(agent.id.clone());
                    }
                }
            }
            if !selected {
                continue;
            }
            // The selected agent's sessions hang under it, on a thin rail.
            let rail_top = y;
            let fresh_row = Rect::new(list.x + INSET + 28.0, y, width - 28.0, SESSION_H - 2.0);
            y += SESSION_H;
            if fresh_row.bottom() >= list.y && fresh_row.y <= list.bottom() {
                let hovered = ui.hovered(fresh_row) && list.contains(ui.mouse);
                let current = self.current.is_none();
                let hover = ui.anim(id("new-session"), f32::from(u8::from(hovered)));
                p.rect(fresh_row, if current { t.hover } else { fade(t.hover, hover) }, theme::RADIUS_SM);
                let color = mix(t.text_muted, t.text, if current { 1.0 } else { hover });
                icon::draw(p, icon::PLUS, (fresh_row.x + 15.0, fresh_row.y + 14.0), 14.0, color);
                p.label("New session", theme::SMALL, fresh_row.x + 30.0, fresh_row.y + 5.0, color);
                if hovered {
                    ui.cursor = CursorIcon::Pointer;
                    fresh |= ui.clicked(fresh_row);
                }
            }
            // A dragged session goes where the mouse is level with: the others make room as it
            // passes. With the mouse off to the side (or out of the window) it stays where it is.
            if dragged.is_some() && (list.x..list.right()).contains(&ui.mouse.0) {
                let end = sessions.clone().count().saturating_sub(1);
                place = Some((((ui.mouse.1 - y) / SESSION_H).max(0.0) as usize).min(end));
            }
            for session in sessions {
                let row = Rect::new(list.x + INSET + 28.0, y, width - 28.0, SESSION_H - 2.0);
                y += SESSION_H;
                if row.bottom() < list.y || row.y > list.bottom() {
                    continue;
                }
                let carried = dragged.as_deref() == Some(session.id.as_str());
                // While one is dragged, the others do not answer the mouse.
                let hovered = ui.hovered(row) && list.contains(ui.mouse) && (carried || dragged.is_none());
                let hover = ui.anim(id(("session", &session.id)), f32::from(u8::from(hovered)));
                let current = self.current.as_deref() == Some(session.id.as_str());
                let fill = if carried {
                    t.active
                } else if current {
                    t.hover
                } else {
                    fade(t.hover, hover)
                };
                p.rect(row, fill, theme::RADIUS_SM);
                let confirming = self.confirm_delete.as_deref() == Some(session.id.as_str());
                let thread = self.threads.get(&session.id);
                let working = thread.is_some_and(|thread| thread.busy);
                // Something runs in the background between its turns: a still dot, as that may take an hour.
                let waiting = thread.is_some_and(|thread| !thread.background.is_empty());
                // Right: a delete button while hovered, a pulse while it works, a dot while it waits.
                let mut on_control = false;
                let right = if (hovered && !carried) || confirming {
                    let bin = Rect::new(row.right() - 26.0, row.y + 3.0, 22.0, 22.0);
                    on_control = bin.contains(ui.mouse);
                    let color = if confirming {
                        t.danger
                    } else if on_control {
                        t.text
                    } else {
                        t.text_faint
                    };
                    icon::draw(p, icon::TRASH, (bin.x + 11.0, bin.y + 11.0), 13.0, color);
                    if on_control && ui.clicked(bin) {
                        delete = Some(session.id.clone());
                    }
                    30.0
                } else if working {
                    let pulse = 0.5 + 0.5 * (ui.time * 4.0).sin();
                    let dot = Rect::new(row.right() - 15.0, row.y + 11.0, 6.0, 6.0);
                    p.rect(dot, fade(t.accent, 0.4 + 0.6 * pulse), 3.0);
                    22.0
                } else if waiting {
                    p.rect(Rect::new(row.right() - 15.0, row.y + 11.0, 6.0, 6.0), fade(t.accent, 0.4), 3.0);
                    22.0
                } else {
                    10.0
                };
                // Armed, it says what goes: with a worktree, its uncommitted changes too.
                let warning = if confirming { self.delete_warning(&session.id) } else { String::new() };
                let title = if confirming {
                    warning.as_str()
                } else if session.title.is_empty() {
                    "New session"
                } else {
                    session.title.as_str()
                };
                let mut title = p.layout(title, theme::SMALL, None);
                title.truncate(p.fonts, row.w - 12.0 - right);
                let color =
                    if confirming { t.danger } else { mix(t.text_muted, t.text, if current { 1.0 } else { hover }) };
                p.text(&title, row.x + 10.0, row.y + (row.h - title.height()) * 0.5, color);
                if carried {
                    ui.cursor = CursorIcon::Grabbing;
                } else if hovered {
                    ui.cursor = CursorIcon::Pointer;
                    if !on_control && ui.pressed {
                        held = Some(session.id.clone());
                    }
                    if !on_control && ui.clicked(row) {
                        open = Some(session.id.clone());
                    }
                }
            }
            p.rect(Rect::new(list.x + INSET + 26.0, rail_top + 2.0, 1.0, y - rail_top - 8.0), t.border_strong, 0.0);
            y += 6.0;
        }
        let content = y + offset - list.y;
        self.sidebar_scroll.finish(ui, content, list.h);
        self.sidebar_scroll.bar(p, ui, list, content);
        if content > list.h {
            // The list fades out at its edges rather than being cut.
            if offset > 0.5 {
                p.gradient(Rect::new(list.x, list.y, list.w, 16.0), t.panel, fade(t.panel, 0.0), 0.0);
            }
            p.gradient(Rect::new(list.x, list.bottom() - 16.0, list.w, 16.0), fade(t.panel, 0.0), t.panel, 0.0);
        }
        p.set_clip(clip);

        // A press takes hold of a session; moved far enough it is dragged, and where it is let go
        // is its place from then on.
        if let Some(session) = held {
            self.drag = Some((session, false));
        }
        if let Some((session, moved)) = self.drag.take() {
            if let Some(place) = place {
                self.move_session(&session, place);
            }
            if ui.down {
                let moved = moved || (ui.mouse.1 - ui.press_pos.1).abs() > DRAG_SLACK;
                self.drag = Some((session, moved));
            } else if moved {
                self.keep_order(actions);
            }
        }

        // A click that is not on the armed delete button disarms it.
        if ui.released && delete.is_none() {
            self.confirm_delete = None;
        }
        if let Some(session) = delete {
            if self.confirm_delete.as_deref() == Some(session.as_str()) {
                self.confirm_delete = None;
                // Changes the armed button named go with it; ones it did not know of are kept.
                let told = self.worktrees.get(&session).is_some_and(|state| state.changes > 0);
                self.delete_session(&session, told, actions);
            } else {
                // What its worktree holds is read, for the button to say.
                self.ask_worktree(&session, actions);
                self.confirm_delete = Some(session);
            }
        } else if let Some(agent) = edit {
            self.modal = Some(Modal::Agent(Box::new(AgentForm::new(Some(&agent)))));
        } else if let Some(agent) = select {
            self.select_agent(&agent, actions);
        } else if let Some(session) = open {
            self.open_session(&session, actions);
        } else if fresh {
            self.new_session(actions);
        }
    }
}
