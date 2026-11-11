//! The agent editor, a modal: who the agent is (its face, name and
//! instructions), the provider it starts with, how much it may do alone,
//! whether it keeps notes, and how it sounds. Its model and effort are chosen
//! under the message box, not here.

use arboard::Clipboard;
use atroxabot_core::Connection;
use atroxabot_core::{Agent, AgentVoice, Autonomy, ConnectionKind};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::Shell;
use super::menu::{Menu, autonomy_glyph};
use super::widgets::{Scroll, dropdown, dropdown_frame, field_label, modal, toggle};
use crate::app::Action;
use crate::color::{ColorPicker, Pick};
use crate::editor::Editor;
use crate::form::Fields;
use crate::orb::{self, Mood};
use crate::paint::{Painter, Rect, fade, hex};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, icon, icon_button, id};

/// The modal's size.
const SIZE: (f32, f32) = (660.0, 660.0);
/// The form's text fields, by index.
const NAME: usize = 0;
const TAGLINE: usize = 1;
const INSTRUCTIONS: usize = 2;
const VOICE: usize = 3;

/// The voice providers on offer: what is stored, and what the list says.
pub(super) const VOICES: [(&str, &str); 3] =
    [("", "The one in Settings"), ("fish", "Fish Audio"), ("elevenlabs", "ElevenLabs")];

/// How the editor was left.
pub(super) enum Done {
    /// With this agent to save.
    Saved(Box<Agent>),
    /// The agent with this id is to be deleted.
    Deleted(String),
    /// Nothing changes.
    Cancelled,
}

/// An agent being created or edited.
pub(super) struct AgentForm {
    /// The agent as it was; a new one when creating.
    agent: Agent,
    /// Whether the agent exists already.
    editing: bool,
    fields: Fields,
    color: String,
    /// The picture's file, empty for the drawn face.
    portrait: String,
    picker: Option<ColorPicker>,
    /// The provider it thinks through (a connection's id).
    pub connection: String,
    /// How much it may do alone.
    pub autonomy: Autonomy,
    memory: bool,
    /// Its voice provider; empty for the one in the settings.
    pub voice: String,
    /// Delete was clicked once: again deletes.
    confirm: bool,
    /// Ctrl/Cmd+S was pressed: save on the next frame.
    submit: bool,
    scroll: Scroll,
}

impl AgentForm {
    /// The editor for `agent`, or for a new one.
    pub fn new(agent: Option<&Agent>) -> Self {
        let editing = agent.is_some();
        let agent = agent.cloned().unwrap_or_else(|| Agent::new(""));
        let field = |text: &str| {
            let mut editor = Editor::default();
            editor.insert(text);
            editor
        };
        let fields = Fields::new(
            [field(&agent.name), field(&agent.tagline), field(&agent.instructions), field(&agent.voice.voice_id)],
            [false, false, true, false],
        );
        Self {
            color: agent.color.clone(),
            portrait: agent.portrait.clone(),
            connection: agent.connection.clone(),
            autonomy: agent.autonomy,
            memory: agent.memory,
            voice: agent.voice.provider.clone(),
            agent,
            editing,
            fields,
            picker: None,
            confirm: false,
            submit: false,
            scroll: Scroll::default(),
        }
    }

    /// The picture picker answered.
    pub fn set_picture(&mut self, picture: &str) {
        picture.clone_into(&mut self.portrait);
    }

    /// The agent the editor describes now.
    fn result(&self) -> Agent {
        let text = |index: usize| self.fields.text(index).trim().to_owned();
        // Another provider has other models: its default, until one is picked under the message box.
        let same = self.connection == self.agent.connection;
        Agent {
            name: text(NAME),
            tagline: text(TAGLINE),
            instructions: text(INSTRUCTIONS),
            model: if same { self.agent.model.clone() } else { String::new() },
            effort: if same { self.agent.effort.clone() } else { String::new() },
            color: self.color.clone(),
            portrait: self.portrait.clone(),
            connection: self.connection.clone(),
            autonomy: self.autonomy,
            memory: self.memory,
            voice: AgentVoice { provider: self.voice.clone(), voice_id: text(VOICE), speed: self.agent.voice.speed },
            ..self.agent.clone()
        }
    }

