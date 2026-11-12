//! The agent loop for API connections (Anthropic's Messages API, OpenAI's
//! Responses API and any OpenAI-compatible gateway): ask the model, run the
//! tools it calls, hand back their results, and go on until it answers
//! without calling any. Beside the workspace's tools the engine answers
//! its own: a task list (`todo`), questions to the user (`ask`) and
//! subagents (`agent`), which do their work in a conversation of their own,
//! side by side.
//!
//! Whatever the engine, a turn is reported as the same [`AgentEvent`]s, and
//! [`Session::apply`] turns those into the saved transcript, so the app
//! shows a Claude Agent session and an API session the same way.
//!
//! Token discipline: the system prompt (rendered once and kept with the
//! session) and tool list never change within a session and history is only
//! ever appended to, so every request reuses the provider's prompt cache and
//! a model's kept reasoning stays valid; tool outputs are bounded
//! (`tools.rs`); a reply goes back as it arrived where its wire can take it;
//! and a conversation that nears the model's context is summarised, by the
//! server on Anthropic's own API and by a request of ours elsewhere.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{Autonomy, FOLLOW_UP};
use crate::attach;
use crate::client::{Client, Model};
use crate::error::Error;
use crate::mcp::{McpHub, McpTool};
use crate::responses::{
    Completion, InputItem, Picture, ResponseRequest, Role, StreamEvent, ToolCall, ToolSpec, Usage, Wire,
};
use crate::session::{Block, ContextPart, Entry, Reply, Session, ToolUse};
use crate::tools::{self, Risk, Workspace};

/// Most model requests one turn may make before it is stopped.
const MAX_ROUNDS: usize = 200;
/// Most tool calls of one reply that run at the same time.
const SIDE_BY_SIDE: usize = 8;
/// Waits before a failed request is sent again, one per failure.
const RETRIES: [Duration; 4] =
    [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)];
/// How full the context may get before the conversation is summarised.
const COMPACT_AT: f64 = 0.75;
/// What an unanswered tool call is answered with when history is replayed.
const INTERRUPTED: &str = "The call was interrupted before it finished.";
/// The engine's own tools: the task list, questions, subagents.
const TODO: &str = "todo";
const ASK: &str = "ask";
const AGENT: &str = "agent";
/// The tool the app's question card answers (Claude's name for a question).
pub const QUESTION_TOOL: &str = "AskUserQuestion";
/// Most subagents that work at the same time.
const SUBAGENTS: usize = 4;
/// Most model requests one subagent may make.
const SUB_ROUNDS: usize = 80;
/// What a subagent is told before its brief.
const SUBAGENT_NOTE: &str = "(You are a subagent: another agent started you with the brief below and sees only \
    your final message. Do the work with your tools, then answer with everything it needs: what you found or did, \
    with paths and names. You cannot ask the user anything, keep a task list or start subagents.)";
/// How the ids of Anthropic's server tool calls start (never one of ours to answer).
const SERVER_CALL: &str = "server-";
/// What a result that showed a picture says once the model turned out to take none.
const UNSEEN: &str = "\n(This model cannot look at pictures: it was not shown.)";
/// What follows a summary made while the agent was at work.
const CARRY_ON: &str = "\n\n(The conversation was summarised to make room while you worked: carry on from where \
    the summary leaves off.)";

/// What the app tells the model to write when it summarises a conversation.
const SUMMARY_PROMPT: &str = "The conversation is getting long. Write a summary of it to continue from, for \
    yourself: what the user wants, what was decided, what you did (files, commands, results), what is still open, \
    and anything the user said about how they want things done. Be specific: names, paths and numbers. Write only \
    the summary.";

/// An update from a running turn, the same for every engine.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The model began a response.
    ReplyStarted,
    /// A turn is under way, one Claude Code started by itself included
    /// (after a task sent to the background ended); nothing to record yet.
    Working,
    /// More of its answer.
    Text(String),
    /// More of its reasoning.
    Thinking(String),
    /// It called a tool; the result follows in [`AgentEvent::ToolResult`].
    ToolCall(ToolUse),
    /// A tool call's result shows the model a picture; its
    /// [`AgentEvent::ToolResult`] follows.
    ToolPicture {
        /// The call's id.
        id: String,
        /// The picture.
        picture: Picture,
    },
    /// A tool call finished.
    ToolResult {
        /// The call's id.
        id: String,
        /// What it returned, or why it failed.
        output: String,
        /// Whether it failed or was refused.
        is_error: bool,
    },
    /// Something a subagent said or did: `event` (text, reasoning or a tool
    /// call) belongs inside the tool call `parent` that started it.
    Sub {
        /// The id of the tool call that started the subagent.
        parent: String,
        /// What it said or did.
        event: Box<AgentEvent>,
    },
    /// What the subagent started by the tool call `id` is doing now, or how it ended.
    Activity {
        /// The tool call's id.
        id: String,
        /// One line on it.
        text: String,
    },
    /// The response is complete.
    ReplyEnded {
        /// The reply as its wire sent it, to replay; empty for the account engines.
        raw: Vec<Value>,
        /// The wire `raw` came by.
        wire: Wire,
        /// The model that wrote it.
        model: String,
        /// What it used.
        usage: Usage,
        /// Its cost in USD, when known.
        cost: f64,
        /// Why it ended badly; empty when it finished.
        problem: String,
    },
    /// Everything before the latest prompt was summarised to this.
    Summarized(String),
    /// A tool call needs the user's approval: answer with a [`Verdict`].
    Permission(Ask),
    /// A pending approval is no longer needed.
    PermissionGone(String),
    /// A passing note on what is happening (a retry, a compaction).
    Status(String),
    /// A line to keep between the messages (the context was summarised).
    Note(String),
    /// How many tokens the model's context holds.
    Window(u64),
    /// Where the account's plan limits stand.
    Limits(Limits),
    /// The account's usage limit stopped the turn, which ends next. When
    /// the limit lifts is in the account's [`Limits`], if its provider said.
    LimitReached,
    /// What the context holds, by kind (system prompt, tools, messages, …).
    Context(Vec<ContextPart>),
    /// The Claude Agent's own session id, to resume it later.
    Remote(String),
    /// The turn took in a message the user sent while it ran: the model
    /// reads it here, after its latest tool results, and carries on.
    FollowUp {
        /// The id it was sent under ([`FollowUp::id`]).
        id: String,
        /// What the user wrote.
        text: String,
        /// The app's line before it, for the model; empty when there is none.
        note: String,
        /// The files that go with it.
        files: Vec<String>,
    },
    /// How many messages the user sent during the turn are still to be
    /// answered, in turns of their own that follow at once. Comes just
    /// before [`AgentEvent::TurnEnded`], and only when there are some.
    Queued(u64),
    /// What the user may write next, as Claude Code guesses it. Comes a
    /// moment after [`AgentEvent::TurnEnded`], and not after every turn;
    /// nothing of it is kept in the session.
    Suggestion(String),
    /// The turn is over; `problem` says why if it did not end well.
    TurnEnded {
        /// What went wrong, if anything.
        problem: Option<String>,
    },
    /// The Claude Agent's process ended; the next message starts it again.
    Disconnected(Option<String>),
    /// The system prompt the session's API engine was started with, kept
    /// with the session from its first turn ([`Session::system`]).
    System(String),
    /// Anthropic refused the reasoning kept with the replies (the
    /// conversation changed under it): they go back as plain text and calls
    /// from now on.
    ForgetReplays,
    /// The provider refused a picture: the model cannot look at any. The
    /// pictures results showed are taken out, each result saying so, and
    /// none is shown from now on ([`Session::blind`]).
    Blind,
}

/// A message the user sent while a turn was running, for the engine to
/// read at its next step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowUp {
    /// Its id, which the app follows it by until the turn takes it in.
    pub id: String,
    /// What the user wrote.
    pub text: String,
    /// The files that go with it.
    pub files: Vec<String>,
}

/// Where an account's plan limits stand, as its provider reports them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// When this was last reported, in Unix seconds.
    pub at: u64,
    /// Whether the account can go on: `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    /// When the limit that was hit lifts, in Unix seconds; 0 when not said.
    pub resets: u64,
    /// Each window of the plan.
    pub windows: Vec<LimitWindow>,
}

/// One window of a plan's limits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitWindow {
    /// What the provider calls it: `five_hour`, `seven_day`, ...
    pub name: String,
    /// How much of it is used, `0..=1`.
    pub used: f64,
    /// When it starts over, in Unix seconds; 0 when not said.
    pub resets: u64,
}

/// A tool call waiting for the user's approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    /// What a [`Verdict`] refers to.
    pub id: String,
    /// The tool.
    pub tool: String,
    /// Its arguments as JSON text.
    pub input: String,
    /// The path, command or address it is about.
    pub summary: String,
    /// Why it needs approval, when the engine says.
    pub reason: String,
}

/// The user's answer to an [`Ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Run it, and stop asking about this kind of call in this session.
    AllowAlways,
    /// Run it with this input instead (JSON text): the user's answers to a question.
    Answer(String),
    /// Do not run it; the text tells the model why.
    Deny(String),
}

