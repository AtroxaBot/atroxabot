//! A question the agent asks the user (Claude's `AskUserQuestion`): a card
//! over the message box with one question at a time, its options to pick
//! from, and a line for an answer of the user's own. Several questions are
//! steps of one card; the answers go back together.

use arboard::Clipboard;
use atroxabot_core::{Ask, Verdict};
use serde_json::{Map, Value, json};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::widgets::line_field;
use crate::editor::Editor;
use crate::paint::{Painter, Rect, fade, mix};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, edit_key, icon, id};

/// The tool whose calls are questions.
pub(super) const TOOL: &str = "AskUserQuestion";
/// Most questions of one card, and most options of one question, that are shown.
const MOST: usize = 8;
/// What the agent is told when the card is dismissed.
const DISMISSED: &str = "The user dismissed the question without answering it.";

/// One question.
struct Question {
    /// Its words, exactly as asked: the answers are filed under them.
    text: String,
    /// A word or two for its step.
    header: String,
    /// What can be picked: each a label and a line about it.
    options: Vec<(String, String)>,
    /// Whether several options may be picked.
    multi: bool,
}

/// The questions in a call's input (JSON text). Whatever is not shaped like
/// one is left out.
fn questions(input: &str) -> Vec<Question> {
    let input = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let list = |value: &Value, key: &str| value.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let question = |asked: &Value| Question {
        text: text(asked, "question"),
        header: text(asked, "header").trim().to_owned(),
        options: list(asked, "options")
            .iter()
            .take(MOST)
            .map(|option| (text(option, "label").trim().to_owned(), text(option, "description").trim().to_owned()))
            .filter(|(label, _)| !label.is_empty())
            .collect(),
        multi: asked.get("multiSelect").and_then(Value::as_bool) == Some(true),
    };
    list(&input, "questions").iter().take(MOST).map(question).filter(|asked| !asked.text.trim().is_empty()).collect()
}

/// The user's answers to the questions of one call, as far as given.
pub(super) struct QuestionForm {
    /// The approval (the call) these answer.
    pub ask: String,
    questions: Vec<Question>,
    /// The question shown.
    step: usize,
    /// Per question, which options are picked.
    picks: Vec<Vec<bool>>,
    /// Per question, an answer of the user's own.
    others: Vec<Editor>,
}

/// A radio button or a checkbox at `center`, `on` or not.
fn mark(p: &mut Painter, center: (f32, f32), multi: bool, on: bool) {
    let t = p.theme;
    let frame = Rect::new(center.0 - 8.0, center.1 - 8.0, 16.0, 16.0);
    let radius = if multi { 4.5 } else { 8.0 };
    if on && multi {
        p.rect(frame, t.accent, radius);
        icon::draw(p, icon::CHECK, center, 11.0, t.on_accent);
    } else {
        p.bordered(frame, fade(t.bg, 0.0), radius, 1.5, if on { t.accent } else { t.border_strong });
        if on {
            p.rect(Rect::new(center.0 - 4.0, center.1 - 4.0, 8.0, 8.0), t.accent, 4.0);
        }
    }
}

impl QuestionForm {
    /// The form for the questions `ask` carries; `None` if it carries none
    /// that can be shown.
    pub fn new(ask: &Ask) -> Option<Self> {
        let questions = questions(&ask.input);
        if questions.is_empty() {
            return None;
        }
        Some(Self {
            ask: ask.id.clone(),
            step: 0,
            picks: questions.iter().map(|question| vec![false; question.options.len()]).collect(),
            others: questions.iter().map(|_| Editor::default()).collect(),
            questions,
        })
    }

