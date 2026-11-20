//! An open session and how its conversation is drawn: the user's messages
//! as bubbles (with what they sent along), the agent's answer as Markdown,
//! its reasoning folded away, and every tool call as a row that opens to
//! show what went in and out. A subagent is a card in the conversation and
//! a row in the side panel's list: either opens what it said and did, which
//! reads like a conversation of its own under a bar that leads back.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atroxabot_core::attach::media_type;
use atroxabot_core::stats::Tally;
use atroxabot_core::{AgentEvent, Ask, Block, Entry, Reply, Session, SessionSummary, Todo, TodoState, ToolUse, tools};
use serde_json::Value;
use winit::window::CursorIcon;

use super::change;
use super::elapsed;
use super::find::Find;
use super::limits::Resume;
use super::menu::folder_name;
use super::queue::Waiting;
use super::widgets::Scroll;
use crate::app::Action;
use crate::doc::{Doc, DocPos, INK_MUTED, INK_TEXT};
use crate::image::Lookup;
use crate::paint::{Color, Painter, Rect, fade, mix};
use crate::text::Style;
use crate::theme;
use crate::ui::{Tip, Ui, icon, id};

/// A file a run of replies changed: its path, the lines added to it and the lines removed from it.
type FileCount = (String, u32, u32);
/// The files a run of replies changed, after the weight of the calls they were gathered from.
type Gathered = (usize, Vec<FileCount>);

/// What a tool call's row shows, worked out once from its input rather than at every frame.
#[derive(Default)]
struct Row {
    /// The length of the input it was worked out from; `None` before it was.
    of: Option<usize>,
    /// The icon of its kind of tool.
    icon: char,
    /// What it was called on, in a line.
    about: String,
    /// The lines it added to a file and removed from it, when it is one that changes a file.
    changed: Option<(u32, u32)>,
}

/// A subagent's kind and what it was started for, read once from its call's input.
struct Brief {
    /// The length of the input they were read from.
    of: usize,
    kind: String,
    description: String,
}

impl Brief {
    /// What the subagent is listed as: what it was started for, or else its kind.
    fn label(&self) -> &str {
        if self.description.is_empty() { &self.kind } else { &self.description }
    }
}

/// Something worked out from a session's messages, after the count of their changes and the number
/// of entries it was worked out at.
type Summed<T> = Option<((u64, usize), T)>;

/// What is known of an entry of the conversation from when it was last drawn: enough to leave it
/// alone while it is out of view.
#[derive(Default)]
struct Seen {
    /// How tall it was; `None` before it was ever drawn.
    height: Option<f32>,
    /// The width and scale it was laid out at.
    fits: (u32, u32),
    /// How much it held and how it stood ([`weight`]): another now, and it is measured again.
    weight: usize,
    /// The blocks of it text can be marked in, in the order they are drawn.
    marks: Vec<(usize, usize)>,
}

/// How much `entry` holds, with whether it is being written (`live`) and whether it ends a run of
/// replies (`ends`): what its height hangs on besides the width, for an entry that is not the newest.
fn weight(entry: &Entry, live: bool, ends: bool) -> usize {
    let held = match entry {
        Entry::User { text, files, .. } => text.len() + files.len(),
        Entry::Reply(reply) => reply.blocks.len(),
        Entry::Summary { text } | Entry::Note { text } => text.len(),
    };
    held * 4 + usize::from(live) * 2 + usize::from(ends)
}

/// The words of `block`, when it is one of the answer's.
fn words(block: &Block) -> Option<&str> {
    if let Block::Text { text } = block { Some(text.as_str()) } else { None }
}

/// About how tall `entry` is, before it was ever measured: where the scroll bar is until it is.
fn guess(entry: &Entry) -> f32 {
    match entry {
        Entry::User { text, .. } => 70.0 + (text.len() / 90) as f32 * 24.0,
        Entry::Reply(reply) => 40.0 + reply.blocks.len() as f32 * 32.0,
        Entry::Summary { .. } | Entry::Note { .. } => 34.0,
    }
}

/// Widest the user's bubble gets, as a share of the column.
const BUBBLE_SHARE: f32 = 0.78;
/// Most files a run of replies changed that are listed under it by name.
const TOUCHED_SHOWN: usize = 8;
/// Most of a tool's output shown when its row is open, in characters.
const DETAIL_CHARS: usize = 6_000;
/// The style of tool details: small monospace.
const DETAIL: Style = Style::mono(12.5);

/// How long one frame may spend measuring what is out of view: at a new
/// width (the window is being resized), or for the first time (a long
/// conversation was just opened). The rest keeps what is known of it for
/// now and follows in the next frames.
const RELAYOUT_BUDGET: Duration = Duration::from_millis(2);

/// Layout keys of what a subagent said and did start here, far from any entry's index.
const SUB_KEYS: usize = 1 << 62;
/// The layout key, within a subagent, of what it was asked.
const BRIEF_KEY: usize = usize::MAX / 2;

/// Whether `call` started a subagent. Not every call with news of a task
/// did: a command or a monitor sent to the background has some too (saved
/// sessions still hold such news), and none of a subagent's own messages.
fn is_agent(call: &ToolUse) -> bool {
    // `agent` is an API agent's.
    matches!(call.name.as_str(), "Agent" | "Task" | "agent") || !call.sub.is_empty()
}

/// The subagent calls in `blocks`, those of subagents included, in order.
fn agents<'a>(blocks: &'a [Block], out: &mut Vec<&'a ToolUse>) {
    for block in blocks {
        if let Block::Tool(call) = block {
            if is_agent(call) {
                out.push(call);
            }
            agents(&call.sub, out);
        }
    }
}

/// The subagents of one turn (what the agent did between two of the user's
/// messages): the turn the `open` one is in, or the latest. Turns are gone
/// through from the latest back, so the latest costs only itself.
fn turn_agents<'a>(entries: &'a [Entry], open: Option<&str>) -> Vec<&'a ToolUse> {
    let (mut end, mut latest) = (entries.len(), None);
    loop {
        let before = &entries[..end];
        let start = before.iter().rposition(|entry| matches!(entry, Entry::User { .. })).map_or(0, |at| at + 1);
        let mut turn = Vec::new();
        for entry in &before[start..] {
            if let Entry::Reply(reply) = entry {
                agents(&reply.blocks, &mut turn);
            }
        }
        if open.is_none_or(|open| turn.iter().any(|call| call.id == open)) {
            return turn;
        }
        latest.get_or_insert(turn);
        if start == 0 {
            return latest.unwrap_or_default();
        }
        end = start - 1;
    }
}

/// Whether the subagent `call` started is still at work: its call has not
/// returned or, for one sent to the background (whose call returns at
/// once), its latest news is not of an end. The words are the ones
/// `atroxabot_core`'s Claude engine writes into `activity`.
fn at_work(call: &ToolUse) -> bool {
    let ended = ["Done", "Failed", "Stopped"].iter().any(|end| call.activity.starts_with(end));
    !ended && (call.output.is_none() || !call.activity.is_empty())
}

/// A subagent of the turn, as the side panel lists it.
pub(super) struct Subagent {
    /// The id of the call that started it.
    pub id: String,
    /// What it was started for.
    pub label: String,
    /// Whether it is still at work.
    pub running: bool,
    /// Whether it failed.
    pub failed: bool,
    /// Whether it is the one open in the conversation's place.
    pub open: bool,
}

/// A task the session's Claude Code runs in the background that is no
/// subagent (a command, a monitor), while it runs. It goes with the
/// process: nothing of it is saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Background {
    /// The id of the call that started it.
    pub id: String,
    /// What it was started for; empty when not said.
    pub what: String,
}

/// How a subagent is doing, as a small mark centred on `at`: a pulsing dot
/// while it works, a warning when it failed, its icon in `ink` otherwise.
pub(super) fn agent_mark(p: &mut Painter, ui: &Ui, at: (f32, f32), (running, failed): (bool, bool), ink: Color) {
    let t = p.theme;
    if running {
        let pulse = ui.motion().map_or(1.0, |time| 0.5 + 0.5 * (time * 4.0).sin());
        p.rect(Rect::new(at.0 - 3.0, at.1 - 3.0, 6.0, 6.0), fade(t.text, 0.35 + 0.65 * pulse), 3.0);
    } else if failed {
        icon::draw(p, icon::ALERT, at, 12.0, t.danger);
    } else {
        icon::draw(p, icon::BOT, at, 12.0, ink);
    }
}

/// A subagent's kind, what it was started for, and what it was asked: from its call's input.
fn agent_brief(call: &ToolUse) -> (String, String, String) {
    let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let kind = text("subagent_type");
    (if kind.is_empty() { call.name.clone() } else { kind }, text("description"), text("prompt"))
}

/// How far a selection dragged past the top or bottom of `area` moves the
/// view this frame: faster the further past it the mouse is.
fn drag_past(ui: &Ui, area: Rect) -> f32 {
    let past = (ui.mouse.1 - area.bottom()).max(0.0) - (area.y - ui.mouse.1).max(0.0);
    past * ui.dt * 14.0
}

/// A dot circling `at`: still running. Held at the top for less motion.
fn spinner(p: &mut Painter, ui: &Ui, at: (f32, f32), color: Color) {
    let angle = ui.motion().map_or(-std::f32::consts::FRAC_PI_2, |time| time * 5.0);
    let dot = (at.0 + angle.cos() * 5.0, at.1 + angle.sin() * 5.0);
    p.rect(Rect::new(dot.0 - 2.0, dot.1 - 2.0, 4.0, 4.0), color, 2.0);
    p.bordered(Rect::new(at.0 - 6.5, at.1 - 6.5, 13.0, 13.0), fade(color, 0.0), 6.5, 1.0, fade(color, 0.25));
}