impl Session {
    /// Records what `event` adds to the conversation. Events that are not
    /// part of the transcript (approvals, status notes) change nothing.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ReplyStarted => self.entries.push(Entry::Reply(Reply::default())),
            AgentEvent::Text(more) => self.with_reply(|reply| match reply.blocks.last_mut() {
                Some(Block::Text { text }) => text.push_str(more),
                _ => reply.blocks.push(Block::Text { text: more.clone() }),
            }),
            AgentEvent::Thinking(more) => self.with_reply(|reply| match reply.blocks.last_mut() {
                Some(Block::Thinking { text }) => text.push_str(more),
                _ => reply.blocks.push(Block::Thinking { text: more.clone() }),
            }),
            AgentEvent::ToolCall(call) => self.with_reply(|reply| reply.blocks.push(Block::Tool(call.clone()))),
            AgentEvent::ToolPicture { id, picture } => {
                if let Some(call) = self.find_tool(id) {
                    call.picture = Some(picture.clone());
                }
            }
            AgentEvent::ToolResult { id, output, is_error } => {
                if let Some(call) = self.find_tool(id) {
                    call.output = Some(output.clone());
                    call.is_error = *is_error;
                }
            }
            AgentEvent::Sub { parent, event } => {
                let Some(call) = self.find_tool(parent) else { return };
                // Its words and reasoning arrive a piece at a time, as the agent's own do.
                match (event.as_ref(), call.sub.last_mut()) {
                    (AgentEvent::Text(more), Some(Block::Text { text }))
                    | (AgentEvent::Thinking(more), Some(Block::Thinking { text })) => text.push_str(more),
                    (AgentEvent::Text(text), _) => call.sub.push(Block::Text { text: text.clone() }),
                    (AgentEvent::Thinking(text), _) => call.sub.push(Block::Thinking { text: text.clone() }),
                    (AgentEvent::ToolCall(inner), _) => call.sub.push(Block::Tool(inner.clone())),
                    _ => return,
                }
            }
            AgentEvent::Activity { id, text } => {
                let Some(call) = self.find_tool(id) else { return };
                call.activity.clone_from(text);
            }
            AgentEvent::Note(text) => self.entries.push(Entry::Note { text: text.clone() }),
            AgentEvent::Window(window) => self.window = *window,
            // Not a change to the conversation: what it fills is only noted.
            AgentEvent::Context(parts) => {
                self.context.clone_from(parts);
                return;
            }
            AgentEvent::ReplyEnded { raw, wire, model, usage, cost, problem } => self.with_reply(|reply| {
                reply.raw.clone_from(raw);
                reply.wire = *wire;
                reply.model.clone_from(model);
                reply.usage = *usage;
                reply.cost = *cost;
                reply.problem.clone_from(problem);
                reply.at = crate::session::unix_now();
            }),
            AgentEvent::Summarized(text) => {
                // Before the prompt being answered, which the summary does not
                // cover; once the work on it is under way, after everything.
                let at = if self.working() {
                    self.entries.len()
                } else {
                    self.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(self.entries.len())
                };
                self.entries.insert(at, Entry::Summary { text: text.clone() });
            }
            AgentEvent::System(text) => self.system.clone_from(text),
            AgentEvent::ForgetReplays => {
                for entry in &mut self.entries {
                    if let Entry::Reply(reply) = entry
                        && reply.wire == Wire::Messages
                    {
                        reply.raw.clear();
                    }
                }
            }
            AgentEvent::Blind => {
                fn unseen(blocks: &mut [Block]) {
                    for block in blocks {
                        if let Block::Tool(call) = block {
                            if call.picture.take().is_some()
                                && let Some(output) = &mut call.output
                            {
                                output.push_str(UNSEEN);
                            }
                            unseen(&mut call.sub);
                        }
                    }
                }
                self.blind = true;
                for entry in &mut self.entries {
                    if let Entry::Reply(reply) = entry {
                        unseen(&mut reply.blocks);
                    }
                }
            }
            AgentEvent::Remote(id) => self.remote.clone_from(id),
            AgentEvent::FollowUp { text, note, files, .. } => {
                self.entries.push(Entry::User { text: text.clone(), note: note.clone(), files: files.clone() });
            }
            AgentEvent::Queued(_)
            | AgentEvent::Suggestion(_)
            | AgentEvent::Working
            | AgentEvent::Permission(_)
            | AgentEvent::PermissionGone(_)
            | AgentEvent::Status(_)
            | AgentEvent::Limits(_)
            | AgentEvent::LimitReached
            | AgentEvent::TurnEnded { .. }
            | AgentEvent::Disconnected(_) => return,
        }
        self.updated = crate::session::unix_now();
    }

    /// Changes the reply being written: the last entry, started if it is not one.
    fn with_reply(&mut self, change: impl FnOnce(&mut Reply)) {
        if !matches!(self.entries.last(), Some(Entry::Reply(_))) {
            self.entries.push(Entry::Reply(Reply::default()));
        }
        if let Some(Entry::Reply(reply)) = self.entries.last_mut() {
            change(reply);
        }
    }

    /// Whether the latest prompt is being (or was) answered: a reply with
    /// something in it follows it.
    #[must_use]
    pub fn working(&self) -> bool {
        let prompt = self.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).map_or(0, |at| at + 1);
        self.entries
            .get(prompt..)
            .unwrap_or_default()
            .iter()
            .any(|entry| matches!(entry, Entry::Reply(reply) if !reply.blocks.is_empty()))
    }

    /// Adds the user's message, naming the session after the first one.
    pub fn prompt(&mut self, text: &str, note: &str) {
        if self.title.is_empty() {
            let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default().trim();
            self.title = line.chars().take(60).collect();
        }
        self.entries.push(Entry::User { text: text.to_owned(), note: note.to_owned(), files: Vec::new() });
        self.updated = crate::session::unix_now();
    }
}

/// Lets the app stop a turn, answer its approvals and hand it the user's
/// follow-ups from another thread.
#[derive(Debug, Clone)]
pub struct Control {
    cancel: Arc<AtomicBool>,
    verdicts: mpsc::Sender<(String, Verdict)>,
    follow_ups: Arc<Mutex<Vec<FollowUp>>>,
}

impl Control {
    /// A control and the receiving end the turn listens on.
    #[must_use]
    pub fn new() -> (Self, Inbox) {
        let (verdicts, inbox) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let follow_ups = Arc::default();
        let control = Self { cancel: Arc::clone(&cancel), verdicts, follow_ups: Arc::clone(&follow_ups) };
        let inbox = Inbox { cancel, verdicts: Mutex::new(inbox), answered: Mutex::default(), follow_ups };
        (control, inbox)
    }

    /// Hands the turn a message the user sent while it runs. It reads it
    /// after the tool calls in progress, as an [`AgentEvent::FollowUp`]; one
    /// it never reached is not reported, and the app sends it again.
    pub fn follow_up(&self, message: FollowUp) {
        self.follow_ups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(message);
    }

    /// Stops the turn: the request is dropped and a running command killed.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Answers the approval `id`.
    pub fn decide(&self, id: &str, verdict: Verdict) {
        // Fails only if the turn is already over.
        let _ = self.verdicts.send((id.to_owned(), verdict));
    }
}

/// The turn's end of a [`Control`]. Subagents working side by side share
/// it: each waits for the answers to its own approvals.
#[derive(Debug)]
pub struct Inbox {
    cancel: Arc<AtomicBool>,
    verdicts: Mutex<mpsc::Receiver<(String, Verdict)>>,
    /// Answers that came in while another approval was being waited for.
    answered: Mutex<HashMap<String, Verdict>>,
    follow_ups: Arc<Mutex<Vec<FollowUp>>>,
}