    /// The answer to question `index`: the labels picked and what was
    /// typed, joined by commas. `None` while there is neither.
    fn answer(&self, index: usize) -> Option<String> {
        let question = self.questions.get(index)?;
        let picked = question
            .options
            .iter()
            .zip(self.picks.get(index)?)
            .filter(|(_, on)| **on)
            .map(|((label, _), _)| label.as_str());
        let own = self.others.get(index).map(|other| other.text().trim()).filter(|own| !own.is_empty());
        let parts: Vec<&str> = picked.chain(own).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// Whether every question has an answer.
    fn complete(&self) -> bool {
        (0..self.questions.len()).all(|index| self.answer(index).is_some())
    }

    /// Goes to the first question without an answer after the one shown.
    fn next_open(&mut self) {
        let count = self.questions.len();
        let open = (1..=count).map(|ahead| (self.step + ahead) % count).find(|index| self.answer(*index).is_none());
        self.step = open.unwrap_or(self.step);
    }

    /// Picks (or, where several may be, toggles) option `option` of the
    /// question shown. A single pick moves on to the next open question.
    fn pick(&mut self, option: usize) {
        let multi = self.questions.get(self.step).is_some_and(|question| question.multi);
        let Some(picks) = self.picks.get_mut(self.step) else { return };
        let Some(on) = picks.get(option).copied() else { return };
        if multi {
            picks[option] = !on;
        } else {
            picks.fill(false);
            picks[option] = true;
            // One answer, not two: the user's own words give way.
            if let Some(other) = self.others.get_mut(self.step) {
                *other = Editor::default();
            }
            self.next_open();
        }
    }

    /// The answers as the tool's new input: `input` with them filed under
    /// each question's words.
    fn verdict(&self, input: &str) -> Verdict {
        let mut updated =
            serde_json::from_str::<Value>(input).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
        let answers: Map<String, Value> = (0..self.questions.len())
            .filter_map(|index| Some((self.questions[index].text.clone(), Value::String(self.answer(index)?))))
            .collect();
        updated["answers"] = Value::Object(answers);
        Verdict::Answer(updated.to_string())
    }

    /// Enter, or the button: on to the next open question, or, with every
    /// one answered, the answers to send.
    fn advance(&mut self, input: &str) -> Option<Verdict> {
        self.answer(self.step)?;
        if self.complete() {
            return Some(self.verdict(input));
        }
        self.next_open();
        None
    }

    /// Text committed by an input method: an answer of the user's own.
    pub fn insert(&mut self, text: &str) {
        if let Some(other) = self.others.get_mut(self.step) {
            other.insert(text);
        }
        self.typed();
    }

    /// The user typed an answer of their own: where one option is picked, it gives way.
    fn typed(&mut self) {
        let single = self.questions.get(self.step).is_some_and(|question| !question.multi);
        let own = self.others.get(self.step).is_some_and(|other| !other.text().trim().is_empty());
        if single
            && own
            && let Some(picks) = self.picks.get_mut(self.step)
        {
            picks.fill(false);
        }
    }

    /// A key: a digit picks an option, Enter goes on (or sends), Tab moves
    /// between the questions, Esc dismisses, and anything else writes an
    /// answer of the user's own. Returns the answer to give the agent, once
    /// there is one. `input` is the call's input.
    pub fn key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<Clipboard>,
        input: &str,
    ) -> Option<Verdict> {
        let count = self.questions.len();
        let writing = self.others.get(self.step).is_some_and(|other| !other.text().is_empty());
        let options = self.questions.get(self.step).map_or(0, |question| question.options.len());
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => return Some(Verdict::Deny(DISMISSED.to_owned())),
            Key::Named(NamedKey::Enter) => return self.advance(input),
            Key::Named(NamedKey::Tab) => {
                self.step = (self.step + if mods.shift_key() { count - 1 } else { 1 }) % count;
            }
            Key::Character(c)
                if !writing
                    && c.len() == 1
                    && matches!(c.parse::<usize>(), Ok(digit) if (1..=options).contains(&digit)) =>
            {
                self.pick(c.parse::<usize>().unwrap_or(1) - 1);
            }
            _ => {
                if let Some(other) = self.others.get_mut(self.step) {
                    edit_key(other, event, mods, clipboard);
                }
                self.typed();
            }
        }
        None
    }

    /// Draws the card ending at `bottom`, `width` wide from `x`. Returns
    /// its top edge and, once the user answered or dismissed it, what to
    /// tell the agent. `input` is the call's input, `agent` who asks.
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, bottom, width): (f32, f32, f32),
        input: &str,
        agent: &str,
    ) -> (f32, Option<Verdict>) {
        let t = p.theme;
        self.step = self.step.min(self.questions.len().saturating_sub(1));
        let step = self.step;
        let Some(question) = self.questions.get(step) else { return (bottom, None) };
        let asked = p.layout(&question.text, theme::BODY, Some(width - 40.0));
        let row_h = |description: &str| if description.is_empty() { 38.0 } else { 50.0 };
        let options_h: f32 = question.options.iter().map(|(_, description)| row_h(description) + 5.0).sum();
        let height = 50.0 + asked.height() + 14.0 + options_h + 42.0 + 60.0;
        let card = Rect::new(x, bottom - height, width, height);
        let appear = ui.appear(id(("question", &self.ask)));
        p.shadow(
            Rect::new(card.x + 8.0, card.y + 8.0, card.w - 16.0, card.h),
            fade(t.shadow, 0.7),
            theme::RADIUS_LG,
            22.0,
        );
        p.bordered(card, t.surface, theme::RADIUS_LG, 1.0, mix(t.border_strong, t.border_focus, appear));

        icon::draw(p, icon::MESSAGE, (card.x + 25.0, card.y + 25.0), 16.0, t.text);
        p.label(&format!("{agent} asks"), theme::LABEL, card.x + 43.0, card.y + 16.0, t.text);
        // Several questions: a step each, at the right of the head.
        let mut jump = None;
        let mut right = card.right() - 16.0;
        for index in (0..self.questions.len()).rev().filter(|_| self.questions.len() > 1) {
            let header = &self.questions[index].header;
            let name = if header.is_empty() { (index + 1).to_string() } else { header.clone() };
            let done = self.answer(index).is_some();
            let chip_w = p.layout(&name, theme::TINY, None).width() + if done { 34.0 } else { 20.0 };
            right -= chip_w;
            let chip = Rect::new(right, card.y + 13.0, chip_w, 24.0);
            let current = index == step;
            let hovered = ui.hovered(chip);
            let fill = if current { t.hover } else { fade(t.hover, f32::from(u8::from(hovered)) * 0.6) };
            p.bordered(chip, fill, 12.0, 1.0, if current { t.border_focus } else { t.border });
            if done {
                icon::draw(p, icon::CHECK, (chip.x + 14.0, chip.y + 12.0), 11.0, t.text_muted);
            }
            let ink = if current { t.text } else { t.text_muted };
            p.label(&name, theme::TINY, chip.x + if done { 23.0 } else { 10.0 }, chip.y + 5.0, ink);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(chip) {
                    jump = Some(index);
                }
            }
            right -= 6.0;
        }

        p.text(&asked, card.x + 20.0, card.y + 48.0, t.text);
        let mut y = card.y + 48.0 + asked.height() + 14.0;
        let mut picked = None;
        for (index, (label, description)) in question.options.iter().enumerate() {
            let row = Rect::new(card.x + 12.0, y, width - 24.0, row_h(description));
            let on = self.picks.get(step).and_then(|picks| picks.get(index)).copied() == Some(true);
            let hovered = ui.hovered(row);
            let hover = ui.anim(id(("answer", &self.ask, step, index)), f32::from(u8::from(hovered)));
            let chosen = ui.anim(id(("answered", &self.ask, step, index)), f32::from(u8::from(on)));
            let border = mix(mix(t.border, t.border_strong, hover), t.border_focus, chosen);
            p.bordered(row, mix(t.bg, t.hover, (hover * 0.6).max(chosen * 0.8)), theme::RADIUS, 1.0, border);
            mark(p, (row.x + 22.0, row.y + if description.is_empty() { 19.0 } else { 18.0 }), question.multi, on);
            let room = row.w - 80.0;
            let mut name = p.layout(label, theme::LABEL, None);
            name.truncate(p.fonts, room);
            if description.is_empty() {
                p.text(&name, row.x + 44.0, row.y + (row.h - name.height()) * 0.5, t.text);
            } else {
                p.text(&name, row.x + 44.0, row.y + 8.0, t.text);
                let mut about = p.layout(description, theme::TINY, None);
                about.truncate(p.fonts, room);
                p.text(&about, row.x + 44.0, row.y + 28.0, t.text_faint);
            }
            let number = p.layout(&(index + 1).to_string(), theme::TINY, None);
            p.text(&number, row.right() - 16.0 - number.width(), row.y + (row.h - number.height()) * 0.5, t.text_faint);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    picked = Some(index);
                }
            }
            y += row.h + 5.0;
        }
        // An answer of the user's own: typing anything writes here.
        let other = Rect::new(card.x + 12.0, y, width - 24.0, 38.0);
        let own = self.others.get(step).is_some_and(|other| !other.text().trim().is_empty());
        p.bordered(other, t.bg, theme::RADIUS, 1.0, if own { t.border_focus } else { t.border });
        mark(p, (other.x + 22.0, other.y + 19.0), question.multi, own);
        if let Some(editor) = self.others.get_mut(step) {
            let field = Rect::new(other.x + 44.0, other.y, other.w - 56.0, other.h);
            line_field(p, ui, field, editor, ("Something else: just type", theme::SMALL), true);
        }

        let foot = card.bottom() - 46.0;
        let keys = if question.options.len() > 1 {
            "1 to pick, ↵ to go on, esc to dismiss"
        } else {
            "↵ to go on, esc to dismiss"
        };
        p.label(keys, theme::TINY, card.x + 20.0, foot + 9.0, t.text_faint);
        let answered = self.answer(step).is_some();
        let sends = answered && self.complete();
        let go = Rect::new(card.right() - 16.0 - 124.0, foot, 124.0, 32.0);
        let mut verdict = None;
        if button(
            p,
            ui,
            go,
            if sends || self.questions.len() == 1 { "Send answer  ↵" } else { "Next  ↵" },
            ButtonStyle::Primary,
            answered,
        ) {
            verdict = self.advance(input);
        }
        if button(p, ui, Rect::new(go.x - 92.0, foot, 84.0, 32.0), "Dismiss", ButtonStyle::Ghost, true) {
            verdict = Some(Verdict::Deny(DISMISSED.to_owned()));
        }
        if let Some(index) = jump {
            self.step = index;
        }
        if let Some(option) = picked {
            self.pick(option);
        }
        (card.y - 6.0, verdict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(input: &Value) -> Ask {
        Ask { id: "q".into(), tool: TOOL.into(), input: input.to_string(), ..Ask::default() }
    }

    #[test]
    fn answers_go_back_under_each_question() {
        let input = json!({ "questions": [
            { "question": "How should I format it?", "header": "Format", "multiSelect": false,
              "options": [{ "label": "Summary", "description": "Brief" }, { "label": "Detailed", "description": "" }] },
            { "question": "Which sections?", "header": "Sections", "multiSelect": true,
              "options": [{ "label": "Intro" }, { "label": "End" }, { "nolabel": 1 }] },
        ]});
        let mut form = QuestionForm::new(&ask(&input)).expect("two questions");
        assert!(form.advance(&input.to_string()).is_none(), "nothing answered yet");
        form.pick(1);
        assert_eq!((form.answer(0).as_deref(), form.step), (Some("Detailed"), 1), "one pick, and on to the next");
        form.pick(0);
        form.pick(1);
        form.pick(0);
        assert_eq!(form.answer(1).as_deref(), Some("End"), "several can be picked, and unpicked");
        form.insert("and an appendix");
        assert_eq!(form.answer(1).as_deref(), Some("End, and an appendix"));
        let Some(Verdict::Answer(sent)) = form.advance(&input.to_string()) else {
            panic!("every question is answered")
        };
        let sent: Value = serde_json::from_str(&sent).expect("JSON");
        assert_eq!(
            sent["answers"],
            json!({ "How should I format it?": "Detailed", "Which sections?": "End, and an appendix" })
        );
        assert_eq!(sent["questions"], input["questions"], "the questions go back as they came");

        // Words of the user's own replace a single pick, and a pick replaces them.
        form.step = 0;
        form.insert("as a table");
        assert_eq!(form.answer(0).as_deref(), Some("as a table"));
        form.pick(0);
        assert_eq!(form.answer(0).as_deref(), Some("Summary"));
    }

    #[test]
    fn what_is_not_a_question_is_not_shown() {
        for input in [json!(null), json!({ "questions": "many" }), json!({ "questions": [{ "question": "  " }, 7] })] {
            assert!(QuestionForm::new(&ask(&input)).is_none(), "{input}");
        }
        let bare = json!({ "questions": [{ "question": "Name?" }] });
        let mut form = QuestionForm::new(&ask(&bare)).expect("a question without options is still one");
        form.insert("Ada");
        assert!(matches!(form.advance("not json"), Some(Verdict::Answer(sent)) if sent.contains("\"Name?\":\"Ada\"")));
    }
}