/// A line between messages saying `text`.
fn divider(p: &mut Painter, text: &str, (x, y, width): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    let note = p.layout(text, theme::TINY, None);
    let mid = y + 8.0;
    let gap = note.width() + 24.0;
    p.rect(Rect::new(x, mid, (width - gap) * 0.5, 1.0), t.border, 0.0);
    p.rect(Rect::new(x + f32::midpoint(width, gap), mid, (width - gap) * 0.5, 1.0), t.border, 0.0);
    p.text(&note, x + (width - note.width()) * 0.5, y, t.text_faint);
    y + 34.0
}

/// One end of what is marked: where its block comes in the order drawn,
/// the block, and a place in its text.
type Mark = (usize, (usize, usize), DocPos);

/// Text marked with the mouse, in as many blocks as the drag passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marked {
    /// Where the drag began.
    from: Mark,
    /// Where it is now.
    to: Mark,
}

impl Marked {
    /// What is marked of the block drawn `rank`th, whose text ends at
    /// `end`: `None` when nothing of it is.
    fn within(&self, rank: usize, end: DocPos) -> Option<(DocPos, DocPos)> {
        let (a, b) = ((self.from.0, self.from.2), (self.to.0, self.to.2));
        let (first, last) = if a <= b { (a, b) } else { (b, a) };
        if rank < first.0 || rank > last.0 {
            return None;
        }
        let from = if rank == first.0 { first.1 } else { (0, 0) };
        let to = if rank == last.0 { last.1 } else { end };
        (from < to).then_some((from, to))
    }
}

/// A laid-out piece of the conversation and the text length it was made from.
struct Cached {
    doc: Doc,
    len: usize,
}

/// An open session.
pub(super) struct Thread {
    /// The session, kept current by the engine's events.
    pub session: Session,
    /// Whether its messages are in memory (a saved one loads on opening).
    pub loaded: bool,
    /// Whether a turn is running.
    pub busy: bool,
    /// What the engine says it is doing; empty when nothing special.
    pub status: String,
    /// Tool calls waiting for the user's approval, oldest first.
    pub asks: Vec<Ask>,
    /// A message to send once the running turn has stopped, or once the
    /// session's messages are read: its words, whether it was spoken, and the
    /// files that go with it.
    pub queued: Option<(String, bool, Vec<String>)>,
    /// Messages the user sent while the agent worked, oldest first, until
    /// it takes them in (`queue.rs`).
    pub follow_ups: Vec<Waiting>,
    /// How many of them Claude Code answers next, in turns of their own.
    pub more: u64,
    /// The turn is being stopped so that the agent reads them at once.
    pub hurry: bool,
    /// When the running turn started.
    pub started: Option<Instant>,
    /// The user stopped the running turn: how it ends is not a problem.
    pub stopping: bool,
    /// The subagent shown in the conversation's place, by the id of the call that started it.
    pub sub: Option<String>,
    /// That subagent's view, and whether it stays at its newest words.
    sub_scroll: Scroll,
    sub_follow: bool,
    /// The opened subagent is not drawn yet: its first frame only measures, so it opens at its end.
    sub_fresh: bool,
    scroll: Scroll,
    /// Not drawn yet: its first frame only measures, so it opens at its end.
    fresh: bool,
    /// Whether the view stays at the newest message as it grows.
    follow: bool,
    /// Layouts by (entry, block); block `usize::MAX` is a tool's detail.
    cache: HashMap<(usize, usize), Cached>,
    /// What each tool call's row shows, by (entry, block).
    rows: HashMap<(usize, usize), Row>,
    /// What each subagent is and is for, by the id of the call that started it.
    briefs: RefCell<HashMap<String, Rc<Brief>>>,
    /// Counts the changes of the session's messages other than more words: what its replies
    /// used hangs on them.
    revision: u64,
    /// Counts those of them that are the agent's own, not a subagent's: its task list hangs on them.
    listed: u64,
    /// The agent's task list, as last worked out.
    todos: RefCell<Summed<Rc<Vec<Todo>>>>,
    /// What its replies used, as last summed.
    total: Cell<Summed<Tally>>,
    /// Reasoning and tool rows the user opened.
    open: HashSet<(usize, usize)>,
    /// The code block last copied and when, to show "Copied" for a moment.
    copied: Option<((usize, usize), usize, f32)>,
    /// Text marked with the mouse.
    selection: Option<Marked>,
    /// Whether a drag is extending the selection.
    selecting: bool,
    /// The blocks text can be marked in, in the order they were last drawn,
    /// and how many of them this frame has drawn so far.
    order: Vec<(usize, usize)>,
    drawn: usize,
    /// What the agent's task list says it is doing, while a turn runs.
    doing: String,
    /// What each entry was like when last drawn: one out of view is left alone.
    seen: Vec<Seen>,
    /// Until when this frame measures what is out of view (anew at another
    /// width, or for the first time).
    deadline: Option<Instant>,
    /// Something out of view is not measured yet: the next frame goes on with it.
    behind: bool,
    /// How many frames it has been measured in without being shown, since it was opened.
    opening: u8,
    /// A picture of the conversation that was just clicked, for the shell to open whole.
    pub clicked_picture: Option<String>,
    /// The search of what the conversation shows, while its bar is open (`find.rs`).
    pub find: Option<Find>,
    /// That search came from the search window, which found its words in
    /// the messages as they were written: if the conversation shows them
    /// nowhere (they stand in Markdown's marks), the message is shown.
    find_first: bool,
    /// The files each run of replies changed (their path, lines added and
    /// lines removed), by the index of the run's last reply, with the
    /// weight of the calls they were gathered from: listed under the run.
    touched: HashMap<usize, Gathered>,
    /// A file of such a list that was just clicked (the run's last reply,
    /// and the file), for the shell to show what the run did to it.
    pub clicked_change: Option<(usize, String)>,
    /// Whether the Claude process the last message went to guesses the
    /// next message: a process keeps that from its start, so another
    /// choice needs a new process. `None` before the first message.
    pub guessing: Option<bool>,
    /// Whether an account's process (Claude Code, Codex) is taken to be up
    /// for it: a message went to one, and it was not closed and did not end
    /// since. One left with nothing to do is closed after a while (`warm.rs`).
    pub live: bool,
    /// When the user last left it for another session, in Unix seconds; 0 if never.
    pub left: u64,
    /// What Claude Code guesses the user writes next, since the last turn
    /// ended: shown in the empty message box, taken with Tab (`composer.rs`).
    pub suggestion: Option<String>,
    /// The provider the last message went to, which its turn runs on; empty before the first.
    pub sent_on: String,
    /// The account's usage limit stopped the running turn, as its agent said.
    pub limited: bool,
    /// The account was refused during the running turn (it may still have
    /// gone on, on extra usage): a turn that fails now failed for it.
    pub refused: bool,
    /// The usage limit stopped the session, which goes on by itself then (`limits.rs`).
    pub resume: Option<Resume>,
    /// How many turns in a row a usage limit stopped.
    pub tries: u32,
    /// What its Claude Code runs in the background, oldest first: while
    /// there is any, the process is not closed by itself (`mod.rs`).
    pub background: Vec<Background>,
    /// The commands and skills its agent's process offers in the session's
    /// folder (its project's own among them), each a name and a line on
    /// it; `None` until a process of it has said.
    pub commands: Option<Vec<(String, String)>>,
}

impl Thread {
    /// A session whose messages are in memory.
    pub fn new(session: Session) -> Self {
        Self {
            session,
            loaded: true,
            busy: false,
            status: String::new(),
            asks: Vec::new(),
            queued: None,
            follow_ups: Vec::new(),
            more: 0,
            hurry: false,
            started: None,
            stopping: false,
            sub: None,
            sub_scroll: Scroll::default(),
            sub_follow: true,
            sub_fresh: false,
            scroll: Scroll::default(),
            fresh: true,
            follow: true,
            cache: HashMap::new(),
            rows: HashMap::new(),
            briefs: RefCell::default(),
            revision: 0,
            listed: 0,
            todos: RefCell::default(),
            total: Cell::default(),
            open: HashSet::new(),
            copied: None,
            selection: None,
            selecting: false,
            order: Vec::new(),
            drawn: 0,
            doing: String::new(),
            seen: Vec::new(),
            deadline: None,
            behind: false,
            opening: 0,
            clicked_picture: None,
            find: None,
            find_first: false,
            touched: HashMap::new(),
            clicked_change: None,
            guessing: None,
            live: false,
            left: 0,
            suggestion: None,
            sent_on: String::new(),
            limited: false,
            refused: false,
            resume: None,
            tries: 0,
            background: Vec::new(),
            commands: None,
        }
    }

    /// A saved session whose messages are being read. Until they are in
    /// it shows what the sidebar `known` of it: whose it is, its title, and
    /// the provider, model and effort it was left with.
    pub fn loading(known: &SessionSummary) -> Self {
        let mut thread = Self::new(Session {
            id: known.id.clone(),
            agent: known.agent.clone(),
            title: known.title.clone(),
            provider: known.provider.clone(),
            model: known.model.clone(),
            effort: known.effort.clone(),
            ..Session::default()
        });
        thread.loaded = false;
        thread
    }

    /// Whether a tool call of the agent's own is running (or waits for the
    /// user's approval): what the user sends now, it reads once it is done.
    pub fn tools_running(&self) -> bool {
        let Some(Entry::Reply(reply)) = self.session.entries.last() else { return false };
        reply.blocks.iter().any(|block| matches!(block, Block::Tool(call) if call.output.is_none()))
    }

    /// Whether its agent is at work: in a turn, or on something it runs in
    /// the background (a command, or a subagent that goes on after the turn
    /// ends or is stopped), which Claude Code answers in a turn of its own
    /// once it ends. Its process is not closed by itself meanwhile.
    pub fn at_work(&self) -> bool {
        self.busy
            || !self.background.is_empty()
            || (self.live && turn_agents(&self.session.entries, None).into_iter().any(at_work))
    }

    /// A turn starts: the view follows it.
    pub fn begin_turn(&mut self) {
        self.busy = true;
        // A guess at what comes after the last turn is no use once another runs.
        self.suggestion = None;
        self.stopping = false;
        (self.limited, self.refused) = (false, false);
        // Whatever started it, the session is not waiting to go on any more.
        self.resume = None;
        self.follow = true;
        self.status.clear();
        self.started = Some(Instant::now());
    }