    /// A key: Esc leaves (returns `true`), Ctrl/Cmd+S saves, the rest edits.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<Clipboard>) -> bool {
        if let Some(picker) = &mut self.picker {
            match picker.key(event, mods, clipboard) {
                Pick::Set(color) => self.color = color,
                Pick::Close => self.picker = None,
                Pick::Stay => {}
            }
            return false;
        }
        let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => return true,
            Key::Character(c) if primary && c.eq_ignore_ascii_case("s") => self.submit = true,
            _ => {
                self.fields.key(event, mods, clipboard);
            }
        }
        false
    }

    /// Text committed by an input method.
    pub fn commit(&mut self, text: &str) {
        match &mut self.picker {
            Some(picker) => {
                if let Pick::Set(color) = picker.insert(text) {
                    self.color = color;
                }
            }
            None => self.fields.insert(text),
        }
    }

    /// Where the caret is, for the input method's window.
    pub fn caret(&self) -> Option<Rect> {
        self.fields.caret()
    }

    /// The agent's face, large: a click picks a picture for it, the dot at
    /// its foot its colour, and the cross (when it wears a picture) takes
    /// the picture off again. Returns the colour dot, which the picker hangs from.
    fn draw_face(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        center: (f32, f32),
        name: &str,
        actions: &mut Vec<Action>,
    ) -> Rect {
        let t = p.theme;
        let size = 76.0;
        let color = theme::agent_color(name, &self.color);
        let face = Rect::new(center.0 - size * 0.5, center.1 - size * 0.5, size, size);
        orb::agent(p, &self.portrait, center, size, color, Mood::Idle, ui.time);
        let dot = Rect::new(face.right() - 22.0, face.bottom() - 22.0, 24.0, 24.0);
        let cross = Rect::new(face.right() - 20.0, face.y - 4.0, 22.0, 22.0);
        let has_picture = !self.portrait.is_empty();
        let on_controls = dot.contains(ui.mouse) || (has_picture && cross.contains(ui.mouse));
        let hovered = ui.hovered(face) && !on_controls && self.picker.is_none();
        let hover = ui.anim(id("agent-face"), f32::from(u8::from(hovered)));
        // Hovering says what a click does: a picture.
        p.rect(face, fade(hex(0x000000), 0.45 * hover), size * 0.5);
        icon::draw(p, icon::IMAGE, center, 22.0, fade(hex(0xFFFFFF), hover));
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(face) {
                actions.push(Action::PickPicture);
            }
        }
        // Its colour, as a dot on the face's rim.
        p.rect(dot, t.bg, 12.0);
        p.bordered(Rect::new(dot.x + 3.0, dot.y + 3.0, 18.0, 18.0), color, 9.0, 1.0, t.border_strong);
        if ui.hovered(dot) {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(dot) {
                let auto = theme::agent_color(name, "");
                self.picker = if self.picker.is_some() { None } else { Some(ColorPicker::new(&self.color, auto)) };
            }
        }
        if has_picture {
            p.bordered(cross, t.surface, 11.0, 1.0, t.border_strong);
            icon::draw(p, icon::X, (cross.x + 11.0, cross.y + 11.0), 12.0, t.text_muted);
            if ui.hovered(cross) {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(cross) {
                    self.portrait.clear();
                }
            }
        }
        dot
    }

    /// Draws the editor over `view`. Returns how it was left, if it was.
    #[expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        view: Rect,
        shell: &mut Shell,
        actions: &mut Vec<Action>,
    ) -> Option<Done> {
        let t = p.theme;
        let (card, outside) = modal(p, ui, view, SIZE, "agent");
        let mut done = None;
        if outside && self.picker.is_none() && shell.menu.is_none() {
            done = Some(Done::Cancelled);
        }
        let (x, w) = (card.x + 28.0, card.w - 56.0);

        // Head: what this is, and a way out.
        p.label(if self.editing { "Edit agent" } else { "New agent" }, theme::HEADING, x, card.y + 20.0, t.text);
        if icon_button(p, ui, Rect::new(card.right() - 44.0, card.y + 14.0, 30.0, 30.0), icon::X, true) {
            done = Some(Done::Cancelled);
        }
        let footer = Rect::new(card.x, card.bottom() - 66.0, card.w, 66.0);
        let body = Rect::new(card.x, card.y + 56.0, card.w, footer.y - card.y - 56.0);
        let interactive = self.picker.is_none() && ui.hovered(body);
        let offset = self.scroll.update(ui, interactive);
        let clip = p.push_clip(body);
        let mut y = body.y + 10.0 - offset;

        // Its face, with its name and what it is for beside it.
        let name = self.fields.text(NAME).trim().to_owned();
        let swatch = self.draw_face(p, ui, (x + 38.0, y + 46.0), &name, actions);
        let side = (x + 100.0, w - 100.0);
        let layout = self.fields.layout(p, NAME, side.1);
        self.fields.draw(NAME, p, ui, Rect::new(side.0, y, side.1, 42.0), layout, "Name", interactive);
        let layout = self.fields.layout(p, TAGLINE, side.1);
        let hint = "What it is for, in a line";
        self.fields.draw(TAGLINE, p, ui, Rect::new(side.0, y + 50.0, side.1, 42.0), layout, hint, interactive);
        y += 116.0;

        field_label(p, icon::FILE, "INSTRUCTIONS", x, y);
        let layout = self.fields.layout(p, INSTRUCTIONS, w);
        let h = (layout.height() + 22.0).max(112.0);
        let hint = "Who it is, what it does and how it should work.";
        self.fields.draw(INSTRUCTIONS, p, ui, Rect::new(x, y + 24.0, w, h), layout, hint, interactive);
        y += h + 46.0;

        // A new agent starts with a provider there is: the first, if its usual one is gone.
        if !self.editing
            && shell.config.connection(&self.connection).is_none()
            && let Some(first) = shell.config.all_connections().first()
        {
            self.connection.clone_from(&first.id);
        }
        // The provider it starts with, and how much it may do alone.
        let connection = shell.config.connection(&self.connection);
        let half = (w - 10.0) * 0.5;
        field_label(p, icon::PLUG, "PROVIDER", x, y);
        field_label(p, icon::SHIELD, "AUTONOMY", x + half + 10.0, y);
        let rect = Rect::new(x, y + 24.0, half, 40.0);
        let name = connection.as_ref().map_or("None", |c| c.name.as_str());
        if dropdown_frame(p, ui, rect, name, shell.menu_open(Menu::FormProvider), true) {
            shell.toggle_menu(Menu::FormProvider, rect);
        }
        match &connection {
            Some(connection) => shell.provider_mark(p, connection, (rect.x + 19.0, rect.y + 20.0), 16.0, t.text_muted),
            None => icon::draw(p, icon::ALERT, (rect.x + 19.0, rect.y + 20.0), 15.0, t.text_muted),
        }
        let rect = Rect::new(x + half + 10.0, y + 24.0, half, 40.0);
        let glyph = autonomy_glyph(self.autonomy);
        if dropdown(p, ui, rect, Some(glyph), self.autonomy.label(), shell.menu_open(Menu::FormAutonomy)) {
            shell.toggle_menu(Menu::FormAutonomy, rect);
        }
        y += 88.0;

        // Whether it keeps notes: a switch of its own.
        let memory = Rect::new(x, y, w, 58.0);
        p.bordered(memory, t.surface, theme::RADIUS, 1.0, t.border);
        icon::draw(p, icon::HISTORY, (memory.x + 24.0, memory.y + 29.0), 16.0, t.text_muted);
        p.label("Memory", theme::LABEL, memory.x + 46.0, memory.y + 11.0, t.text);
        let about = "Keeps notes between sessions, in a file of its own that it updates itself.";
        p.label(about, theme::TINY, memory.x + 46.0, memory.y + 31.0, t.text_faint);
        if toggle(p, ui, (memory.right() - 52.0, memory.y + 18.0), self.memory, "memory") {
            self.memory = !self.memory;
        }
        y += 58.0 + 24.0;

        // How it sounds on a call.
        field_label(p, icon::AUDIO, "VOICE", x, y);
        let voice = VOICES.iter().find(|(value, _)| *value == self.voice).map_or("", |(_, label)| *label);
        let rect = Rect::new(x, y + 24.0, half, 40.0);
        if dropdown(p, ui, rect, None, voice, shell.menu_open(Menu::FormVoice)) {
            shell.toggle_menu(Menu::FormVoice, rect);
        }
        let layout = self.fields.layout(p, VOICE, half);
        let field = Rect::new(x + half + 10.0, y + 24.0, half, 40.0);
        self.fields.draw(VOICE, p, ui, field, layout, "Voice id (its default if empty)", interactive);
        y += 64.0 + 28.0;

        self.scroll.finish(ui, y + offset - body.y, body.h);
        p.set_clip(clip);
        self.scroll.bar(p, ui, body, y + offset - body.y);

        // Foot: delete on the left, cancel and save on the right.
        p.rect(Rect::new(footer.x + 1.0, footer.y, footer.w - 2.0, 1.0), t.border, 0.0);
        let by = footer.y + 16.0;
        if self.editing {
            let label = if self.confirm { "Delete it and its sessions?" } else { "Delete" };
            let width = if self.confirm { 210.0 } else { 84.0 };
            if button(p, ui, Rect::new(x, by, width, 34.0), label, ButtonStyle::Danger, true) {
                if self.confirm {
                    done = Some(Done::Deleted(self.agent.id.clone()));
                }
                self.confirm = !self.confirm;
            }
        }
        if button(p, ui, Rect::new(x + w - 196.0, by, 90.0, 34.0), "Cancel", ButtonStyle::Ghost, true) {
            done = Some(Done::Cancelled);
        }
        let valid = !name.is_empty();
        let label = if self.editing { "Save" } else { "Create" };
        let save = button(p, ui, Rect::new(x + w - 98.0, by, 98.0, 34.0), label, ButtonStyle::Primary, valid);
        if (save || std::mem::take(&mut self.submit)) && valid {
            done = Some(Done::Saved(Box::new(self.result())));
        }

        // The colour picker floats over the form.
        if let Some(picker) = &mut self.picker {
            match picker.draw(p, ui, swatch, &self.color, theme::agent_color(name, "")) {
                Pick::Set(color) => self.color = color,
                Pick::Close => self.picker = None,
                Pick::Stay => {}
            }
        }
        done
    }
}