impl Inbox {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The follow-ups handed over so far, oldest first; they are the turn's from here on.
    fn take_follow_ups(&self) -> Vec<FollowUp> {
        std::mem::take(&mut *self.follow_ups.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Waits for the answer to approval `id`; `None` if the turn was stopped.
    fn verdict(&self, id: &str) -> Option<Verdict> {
        loop {
            if self.stopped() {
                return None;
            }
            {
                let verdicts = self.verdicts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut answered = self.answered.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut gone = false;
                loop {
                    match verdicts.try_recv() {
                        Ok((answering, verdict)) => {
                            answered.insert(answering, verdict);
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            gone = true;
                            break;
                        }
                    }
                }
                if let Some(verdict) = answered.remove(id) {
                    return Some(verdict);
                }
                if gone {
                    return None;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Sleeps for `wait` unless the turn is stopped first; `false` if it was.
    fn pause(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.stopped()
    }
}

/// Everything a turn through an API needs.
#[derive(Debug, Clone)]
pub struct ApiEngine {
    /// The provider.
    pub client: Client,
    /// Model identifier.
    pub model: String,
    /// Reasoning effort, when the agent sets one.
    pub effort: Option<String>,
    /// The session's system prompt: the one it was started with
    /// ([`Session::system`]), or for its first turn a new one.
    pub system: String,
    /// Where the agent works.
    pub workspace: Workspace,
    /// How much it may do without asking.
    pub autonomy: Autonomy,
    /// Let Claude use Anthropic's web search and fetch.
    pub web: bool,
    /// The model's prices and context window, when the provider lists them.
    pub pricing: Option<Model>,
    /// Tools approved for the rest of the session, shared between its turns.
    pub approved: Arc<Mutex<HashSet<String>>>,
    /// The user's MCP servers, whose tools the model is offered too.
    pub mcp: Option<McpHub>,
}

/// The engine's own tools, beside the workspace's: a task list, questions
/// to the user, and subagents. Their names and descriptions, in a fixed
/// order like the workspace's (see [`tools::definitions`]).
#[must_use]
pub fn engine_tools() -> Vec<(&'static str, &'static str, Value)> {
    let text = |description: &str| json!({ "type": "string", "description": description });
    let object = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false });
    let todo = object(
        json!({
            "content": text("The task, as an order: `Fix the parser`."),
            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
            "activeForm": text("The same while it is being done: `Fixing the parser`."),
        }),
        &["content", "status"],
    );
    let option = object(
        json!({ "label": text("The choice, in one to five words."), "description": text("What picking it means.") }),
        &["label"],
    );
    let question = object(
        json!({
            "question": text("The question, ending with a question mark."),
            "header": text("A word or two for it, at most 12 characters."),
            "options": { "type": "array", "description": "Two to four choices; the user can also answer in their own words.", "items": option },
            "multiSelect": { "type": "boolean", "description": "Whether several choices may be picked." },
        }),
        &["question", "header", "options"],
    );
    vec![
        (
            TODO,
            "Keep your task list, for work of three or more steps; the user sees it beside the conversation. Send the whole list every time, each task with its status. Mark a task in progress when you start it and completed as soon as it is done, one in progress at a time.",
            object(json!({ "todos": { "type": "array", "items": todo } }), &["todos"]),
        ),
        (
            ASK,
            "Ask the user when a decision is theirs to make, or when they ask you to interview them: one to four questions, each with a few options to pick from. Waits for their answers. Do not ask what you can find out yourself.",
            object(json!({ "questions": { "type": "array", "items": question } }), &["questions"]),
        ),
        (
            AGENT,
            "Start a subagent: a copy of you with a fresh context and your tools (no questions, task list or subagents of its own), which works on a brief and answers with what it found or did. Use it for broad searches and for pieces of work that do not depend on each other; several started in one reply work side by side. Its work does not fill your context, only its answer does. It knows nothing of this conversation: give it a complete brief.",
            object(
                json!({
                    "description": text("What it is for, in three to five words."),
                    "prompt": text("The brief: the goal, what you already know, where to look, and what to answer with."),
                }),
                &["description", "prompt"],
            ),
        ),
    ]
}

impl ApiEngine {
    /// Runs one turn: answers the latest prompt in `session`, applying every
    /// event to it and passing it to `emit`. Blocks until the turn ends;
    /// always ends with [`AgentEvent::TurnEnded`].
    pub fn run(&self, session: &mut Session, inbox: &Inbox, emit: &mut dyn FnMut(AgentEvent)) {
        let mut emit = |session: &mut Session, event: AgentEvent| {
            session.apply(&event);
            emit(event);
        };
        let problem = self.turn(session, inbox, &mut emit).err();
        emit(session, AgentEvent::Status(String::new()));
        emit(session, AgentEvent::TurnEnded { problem });
    }

    /// Sets the turn up (its system prompt kept with the session, its tools)
    /// and does its work; `Err` carries what to tell the user.
    fn turn(
        &self,
        session: &mut Session,
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<(), String> {
        // Rendered once and kept: a prompt that changed under earlier replies
        // would cost the cache, and Claude's reasoning with it.
        if session.system != self.system {
            emit(session, AgentEvent::System(self.system.clone()));
        }
        // The app's own tools, then those of the user's MCP servers: always in that order.
        let mut remote = Vec::new();
        if let Some(hub) = &self.mcp {
            // Only the first turn waits for the servers to start.
            if hub.cold() {
                emit(session, AgentEvent::Status("Starting the MCP servers…".to_owned()));
            }
            let (tools, problems) = hub.tools();
            remote = tools;
            // A server that is down costs the agent its tools, not the turn.
            emit(session, AgentEvent::Status(problems.join(" ")));
        }
        let definitions: Vec<_> = tools::definitions().into_iter().chain(engine_tools()).collect();
        // The skills are those of the session's start, so the tool list stays as it was.
        let skill = crate::skills::tool(&self.workspace.skills);
        let specs: Vec<ToolSpec<'_>> = definitions
            .iter()
            .map(|(name, description, parameters)| ToolSpec { name, description, parameters })
            .chain(skill.iter().map(|(description, parameters)| ToolSpec {
                name: crate::skills::TOOL,
                description,
                parameters,
            }))
            .chain(remote.iter().map(|tool| ToolSpec {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.schema,
            }))
            .collect();
        // How much of each kind the request holds, in characters: what tells its tokens apart.
        let memory = [crate::agent::PROJECT_HEADING, crate::agent::NOTES_HEADING]
            .iter()
            .filter_map(|heading| self.system.find(heading))
            .min()
            .unwrap_or(self.system.len());
        let (prompt, notes) = self.system.split_at(memory);
        let (own, rest) = specs.split_at(definitions.len().min(specs.len()));
        let (skills, theirs) = rest.split_at(skill.iter().count().min(rest.len()));
        let fixed = [
            ("System prompt", prompt.chars().count()),
            ("System tools", spec_size(own)),
            ("MCP tools", spec_size(theirs)),
            ("Skills", spec_size(skills)),
            ("Memory files", notes.chars().count()),
        ];
        self.work(session, &specs, &remote, inbox, emit, Some(&fixed))
    }

    /// The loop of one conversation: ask the model, run the tools it calls,
    /// hand back their results, until it answers without calling any. A
    /// turn's (with its context's `fixed` parts: what it holds is reported,
    /// and the user's follow-ups are taken in) or a subagent's (`None`).
    fn work(
        &self,
        session: &mut Session,
        specs: &[ToolSpec<'_>],
        remote: &[McpTool],
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
        fixed: Option<&[(&str, usize)]>,
    ) -> Result<(), String> {
        let nested = fixed.is_none();
        let mut window = self.window(session);
        // Summarising is tried once until a reply shows it helped.
        let mut may_compact = true;
        for _ in 0..if nested { SUB_ROUNDS } else { MAX_ROUNDS } {
            if inbox.stopped() {
                return Ok(());
            }
            // Anthropic's own API summarises on its side (see `messages.rs`).
            let full = window > 0 && session.context_tokens() as f64 > window as f64 * COMPACT_AT;
            if full && !self.client.first_party && may_compact {
                may_compact = false;
                self.compact(session, inbox, emit)?;
            }
            let completion = match self.respond(session, specs, inbox, emit) {
                Ok(Some(completion)) => completion,
                Ok(None) => return Ok(()),
                Err(e) if e.is_context_overflow() && may_compact => {
                    // A provider that says how large the window is is believed from now on.
                    if let Some(limit) = e.context_limit() {
                        window = limit;
                        if !nested {
                            emit(session, AgentEvent::Window(limit));
                        }
                    }
                    may_compact = false;
                    self.compact(session, inbox, emit)?;
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
            may_compact = true;
            let problem = match completion.incomplete.as_deref() {
                None => String::new(),
                Some("refusal") => "The model declined to answer this.".to_owned(),
                Some("max_output_tokens") => "The reply was cut off at the model's output limit.".to_owned(),
                Some(other) => format!("The reply stopped early ({other})."),
            };
            // A cut-off reply's calls may be half-written: they never run,
            // and its blocks are not replayed (they would lack results).
            let finished = problem.is_empty();
            let calls = if finished { completion.tool_calls } else { Vec::new() };
            for call in &calls {
                let call = ToolUse {
                    id: call.call_id.clone(),
                    name: call.name.clone(),
                    input: call.arguments.clone(),
                    ..ToolUse::default()
                };
                emit(session, AgentEvent::ToolCall(call));
            }
            let cost = self.pricing.as_ref().map_or(0.0, |model| model.cost(completion.usage));
            let raw = if finished { completion.content } else { Vec::new() };
            emit(
                session,
                AgentEvent::ReplyEnded {
                    raw,
                    wire: self.client.wire(&self.model),
                    model: self.model.clone(),
                    usage: completion.usage,
                    cost,
                    problem: problem.clone(),
                },
            );
            if let Some(fixed) = fixed {
                emit(session, AgentEvent::Context(context_parts(fixed, session.context_tokens())));
            }
            if !finished {
                return Err(problem);
            }
            if calls.is_empty() && !completion.paused {
                // What the user sent meanwhile is answered in this turn.
                if !nested && Self::take_follow_ups(session, inbox, emit) {
                    continue;
                }
                return Ok(());
            }
            self.run_calls(&calls, specs, remote, inbox, session, emit, nested);
            // What the user sent while the tools ran goes after their results.
            if !nested {
                Self::take_follow_ups(session, inbox, emit);
            }
        }
        if nested {
            return Err(format!("The subagent stopped after {SUB_ROUNDS} steps without finishing."));
        }
        Err(format!("Stopped after {MAX_ROUNDS} steps in one turn. Say \"continue\" to let it go on."))
    }

    /// How many tokens the model's context holds, as far as known: as the
    /// provider lists it, or as an overflow said while the session talked
    /// to this model; 0 when not known.
    fn window(&self, session: &Session) -> u64 {
        let listed = self.pricing.as_ref().map_or(0, |model| model.context_window);
        if listed > 0 {
            return listed;
        }
        let latest = session.entries.iter().rev().find_map(|entry| match entry {
            Entry::Reply(reply) if !reply.model.is_empty() => Some(reply.model.as_str()),
            _ => None,
        });
        if latest == Some(self.model.as_str()) { session.window } else { 0 }
    }

    /// Runs the calls of one reply and reports each one's result, in the
    /// order they were made. Calls that only look and follow one another
    /// run side by side, as nothing they do can change what another sees,
    /// and so do subagents started one after another; any other call runs
    /// alone, after those before it. In a subagent (`nested`), the engine's
    /// own tools are refused.
    #[expect(clippy::too_many_arguments, reason = "the loop's state, passed on as it is")]
    fn run_calls(
        &self,
        calls: &[ToolCall],
        specs: &[ToolSpec<'_>],
        remote: &[McpTool],
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
        nested: bool,
    ) {
        let cancel: &AtomicBool = &inbox.cancel;
        let ours = |call: &ToolCall| !remote.iter().any(|tool| tool.name == call.name);
        let mut rest = calls;
        while let Some((call, after)) = rest.split_first() {
            if call.name == AGENT && ours(call) && !nested && !inbox.stopped() {
                let starting: Vec<&ToolCall> =
                    rest.iter().take(SUBAGENTS).take_while(|call| call.name == AGENT && ours(call)).collect();
                self.run_agents(&starting, specs, remote, inbox, session, emit);
                rest = rest.get(starting.len()..).unwrap_or_default();
                continue;
            }
            let looking: Vec<(&ToolCall, Value)> = rest
                .iter()
                .take(SIDE_BY_SIDE)
                .map_while(|call| self.looks(call, remote).map(|input| (call, input)))
                .collect();
            if looking.len() < 2 || inbox.stopped() {
                let result = self.execute(call, remote, inbox, session, emit, nested);
                self.report(call, result, session, emit);
                rest = after;
                continue;
            }
            let outputs: Vec<Result<String, String>> = std::thread::scope(|scope| {
                let running: Vec<_> = looking
                    .iter()
                    .map(|(call, input)| scope.spawn(|| tools::run(&call.name, input, &self.workspace, cancel)))
                    .collect();
                running
                    .into_iter()
                    .map(|run| run.join().unwrap_or_else(|_| Err("The tool stopped unexpectedly.".to_owned())))
                    .collect()
            });
            for ((call, _), result) in looking.iter().zip(outputs) {
                self.report(call, result, session, emit);
            }
            rest = rest.get(looking.len()..).unwrap_or_default();
        }
    }

    /// Reports what `call` came to: the picture a `read` showed, when it
    /// showed one, then its result.
    fn report(
        &self,
        call: &ToolCall,
        result: Result<String, String>,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) {
        if result.is_ok() && call.name == "read" && !session.blind {
            let input = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
            if let Some(picture) = tools::picture(&input, &self.workspace) {
                emit(session, AgentEvent::ToolPicture { id: call.call_id.clone(), picture });
                let output = format!("{} It is shown to you with this result.", result.unwrap_or_default());
                emit(session, AgentEvent::ToolResult { id: call.call_id.clone(), output, is_error: false });
                return;
            }
        }
        let (output, is_error) = match result {
            Ok(output) => (output, false),
            Err(output) => (output, true),
        };
        emit(session, AgentEvent::ToolResult { id: call.call_id.clone(), output, is_error });
    }

    /// Runs the subagents `starting` asks for, side by side, passing on what
    /// each says and does as it happens, and reports each one's answer as
    /// its call's result.
    fn run_agents(
        &self,
        starting: &[&ToolCall],
        specs: &[ToolSpec<'_>],
        remote: &[McpTool],
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) {
        let key = session.id.clone();
        let (events, heard) = mpsc::channel::<AgentEvent>();
        let answers: Vec<Result<String, String>> = std::thread::scope(|scope| {
            let running: Vec<_> = starting
                .iter()
                .map(|call| {
                    let events = events.clone();
                    let key = key.as_str();
                    scope.spawn(move || self.subagent(call, specs, remote, inbox, &events, key))
                })
                .collect();
            // Every subagent holds a sender until it is done: the last one ends this.
            drop(events);
            for event in heard {
                emit(session, event);
            }
            running
                .into_iter()
                .map(|run| run.join().unwrap_or_else(|_| Err("The subagent stopped unexpectedly.".to_owned())))
                .collect()
        });
        for (call, answer) in starting.iter().zip(answers) {
            let (output, is_error) = match answer {
                Ok(output) => (output, false),
                Err(output) => (output, true),
            };
            let ended = if inbox.stopped() {
                "Stopped"
            } else if is_error {
                "Failed"
            } else {
                "Done"
            };
            emit(session, AgentEvent::Activity { id: call.call_id.clone(), text: ended.to_owned() });
            emit(session, AgentEvent::ToolResult { id: call.call_id.clone(), output: bounded(&output), is_error });
        }
    }

    /// One subagent's work on the brief `call` gave it, in a conversation
    /// of its own that shares the turn's system prompt and tools (and so
    /// the provider's cache of them). What it says and does goes out on
    /// `events`, as part of `call`; its answer is what its last reply said.
    fn subagent(
        &self,
        call: &ToolCall,
        specs: &[ToolSpec<'_>],
        remote: &[McpTool],
        inbox: &Inbox,
        events: &mpsc::Sender<AgentEvent>,
        key: &str,
    ) -> Result<String, String> {
        let input = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
        let brief = input.get("prompt").and_then(Value::as_str).filter(|brief| !brief.trim().is_empty());
        let brief = brief.ok_or_else(|| "missing `prompt`: the subagent's brief".to_owned())?;
        let mut sub = Session::new("subagent");
        // The turn's own key: its requests are kept on the machines that cache the shared prefix.
        key.clone_into(&mut sub.id);
        sub.prompt(brief, SUBAGENT_NOTE);
        let parent = call.call_id.clone();
        let mut emit = |sub: &mut Session, event: AgentEvent| {
            sub.apply(&event);
            let passed = match event {
                AgentEvent::ToolCall(ref inner) => {
                    let summary = tools::summary(&serde_json::from_str(&inner.input).unwrap_or(Value::Null));
                    let doing =
                        if summary.is_empty() { inner.name.clone() } else { format!("{} {summary}", inner.name) };
                    let _ = events.send(AgentEvent::Activity { id: parent.clone(), text: doing });
                    Some(AgentEvent::Sub { parent: parent.clone(), event: Box::new(event) })
                }
                // Its usage is logged apart (it is no reply of the session's).
                AgentEvent::Text(_) | AgentEvent::Thinking(_) | AgentEvent::ReplyEnded { .. } => {
                    Some(AgentEvent::Sub { parent: parent.clone(), event: Box::new(event) })
                }
                AgentEvent::ToolResult { .. }
                | AgentEvent::ToolPicture { .. }
                | AgentEvent::Permission(_)
                | AgentEvent::PermissionGone(_) => Some(event),
                AgentEvent::Status(text) if !text.is_empty() => Some(AgentEvent::Activity { id: parent.clone(), text }),
                _ => None,
            };
            if let Some(event) = passed {
                let _ = events.send(event);
            }
        };
        self.work(&mut sub, specs, remote, inbox, &mut emit, None)?;
        if inbox.stopped() {
            return Err("Stopped by the user.".to_owned());
        }
        let answer = sub
            .entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::Reply(reply) => Some(
                    reply
                        .blocks
                        .iter()
                        .filter_map(|block| if let Block::Text { text } = block { Some(text.as_str()) } else { None })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        if answer.trim().is_empty() {
            return Err("The subagent ended without an answer.".to_owned());
        }
        Ok(answer)
    }

    /// The arguments of `call` when it only looks and needs no asking (a
    /// `read`, `list` or `search` of what the agent may see): such a call
    /// can run beside others.
    fn looks(&self, call: &ToolCall, remote: &[McpTool]) -> Option<Value> {
        let own = matches!(call.name.as_str(), "read" | "list" | "search")
            && !remote.iter().any(|tool| tool.name == call.name);
        let input = serde_json::from_str::<Value>(&call.arguments).ok().filter(|input| own && input.is_object())?;
        (tools::risk(&call.name, &input, &self.workspace) == Risk::Safe).then_some(input)
    }

    /// Takes the user's follow-ups into the conversation, after what is
    /// there, unless the turn was stopped (the app then sends them again).
    /// Returns whether there were any.
    fn take_follow_ups(session: &mut Session, inbox: &Inbox, emit: &mut dyn FnMut(&mut Session, AgentEvent)) -> bool {
        if inbox.stopped() {
            return false;
        }
        let taken = inbox.take_follow_ups();
        for FollowUp { id, text, files } in &taken {
            let (id, text, files) = (id.clone(), text.clone(), files.clone());
            emit(session, AgentEvent::FollowUp { id, text, note: FOLLOW_UP.to_owned(), files });
        }
        !taken.is_empty()
    }

    /// One streamed response, retried while the provider asks for patience.
    /// `Ok(None)` when the turn was stopped.
    fn respond(
        &self,
        session: &mut Session,
        specs: &[ToolSpec<'_>],
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<Option<Completion>, Error> {
        let wire = self.client.wire(&self.model);
        let key = session.id.clone();
        let mut failures = 0;
        let (mut forgot, mut blinded) = (false, false);
        loop {
            let input = history(&session.entries, Some((wire, &self.model)));
            let request = ResponseRequest {
                model: &self.model,
                instructions: Some(&self.system),
                reasoning: self.effort.as_deref(),
                input: &input,
                tools: specs,
                tool_choice: None,
                web: self.web,
                cache_key: Some(&key),
            };
            emit(session, AgentEvent::ReplyStarted);
            let (mut completion, mut written, mut server_tools) = (None, false, 0u32);
            let result = self.client.stream_response(&request, &inbox.cancel, |event| match event {
                StreamEvent::Text(text) => {
                    written = true;
                    emit(session, AgentEvent::Text(text));
                }
                StreamEvent::Reasoning(text) => {
                    written = true;
                    emit(session, AgentEvent::Thinking(text));
                }
                StreamEvent::ServerTool { name, input } => {
                    written = true;
                    server_tools += 1;
                    // Not one of ours to answer: shown as done.
                    let id = format!("{SERVER_CALL}{server_tools}-{}", crate::session::new_id());
                    emit(session, AgentEvent::ToolCall(ToolUse { id: id.clone(), name, input, ..ToolUse::default() }));
                    emit(
                        session,
                        AgentEvent::ToolResult {
                            id,
                            output: "Done on Anthropic's servers.".to_owned(),
                            is_error: false,
                        },
                    );
                }
                StreamEvent::Completed(done) => completion = Some(done),
                StreamEvent::Ping
                | StreamEvent::ToolCallStarted { .. }
                | StreamEvent::ToolCallDelta { .. }
                | StreamEvent::Charged(_) => {}
            });
            let error = match (result, completion) {
                (Ok(true), Some(completion)) => return Ok(Some(completion)),
                (Ok(_), _) if inbox.stopped() => {
                    emit(session, ended("Stopped."));
                    return Ok(None);
                }
                (Ok(_), _) => Error::Response {
                    code: None,
                    message: "The connection closed before the reply was complete.".to_owned(),
                },
                (Err(e), _) => e,
            };
            // The empty reply it started is taken back when it is asked for again.
            let take_back = |session: &mut Session| {
                if let Some(Entry::Reply(reply)) = session.entries.last()
                    && reply.blocks.is_empty()
                {
                    session.entries.pop();
                }
            };
            // Anthropic refuses reasoning kept under a conversation that
            // changed since (a reply saved before the prompt was kept): from
            // here on those replies go back as plain text and calls.
            if wire == Wire::Messages && !written && !forgot && refuses_replay(&error) && replays_kept(session) {
                forgot = true;
                take_back(session);
                emit(session, AgentEvent::ForgetReplays);
                continue;
            }
            // A model that cannot look at pictures is shown none from now on.
            if !written && !blinded && refuses_pictures(&error) && shows_pictures(session) {
                blinded = true;
                take_back(session);
                emit(session, AgentEvent::Blind);
                continue;
            }
            // A full context is the caller's to deal with, by summarising.
            if error.is_context_overflow() && !written {
                take_back(session);
                return Err(error);
            }
            // Only a reply that showed nothing yet is asked for again.
            let wait = RETRIES.get(failures).copied().filter(|_| error.is_retryable() && !written);
            let Some(wait) = wait else {
                emit(session, ended(&error.to_string()));
                return Err(error);
            };
            failures += 1;
            let wait = error.retry_after().unwrap_or(wait);
            take_back(session);
            emit(session, AgentEvent::Status(format!("{error}. Trying again in {}s…", wait.as_secs().max(1))));
            if !inbox.pause(wait) {
                return Ok(None);
            }
            emit(session, AgentEvent::Status(String::new()));
        }
    }

    /// Decides whether `call` may run, asks if it must, and runs it: one
    /// of the app's own tools, one of the engine's, or one of an MCP
    /// server's (`remote`). `Err` is the failure or refusal the model reads.
    fn execute(
        &self,
        call: &ToolCall,
        remote: &[McpTool],
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
        nested: bool,
    ) -> Result<String, String> {
        if inbox.stopped() {
            return Err("Stopped by the user before it ran.".to_owned());
        }
        // Eager streaming does not check the JSON for us.
        let Some(input) = serde_json::from_str::<Value>(&call.arguments).ok().filter(Value::is_object) else {
            return Err("INVALID_JSON: the arguments were not a JSON object. Send the call again.".to_owned());
        };
        // What a server's tool does is the server's business: it is treated as anything that needs asking.
        let theirs = remote.iter().find(|tool| tool.name == call.name);
        if theirs.is_none() && matches!(call.name.as_str(), TODO | ASK | AGENT) {
            if nested {
                return Err(format!("A subagent has no `{}` tool: do the work and answer.", call.name));
            }
            return match call.name.as_str() {
                TODO => todo(&input),
                ASK => Self::question(call, &input, inbox, session, emit),
                _ => Err("The subagent could not be started.".to_owned()),
            };
        }
        let risk = if theirs.is_some() { Risk::Ask } else { tools::risk(&call.name, &input, &self.workspace) };
        let approved = || self.approved.lock().is_ok_and(|approved| approved.contains(&call.name));
        let allowed = match (risk, self.autonomy) {
            (Risk::Forbidden, _) => return Err("Refused: AtroxaBot's own data folder is off limits.".to_owned()),
            (Risk::Safe, _) | (_, Autonomy::Full) | (Risk::Edit, Autonomy::Edits | Autonomy::Auto) => true,
            (_, Autonomy::ReadOnly) => {
                return Err("Refused: this agent is read-only. Tell the user what you would have done.".to_owned());
            }
            _ => approved(),
        };
        if !allowed {
            self.ask(call, &input, inbox, session, emit)?;
        }
        match (theirs, &self.mcp) {
            (Some(tool), Some(hub)) => hub.call(tool, &input, &inbox.cancel),
            _ => tools::run(&call.name, &input, &self.workspace, &inbox.cancel),
        }
    }

    /// Asks the user the questions of an `ask` call, on the card the app
    /// shows for them, and returns their answers. `Err` when they dismissed
    /// it or the turn was stopped.
    fn question(
        call: &ToolCall,
        input: &Value,
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<String, String> {
        let asked = questions(input)?;
        let ask = Ask {
            id: call.call_id.clone(),
            tool: QUESTION_TOOL.to_owned(),
            input: call.arguments.clone(),
            summary: asked.first().cloned().unwrap_or_default(),
            reason: String::new(),
        };
        emit(session, AgentEvent::Permission(ask));
        let verdict = inbox.verdict(&call.call_id);
        emit(session, AgentEvent::PermissionGone(call.call_id.clone()));
        match verdict {
            Some(Verdict::Answer(updated)) => Ok(answers(&asked, &updated)),
            Some(Verdict::Deny(why)) if !why.trim().is_empty() => Err(why.trim().to_owned()),
            Some(Verdict::Deny(_) | Verdict::Allow | Verdict::AllowAlways) => {
                Err("The user dismissed the question without answering it.".to_owned())
            }
            None => Err("Stopped by the user before they answered.".to_owned()),
        }
    }

    /// Asks the user whether `call` may run and waits for the answer.
    /// `Err` is the refusal the model reads.
    fn ask(
        &self,
        call: &ToolCall,
        input: &Value,
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<(), String> {
        let ask = Ask {
            id: call.call_id.clone(),
            tool: call.name.clone(),
            input: call.arguments.clone(),
            summary: tools::summary(input),
            reason: String::new(),
        };
        emit(session, AgentEvent::Permission(ask));
        let verdict = inbox.verdict(&call.call_id);
        emit(session, AgentEvent::PermissionGone(call.call_id.clone()));
        match verdict {
            Some(Verdict::Allow | Verdict::Answer(_)) => Ok(()),
            Some(Verdict::AllowAlways) => {
                if let Ok(mut approved) = self.approved.lock() {
                    approved.insert(call.name.clone());
                }
                Ok(())
            }
            Some(Verdict::Deny(why)) if why.trim().is_empty() => Err("The user declined this.".to_owned()),
            Some(Verdict::Deny(why)) => Err(format!("The user declined this: {}", why.trim())),
            None => Err("Stopped by the user before it ran.".to_owned()),
        }
    }

    /// Asks the model for a summary and continues from it: of everything
    /// before the latest prompt when the turn has only just begun, and of
    /// everything, the turn's own work included, once it is under way
    /// (what fills a context mid-turn is that work).
    fn compact(
        &self,
        session: &mut Session,
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<(), String> {
        let end = if session.working() {
            session.entries.len()
        } else {
            session.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(session.entries.len())
        };
        let Some(earlier) = session.entries.get(..end).filter(|earlier| !earlier.is_empty()) else {
            return Err("This prompt alone does not fit the model's context.".to_owned());
        };
        let mut input = history(earlier, None);
        // The words are what is summarised; pictures would only cost.
        for item in &mut input {
            if let InputItem::ToolOutput { picture, .. } = item {
                *picture = None;
            }
        }
        input.push(InputItem::text(Role::User, SUMMARY_PROMPT));
        emit(session, AgentEvent::Status("Summarising the conversation to make room…".to_owned()));
        let request = ResponseRequest {
            model: &self.model,
            instructions: Some(&self.system),
            reasoning: None,
            input: &input,
            tools: &[],
            tool_choice: None,
            web: false,
            cache_key: None,
        };
        let mut summary = String::new();
        let result = self.client.stream_response(&request, &inbox.cancel, |event| {
            if let StreamEvent::Text(text) = event {
                summary.push_str(&text);
            }
        });
        emit(session, AgentEvent::Status(String::new()));
        match result {
            Ok(true) if !summary.trim().is_empty() => {
                emit(session, AgentEvent::Summarized(summary.trim().to_owned()));
                Ok(())
            }
            Ok(_) if inbox.stopped() => Err("Stopped.".to_owned()),
            Ok(_) => Err("The conversation is too long for this model, and it could not be summarised.".to_owned()),
            Err(e) => Err(format!("The conversation is too long for this model, and summarising it failed: {e}")),
        }
    }

    /// `/compact`: summarises the conversation now, so the next turn starts
    /// from the summary. Always ends with [`AgentEvent::TurnEnded`].
    pub fn compact_now(&self, session: &mut Session, inbox: &Inbox, emit: &mut dyn FnMut(AgentEvent)) {
        let mut emit = |session: &mut Session, event: AgentEvent| {
            session.apply(&event);
            emit(event);
        };
        // The latest prompt is answered: everything so far is summarised.
        let problem = if session.working() {
            self.compact(session, inbox, &mut emit).err()
        } else {
            Some("There is nothing to summarise yet.".to_owned())
        };
        if problem.is_none() {
            emit(session, AgentEvent::Note("Conversation summarised".to_owned()));
        }
        emit(session, AgentEvent::Status(String::new()));
        emit(session, AgentEvent::TurnEnded { problem });
    }
}

/// What an `ask` call asks: its questions' words. `Err` says what is wrong
/// with a call that asks nothing a card can show.
fn questions(input: &Value) -> Result<Vec<String>, String> {
    let listed = input.get("questions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let asked: Vec<String> = listed
        .iter()
        .filter_map(|question| question.get("question").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect();
    if asked.is_empty() {
        return Err("`questions` must hold at least one question, each with its `question` and `options`.".to_owned());
    }
    if asked.len() > 4 {
        return Err("Ask at most four questions at a time.".to_owned());
    }
    Ok(asked)
}

/// The user's answers, as the model reads them: the card files each under
/// its question in the call's new input (`answers`).
fn answers(asked: &[String], updated: &str) -> String {
    let updated = serde_json::from_str::<Value>(updated).unwrap_or(Value::Null);
    let given = updated.get("answers");
    let mut out = String::from("The user answered:\n");
    for question in asked {
        let answer = given.and_then(|given| given.get(question)).map_or_else(String::new, |answer| match answer {
            Value::String(text) => text.trim().to_owned(),
            Value::Array(picked) => picked.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            other => other.to_string(),
        });
        let answer = if answer.is_empty() { "(no answer)".to_owned() } else { answer };
        let _ = write!(out, "\n- {question} {answer}");
    }
    out
}

/// Takes the agent's task list (a `todo` call): what it says back. The
/// list itself is read from the call by [`Session::todos`].
fn todo(input: &Value) -> Result<String, String> {
    let listed = input.get("todos").and_then(Value::as_array).ok_or("missing `todos`: the whole list")?;
    let (mut done, mut active, mut waiting) = (0, 0, 0);
    for (number, task) in (1..).zip(listed) {
        let field = |key: &str| task.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default();
        if field("content").is_empty() {
            return Err(format!("task {number} has no `content`"));
        }
        match field("status") {
            "completed" => done += 1,
            "in_progress" => active += 1,
            "pending" => waiting += 1,
            other => {
                return Err(format!(
                    "task {number} has the status `{other}`; it must be `pending`, `in_progress` or `completed`"
                ));
            }
        }
    }
    let mut out = format!("Task list saved: {done} done, {active} in progress, {waiting} to do.");
    if active > 1 {
        out.push_str(" Keep one task in progress at a time.");
    }
    Ok(out)
}

/// `output` cut to what one tool result may hold, keeping its start and end.
fn bounded(output: &str) -> String {
    const MOST: usize = 30_000;
    if output.chars().count() <= MOST {
        return output.to_owned();
    }
    let head: String = output.chars().take(MOST / 2).collect();
    let tail: String = output.chars().rev().take(MOST / 2).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}\n\n[… the middle was left out …]\n\n{tail}")
}

/// Whether Anthropic refused a request for the reasoning it replays: kept
/// under a conversation that is not as it was when it was written.
fn refuses_replay(error: &Error) -> bool {
    let Error::Api { status: 400, message, .. } = error else { return false };
    let message = message.to_ascii_lowercase();
    message.contains("thinking")
        && ["binding", "prefix", "mismatch", "cannot be modified", "modified"].iter().any(|word| message.contains(word))
}

/// Whether a provider refused a request for the pictures in it: the model
/// takes none (DeepSeek's "unknown variant `image_url`", OpenAI's "only
/// supported by certain models", "does not support images").
fn refuses_pictures(error: &Error) -> bool {
    let (Error::Api { status: 400 | 404 | 415 | 422, message, .. } | Error::Response { message, .. }) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    (message.contains("image") || message.contains("vision") || message.contains("multimodal"))
        && ["support", "unknown variant", "not allowed", "invalid content", "must be a string", "only"]
            .iter()
            .any(|words| message.contains(words))
}

/// Whether a picture a result showed goes with `session`'s next request.
fn shows_pictures(session: &Session) -> bool {
    fn shown(blocks: &[Block]) -> bool {
        blocks.iter().any(|block| matches!(block, Block::Tool(call) if call.picture.is_some() || shown(&call.sub)))
    }
    session.entries.iter().any(|entry| matches!(entry, Entry::Reply(reply) if shown(&reply.blocks)))
}

/// Whether any of `session`'s replies goes back to Anthropic as it arrived.
fn replays_kept(session: &Session) -> bool {
    session
        .entries
        .iter()
        .any(|entry| matches!(entry, Entry::Reply(reply) if reply.wire == Wire::Messages && !reply.raw.is_empty()))
}

/// The event closing a reply that failed with `problem`.
fn ended(problem: &str) -> AgentEvent {
    AgentEvent::ReplyEnded {
        raw: Vec::new(),
        wire: Wire::default(),
        model: String::new(),
        usage: Usage::default(),
        cost: 0.0,
        problem: problem.to_owned(),
    }
}

/// How many characters `specs` take as the model is sent them.
fn spec_size(specs: &[ToolSpec<'_>]) -> usize {
    specs
        .iter()
        .map(|spec| spec.name.len() + spec.description.chars().count() + spec.parameters.to_string().len())
        .sum()
}

/// What a context of `total` tokens (as the provider counted them) holds,
/// by kind: each of the `fixed` parts (a name and its size in characters)
/// at about four characters a token, and the conversation the rest. Parts
/// that take nothing are left out; empty while nothing was counted.
fn context_parts(fixed: &[(&str, usize)], total: u64) -> Vec<ContextPart> {
    if total == 0 {
        return Vec::new();
    }
    let estimated: Vec<u64> = fixed.iter().map(|(_, size)| (*size as u64).div_ceil(4)).collect();
    let sum: u64 = estimated.iter().sum();
    // The estimates are rough: they never take more than the provider counted.
    let scale = if sum > total { total as f64 / sum as f64 } else { 1.0 };
    let mut parts: Vec<ContextPart> = fixed
        .iter()
        .zip(estimated)
        .map(|((name, _), tokens)| ContextPart { name: (*name).to_owned(), tokens: (tokens as f64 * scale) as u64 })
        .collect();
    let held: u64 = parts.iter().map(|part| part.tokens).sum();
    parts.push(ContextPart { name: "Messages".to_owned(), tokens: total.saturating_sub(held) });
    parts.retain(|part| part.tokens > 0);
    parts
}

/// The request input for `entries`, from the latest summary on. A reply
/// goes back as it arrived when the wire and model it is `sent_to` can
/// take it (see [`replays`]), else as text and calls; `None`
/// sends every reply as text and calls. Every call is answered, as every
/// API requires, and a call's picture goes with its result.
fn history(entries: &[Entry], sent_to: Option<(Wire, &str)>) -> Vec<InputItem> {
    let start = entries.iter().rposition(|e| matches!(e, Entry::Summary { .. })).unwrap_or(0);
    let entries = entries.get(start..).unwrap_or_default();
    let mut input = Vec::new();
    for (at, entry) in entries.iter().enumerate() {
        match entry {
            Entry::Summary { text } => {
                let mut said = format!("Summary of the conversation so far:\n\n{text}");
                // Summarised mid-turn: nothing follows yet, and the work goes on.
                if at + 1 == entries.len() {
                    said.push_str(CARRY_ON);
                }
                input.push(InputItem::text(Role::User, said));
            }
            Entry::User { text, note, files } => {
                // Pictures first, then the words (and the paths of whatever is no picture).
                let (images, named) = attach::split(files);
                input.extend(
                    images.into_iter().map(|image| InputItem::Image { media_type: image.media_type, data: image.data }),
                );
                let text = attach::with_paths(text, &named);
                input.push(InputItem::text(
                    Role::User,
                    if note.is_empty() { text } else { format!("{note}\n\n{text}") },
                ));
            }
            Entry::Note { .. } => {}
            Entry::Reply(reply) => {
                let replayed = sent_to.is_some_and(|(wire, model)| replays(reply, wire, model));
                if replayed {
                    input.push(InputItem::Replay(reply.raw.clone()));
                }
                let mut said = String::new();
                let mut outputs = Vec::new();
                for block in &reply.blocks {
                    match block {
                        Block::Text { text } => said.push_str(text),
                        Block::Thinking { .. } => {}
                        Block::Tool(call) => {
                            // Among replayed blocks, only our own calls await an answer;
                            // Anthropic's server tools mean nothing anywhere else.
                            if replayed && !holds_call(reply, &call.id) {
                                continue;
                            }
                            if !replayed && call.id.starts_with(SERVER_CALL) {
                                continue;
                            }
                            if !replayed {
                                let call = ToolCall {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments: call.input.clone(),
                                };
                                outputs.push(InputItem::ToolCall(call));
                            }
                            outputs.push(InputItem::ToolOutput {
                                call_id: call.id.clone(),
                                output: call.output.clone().unwrap_or_else(|| INTERRUPTED.to_owned()),
                                is_error: call.is_error || call.output.is_none(),
                                picture: call.picture.clone(),
                            });
                        }
                    }
                }
                if !replayed && !said.trim().is_empty() {
                    input.push(InputItem::text(Role::Assistant, said));
                }
                input.extend(outputs);
            }
        }
    }
    input
}

/// Whether `reply` goes back as it arrived to `model` over `wire`: only
/// over the wire it came by, and (but for Claude, whose API leaves out
/// what a model cannot read) only to the model that wrote it, whose own
/// reasoning it carries.
fn replays(reply: &Reply, wire: Wire, model: &str) -> bool {
    !reply.raw.is_empty() && reply.wire == wire && (wire == Wire::Messages || reply.model == model)
}

/// Whether the call `id` is one of those `reply` holds as it arrived.
fn holds_call(reply: &Reply, id: &str) -> bool {
    match reply.wire {
        Wire::Messages => reply.raw.iter().any(|block| block["type"] == "tool_use" && block["id"] == id),
        Wire::Chat => reply
            .raw
            .first()
            .and_then(|message| message["tool_calls"].as_array())
            .is_some_and(|calls| calls.iter().any(|call| call["id"] == id)),
        Wire::Responses => reply.raw.iter().any(|item| item["type"] == "function_call" && item["call_id"] == id),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_context_is_told_apart_by_kind() {
        let fixed = [("System prompt", 4_000), ("System tools", 8_000), ("MCP tools", 0), ("Memory files", 400)];
        let parts = context_parts(&fixed, 10_000);
        let seen: Vec<_> = parts.iter().map(|part| (part.name.as_str(), part.tokens)).collect();
        assert_eq!(
            seen,
            [("System prompt", 1_000), ("System tools", 2_000), ("Memory files", 100), ("Messages", 6_900)],
            "what takes nothing is left out, and the conversation has the rest"
        );
        // Estimates over the count shrink to fit it; nothing counted, nothing to say.
        let tight = context_parts(&fixed, 1_000);
        assert_eq!(tight.iter().map(|part| part.tokens).sum::<u64>(), 1_000);
        assert!(context_parts(&fixed, 0).is_empty());
    }

    fn tool(id: &str, output: Option<&str>) -> Block {
        Block::Tool(ToolUse {
            id: id.into(),
            name: "read".into(),
            input: "{}".into(),
            output: output.map(str::to_owned),
            ..ToolUse::default()
        })
    }

    #[test]
    fn a_subagent_lives_inside_the_call_that_started_it() {
        let mut session = Session::new("a");
        session.prompt("go", "");
        session.attach(&["/shots/a.png".into()]);
        assert!(matches!(&session.entries[0], Entry::User { files, .. } if files == &["/shots/a.png"]));
        let call = |id: &str, name: &str| {
            AgentEvent::ToolCall(ToolUse { id: id.into(), name: name.into(), ..ToolUse::default() })
        };
        let sub = |event: AgentEvent| AgentEvent::Sub { parent: "a1".into(), event: Box::new(event) };
        for event in [
            call("a1", "Agent"),
            sub(AgentEvent::Text("Look".into())),
            sub(AgentEvent::Text("ing.".into())),
            sub(call("g1", "Glob")),
            AgentEvent::ToolResult { id: "g1".into(), output: "hello.txt".into(), is_error: false },
            AgentEvent::Activity { id: "a1".into(), text: "Done".into() },
            AgentEvent::Sub { parent: "nobody".into(), event: Box::new(AgentEvent::Text("lost".into())) },
            AgentEvent::Window(1_000_000),
            AgentEvent::Note("Summarised".into()),
            AgentEvent::Text("After.".into()),
        ] {
            session.apply(&event);
        }
        let Entry::Reply(reply) = &session.entries[1] else { panic!("a reply") };
        let Block::Tool(agent) = &reply.blocks[0] else { panic!("the call") };
        assert_eq!((agent.activity.as_str(), agent.sub.len()), ("Done", 2));
        assert!(matches!(&agent.sub[0], Block::Text { text } if text == "Looking."), "streamed pieces are one block");
        assert!(matches!(&agent.sub[1], Block::Tool(glob) if glob.output.as_deref() == Some("hello.txt")));
        assert_eq!(session.window, 1_000_000);
        // The note stands between the two parts of the reply, and no model is sent it.
        assert!(matches!(&session.entries[2], Entry::Note { text } if text == "Summarised"));
        assert!(matches!(&session.entries[3], Entry::Reply(reply) if reply.blocks.len() == 1));
        assert!(
            history(&session.entries, None)
                .iter()
                .all(|item| !matches!(item, InputItem::Message { text, .. } if text.contains("Summarised")))
        );
        assert_eq!(session.find_tool("g1").map(|tool| tool.name.clone()).as_deref(), Some("Glob"));
        assert!(session.find_tool("zz").is_none());
    }

    #[test]
    fn events_build_the_transcript() {
        let mut session = Session::new("a");
        session.prompt("  \nFix the parser please\nthanks", "");
        assert_eq!(session.title, "Fix the parser please");
        for event in [
            AgentEvent::ReplyStarted,
            AgentEvent::Thinking("Let me ".into()),
            AgentEvent::Thinking("look.".into()),
            AgentEvent::Text("Reading".into()),
            AgentEvent::Text(" it.".into()),
            AgentEvent::ToolCall(ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: "{}".into(),
                ..ToolUse::default()
            }),
            AgentEvent::Status("ignored".into()),
            AgentEvent::ReplyEnded {
                raw: vec![json!({ "type": "text" })],
                wire: Wire::Messages,
                model: "m".into(),
                usage: Usage::new(7, 3),
                cost: 0.5,
                problem: String::new(),
            },
            AgentEvent::ToolResult { id: "t1".into(), output: "fn main".into(), is_error: false },
            AgentEvent::ReplyStarted,
            AgentEvent::Text("Done.".into()),
            AgentEvent::Remote("remote-1".into()),
            AgentEvent::TurnEnded { problem: None },
        ] {
            session.apply(&event);
        }
        let Entry::Reply(first) = &session.entries[1] else { panic!("a reply") };
        assert_eq!(first.blocks[0], Block::Thinking { text: "Let me look.".into() });
        assert_eq!(first.blocks[1], Block::Text { text: "Reading it.".into() });
        assert_eq!(first.blocks[2], tool("t1", Some("fn main")));
        assert_eq!((first.model.as_str(), first.usage.output_tokens, first.raw.len()), ("m", 3, 1));
        assert_eq!((session.entries.len(), session.remote.as_str()), (3, "remote-1"));

        session.prompt("next", "");
        session.apply(&AgentEvent::Summarized("So far.".into()));
        assert!(matches!(&session.entries[3], Entry::Summary { text } if text == "So far."));
        assert!(matches!(&session.entries[4], Entry::User { text, .. } if text == "next"));
    }

    #[test]
    fn history_answers_every_call_and_starts_at_the_summary() {
        let raw = vec![
            json!({ "type": "thinking", "thinking": "", "signature": "s" }),
            json!({ "type": "tool_use", "id": "t1", "name": "read", "input": {} }),
        ];
        let entries = vec![
            Entry::User { text: "old".into(), note: String::new(), files: Vec::new() },
            Entry::Summary { text: "Earlier.".into() },
            Entry::User { text: "hi".into(), note: "(on a call)".into(), files: Vec::new() },
            Entry::Reply(Reply {
                blocks: vec![Block::Text { text: "Looking.".into() }, tool("server-1", Some("x")), tool("t1", None)],
                raw: raw.clone(),
                ..Reply::default()
            }),
        ];
        let claude = history(&entries, Some((Wire::Messages, "another-claude")));
        assert_eq!(claude[0], InputItem::text(Role::User, "Summary of the conversation so far:\n\nEarlier."));
        assert_eq!(claude[1], InputItem::text(Role::User, "(on a call)\n\nhi"));
        assert_eq!(claude[2], InputItem::Replay(raw), "Claude's API leaves out what another model cannot read");
        assert_eq!(
            claude[3],
            InputItem::ToolOutput { call_id: "t1".into(), output: INTERRUPTED.into(), is_error: true, picture: None }
        );
        assert_eq!(claude.len(), 4, "the server's own tool is not answered");

        for other in [history(&entries, None), history(&entries, Some((Wire::Chat, "m")))] {
            assert_eq!(other[2], InputItem::text(Role::Assistant, "Looking."));
            assert!(matches!(&other[3], InputItem::ToolCall(call) if call.call_id == "t1"), "no server call");
            assert!(matches!(&other[4], InputItem::ToolOutput { call_id, .. } if call_id == "t1"));
            assert_eq!(other.len(), 5);
        }
    }

    #[test]
    fn a_reply_goes_back_as_it_came_only_where_it_can() {
        let reply = |wire: Wire, model: &str, raw: Value| {
            Entry::Reply(Reply {
                blocks: vec![tool("c1", Some("fn main"))],
                raw: vec![raw],
                wire,
                model: model.into(),
                ..Reply::default()
            })
        };
        let chat = json!({ "role": "assistant", "content": null, "reasoning_content": "Hm.", "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{}" } }] });
        let entries = [
            Entry::User { text: "go".into(), note: String::new(), files: Vec::new() },
            reply(Wire::Chat, "deepseek-reasoner", chat.clone()),
        ];
        let same = history(&entries, Some((Wire::Chat, "deepseek-reasoner")));
        assert_eq!(same[1], InputItem::Replay(vec![chat]));
        assert!(matches!(&same[2], InputItem::ToolOutput { call_id, .. } if call_id == "c1"));
        // Another model, or another wire, gets text and calls.
        for (wire, model) in [
            (Wire::Chat, "deepseek-chat"),
            (Wire::Responses, "deepseek-reasoner"),
            (Wire::Messages, "deepseek-reasoner"),
        ] {
            let other = history(&entries, Some((wire, model)));
            assert!(matches!(&other[1], InputItem::ToolCall(call) if call.call_id == "c1"), "{wire:?} {model}");
        }
        let item = json!({ "type": "function_call", "call_id": "c1", "name": "read", "arguments": "{}" });
        let entries = [
            Entry::User { text: "go".into(), note: String::new(), files: Vec::new() },
            reply(Wire::Responses, "gpt-5.6", item),
        ];
        let responses = history(&entries, Some((Wire::Responses, "gpt-5.6")));
        assert!(matches!(&responses[1], InputItem::Replay(_)) && responses.len() == 3);

        // A picture a result showed goes with it, every time.
        let mut session = Session::new("a");
        session.prompt("look", "");
        session.apply(&AgentEvent::ToolCall(ToolUse { id: "r1".into(), name: "read".into(), ..ToolUse::default() }));
        let picture = Picture { media_type: "image/png".into(), data: "AAAA".into() };
        session.apply(&AgentEvent::ToolPicture { id: "r1".into(), picture: picture.clone() });
        session.apply(&AgentEvent::ToolResult { id: "r1".into(), output: "A picture.".into(), is_error: false });
        let input = history(&session.entries, None);
        assert!(matches!(&input[2], InputItem::ToolOutput { picture: Some(shown), .. } if *shown == picture));

        // Anthropic refused the kept reasoning: Claude's replies go back plain from then on.
        let mut session = Session::new("a");
        session.entries = vec![reply(
            Wire::Messages,
            "claude-opus-5-5",
            json!({ "type": "tool_use", "id": "c1", "name": "read", "input": {} }),
        )];
        assert!(replays_kept(&session));
        session.apply(&AgentEvent::ForgetReplays);
        assert!(!replays_kept(&session));
        let refused =
            |message: &str| Error::Api { status: 400, code: None, message: message.into(), retry_after: None };
        assert!(refuses_replay(&refused("messages.3.content.0: thinking block prefix binding mismatch")));
        assert!(!refuses_replay(&refused("max_tokens: must be less than 128000")));
    }

    #[test]
    fn a_model_that_refuses_pictures_is_shown_none_again() {
        let mut session = Session::new("a");
        session.prompt("look", "");
        session.apply(&AgentEvent::ToolCall(ToolUse { id: "r1".into(), name: "read".into(), ..ToolUse::default() }));
        let picture = Picture { media_type: "image/png".into(), data: "AAAA".into() };
        session.apply(&AgentEvent::ToolPicture { id: "r1".into(), picture });
        session.apply(&AgentEvent::ToolResult {
            id: "r1".into(),
            output: "shot.png: a PNG picture.".into(),
            is_error: false,
        });
        assert!(shows_pictures(&session));
        let refused =
            |status, message: &str| Error::Api { status, code: None, message: message.into(), retry_after: None };
        for message in [
            "Failed to deserialize the JSON body: unknown variant `image_url`, expected `text`",
            "Invalid content type. image_url is only supported by certain models.",
            "model does not support images",
        ] {
            assert!(refuses_pictures(&refused(400, message)), "{message}");
        }
        assert!(!refuses_pictures(&refused(400, "Invalid tool schema")));
        assert!(!refuses_pictures(&refused(429, "image generation rate limit; only 5 per minute")));
        session.apply(&AgentEvent::Blind);
        assert!(session.blind && !shows_pictures(&session));
        let input = history(&session.entries, None);
        assert!(
            matches!(&input[2], InputItem::ToolOutput { picture: None, output, .. } if output.ends_with("it was not shown.)")),
            "{:?}",
            input[2]
        );
    }

    #[test]
    fn a_summary_mid_turn_takes_the_work_and_goes_on() {
        let mut session = Session::new("a");
        session.prompt("Refactor everything", "");
        assert!(!session.working());
        session.apply(&AgentEvent::Summarized("Before.".into()));
        assert!(matches!(&session.entries[0], Entry::Summary { .. }), "before the prompt, which it does not cover");
        session.apply(&AgentEvent::ReplyStarted);
        session.apply(&AgentEvent::ToolCall(ToolUse { id: "t1".into(), name: "read".into(), ..ToolUse::default() }));
        session.apply(&AgentEvent::ToolResult { id: "t1".into(), output: "fn main".into(), is_error: false });
        assert!(session.working());
        session.apply(&AgentEvent::Summarized("Read main; next the parser.".into()));
        assert!(matches!(session.entries.last(), Some(Entry::Summary { .. })), "after the turn's work");
        let input = history(&session.entries, None);
        assert_eq!(input.len(), 1);
        let InputItem::Message { role: Role::User, text } = &input[0] else { panic!("the summary") };
        assert!(text.contains("Read main") && text.ends_with("leaves off.)"));
        // A system prompt is kept with the session.
        session.apply(&AgentEvent::System("You are Scout.".into()));
        assert_eq!(session.system, "You are Scout.");
    }

    #[test]
    fn the_task_list_and_questions_are_checked_and_read_back() {
        let list = json!({ "todos": [
            { "content": "Read the code", "status": "completed", "activeForm": "Reading the code" },
            { "content": "Fix the bug", "status": "in_progress", "activeForm": "Fixing the bug" },
            { "content": "Run the tests", "status": "pending" },
        ] });
        assert_eq!(todo(&list).unwrap(), "Task list saved: 1 done, 1 in progress, 1 to do.");
        assert!(todo(&json!({ "todos": [{ "content": "x", "status": "doing" }] })).unwrap_err().contains("`doing`"));
        assert!(todo(&json!({ "todos": [{ "status": "pending" }] })).is_err());
        assert!(todo(&json!({})).is_err());
        let mut session = Session::new("a");
        session.apply(&AgentEvent::ToolCall(ToolUse {
            id: "t".into(),
            name: TODO.into(),
            input: list.to_string(),
            output: Some("ok".into()),
            ..ToolUse::default()
        }));
        let todos = session.todos();
        assert_eq!(todos.len(), 3);
        assert_eq!((todos[1].doing.as_str(), todos[1].state), ("Fixing the bug", crate::session::TodoState::Active));

        let asked = json!({ "questions": [
            { "question": "Which database?", "header": "Database", "options": [{ "label": "SQLite" }, { "label": "Postgres" }] },
            { "question": "Which tests?", "header": "Tests", "options": [{ "label": "Unit" }, { "label": "Live" }], "multiSelect": true },
        ] });
        let questions = questions(&asked).unwrap();
        assert_eq!(questions, ["Which database?", "Which tests?"]);
        assert!(super::questions(&json!({ "questions": [] })).is_err());
        let updated =
            json!({ "answers": { "Which database?": "SQLite", "Which tests?": ["Unit", "Live"] } }).to_string();
        assert_eq!(
            answers(&questions, &updated),
            "The user answered:\n\n- Which database? SQLite\n- Which tests? Unit, Live"
        );
        assert!(answers(&questions, "{}").contains("(no answer)"));
    }

    #[test]
    fn approvals_reach_whoever_waits_for_them() {
        let (control, inbox) = Control::new();
        control.decide("b", Verdict::Allow);
        control.decide("a", Verdict::Deny("no".into()));
        // Two subagents wait side by side: each gets its own answer, whichever came first.
        std::thread::scope(|scope| {
            let a = scope.spawn(|| inbox.verdict("a"));
            let b = scope.spawn(|| inbox.verdict("b"));
            assert_eq!(a.join().unwrap(), Some(Verdict::Deny("no".into())));
            assert_eq!(b.join().unwrap(), Some(Verdict::Allow));
        });
        drop(control);
        assert_eq!(inbox.verdict("c"), None, "nobody left to answer");
    }

    #[test]
    fn control_stops_and_answers() {
        let (control, inbox) = Control::new();
        control.decide("other", Verdict::Allow);
        control.decide("t1", Verdict::Deny("no".into()));
        assert_eq!(inbox.verdict("t1"), Some(Verdict::Deny("no".into())));
        assert!(inbox.pause(Duration::from_millis(1)));
        control.stop();
        assert_eq!(inbox.verdict("t2"), None);
        assert!(!inbox.pause(Duration::from_secs(5)));
    }

    #[test]
    fn a_follow_up_is_read_after_the_tool_results_it_waited_for() {
        let (control, inbox) = Control::new();
        let mut session = Session::new("a");
        session.prompt("Fix the bug", "");
        session.apply(&AgentEvent::ReplyStarted);
        session.apply(&AgentEvent::ToolCall(ToolUse { id: "t1".into(), name: "read".into(), ..ToolUse::default() }));
        // Handed over while the tool runs.
        control.follow_up(FollowUp { id: "f1".into(), text: "Also check the tests.".into(), files: Vec::new() });
        session.apply(&AgentEvent::ToolResult { id: "t1".into(), output: "fn main".into(), is_error: false });
        let mut seen = Vec::new();
        let mut emit = |session: &mut Session, event: AgentEvent| {
            session.apply(&event);
            seen.push(event);
        };
        assert!(ApiEngine::take_follow_ups(&mut session, &inbox, &mut emit));
        assert!(!ApiEngine::take_follow_ups(&mut session, &inbox, &mut emit), "taken once");
        // A stopped turn takes nothing in: the app sends it again.
        control.follow_up(FollowUp { id: "f2".into(), ..FollowUp::default() });
        control.stop();
        assert!(!ApiEngine::take_follow_ups(&mut session, &inbox, &mut emit));
        assert!(matches!(&seen[..], [AgentEvent::FollowUp { id, note, .. }] if id == "f1" && note == FOLLOW_UP));
        // The model reads it after the result, with the note that says when it came; nothing before changed.
        for replay in [Some((Wire::Messages, "m")), None] {
            let input = history(&session.entries, replay);
            let n = input.len();
            assert!(matches!(&input[n - 2], InputItem::ToolOutput { call_id, .. } if call_id == "t1"));
            let InputItem::Message { role: Role::User, text } = &input[n - 1] else { panic!("the follow-up is last") };
            assert_eq!(text, &format!("{FOLLOW_UP}\n\nAlso check the tests."));
        }
    }

    /// A provider on this computer that answers Chat Completions requests
    /// as `answer` says, each connection on a thread of its own; returns its
    /// address.
    fn fake_provider(answer: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static) -> String {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/v1", listener.local_addr().unwrap());
        let answer = Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let answer = Arc::clone(&answer);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let chunks = answer(&request);
                    // A first chunk with a `status` is a refusal, its `body` the error.
                    let out = if let Some(status) = chunks.first().and_then(|chunk| chunk["status"].as_u64()) {
                        let body = chunks[0]["body"].to_string();
                        format!(
                            "HTTP/1.1 {status} Refused\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        let mut out = String::from(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                        );
                        for chunk in chunks {
                            let _ = write!(out, "data: {chunk}\n\n");
                        }
                        out.push_str("data: [DONE]\n\n");
                        out
                    };
                    let mut stream = stream;
                    let _ = stream.write_all(out.as_bytes());
                });
            }
        });
        address
    }

    #[test]
    fn subagents_work_side_by_side_and_answer_their_calls() {
        use std::sync::atomic::AtomicUsize;
        let (busy, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (busy_now, most_seen) = (Arc::clone(&busy), Arc::clone(&most));
        let address = fake_provider(move |request| {
            let messages = request["messages"].as_array().cloned().unwrap_or_default();
            let said = |message: &Value| message["content"].as_str().unwrap_or_default().to_owned();
            let text = |words: &str| {
                vec![
                    json!({ "choices": [{ "delta": { "content": words } }] }),
                    json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }], "usage": { "prompt_tokens": 10, "completion_tokens": 2 } }),
                ]
            };
            if messages.iter().any(|message| said(message).contains("You are a subagent")) {
                // A subagent: it takes a moment, so two that run side by side overlap.
                let now = busy_now.fetch_add(1, Ordering::SeqCst) + 1;
                most_seen.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(400));
                busy_now.fetch_sub(1, Ordering::SeqCst);
                let place = if messages.iter().any(|message| said(message).contains("Look in A")) {
                    "A holds 1."
                } else {
                    "B holds 2."
                };
                return text(place);
            }
            if messages.last().is_some_and(|message| message["role"] == "tool") {
                return text("Both done.");
            }
            let call = |index: u64, id: &str, name: &str, arguments: Value| json!({ "index": index, "id": id, "function": { "name": name, "arguments": arguments.to_string() } });
            let todos = json!({ "todos": [{ "content": "Look", "status": "in_progress", "activeForm": "Looking" }] });
            vec![
                json!({ "choices": [{ "delta": { "tool_calls": [
                    call(0, "t1", TODO, todos),
                    call(1, "a1", AGENT, json!({ "description": "Look in A", "prompt": "Look in A" })),
                    call(2, "a2", AGENT, json!({ "description": "Look in B", "prompt": "Look in B" })),
                ] } }] }),
                json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            ]
        });
        let dir = std::env::temp_dir().join(format!("atroxabot-subagents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = ApiEngine {
            client: Client::new(&address, None),
            model: "fake".into(),
            effort: None,
            system: "You are Scout.".into(),
            workspace: Workspace::new(&dir, None),
            autonomy: Autonomy::Full,
            web: false,
            pricing: None,
            approved: Arc::default(),
            mcp: None,
        };
        let (_control, inbox) = Control::new();
        let mut session = Session::new("a");
        session.prompt("Go", "");
        let mut events = Vec::new();
        engine.run(&mut session, &inbox, &mut |event| events.push(event));

        assert!(matches!(events.last(), Some(AgentEvent::TurnEnded { problem: None })), "{:?}", events.last());
        assert_eq!(session.system, "You are Scout.", "the prompt is kept with the session");
        assert!(most.load(Ordering::SeqCst) >= 2, "the subagents worked side by side");
        let Entry::Reply(first) = &session.entries[1] else { panic!("the reply that started them") };
        let answers: Vec<(&str, Option<&str>, &str)> = first
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Tool(call) if call.name == AGENT => {
                    Some((call.id.as_str(), call.output.as_deref(), call.activity.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(answers, [("a1", Some("A holds 1."), "Done"), ("a2", Some("B holds 2."), "Done")]);
        let Some(Block::Tool(agent)) =
            first.blocks.iter().find(|block| matches!(block, Block::Tool(call) if call.id == "a1"))
        else {
            panic!("the subagent's call")
        };
        assert!(
            matches!(&agent.sub[..], [Block::Text { text }] if text == "A holds 1."),
            "what it said is part of its call"
        );
        assert_eq!(session.todos().len(), 1);
        let spent = events.iter().filter(|event| matches!(event, AgentEvent::Sub { event, .. } if matches!(**event, AgentEvent::ReplyEnded { .. }))).count();
        assert_eq!(spent, 2, "what the subagents used is reported apart");
        let Some(Entry::Reply(last)) = session.entries.last() else { panic!("the final reply") };
        assert!(matches!(&last.blocks[..], [Block::Text { text }] if text == "Both done."));
        assert_eq!((last.wire, last.raw.len()), (Wire::Chat, 1), "kept as it arrived, to go back to the same model");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_context_that_fills_mid_turn_is_summarised_and_the_work_goes_on() {
        let address = fake_provider(|request| {
            let messages = request["messages"].as_array().cloned().unwrap_or_default();
            let all = serde_json::to_string(&messages).unwrap_or_default();
            let text = |words: &str| {
                vec![
                    json!({ "choices": [{ "delta": { "content": words } }] }),
                    json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }], "usage": { "prompt_tokens": 100, "completion_tokens": 5 } }),
                ]
            };
            if all.contains("Write a summary of it to continue from") {
                return text("Read notes.txt; it says hello.");
            }
            if all.contains("Summary of the conversation so far") {
                return text("Done.");
            }
            if messages.last().is_some_and(|message| message["role"] == "tool") {
                let message =
                    "This model's maximum context length is 9,000 tokens. However, you requested 12000 tokens.";
                return vec![
                    json!({ "status": 400, "body": { "error": { "message": message, "type": "invalid_request_error" } } }),
                ];
            }
            let call = json!({ "index": 0, "id": "r1", "function": { "name": "read", "arguments": "{\"path\":\"notes.txt\"}" } });
            vec![
                json!({ "choices": [{ "delta": { "tool_calls": [call] } }] }),
                json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }], "usage": { "prompt_tokens": 50, "completion_tokens": 5 } }),
            ]
        });
        let dir = std::env::temp_dir().join(format!("atroxabot-overflow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        let engine = ApiEngine {
            client: Client::new(&address, None),
            model: "fake".into(),
            effort: None,
            system: "You are Scout.".into(),
            workspace: Workspace::new(&dir, None),
            autonomy: Autonomy::Full,
            web: false,
            pricing: None,
            approved: Arc::default(),
            mcp: None,
        };
        let (_control, inbox) = Control::new();
        let mut session = Session::new("a");
        session.prompt("Read the notes", "");
        let mut events = Vec::new();
        engine.run(&mut session, &inbox, &mut |event| events.push(event));
        assert!(matches!(events.last(), Some(AgentEvent::TurnEnded { problem: None })), "{:?}", events.last());
        assert!(events.contains(&AgentEvent::Window(9_000)), "the window the provider named is believed");
        assert!(events.iter().any(|event| matches!(event, AgentEvent::Summarized(text) if text.contains("hello"))));
        let kinds: Vec<&str> = session
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::User { .. } => "user",
                Entry::Reply(reply) if reply.problem.is_empty() => "reply",
                Entry::Reply(_) => "failed",
                Entry::Summary { .. } => "summary",
                Entry::Note { .. } => "note",
            })
            .collect();
        assert_eq!(kinds, ["user", "reply", "summary", "reply"], "no failed reply is left behind");
        assert_eq!(session.window, 9_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn calls_are_gated_by_autonomy() {
        let dir = std::env::temp_dir().join(format!("atroxabot-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = |autonomy| ApiEngine {
            client: Client::default(),
            model: "m".into(),
            effort: None,
            system: String::new(),
            workspace: Workspace::new(&dir, None),
            autonomy,
            web: false,
            pricing: None,
            approved: Arc::default(),
            mcp: None,
        };
        let call = |name: &str, arguments: &str| ToolCall {
            call_id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        let mut session = Session::new("a");
        let mut asked = 0;
        let mut run = |engine: &ApiEngine, call: &ToolCall, verdict: Option<Verdict>| {
            let (control, inbox) = Control::new();
            if let Some(verdict) = verdict {
                control.decide("c", verdict);
            }
            engine.execute(
                call,
                &[],
                &inbox,
                &mut session,
                &mut |_, event| {
                    asked += usize::from(matches!(event, AgentEvent::Permission(_)));
                },
                false,
            )
        };
        let write = call("write", r#"{"path":"a.txt","content":"x"}"#);
        assert!(run(&engine(Autonomy::ReadOnly), &write, None).unwrap_err().contains("read-only"));
        assert!(
            run(&engine(Autonomy::Ask), &write, Some(Verdict::Deny(String::new()))).unwrap_err().contains("declined")
        );
        assert!(run(&engine(Autonomy::Edits), &write, None).is_ok(), "edits in its folder need no approval");
        assert!(run(&engine(Autonomy::Ask), &call("read", r#"{"path":"a.txt"}"#), None).is_ok());
        assert!(run(&engine(Autonomy::Full), &call("read", "{"), None).unwrap_err().starts_with("INVALID_JSON"));
        let shell = call("shell", r#"{"command":"echo hi"}"#);
        let asking = engine(Autonomy::Edits);
        assert!(run(&asking, &shell, Some(Verdict::AllowAlways)).unwrap().contains("hi"));
        assert!(run(&asking, &shell, None).unwrap().contains("hi"), "approved for the session");
        assert_eq!(asked, 2);

        // Calls that only look run side by side, and every result still comes in the order of the calls.
        let looking = engine(Autonomy::Ask);
        let read = call("read", r#"{"path":"a.txt"}"#);
        assert!(
            looking.looks(&read, &[]).is_some() && looking.looks(&call("search", r#"{"pattern":"x"}"#), &[]).is_some()
        );
        assert!(looking.looks(&write, &[]).is_none() && looking.looks(&shell, &[]).is_none());
        assert!(looking.looks(&call("read", r#"{"path":"../b.txt"}"#), &[]).is_none(), "outside its folder: asked");
        assert!(looking.looks(&call("read", "{"), &[]).is_none());
        let numbered = |id: &str, name: &str, arguments: &str| ToolCall { call_id: id.into(), ..call(name, arguments) };
        let calls = [
            numbered("1", "read", r#"{"path":"a.txt"}"#),
            numbered("2", "search", r#"{"pattern":"x"}"#),
            numbered("3", "read", r#"{"path":"missing.txt"}"#),
            numbered("4", "write", r#"{"path":"a.txt","content":"y"}"#),
            numbered("5", "read", r#"{"path":"a.txt"}"#),
        ];
        let (_control, inbox) = Control::new();
        let mut results = Vec::new();
        let mut collect = |_: &mut Session, event| {
            if let AgentEvent::ToolResult { id, output, is_error } = event {
                results.push((id, output, is_error));
            }
        };
        engine(Autonomy::Edits).run_calls(&calls, &[], &[], &inbox, &mut session, &mut collect, false);
        let ids: Vec<&str> = results.iter().map(|(id, ..)| id.as_str()).collect();
        assert_eq!(ids, ["1", "2", "3", "4", "5"]);
        assert!(results[0].1.contains("1  x") && results[1].1.contains("1: x") && results[2].2);
        assert!(results[4].1.contains("1  y"), "a read after a write sees what was written");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