    /// The session's messages changed in a way that what is worked out from them
    /// ([`Thread::todos`], [`Thread::total`]) may have too.
    pub fn touch(&mut self) {
        self.revision += 1;
        self.listed += 1;
    }

    /// The engine said `event`, which the session has taken in. More words
    /// change nothing that is worked out from it, and what a subagent does
    /// is no part of the agent's own task list.
    pub fn heard(&mut self, event: &AgentEvent) {
        let words = |event: &AgentEvent| matches!(event, AgentEvent::Text(_) | AgentEvent::Thinking(_));
        match event {
            AgentEvent::Sub { event, .. } if words(event) => {}
            AgentEvent::Sub { .. } => self.revision += 1,
            event if words(event) => {}
            _ => self.touch(),
        }
    }

    /// The agent's task list ([`Session::todos`]): worked out when a tool
    /// call of its own came or was answered, not at every frame.
    pub fn todos(&self) -> Rc<Vec<Todo>> {
        let stamp = (self.listed, self.session.entries.len());
        let mut kept = self.todos.borrow_mut();
        match kept.as_ref() {
            Some((of, todos)) if *of == stamp => Rc::clone(todos),
            _ => {
                let todos = Rc::new(self.session.todos());
                *kept = Some((stamp, Rc::clone(&todos)));
                todos
            }
        }
    }

    /// What the session's replies used so far: summed when its messages
    /// changed, not at every frame.
    pub fn total(&self) -> Tally {
        let stamp = (self.revision, self.session.entries.len());
        match self.total.get() {
            Some((of, total)) if of == stamp => total,
            _ => {
                let total = Tally::of(&self.session);
                self.total.set(Some((stamp, total)));
                total
            }
        }
    }

    /// The turn is over; a problem no reply carries is shown on the last one.
    pub fn end_turn(&mut self, problem: Option<String>) {
        self.touch();
        self.busy = false;
        self.asks.clear();
        self.status.clear();
        self.started = None;
        let stopped = std::mem::take(&mut self.stopping);
        // Changed only where there is something to change: a reply left alone stays shared with what
        // was last saved of it.
        if let Some(Entry::Reply(reply)) = self.session.entries.last_mut() {
            if stopped {
                // Being interrupted is what was asked for, not a failure.
                if !reply.problem.is_empty() {
                    Arc::make_mut(reply).problem.clear();
                }
            } else if let Some(problem) = problem
                && reply.problem.is_empty()
            {
                Arc::make_mut(reply).problem = problem;
            }
        }
    }

    /// Claude Code said a task of its runs in the background (`ended` false)
    /// or no longer does.
    pub fn background_task(&mut self, id: String, what: String, ended: bool) {
        if ended {
            self.background.retain(|task| task.id != id);
        } else if !self.background.iter().any(|task| task.id == id) {
            self.background.push(Background { id, what });
        }
    }

    /// The process ends with tasks still running in the background: a line
    /// in the conversation says each was stopped (between turns only, as a
    /// running reply is written at the end). Returns whether any was, for
    /// the session to be saved.
    pub fn background_lost(&mut self) -> bool {
        let lost = std::mem::take(&mut self.background);
        if self.busy {
            return false;
        }
        self.touch();
        for task in &lost {
            let text = if task.what.is_empty() {
                "A background task was stopped with the agent's process".to_owned()
            } else {
                format!("A background task was stopped with the agent's process: {}", task.what)
            };
            self.session.entries.push(Entry::Note { text });
        }
        !lost.is_empty()
    }

    /// Opens the reasoning or tool row at (entry, block), as a click would.
    #[cfg(test)]
    pub fn open_block(&mut self, entry: usize, block: usize) {
        self.open.insert((entry, block));
    }

    /// Entries moved (a summary was inserted): every layout is made again.
    pub fn invalidate(&mut self) {
        self.touch();
        self.cache.clear();
        self.rows.clear();
        self.touched.clear();
        self.seen.clear();
        self.open.clear();
        self.selection = None;
        // What it found was in blocks that are others now.
        if let Some(find) = self.find.take() {
            self.find = Some(Find::new(find.editor.text(), find.focused));
        }
    }

    /// Opens the bar that finds `words` in the conversation, with the
    /// keyboard when `focused`. `first` when the search window found them
    /// in this session's messages: the view goes to the first of them.
    pub fn open_find(&mut self, words: &str, focused: bool, first: bool) {
        self.find = Some(Find::new(words, focused));
        self.find_first = first;
    }

    /// The Markdown layout of block `key`, made again when its text grew or
    /// the width changed; what was laid out before is reused where unchanged.
    /// The pictures it shows are found from the session's folder.
    fn markdown(&mut self, p: &Painter, key: (usize, usize), text: &str, width: f32, ink: u8) -> &mut Doc {
        let stale = self.cache.get(&key).is_none_or(|c| c.len != text.len() || !c.doc.fits(width, p.scale));
        if stale {
            let previous = self.cache.remove(&key).map(|c| c.doc);
            let folder = self.session.folder();
            let doc = Doc::markdown_in(p.fonts, text, (width, p.scale), ink, &folder, previous);
            self.cache.insert(key, Cached { doc, len: text.len() });
        }
        &mut self.cache.entry(key).or_insert_with(|| Cached { doc: Doc::default(), len: 0 }).doc
    }