/// What `level` means with the provider `connection`: auto mode is Claude
/// Code's, so elsewhere it can only edit freely.
pub(super) fn autonomy_hint(level: Autonomy, connection: Option<&Connection>) -> &'static str {
    let claude = connection.is_some_and(|c| c.kind == ConnectionKind::ClaudeAgent);
    if level == Autonomy::Auto && !claude {
        "With a Claude account. Here it edits freely and asks for the rest."
    } else {
        level.hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_round_trips_an_agent() {
        let agent = Agent {
            tagline: "Finds bugs.".into(),
            instructions: "Be brief.".into(),
            workdir: "/work".into(),
            model: "opus".into(),
            effort: "high".into(),
            autonomy: Autonomy::Auto,
            portrait: "/pictures/scout.png".into(),
            voice: AgentVoice { provider: "fish".into(), voice_id: "v1".into(), speed: 1.1 },
            ..Agent::new("Scout")
        };
        let mut form = AgentForm::new(Some(&agent));
        assert_eq!(form.result(), agent, "untouched, it is the same agent: its folder, model and effort included");
        form.set_picture("");
        form.commit("!");
        let changed = form.result();
        assert_eq!((changed.model.as_str(), changed.name.as_str(), changed.id == agent.id), ("opus", "Scout!", true));
        assert!(changed.portrait.is_empty() && AgentForm::new(None).result().name.is_empty());
        // Another provider has other models: the agent starts on its default.
        form.connection = "elsewhere".into();
        let moved = form.result();
        assert_eq!((moved.model.as_str(), moved.effort.as_str()), ("", ""));
        assert!(autonomy_hint(Autonomy::Auto, None).starts_with("With a Claude account"));
        assert_eq!(autonomy_hint(Autonomy::Auto, Some(&Connection::claude_agent())), Autonomy::Auto.hint());
    }
}
