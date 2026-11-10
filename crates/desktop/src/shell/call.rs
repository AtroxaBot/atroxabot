//! A voice call with the selected agent: its face fills the canvas, moving
//! with whoever speaks, over what was just heard and said. The conversation
//! is the same session as when typing, so a call can be read back later.

use atroxabot_core::{Block, Entry};
use atroxabot_voice::{CallOptions, CallState, Provider, VoiceConfig};
use winit::window::CursorIcon;

use super::Shell;
use crate::app::Action;
use crate::orb::{self, Mood};
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::Align;
use crate::theme;
use crate::ui::{Ui, icon, id};

/// A call in progress.
pub(super) struct CallView {
    /// The session the call is part of; empty until the first words start one.
    pub session: String,
    /// What the call is doing.
    pub state: CallState,
    /// How loud the microphone is, `0..=1`.
    pub input: f32,
    /// How loud the agent's voice is, `0..=1`.
    pub output: f32,
    /// What the user said last.
    pub heard: String,
    /// Whether the agent was told that a call began.
    pub announced: bool,
    /// The reply in progress was interrupted: what is left of it is not spoken.
    pub hold: bool,
    pub muted: bool,
}

impl Shell {
    /// Starts a call with the selected agent, if a voice provider is set up.
    pub(super) fn start_call(&mut self, actions: &mut Vec<Action>) {
        let Some(agent) = self.selected_agent() else { return };
        let voice = &self.config.voice;
        let chosen =
            if agent.voice.provider.is_empty() { voice.provider.as_str() } else { agent.voice.provider.as_str() };
        let provider = Provider::from_key(chosen).unwrap_or_default();
        let key = match provider {
            Provider::Fish => voice.fish_key.clone(),
            Provider::ElevenLabs => voice.elevenlabs_key.clone(),
        };
        let Some(api_key) = key else {
            self.notify(format!("Add your {} API key in Settings to call {}.", provider.label(), agent.name));
            return;
        };
        let config = VoiceConfig {
            provider,
            api_key,
            voice_id: agent.voice.voice_id.clone(),
            language: voice.language.clone(),
            speed: agent.voice.speed,
            ..VoiceConfig::default()
        };
        let device = |name: &str| Some(name.to_owned()).filter(|name| !name.is_empty());
        let options = CallOptions {
            input_device: device(&voice.input_device),
            output_device: device(&voice.output_device),
            ..CallOptions::default()
        };
        self.menu = None;
        self.call = Some(CallView {
            session: self.current.clone().unwrap_or_default(),
            state: CallState::Listening,
            input: 0.0,
            output: 0.0,
            heard: String::new(),
            announced: false,
            hold: false,
            muted: false,
        });
        actions.push(Action::StartCall { config: Box::new(config), options: Box::new(options) });
    }

    /// Hangs up.
    pub(super) fn end_call(&mut self, actions: &mut Vec<Action>) {
        if self.call.take().is_some() {
            actions.push(Action::EndCall);
        }
    }

    /// Mutes or unmutes the microphone.
    pub(super) fn toggle_mute(&mut self, actions: &mut Vec<Action>) {
        if let Some(call) = &mut self.call {
            call.muted = !call.muted;
            actions.push(Action::Mute(call.muted));
        }
    }