    /// Whether this frame's time for measuring what is out of view is
    /// spent: the rest keeps what is known of it and follows in the next
    /// frames. Resizing the window, or opening a long conversation, then
    /// costs what is in view, not the whole conversation.
    fn spent(&self) -> bool {
        self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// The plain layout of block `key` in `style`.
    fn plain(&mut self, p: &Painter, key: (usize, usize), text: &str, style: Style, width: f32) -> &mut Doc {
        let scale = p.scale;
        let outdated = self.cache.get(&key).is_none_or(|c| c.len != text.len() || !c.doc.fits(width, scale));
        if outdated {
            let doc = Doc::plain(p.fonts, text, style, width, p.scale);
            self.cache.insert(key, Cached { doc, len: text.len() });
        }
        &mut self.cache.entry(key).or_insert_with(|| Cached { doc: Doc::default(), len: 0 }).doc
    }

    /// What the row of the tool call `call` at `key` shows: its icon, what
    /// it was called on, and how many lines it added to a file and removed
    /// from it when it is one that changes a file (`change.rs`). Its input
    /// is read once for that, not at every frame.
    fn row(&mut self, key: (usize, usize), call: &ToolUse) -> &Row {
        let known = self.rows.entry(key).or_default();
        if known.of != Some(call.input.len()) {
            let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
            *known = Row {
                of: Some(call.input.len()),
                icon: tool_icon(&call.name),
                about: tools::summary(&input),
                changed: change::of(&call.name, &input).map(|change| (change.added, change.removed)),
            };
        }
        known
    }

    /// The kind of the subagent `call` started and what it was started for.
    fn brief(&self, call: &ToolUse) -> Rc<Brief> {
        let mut briefs = self.briefs.borrow_mut();
        if let Some(known) = briefs.get(&call.id).filter(|known| known.of == call.input.len()) {
            return Rc::clone(known);
        }
        let (kind, description, _) = agent_brief(call);
        let brief = Rc::new(Brief { of: call.input.len(), kind, description });
        briefs.insert(call.id.clone(), Rc::clone(&brief));
        brief
    }

    /// What went into the tool call `call` and what came out of it, laid
    /// out for the detail of its row at `key`: its input (not of a call
    /// that `edited` a file, whose lines say it), then the first of its
    /// output.
    fn detail(&mut self, p: &Painter, key: (usize, usize), call: &ToolUse, edited: bool, width: f32) -> &mut Doc {
        let said = call.output.as_ref().map_or(0, |output| output.len() + 1);
        let of = call.input.len().wrapping_mul(31).wrapping_add(said);
        let scale = p.scale;
        let outdated = self.cache.get(&key).is_none_or(|c| c.len != of || !c.doc.fits(width, scale));
        if outdated {
            let mut detail = if edited {
                String::new()
            } else {
                let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
                serde_json::to_string_pretty(&input).unwrap_or_else(|_| call.input.clone())
            };
            if let Some(output) = &call.output {
                let shown: String = output.chars().take(DETAIL_CHARS).collect();
                if !detail.is_empty() {
                    detail.push_str("\n\n");
                }
                detail.push_str(&shown);
                if shown.len() < output.len() {
                    detail.push_str("\n…");
                }
            }
            let doc = Doc::plain(p.fonts, &detail, DETAIL, width, scale);
            self.cache.insert(key, Cached { doc, len: of });
        }
        &mut self.cache.entry(key).or_insert_with(|| Cached { doc: Doc::default(), len: 0 }).doc
    }

    /// The lines `call` changed, laid out as a diff for the detail of its row at `key`.
    fn diff(&mut self, p: &Painter, key: (usize, usize), call: &ToolUse, width: f32) -> &mut Doc {
        let scale = p.scale;
        let outdated = self.cache.get(&key).is_none_or(|c| c.len != call.input.len() || !c.doc.fits(width, scale));
        if outdated {
            let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
            let lines = change::of(&call.name, &input).map(|change| change.lines).unwrap_or_default();
            let doc = Doc::diff(p.fonts, &lines, DETAIL, width, scale);
            self.cache.insert(key, Cached { doc, len: call.input.len() });
        }
        &mut self.cache.entry(key).or_insert_with(|| Cached { doc: Doc::default(), len: 0 }).doc
    }

    /// What is marked in the conversation, if anything: the marked part of
    /// each block, a blank line between them.
    pub fn selected_text(&self) -> Option<String> {
        let marked = self.selection?;
        let (first, last) = (marked.from.0.min(marked.to.0), marked.from.0.max(marked.to.0));
        let parts: Vec<String> = (first..=last)
            .filter_map(|rank| {
                let doc = &self.cache.get(self.order.get(rank)?)?.doc;
                let (from, to) = marked.within(rank, doc.end())?;
                Some(doc.text_between(from, to))
            })
            .collect();
        let text = parts.join("\n\n");
        (!text.is_empty()).then_some(text)
    }

    /// Lets the mouse mark text in the block `key` (laid out already, drawn
    /// at `origin`, `size` large) and draws what is marked: a drag selects,
    /// on into the blocks it passes; a double click a word, a triple click
    /// the block.
    fn select(&mut self, p: &mut Painter, ui: &mut Ui, key: (usize, usize), origin: (f32, f32), size: (f32, f32)) {
        let rank = self.drawn;
        self.drawn += 1;
        if self.order.get(rank) != Some(&key) {
            self.order.truncate(rank);
            self.order.push(key);
        }
        self.mark_found(p, key, origin);
        // Measured out of sight: there is nothing to mark it by.
        if p.clip().w <= 0.0 {
            return;
        }
        let Some(doc) = self.cache.get_mut(&key).map(|cached| &mut cached.doc) else { return };
        let at = (ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
        let over = Rect::new(origin.0, origin.1, size.0, size.1).contains(ui.mouse) && ui.hovered(p.clip());
        if over {
            ui.cursor = CursorIcon::Text;
        }
        if ui.pressed
            && over
            && let Some(pos) = doc.hit(at.0, at.1)
        {
            let (from, to) = match ui.clicks {
                0 | 1 => (pos, pos),
                2 => {
                    let word = doc.word(pos);
                    ((pos.0, word.start), (pos.0, word.end))
                }
                _ => ((0, 0), doc.end()),
            };
            self.selection = Some(Marked { from: (rank, key, from), to: (rank, key, to) });
            self.selecting = ui.clicks < 2;
        }
        let Some(mut marked) = self.selection else { return };
        // A drag takes the end of what is marked to the block the mouse is level with: past the
        // view's edge, the block at that edge, which the view scrolls on from.
        let view = p.clip();
        let height = ui.mouse.1.min(view.bottom() - 1.0).max(view.y) - origin.1;
        if self.selecting
            && ui.down
            && (0.0..size.1).contains(&height)
            && let Some(pos) = doc.hit(at.0, height)
        {
            marked.to = (rank, key, pos);
            self.selection = Some(marked);
        }
        if let Some((from, to)) = marked.within(rank, doc.end()) {
            doc.mark_selection(from, to, p.theme.selection);
        }
    }

    /// Has the places a search found in the block `key` washed when it is
    /// drawn, at `origin`, next: call it for every block that shows text,
    /// in the order they are drawn, which is the order the matches are
    /// gone through in.
    fn mark_found(&mut self, p: &Painter, key: (usize, usize), origin: (f32, f32)) {
        if let Some(find) = &mut self.find
            && let Some(cached) = self.cache.get_mut(&key)
        {
            find.block(key, (&mut cached.doc, cached.len), origin, p.theme.accent);
        }
    }

    /// Draws the conversation in `area` and handles scrolling and clicks:
    /// the whole of it, or the subagent that is open in its place. `agent`
    /// is the agent's name and colour.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, agent: (&str, Color), actions: &mut Vec<Action>) {
        let t = p.theme;
        if !self.loaded {
            p.label_centered("Opening…", theme::SMALL, area, t.text_faint);
            return;
        }
        self.drawn = 0;
        // Blocks moved under what was marked: it is let go rather than shown on other text.
        let moved = |(rank, key, _): Mark| self.order.get(rank) != Some(&key);
        if self.selection.is_some_and(|marked| moved(marked.from) || moved(marked.to)) {
            self.selection = None;
        }
        let doing = |todo: &Todo| if todo.doing.is_empty() { todo.subject.clone() } else { todo.doing.clone() };
        let active = self.busy.then(|| self.todos().iter().find(|todo| todo.state == TodoState::Active).map(doing));
        self.doing = active.flatten().unwrap_or_default();
        // The bar of a search floats over the conversation: what is under it is not hovered or clicked.
        let bar = self.find.as_mut().map(|find| {
            find.begin();
            Find::bar(area)
        });
        let under = ui.blocker;
        if under.is_none() {
            ui.blocker = bar;
        }
        // The entries are taken out while they are drawn: drawing needs the
        // layout cache, which lives beside them.
        let entries = std::mem::take(&mut self.session.entries);
        // Only an open subagent has its turn's calls looked up.
        let calls = if self.sub.is_some() { turn_agents(&entries, self.sub.as_deref()) } else { Vec::new() };
        let open = self.sub.as_deref().and_then(|sub| calls.iter().find(|call| call.id == sub).copied());
        if let Some(call) = open {
            let body = self.draw_bar(p, ui, area, call);
            let clip = p.push_clip(body);
            self.draw_sub(p, ui, body, call, agent, actions);
            p.set_clip(clip);
        } else {
            self.sub = None;
            self.draw_main(p, ui, area, &entries, agent, actions);
        }
        self.session.entries = entries;
        self.order.truncate(self.drawn);
        ui.blocker = under;
        if self.find.as_mut().is_some_and(|find| find.draw(p, ui, area)) {
            self.find = None;
            ui.animating = true;
        }
    }

    /// The conversation was drawn in `area`, scrolled by `offset`: a search
    /// brings the match it has in hand into view. `opened` on the frame the
    /// conversation is first shown, where the view goes there at once.
    fn show_found(&mut self, ui: &mut Ui, entries: &[Entry], (area, offset, opened): (Rect, f32, bool), main: bool) {
        let Some(find) = &mut self.find else { return };
        let (scroll, follow) =
            if main { (&mut self.scroll, &mut self.follow) } else { (&mut self.sub_scroll, &mut self.sub_follow) };
        let mut moved = find.end(scroll, area, offset);
        if !moved && main && self.find_first && find.pending() && !find.words().is_empty() {
            // Found in a message as it was written, not as it is shown: the message itself is shown.
            let words = find.words();
            let holds = |text: &str| text.to_lowercase().contains(&words);
            let said = |entry: &Entry| match entry {
                Entry::User { text, .. } | Entry::Summary { text } => holds(text),
                Entry::Reply(reply) => {
                    reply.blocks.iter().any(|block| matches!(block, Block::Text { text } if holds(text)))
                }
                Entry::Note { .. } => false,
            };
            if let Some(index) = entries.iter().position(said) {
                let above: f32 = self.seen.iter().take(index).filter_map(|seen| seen.height).sum();
                let tall = self.seen.get(index).and_then(|seen| seen.height).unwrap_or(0.0);
                scroll.reveal(above, above + 56.0 + tall.min(area.h - 56.0), area.h);
                moved = true;
            }
            find.settle();
        }
        if moved {
            (*follow, ui.animating) = (false, true);
            if opened {
                scroll.settle();
            }
            if !find.pending() {
                self.find_first = false;
            }
        }
    }

    /// Shows the subagent started by the call `id` in the conversation's place; `None` goes back to it.
    pub fn open_agent(&mut self, id: Option<String>) {
        self.sub_fresh = id.is_some();
        self.sub = id;
        self.sub_scroll.reset();
        (self.sub_follow, self.selection) = (true, None);
    }

    /// How far down the conversation's view is (`0..=1`), when it scrolls.
    #[cfg(test)]
    pub fn progress(&self) -> Option<f32> {
        self.scroll.progress()
    }

    /// How many tool calls' rows were worked out: those that came into view.
    #[cfg(test)]
    pub fn rows_read(&self) -> usize {
        self.rows.len()
    }

    /// Whether the conversation is shown: it was opened, and measured enough to be drawn at its end.
    #[cfg(test)]
    pub fn shown(&self) -> bool {
        self.loaded && !self.fresh
    }

    /// Whether every entry of the conversation was measured.
    #[cfg(test)]
    pub fn measured(&self) -> bool {
        self.seen.len() == self.session.entries.len() && self.seen.iter().all(|seen| seen.height.is_some())
    }

    /// Where the entry `index` begins under the top of the conversation's view, by what is known
    /// of the entries before it.
    #[cfg(test)]
    pub fn top_of(&self, index: usize) -> f32 {
        let above: f32 = self.seen.iter().take(index).filter_map(|seen| seen.height).sum();
        28.0 + above - self.scroll.position()
    }

    /// How far down the open subagent's view is (`0..=1`), when it scrolls.
    #[cfg(test)]
    pub fn sub_progress(&self) -> Option<f32> {
        self.sub_scroll.progress()
    }

    /// The subagents of the turn (the open one's, or the latest), in the
    /// order they were started, for the side panel's list.
    pub fn subagents(&self) -> Vec<Subagent> {
        let calls = turn_agents(&self.session.entries, self.sub.as_deref());
        let listed = |call: &ToolUse| Subagent {
            id: call.id.clone(),
            label: self.brief(call).label().to_owned(),
            // One sent to the background goes on after the turn, for as long as its process does.
            running: (self.busy || self.live) && at_work(call),
            failed: call.is_error,
            open: self.sub.as_deref() == Some(call.id.as_str()),
        };
        calls.into_iter().map(listed).collect()
    }

    /// The bar over an open subagent: the way back to the conversation,
    /// then which subagent this is and how it is doing. Returns what is
    /// left of `area` under it.
    fn draw_bar(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, call: &ToolUse) -> Rect {
        let t = p.theme;
        let strip = Rect::new(area.x, area.y, area.w, 40.0);
        let width = theme::COLUMN_WIDTH.min(area.w - 48.0);
        let x = area.x + ((area.w - width) * 0.5).round() - 8.0;
        let back = p.layout("Conversation", theme::SMALL, None);
        let chip = Rect::new(x, strip.y + 6.0, back.width() + 37.0, 28.0);
        let hovered = ui.hovered(chip);
        let hover = ui.anim(id("subagent-back"), f32::from(u8::from(hovered)));
        p.rect(chip, fade(t.hover, hover), theme::RADIUS_SM);
        let ink = mix(t.text_muted, t.text, hover);
        icon::draw(p, icon::CHEVRON_LEFT, (chip.x + 14.0, chip.y + 14.0), 14.0, ink);
        p.text(&back, chip.x + 26.0, chip.y + (chip.h - back.height()) * 0.5, ink);
        let mark = (chip.right() + 19.0, chip.y + 14.0);
        agent_mark(p, ui, mark, (self.busy && at_work(call), call.is_error), t.text_muted);
        let mut label = p.layout(self.brief(call).label(), theme::SMALL, None);
        label.truncate(p.fonts, (x + width + 16.0 - mark.0 - 11.0).max(0.0));
        p.text(&label, mark.0 + 11.0, chip.y + (chip.h - label.height()) * 0.5, t.text);
        p.rect(Rect::new(area.x, strip.bottom() - 1.0, area.w, 1.0), t.border, 0.0);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(chip) {
                // The conversation is back with the next frame.
                self.open_agent(None);
                ui.animating = true;
            }
        }
        Rect::new(area.x, strip.bottom(), area.w, area.h - strip.h)
    }

    /// A subagent's own conversation: what it was asked, what it said and
    /// did, and how it ended.
    fn draw_sub(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        area: Rect,
        call: &ToolUse,
        agent: (&str, Color),
        actions: &mut Vec<Action>,
    ) {
        let t = p.theme;
        let width = theme::COLUMN_WIDTH.min(area.w - 48.0);
        let x = area.x + ((area.w - width) * 0.5).round();
        let hovered = ui.hovered(area);
        if hovered && ui.scroll < 0.0 {
            self.sub_follow = false;
        }
        if self.selecting && ui.down && drag_past(ui, area) != 0.0 {
            self.sub_scroll.nudge(drag_past(ui, area));
            (self.sub_follow, ui.animating) = (false, true);
        }
        let offset = self.sub_scroll.update(ui, hovered);
        if ui.pressed && hovered {
            self.selection = None;
        }
        if !ui.down {
            self.selecting = false;
        }
        let top = area.y + 24.0 - offset;
        // The first frame only measures: like the conversation, it opens at its end, not its start.
        let hidden = self.sub_fresh.then(|| p.push_clip(Rect::new(area.x, area.y, 0.0, 0.0)));
        let base = SUB_KEYS | (id(&call.id) as usize >> 8);
        let (_, _, brief) = agent_brief(call);
        let mut y = top;
        if !brief.trim().is_empty() {
            y = self.draw_prompt(p, ui, ((base, BRIEF_KEY), &brief, &[]), (x, y, width));
        }
        let live = self.busy && at_work(call);
        y = self.draw_blocks(p, ui, (base, &call.sub, live), (x, y, width), agent, actions);
        if live && call.sub.is_empty() {
            y = self.draw_working(p, ui, (x, y), agent.1);
        }
        if !call.activity.is_empty() {
            let ink = if call.is_error { t.danger } else { t.text_faint };
            p.label(&call.activity, theme::TINY, x, y + 4.0, ink);
            y += 28.0;
        }
        if let Some(clip) = hidden {
            p.set_clip(clip);
        }
        let content = y - top + 24.0 + 24.0;
        self.sub_scroll.finish(ui, content, area.h);
        if hovered && ui.scroll > 0.0 && self.sub_scroll.at_end() {
            self.sub_follow = true;
        }
        if self.sub_scroll.dragging() {
            // Dragged to the end, it follows the newest words again; anywhere else, it stays.
            self.sub_follow = self.sub_scroll.at_end();
        }
        let opened = std::mem::take(&mut self.sub_fresh);
        if opened {
            self.sub_scroll.jump_to_end();
            ui.animating = true;
        } else if live && self.sub_follow && !self.sub_scroll.at_end() {
            self.sub_scroll.follow_end();
            ui.animating = true;
        }
        self.show_found(ui, &[], (area, offset, opened), false);
        self.sub_scroll.bar(p, ui, area, content);
    }

    /// The whole conversation.
    fn draw_main(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        area: Rect,
        entries: &[Entry],
        agent: (&str, Color),
        actions: &mut Vec<Action>,
    ) {
        let width = theme::COLUMN_WIDTH.min(area.w - 48.0);
        let x = area.x + ((area.w - width) * 0.5).round();
        let hovered = ui.hovered(area);
        if hovered && ui.scroll < 0.0 {
            // Scrolling up lets go of the newest message.
            self.follow = false;
        }
        if self.selecting && ui.down && drag_past(ui, area) != 0.0 {
            self.scroll.nudge(drag_past(ui, area));
            (self.follow, ui.animating) = (false, true);
        }
        let offset = self.scroll.update(ui, hovered);
        // A press anywhere starts over; the block under it marks anew.
        if ui.pressed && ui.hovered(area) {
            self.selection = None;
        }
        if !ui.down {
            self.selecting = false;
        }
        let top = area.y + 28.0 - offset;
        // A conversation just opened only measures until it can be shown at its end, not its start.
        let nowhere = Rect::new(area.x, area.y, 0.0, 0.0);
        let hidden = self.fresh.then(|| p.push_clip(nowhere));
        let mut y = top;
        let last = entries.len().saturating_sub(1);
        // Half a view above and below counts as in view: it is laid out at once.
        let (above, below) = (area.y - area.h * 0.5, area.bottom() + area.h * 0.5);
        (self.deadline, self.behind) = (Some(Instant::now() + RELAYOUT_BUDGET), false);
        self.seen.resize_with(entries.len(), Seen::default);
        let fits = (width.to_bits(), p.scale.to_bits());
        // How much taller what was measured above the view turned out to be: the view goes down
        // with it, so that what is looked at stays where it is.
        let mut pushed = 0.0;
        // Something in view was placed by a guess, or by a height that is no longer its own.
        let mut guessed = false;
        for (index, entry) in entries.iter().enumerate() {
            let live = self.busy && index == last;
            let ends = !live && !matches!(entries.get(index + 1), Some(Entry::Reply(_)));
            let weight = weight(entry, live, ends);
            let seen = &self.seen[index];
            let tall = seen.height.unwrap_or_else(|| guess(entry));
            let current = seen.height.is_some() && seen.fits == fits && seen.weight == weight;
            // What is out of view is its height and nothing more: once it is measured, or while this
            // frame has no time left to measure it. The newest entry is the one that changes, and
            // a search goes through every block.
            let out = y + tall < above || y > below;
            if out && index != last && self.find.is_none() && (current || self.spent()) {
                self.behind |= !current;
                let marks = &seen.marks;
                if self.order.get(self.drawn..self.drawn + marks.len()) != Some(marks.as_slice()) {
                    self.order.truncate(self.drawn);
                    self.order.extend_from_slice(marks);
                }
                self.drawn += marks.len();
                y += tall;
                continue;
            }
            // Measured out of sight above the view, it is not drawn: only how tall it is matters.
            let unseen = !current && y + tall <= area.y;
            guessed |= !current && !unseen;
            let clip = unseen.then(|| p.push_clip(nowhere));
            let (start, first) = (y, self.drawn);
            match entry {
                Entry::User { text, files, .. } => {
                    y = self.draw_prompt(p, ui, ((index, 0), text, files), (x, y, width));
                }
                Entry::Reply(reply) => {
                    // Under the last reply of a run that is over: the files the run changed.
                    let touched = if ends { self.take_touched(index, entries) } else { None };
                    let files = touched.as_ref().map_or(&[][..], |(_, files)| files.as_slice());
                    y = self.draw_reply(p, ui, (index, reply, live), (x, y, width), (agent, files), actions);
                    if let Some(touched) = touched {
                        self.touched.insert(index, touched);
                    }
                }
                Entry::Summary { .. } => {
                    y = divider(p, "Earlier messages were summarised to make room", (x, y, width));
                }
                Entry::Note { text } => y = divider(p, text, (x, y, width)),
            }
            if let Some(clip) = clip {
                p.set_clip(clip);
            }
            let height = y - start;
            if unseen {
                // It keeps its foot where it was: what is under it does not move.
                pushed += height - tall;
                y = start + tall;
            }
            let seen = &mut self.seen[index];
            (seen.height, seen.fits, seen.weight) = (Some(height), fits, weight);
            let marks = self.order.get(first..self.drawn).unwrap_or_default();
            if seen.marks != marks {
                seen.marks.clear();
                seen.marks.extend_from_slice(marks);
            }
        }
        self.deadline = None;
        let behind = std::mem::take(&mut self.behind);
        if behind {
            ui.animating = true;
        }
        // A turn that has not written anything yet still shows it is alive.
        if self.busy && !matches!(entries.last(), Some(Entry::Reply(reply)) if !reply.blocks.is_empty()) {
            y = self.draw_working(p, ui, (x, y), agent.1);
        }

        if let Some(clip) = hidden {
            p.set_clip(clip);
        }
        let content = y - top + pushed + 28.0 + 24.0;
        self.scroll.shift(pushed);
        self.scroll.finish(ui, content, area.h);
        if hovered && ui.scroll > 0.0 && self.scroll.at_end() {
            self.follow = true;
        }
        if self.scroll.dragging() {
            // Dragged to the end, it follows the newest message again; anywhere else, it stays.
            self.follow = self.scroll.at_end();
        }
        let opened = self.fresh;
        if opened {
            self.scroll.jump_to_end();
            // It is shown with the next frame once all of it is measured, or once a frame placed
            // what is in view by nothing but heights that were measured before.
            self.opening = self.opening.saturating_add(1);
            self.fresh = behind && guessed && self.opening < 12;
            ui.animating = true;
        } else if self.follow && !self.scroll.at_end() {
            // What was just written is brought into view.
            self.scroll.follow_end();
            // A busy session alone does not ask for every frame: what shows it works is drawn at a
            // gentler rate (`App::frame`), where every refresh of a fast screen would cost for nothing.
            ui.animating = true;
        }
        self.show_found(ui, entries, (area, offset, opened), true);
        self.scroll.bar(p, ui, area, content);
    }