    /// The last thing the agent said in the call's session, for the caption.
    fn spoken(&self) -> Option<&str> {
        let call = self.call.as_ref()?;
        let thread = self.threads.get(&call.session)?;
        thread.session.entries.iter().rev().take_while(|entry| !matches!(entry, Entry::User { .. })).find_map(|entry| {
            let Entry::Reply(reply) = entry else { return None };
            reply.blocks.iter().rev().find_map(|block| match block {
                Block::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
        })
    }

    /// Draws the call over the canvas.
    pub(super) fn draw_call(&mut self, p: &mut Painter, ui: &mut Ui, canvas: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let (Some(agent), Some(call)) = (self.selected_agent(), &self.call) else { return };
        let color = Self::color_of(agent);
        let working = self.threads.get(&call.session).is_some_and(|thread| thread.busy);
        let (mood, saying) = match call.state {
            _ if call.muted => (Mood::Idle, "Muted"),
            CallState::Hearing => (Mood::Hearing(call.input), "Listening"),
            CallState::Speaking => (Mood::Speaking(call.output), "Speaking"),
            CallState::Transcribing | CallState::Thinking => (Mood::Thinking, "Thinking"),
            // Tools take a while: the face keeps thinking until words come.
            CallState::Listening if working => (Mood::Thinking, "Working"),
            CallState::Listening => (Mood::Listening, "Listening"),
        };
        ui.animating = true;

        // Its picture, blurred to a wash, and its colour run down from the top of the canvas.
        if !agent.portrait.is_empty() && !t.is_light() {
            p.backdrop(
                &agent.portrait,
                Rect::new(canvas.x, canvas.y, canvas.w, canvas.h * 0.7),
                theme::CANVAS_RADIUS,
                0.22,
            );
            p.gradient(
                Rect::new(canvas.x, canvas.y + canvas.h * 0.2, canvas.w, canvas.h * 0.5 + 1.0),
                fade(t.bg, 0.0),
                t.bg,
                0.0,
            );
        }
        let wash = if t.is_light() { 0.10 } else { 0.16 };
        p.gradient(
            Rect::new(canvas.x, canvas.y, canvas.w, canvas.h * 0.7),
            fade(color, wash),
            fade(color, 0.0),
            theme::CANVAS_RADIUS,
        );

        let center = (canvas.x + canvas.w * 0.5, canvas.y + canvas.h * 0.40);
        let size = (canvas.h * 0.26).clamp(96.0, 190.0);
        orb::reactor(p, &agent.portrait, center, size, color, mood, ui.time);

        let mut y = center.1 + size * 0.95;
        let name = p.layout(&agent.name, theme::TITLE, None);
        p.text(&name, center.0 - name.width() * 0.5, y, t.text);
        y += name.height() + 4.0;
        let state = p.layout(saying, theme::SMALL, None);
        p.text(&state, center.0 - state.width() * 0.5, y, mix(t.text_faint, color, 0.6));
        y += state.height() + 22.0;

        // Captions: what was heard, then the tail of what is being said.
        let caption_w = (canvas.w - 96.0).min(620.0);
        let left = center.0 - caption_w * 0.5;
        if !call.heard.is_empty() {
            let heard = p.layout(&call.heard, theme::SMALL, Some(caption_w));
            p.text_aligned(&heard, left, y, Align::Center, caption_w, t.text_faint);
            y += heard.height() + 10.0;
        }
        if let Some(said) = self.spoken() {
            // The newest words matter: the last few lines, clear of the buttons.
            let said = caption(said);
            let said = p.layout(&said, theme::BODY, Some(caption_w));
            let room = (canvas.bottom() - 104.0 - y).max(0.0);
            let clip = p.push_clip(Rect::new(left, y, caption_w, room));
            p.text_aligned(&said, left, y - (said.height() - room).max(0.0), Align::Center, caption_w, t.text);
            p.set_clip(clip);
        }

        // Mute and hang up, at the foot.
        let muted = call.muted;
        let by = canvas.bottom() - 84.0;
        let mute = Rect::new(center.0 - 62.0, by, 52.0, 52.0);
        let hang = Rect::new(center.0 + 10.0, by, 52.0, 52.0);
        let round = |p: &mut Painter,
                     ui: &mut Ui,
                     rect: Rect,
                     key: &str,
                     glyph: char,
                     (fill, ink): (crate::paint::Color, crate::paint::Color)| {
            let hovered = ui.hovered(rect);
            let hover = ui.anim(id(("call", key)), f32::from(u8::from(hovered)));
            p.rect(rect, fade(fill, 1.0 - 0.14 * hover), 26.0);
            icon::draw(p, glyph, (rect.x + 26.0, rect.y + 26.0), 20.0, ink);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
            }
            ui.clicked(rect)
        };
        let mute_look = if muted { (t.accent, t.on_accent) } else { (t.surface, t.text) };
        if round(p, ui, mute, "mute", if muted { icon::MIC_OFF } else { icon::MIC }, mute_look) {
            self.toggle_mute(actions);
        }
        if round(p, ui, hang, "hang", icon::PHONE_OFF, (t.danger, crate::paint::hex(0xFFFFFF))) {
            self.end_call(actions);
        }
        let hint = p.layout("M mutes · Esc hangs up", theme::TINY, None);
        p.text(&hint, center.0 - hint.width() * 0.5, canvas.bottom() - 24.0, t.text_faint);
    }
}

/// The tail of what the agent said, as a caption: without code or Markdown
/// marks, which are not spoken either.
fn caption(said: &str) -> String {
    let mut out = String::new();
    let mut code = false;
    for line in said.lines() {
        if line.trim_start().starts_with("```") {
            code = !code;
        } else if !code && !line.trim().is_empty() {
            out.push_str(line.trim());
            out.push(' ');
        }
    }
    let out: String = out.chars().filter(|c| !matches!(c, '*' | '`' | '#')).collect();
    let out = out.trim();
    let start = out.char_indices().rev().nth(239).map_or(0, |(at, _)| at);
    // From the start of a word, not the middle of one.
    let tail = &out[start..];
    if start == 0 { tail.to_owned() } else { tail.split_once(' ').map_or(tail, |(_, rest)| rest).to_owned() }
}

#[cfg(test)]
mod tests {
    use super::caption;

    #[test]
    fn captions_drop_code_and_marks_and_keep_the_tail() {
        assert_eq!(
            caption("**Found it.**\n\n```rust\nlet x = 1;\n```\nThe `test` passes."),
            "Found it. The test passes."
        );
        let long = "word ".repeat(100);
        let tail = caption(&long);
        assert!(tail.len() <= 240 && tail.starts_with("word") && tail.ends_with("word"));
        assert_eq!(caption(""), "");
    }
}