    /// The user's message: what they sent along (pictures as thumbnails, a
    /// click on one opens it whole; other files as pills), then their words
    /// in a bubble, all on the right. Returns where it ends.
    fn draw_prompt(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (key, text, files): ((usize, usize), &str, &[String]),
        (x, mut y, width): (f32, f32, f32),
    ) -> f32 {
        let t = p.theme;
        let right = x + width;
        let (pictures, others): (Vec<&String>, Vec<&String>) =
            files.iter().partition(|file| media_type(file).is_some());
        let (wide, tall, gap) = (136.0, 98.0, 8.0);
        let across = ((width * BUBBLE_SHARE + gap) / (wide + gap)).floor().max(1.0) as usize;
        for row in pictures.chunks(across) {
            let mut left = right - row.len() as f32 * (wide + gap) + gap;
            for file in row {
                let frame = Rect::new(left, y, wide, tall);
                if !matches!(p.image(file, frame, theme::RADIUS), Lookup::Ready(_)) {
                    p.bordered(frame, t.surface, theme::RADIUS, 1.0, t.border);
                    icon::draw(p, icon::IMAGE, (frame.x + wide * 0.5, frame.y + tall * 0.5), 18.0, t.text_faint);
                } else if ui.hovered(frame) && p.clip().contains(ui.mouse) {
                    ui.cursor = CursorIcon::Pointer;
                    if ui.clicked(frame) {
                        self.clicked_picture = Some((*file).clone());
                    }
                }
                left += wide + gap;
            }
            y += tall + gap;
        }
        if !others.is_empty() {
            let mut left = right;
            for file in others.iter().rev() {
                let mut name = p.layout(folder_name(file), theme::TINY, None);
                name.truncate(p.fonts, 190.0);
                let pill = Rect::new(left - name.width() - 36.0, y, name.width() + 36.0, 26.0);
                p.bordered(pill, t.bg, 13.0, 1.0, t.border_strong);
                icon::draw(p, icon::FILE, (pill.x + 15.0, pill.y + 13.0), 12.0, t.text_faint);
                p.text(&name, pill.x + 26.0, pill.y + (pill.h - name.height()) * 0.5, t.text_muted);
                left = pill.x - 6.0;
            }
            y += 26.0 + gap;
        }
        if text.trim().is_empty() {
            return y + 14.0;
        }
        let pad = (14.0, 10.0);
        let max = (width * BUBBLE_SHARE - 2.0 * pad.0).round();
        let doc = self.plain(p, key, text, theme::BODY, max);
        let (text_w, height) = (doc.texts.first().map_or(0.0, |piece| piece.layout.width()), doc.height);
        let bubble = Rect::new(right - text_w - 2.0 * pad.0, y, text_w + 2.0 * pad.0, height + 2.0 * pad.1);
        // On a light canvas the surface is the canvas: the panel sets the bubble apart.
        p.rect(bubble, if t.is_light() { t.panel } else { t.surface }, theme::RADIUS_LG);
        let origin = (bubble.x + pad.0, bubble.y + pad.1);
        self.select(p, ui, key, origin, (text_w, height));
        if let Some(cached) = self.cache.get_mut(&key) {
            cached.doc.draw(p, ui, origin, t.text, false, None);
        }
        bubble.bottom() + 22.0
    }

    /// The pulse shown while the agent has not written anything yet.
    fn draw_working(&mut self, p: &mut Painter, ui: &mut Ui, (x, y): (f32, f32), color: Color) -> f32 {
        let t = p.theme;
        for dot in 0..3u8 {
            // Still for less motion: three dots in a row, half lit.
            let lift = ui.motion().map_or(0.0, |time| {
                let phase = (time * 1.6 - f32::from(dot) * 0.18).rem_euclid(1.0);
                (phase * std::f32::consts::PI).sin().max(0.0)
            });
            let at = Rect::new(x + f32::from(dot) * 11.0, y + 7.0 - 3.0 * lift, 6.0, 6.0);
            p.rect(at, mix(t.text_faint, color, if ui.still { 0.5 } else { lift }), 3.0);
        }
        let seconds = elapsed(self.started);
        // What the engine says it is doing, else the task in hand, else how long it has been.
        let label = match (self.status.as_str(), self.doing.as_str()) {
            ("", "") if seconds >= 3 => format!("{seconds}s"),
            ("", doing) => doing.to_owned(),
            (status, _) => status.to_owned(),
        };
        p.label(&label, theme::SMALL, x + 42.0, y + 1.0, t.text_faint);
        y + 30.0
    }

    /// One response of the agent. Returns where it ends.
    fn draw_reply(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (index, reply, live): (usize, &Reply, bool),
        (x, y, width): (f32, f32, f32),
        (agent, files): ((&str, Color), &[FileCount]),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let mut y = self.draw_blocks(p, ui, (index, &reply.blocks, live), (x, y, width), agent, actions);
        if !reply.problem.is_empty() {
            let problem = p.layout(&reply.problem, theme::SMALL, Some(width - 28.0));
            icon::draw(p, icon::ALERT, (x + 8.0, y + 9.0), 14.0, t.danger);
            p.text(&problem, x + 24.0, y, t.danger);
            y += problem.height() + 12.0;
        }
        if !files.is_empty() {
            y = self.draw_touched(p, ui, (index, files), (x, y, width));
        }
        if !live {
            y = Self::draw_footer(p, ui, (index, reply), (x, y, width), actions);
        }
        y + 10.0
    }

    /// The replies of the run that ends with the reply at `index`: the
    /// replies that follow each other there, with nothing else between.
    fn run(entries: &[Entry], index: usize) -> impl Iterator<Item = &Reply> {
        fn reply(entry: &Entry) -> Option<&Reply> {
            if let Entry::Reply(reply) = entry { Some(reply) } else { None }
        }
        let before = entries.get(..index).unwrap_or_default();
        let start = index - before.iter().rev().take_while(|entry| reply(entry).is_some()).count();
        entries.get(start..=index).unwrap_or_default().iter().filter_map(reply)
    }

    /// The files the run that ends with the reply at `index` changed, with
    /// the weight of the calls they were gathered from: taken out of where
    /// they are kept (to be put back once drawn), gathered again when those
    /// calls changed. `None` for a run that changed no file.
    fn take_touched(&mut self, index: usize, entries: &[Entry]) -> Option<Gathered> {
        let weight: usize = Self::run(entries, index).map(|reply| change::weight(&reply.blocks)).sum();
        if weight == 0 {
            return None;
        }
        let kept = self.touched.remove(&index).filter(|(of, _)| *of == weight);
        let (_, files) = kept.unwrap_or_else(|| {
            let calls = Self::run(entries, index).flat_map(|reply| change::calls(&reply.blocks));
            let files = change::touched(calls).into_iter().map(|file| (file.path, file.added, file.removed));
            (weight, files.collect())
        });
        Some((weight, files)).filter(|(_, files)| !files.is_empty())
    }

    /// What the run that ends with the reply at `index` did to the file
    /// `path`, as its own edits say.
    pub fn run_change(&self, index: usize, path: &str) -> Option<change::Touched> {
        let calls = Self::run(&self.session.entries, index).flat_map(|reply| change::calls(&reply.blocks));
        change::touched(calls).into_iter().find(|file| file.path == path)
    }

    /// The files a run of replies changed, under it: each by its name with
    /// the lines added to it and removed from it, a click on one showing
    /// what the run did to it. Past [`TOUCHED_SHOWN`] the rest is counted,
    /// and listed under the mouse. Returns where they end.
    fn draw_touched(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (index, files): (usize, &[FileCount]),
        (x, y, width): (f32, f32, f32),
    ) -> f32 {
        let t = p.theme;
        icon::draw(p, icon::PEN, (x + 7.0, y + 12.0), 13.0, t.text_faint);
        let (start, right) = (x + 20.0, x + width);
        let (mut left, mut top) = (start, y);
        for (path, added, removed) in files.iter().take(TOUCHED_SHOWN) {
            let mut name = p.layout(folder_name(path), theme::SMALL, None);
            name.truncate(p.fonts, 220.0);
            let counted: f32 = [(*added, '+'), (*removed, '−')]
                .iter()
                .filter(|(count, _)| *count > 0)
                .map(|(count, sign)| p.layout(&format!("{sign}{count}"), theme::TINY, None).width() + 6.0)
                .sum();
            let wide = name.width() + counted + 18.0;
            if left + wide > right && left > start {
                (left, top) = (start, top + 26.0);
            }
            let chip = Rect::new(left, top, wide, 24.0);
            let hovered = ui.hovered(chip) && p.clip().contains(ui.mouse);
            let hover = ui.anim(id(("touched", index, path)), f32::from(u8::from(hovered)));
            p.rect(chip, fade(t.hover, hover), theme::RADIUS_SM);
            p.text(&name, chip.x + 8.0, chip.y + (chip.h - name.height()) * 0.5, mix(t.text_muted, t.text, hover));
            super::panel::counts(p, (*added, *removed), chip.right() - 8.0, chip.y + 6.0);
            ui.tip(id(("touched-path", index, path)), chip, || Tip::text(path.clone()));
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(chip) {
                    self.clicked_change = Some((index, path.clone()));
                }
            }
            left = chip.right() + 4.0;
        }
        if let Some(rest) = files.get(TOUCHED_SHOWN..).filter(|rest| !rest.is_empty()) {
            let more = p.layout(&format!("and {} more", rest.len()), theme::SMALL, None);
            if left + more.width() + 16.0 > right && left > start {
                (left, top) = (start, top + 26.0);
            }
            let label = Rect::new(left, top, more.width() + 16.0, 24.0);
            p.text(&more, label.x + 8.0, label.y + (label.h - more.height()) * 0.5, t.text_faint);
            let listed = |(path, added, removed): &FileCount| {
                let counts = match (added, removed) {
                    (added, 0) => format!("+{added}"),
                    (0, removed) => format!("−{removed}"),
                    (added, removed) => format!("+{added} −{removed}"),
                };
                (folder_name(path).to_owned(), counts)
            };
            ui.tip(id(("touched-more", index)), label, || Tip {
                text: String::new(),
                rows: rest.iter().map(listed).collect(),
            });
        }
        top + 24.0 + 10.0
    }

    /// The pieces of a reply, or of what a subagent said and did; `base`
    /// is the first half of their layout keys. Returns where they end.
    fn draw_blocks(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (base, blocks, live): (usize, &[Block], bool),
        (x, mut y, width): (f32, f32, f32),
        agent: (&str, Color),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        let last_block = blocks.len().saturating_sub(1);
        for (b, block) in blocks.iter().enumerate() {
            let key = (base, b);
            let writing = live && b == last_block;
            match block {
                Block::Text { text } if text.trim().is_empty() => {}
                Block::Text { text } => {
                    let height = self.markdown(p, key, text, width, INK_TEXT).height;
                    self.select(p, ui, key, (x, y), (width, height));
                    let copied =
                        self.copied.filter(|(k, _, at)| *k == key && ui.time - at < 1.5).map(|(_, code, _)| code);
                    if let Some(cached) = self.cache.get_mut(&key) {
                        let event = cached.doc.draw(p, ui, (x, y), t.text, true, copied);
                        if let Some(code) = event.copy_code
                            && let Some(source) = cached.doc.code(code)
                        {
                            actions.push(Action::Copy(source.to_owned()));
                            self.copied = Some((key, code, ui.time));
                        }
                        if let Some(link) = event.open_link {
                            actions.push(Action::OpenLink(link));
                        }
                        if let Some(picture) = event.open_picture {
                            self.clicked_picture = Some(picture);
                        }
                    }
                    y += height + 14.0;
                }
                Block::Thinking { text } => y = self.draw_thinking(p, ui, (key, text, writing), (x, y, width)),
                Block::Tool(call) if is_agent(call) => {
                    y = self.draw_agent(p, ui, (key, call, live), (x, y, width), agent.1);
                }
                Block::Tool(call) => y = self.draw_tool(p, ui, (key, call, live), (x, y, width), agent.1),
            }
        }
        if live && blocks.last().is_some_and(|block| matches!(block, Block::Tool(call) if call.output.is_some())) {
            // Between a tool's result and the agent's next words.
            y = self.draw_working(p, ui, (x, y + 2.0), agent.1);
        }
        y
    }

    /// A subagent, as a card in the conversation: its kind, what it was
    /// started for, and what it is doing now or how it ended. A click opens
    /// it in the conversation's place.
    fn draw_agent(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (key, call, live): ((usize, usize), &ToolUse, bool),
        (x, y, width): (f32, f32, f32),
        color: Color,
    ) -> f32 {
        let t = p.theme;
        let card = Rect::new(x - 8.0, y - 1.0, width + 16.0, 52.0);
        // Out of sight, a card is its height and nothing more.
        let view = p.clip();
        if card.bottom() < view.y || card.y > view.bottom() || view.w <= 0.0 {
            return card.bottom() + 11.0;
        }
        let brief = self.brief(call);
        let hovered = ui.hovered(card) && view.contains(ui.mouse);
        let hover = ui.anim(id(("agent-call", key)), f32::from(u8::from(hovered)));
        p.bordered(
            card,
            mix(fade(t.surface, 0.55), t.hover, hover),
            theme::RADIUS,
            1.0,
            mix(t.border, t.border_strong, hover),
        );
        icon::draw(p, icon::BOT, (card.x + 23.0, card.y + 26.0), 16.0, mix(t.text_muted, t.text, hover));
        let name_w = p.label(&brief.kind, theme::LABEL, card.x + 44.0, card.y + 9.0, t.text);
        let mut about = p.layout(&brief.description, theme::SMALL, None);
        about.truncate(p.fonts, (card.w - name_w - 104.0).max(0.0));
        p.text(&about, card.x + 44.0 + name_w + 10.0, card.y + 9.0, t.text_muted);
        let running = live && at_work(call);
        let doing = match (call.activity.as_str(), running, &call.output) {
            ("", true, _) => "Working…",
            ("", false, Some(_)) => "Done",
            ("", false, None) => "Stopped",
            (activity, ..) => activity,
        };
        let mut activity = p.layout(doing, theme::TINY, None);
        activity.truncate(p.fonts, card.w - 96.0);
        p.text(&activity, card.x + 44.0, card.y + 30.0, if call.is_error { t.danger } else { t.text_faint });
        let state = (card.right() - 21.0, card.y + 26.0);
        if running {
            spinner(p, ui, state, color);
        } else if call.is_error {
            icon::draw(p, icon::ALERT, state, 14.0, t.danger);
        } else {
            icon::draw(p, icon::CHEVRON_RIGHT, state, 14.0, mix(t.text_faint, t.text, hover));
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(card) {
                self.open_agent(Some(call.id.clone()));
                ui.animating = true;
            }
        }
        card.bottom() + 11.0
    }

    /// Under a finished reply: its model and tokens, and a copy button, both
    /// faint until the mouse comes near.
    fn draw_footer(
        p: &mut Painter,
        ui: &mut Ui,
        (index, reply): (usize, &Reply),
        (x, y, width): (f32, f32, f32),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let t = p.theme;
        if reply.blocks.iter().filter_map(words).all(|text| text.trim().is_empty()) {
            return y;
        }
        let row = Rect::new(x - 6.0, y - 4.0, width + 12.0, 26.0);
        // Out of sight, it is its height and nothing more.
        let view = p.clip();
        if row.bottom() < view.y || row.y > view.bottom() || view.w <= 0.0 {
            return y + 24.0;
        }
        let near = ui.anim(id(("reply-footer", index)), f32::from(u8::from(ui.hovered(row))));
        let copy = Rect::new(x - 5.0, y - 3.0, 24.0, 24.0);
        let over = ui.hovered(copy) && view.contains(ui.mouse);
        icon::draw(
            p,
            icon::COPY,
            (copy.x + 12.0, copy.y + 12.0),
            14.0,
            fade(if over { t.text } else { t.text_faint }, 0.35 + 0.65 * near),
        );
        if over {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(copy) {
                // What it said is put together when it is asked for, not at every frame.
                actions.push(Action::Copy(reply.blocks.iter().filter_map(words).collect::<Vec<_>>().join("\n\n")));
            }
        }
        let tokens = reply.usage.input_tokens.saturating_add(reply.usage.output_tokens);
        if tokens > 0 {
            let mut caption = reply.model.clone();
            if !caption.is_empty() {
                caption.push_str(" · ");
            }
            let _ = write!(caption, "{} in, {} out", reply.usage.input_tokens, reply.usage.output_tokens);
            if reply.usage.cached_tokens > 0 {
                let share = reply.usage.cached_tokens * 100 / reply.usage.input_tokens.max(1);
                let _ = write!(caption, " · {share}% cached");
            }
            if reply.cost > 0.0 {
                let _ = write!(caption, " · ${:.3}", reply.cost);
            }
            p.label(&caption, theme::TINY, x + 28.0, y + 2.0, fade(t.text_faint, near));
        }
        y + 24.0
    }

    /// The agent's reasoning: a line that opens to show it.
    fn draw_thinking(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (key, text, writing): ((usize, usize), &str, bool),
        (x, mut y, width): (f32, f32, f32),
    ) -> f32 {
        let t = p.theme;
        let open = self.open.contains(&key);
        let row = Rect::new(x - 6.0, y - 2.0, 150.0, 26.0);
        let hovered = ui.hovered(row) && p.clip().contains(ui.mouse);
        let hover = ui.anim(id(("thinking", key)), f32::from(u8::from(hovered)));
        // Its turn is running meanwhile, which draws the next frame.
        let color = if writing {
            mix(t.text_faint, t.text, ui.motion().map_or(0.4, |time| (time * 2.6).sin() * 0.35 + 0.4))
        } else {
            mix(t.text_faint, t.text_muted, hover)
        };
        icon::draw(p, icon::BRAIN, (x + 7.0, y + 11.0), 14.0, color);
        let label = if writing { "Thinking" } else { "Thought" };
        let label_w = p.label(label, theme::SMALL, x + 22.0, y + 2.0, color);
        if !text.trim().is_empty() {
            let glyph = if open { icon::CHEVRON_DOWN } else { icon::CHEVRON_RIGHT };
            icon::draw(p, glyph, (x + 22.0 + label_w + 11.0, y + 11.0), 13.0, fade(color, 0.8));
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) && !self.open.remove(&key) {
                    self.open.insert(key);
                }
            }
        }
        y += 28.0;
        if open {
            let height = self.markdown(p, key, text, width - 22.0, INK_MUTED).height;
            p.rect(Rect::new(x + 6.0, y, 2.0, height), t.border_strong, 1.0);
            self.mark_found(p, key, (x + 22.0, y));
            let doc = self.markdown(p, key, text, width - 22.0, INK_MUTED);
            if let Some(picture) = doc.draw(p, ui, (x + 22.0, y), t.text_muted, true, None).open_picture {
                self.clicked_picture = Some(picture);
            }
            y += height + 12.0;
        }
        y
    }

    /// A tool call: what it was, on what, and how it went; opens to its
    /// input and output. One that changes a file says how many lines it
    /// added and removed, and opens to those lines instead: what went, in
    /// the diff's red, and what came, in its green.
    fn draw_tool(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        (key, call, live): ((usize, usize), &ToolUse, bool),
        (x, mut y, width): (f32, f32, f32),
        color: Color,
    ) -> f32 {
        let t = p.theme;
        let open = self.open.contains(&key);
        let row = Rect::new(x - 8.0, y - 3.0, width + 16.0, 28.0);
        // A closed row out of sight is its height and nothing more.
        let view = p.clip();
        if !open && (row.bottom() < view.y || row.y > view.bottom() || view.w <= 0.0) {
            return y + 28.0;
        }
        let hovered = ui.hovered(row) && view.contains(ui.mouse);
        let hover = ui.anim(id(("tool", key)), f32::from(u8::from(hovered || open)));
        p.rect(row, fade(t.hover, hover * 0.8), theme::RADIUS_SM);
        let mid = y + 11.0;
        let (glyph, changed, mut about) = {
            let known = self.row(key, call);
            (known.icon, known.changed, p.layout(&known.about, Style::mono(12.5), None))
        };
        icon::draw(p, glyph, (x + 7.0, mid), 14.0, mix(t.text_faint, t.text_muted, hover));
        let name_w = p.label(&call.name, theme::LABEL, x + 22.0, y + 2.0, mix(t.text_muted, t.text, hover));
        // What it added and removed, before the mark of how it went: one that failed changed nothing.
        let counted = changed.filter(|_| !call.is_error).map_or(0.0, |lines| {
            let right = x + width - 24.0;
            right - super::panel::counts(p, lines, right, y + 5.0) + 8.0
        });
        // One the user said no to never ran: it says so, where a failure would show its warning.
        let refused = call.denied.then(|| p.layout("Not run", theme::TINY, None));
        let refused_w = refused.as_ref().map_or(0.0, |note| note.width() + 10.0);
        about.truncate(p.fonts, (width - name_w - 76.0 - counted - refused_w).max(0.0));
        p.text(&about, x + 22.0 + name_w + 10.0, y + 3.0, t.text_faint);
        // How it went, at the row's right.
        let state = (x + width - 8.0, mid);
        if let Some(note) = &refused {
            p.text(note, state.0 - 12.0 - note.width(), mid - note.height() * 0.5, t.text_faint);
        }
        match (&call.output, call.is_error) {
            _ if call.denied => icon::draw(p, icon::X, state, 13.0, t.text_faint),
            (None, _) if live => spinner(p, ui, state, color),
            (None, _) => icon::draw(p, icon::X, state, 13.0, t.text_faint),
            (Some(_), true) => icon::draw(p, icon::ALERT, state, 14.0, t.danger),
            (Some(_), false) => icon::draw(p, icon::CHECK, state, 14.0, fade(t.text_faint, 0.9)),
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(row) && !self.open.remove(&key) {
                self.open.insert(key);
            }
        }
        y += 28.0;
        if open && changed.is_some() {
            let lines = (key.0, usize::MAX / 2 - key.1);
            let height = self.diff(p, lines, call, width - 46.0).height;
            let frame = Rect::new(x + 22.0, y + 2.0, width - 22.0, height + 20.0);
            p.bordered(frame, t.code_bg, theme::RADIUS_SM, 1.0, t.border);
            self.mark_found(p, lines, (frame.x + 12.0, frame.y + 10.0));
            let doc = self.diff(p, lines, call, width - 46.0);
            doc.draw(p, ui, (frame.x + 12.0, frame.y + 10.0), t.text_muted, false, None);
            y = frame.bottom() + if call.is_error { 6.0 } else { 10.0 };
        }
        // The lines say what an edit did: of its answer, only why it failed is worth reading.
        if open && (changed.is_none() || call.is_error) {
            // An edit that said nothing has nothing to show here.
            let nothing = changed.is_some() && call.output.as_deref().is_none_or(str::is_empty);
            if !nothing {
                let said = (key.0, usize::MAX - key.1);
                let height = self.detail(p, said, call, changed.is_some(), width - 46.0).height;
                let frame = Rect::new(x + 22.0, y + 2.0, width - 22.0, height + 20.0);
                p.bordered(frame, t.code_bg, theme::RADIUS_SM, 1.0, t.border);
                let ink = if call.is_error { mix(t.text_muted, t.danger, 0.5) } else { t.text_muted };
                self.mark_found(p, said, (frame.x + 12.0, frame.y + 10.0));
                let doc = self.detail(p, said, call, changed.is_some(), width - 46.0);
                doc.draw(p, ui, (frame.x + 12.0, frame.y + 10.0), ink, false, None);
                y = frame.bottom() + 10.0;
            }
        }
        y
    }
}

/// The icon that says what kind of tool `name` is, whichever engine named it.
fn tool_icon(name: &str) -> char {
    let name = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| name.contains(word));
    if has(&["todo", "plan", "taskcreate", "taskupdate", "tasklist", "taskget"]) {
        icon::LIST
    } else if has(&["bash", "shell", "powershell", "command"]) {
        icon::TERMINAL
    } else if has(&["web", "fetch", "browser"]) {
        icon::GLOBE
    } else if has(&["write", "edit", "notebook"]) {
        icon::PEN
    } else if has(&["grep", "search", "glob", "find", "list"]) {
        icon::SEARCH
    } else if has(&["read", "file"]) {
        icon::FILE
    } else if has(&["task", "agent"]) {
        icon::BOT
    } else {
        icon::WRENCH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_get_icons_by_what_they_do() {
        assert_eq!(tool_icon("Bash"), icon::TERMINAL);
        assert_eq!(tool_icon("WebSearch"), icon::GLOBE);
        assert_eq!(tool_icon("search"), icon::SEARCH);
        assert_eq!(tool_icon("Read"), icon::FILE);
        assert_eq!(tool_icon("MultiEdit"), icon::PEN);
        assert_eq!(tool_icon("Task"), icon::BOT);
        assert_eq!((tool_icon("TaskUpdate"), tool_icon("TodoWrite")), (icon::LIST, icon::LIST));
        assert_eq!(tool_icon("mcp__thing__do"), icon::WRENCH);
    }

    #[test]
    fn the_turns_subagents_are_listed_with_how_each_is_doing() {
        let agent = |id: &str, about: &str, output: Option<&str>, activity: &str| {
            Block::Tool(ToolUse {
                id: id.into(),
                name: "Agent".into(),
                input: serde_json::json!({ "subagent_type": "Explore", "description": about }).to_string(),
                output: output.map(str::to_owned),
                activity: activity.into(),
                ..ToolUse::default()
            })
        };
        let reply = |blocks: Vec<Block>| Entry::from(Reply { blocks, ..Reply::default() });
        let mut thread = Thread::new(Session::new("a"));
        thread.session.prompt("first", "");
        let earlier = agent("old", "", Some("found"), "Done · 1 tool call · 2.0k tokens · 3s");
        thread.session.entries.push(reply(vec![earlier]));
        thread.session.prompt("second", "");
        thread.session.entries.push(reply(vec![
            agent("a1", "Find the escapes", None, "Reading src/tokenizer.rs · 2 tool calls"),
            agent("a2", "List the tests", Some("14 tests"), "Done · 3 tool calls · 28.8k tokens · 12s"),
        ]));
        // Sent to the background: its call returned at once, and it works on.
        let background = agent("a3", "Run the tests", Some("Started."), "cargo test · 1 tool call");
        // A command sent to the background, with the task news older sessions saved for it: no subagent.
        let command = Block::Tool(ToolUse {
            id: "b1".into(),
            name: "Bash".into(),
            output: Some("Command running in background".into()),
            activity: "Done · 0 tool calls · 0 tokens · 0s".into(),
            ..ToolUse::default()
        });
        thread.session.entries.push(reply(vec![background, command]));
        thread.busy = true;
        let listed = |thread: &Thread| -> Vec<(String, bool, bool)> {
            thread.subagents().into_iter().map(|agent| (agent.label, agent.running, agent.open)).collect()
        };
        let row = |label: &str, running, open| (label.to_owned(), running, open);
        assert_eq!(
            listed(&thread),
            [
                row("Find the escapes", true, false),
                row("List the tests", false, false),
                row("Run the tests", true, false)
            ],
            "the latest turn's, across its replies"
        );
        thread.open_agent(Some("a2".into()));
        assert_eq!(listed(&thread)[1], row("List the tests", false, true));
        // One of an earlier turn, opened from its card, lists that turn's; with no description, its kind.
        thread.open_agent(Some("old".into()));
        assert_eq!(listed(&thread), [row("Explore", false, true)]);
        // Nothing works once the turn is over.
        thread.open_agent(None);
        thread.busy = false;
        assert!(thread.subagents().iter().all(|agent| !agent.running));
        assert!(Thread::new(Session::new("a")).subagents().is_empty());
    }

    #[test]
    fn what_is_marked_runs_from_block_to_block() {
        // Dragged from the middle of the third block drawn up into the first.
        let marked = Marked { from: (2, (5, 0), (1, 4)), to: (0, (3, 0), (0, 7)) };
        let end = (3, 9);
        assert_eq!(marked.within(0, end), Some(((0, 7), end)), "from where the drag is to the block's end");
        assert_eq!(marked.within(1, end), Some(((0, 0), end)), "all of a block between");
        assert_eq!(marked.within(2, end), Some(((0, 0), (1, 4))), "up to where the drag began");
        assert_eq!(marked.within(3, end), None);
        // Within one block, either way round; nothing marked is nothing.
        let one = Marked { from: (1, (4, 0), (0, 9)), to: (1, (4, 0), (0, 2)) };
        assert_eq!(one.within(1, end), Some(((0, 2), (0, 9))));
        assert_eq!(Marked { to: one.from, ..one }.within(1, end), None);
    }

    #[test]
    fn a_turns_problem_lands_on_its_reply() {
        let mut thread = Thread::new(Session::new("a"));
        thread.session.prompt("hi", "");
        thread.begin_turn();
        thread.session.apply(&atroxabot_core::AgentEvent::Text("Half an ans".into()));
        thread.end_turn(Some("The connection dropped.".into()));
        assert!(!thread.busy && thread.started.is_none());
        // A turn the user stopped is not a failure, whatever the engine says.
        let mut stopped = Thread::new(Session::new("a"));
        stopped.session.prompt("hi", "");
        stopped.begin_turn();
        stopped.session.apply(&atroxabot_core::AgentEvent::Text("Half".into()));
        stopped.stopping = true;
        stopped.end_turn(Some("The turn ended with an error.".into()));
        assert!(matches!(stopped.session.entries.last(), Some(Entry::Reply(r)) if r.problem.is_empty()));
        assert!(
            matches!(thread.session.entries.last(), Some(Entry::Reply(r)) if r.problem == "The connection dropped.")
        );
    }
}
