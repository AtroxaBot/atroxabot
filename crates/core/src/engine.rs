//! The agent loop for API connections (Anthropic's Messages API and any
//! OpenAI-compatible gateway): ask the model, run the tools it calls, hand
//! back their results, and go on until it answers without calling any.
//!
//! Whatever the engine, a turn is reported as the same [`AgentEvent`]s, and
//! [`Session::apply`] turns those into the saved transcript, so the app
//! shows a Claude Agent session and an API session the same way.
//!
//! Token discipline: the system prompt and tool list never change within a
//! session and history is only ever appended to, so every request reuses
//! the provider's prompt cache; tool outputs are bounded (`tools.rs`); and
//! a conversation that nears the model's context is summarised, by the
//! server on Anthropic's own API and by a request of ours elsewhere.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{Autonomy, FOLLOW_UP};
use crate::attach;
use crate::client::{Client, Model};
use crate::error::Error;
use crate::mcp::{McpHub, McpTool};
use crate::responses::{Completion, InputItem, ResponseRequest, Role, StreamEvent, ToolCall, ToolSpec, Usage};
use crate::session::{Block, ContextPart, Entry, Reply, Session, ToolUse};
use crate::tools::{self, Risk, Workspace};

/// Most model requests one turn may make before it is stopped.
const MAX_ROUNDS: usize = 200;
/// Waits before a failed request is sent again, one per failure.
const RETRIES: [Duration; 4] =
    [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)];
/// How full the context may get before the conversation is summarised.
const COMPACT_AT: f64 = 0.75;
/// What an unanswered tool call is answered with when history is replayed.
const INTERRUPTED: &str = "The call was interrupted before it finished.";

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
        /// The Messages API's blocks to replay; empty for other engines.
        raw: Vec<Value>,
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
    /// The turn is over; `problem` says why if it did not end well.
    TurnEnded {
        /// What went wrong, if anything.
        problem: Option<String>,
    },
    /// The Claude Agent's process ended; the next message starts it again.
    Disconnected(Option<String>),
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
            AgentEvent::ReplyEnded { raw, model, usage, cost, problem } => self.with_reply(|reply| {
                reply.raw.clone_from(raw);
                reply.model.clone_from(model);
                reply.usage = *usage;
                reply.cost = *cost;
                reply.problem.clone_from(problem);
                reply.at = crate::session::unix_now();
            }),
            AgentEvent::Summarized(text) => {
                // Before the prompt being answered, which the summary does not cover.
                let at =
                    self.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(self.entries.len());
                self.entries.insert(at, Entry::Summary { text: text.clone() });
            }
            AgentEvent::Remote(id) => self.remote.clone_from(id),
            AgentEvent::FollowUp { text, note, files, .. } => {
                self.entries.push(Entry::User { text: text.clone(), note: note.clone(), files: files.clone() });
            }
            AgentEvent::Queued(_)
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
        (control, Inbox { cancel, verdicts: inbox, follow_ups })
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

/// The turn's end of a [`Control`].
#[derive(Debug)]
pub struct Inbox {
    cancel: Arc<AtomicBool>,
    verdicts: mpsc::Receiver<(String, Verdict)>,
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
            match self.verdicts.recv_timeout(Duration::from_millis(100)) {
                Ok((answered, verdict)) if answered == id => return Some(verdict),
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
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
    /// The session's system prompt; never changes within a session.
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

    /// The loop; `Err` carries what to tell the user.
    fn turn(
        &self,
        session: &mut Session,
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<(), String> {
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
        let definitions = tools::definitions();
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
        let (prompt, notes) = self.system.split_once(crate::agent::NOTES_HEADING).unwrap_or((&self.system, ""));
        let (own, rest) = specs.split_at(definitions.len().min(specs.len()));
        let (skills, theirs) = rest.split_at(skill.iter().count().min(rest.len()));
        let fixed = [
            ("System prompt", prompt.chars().count()),
            ("System tools", spec_size(own)),
            ("MCP tools", spec_size(theirs)),
            ("Skills", spec_size(skills)),
            ("Memory files", notes.chars().count()),
        ];
        let window = self.pricing.as_ref().map_or(0, |model| model.context_window);
        let mut compacted = false;
        for _ in 0..MAX_ROUNDS {
            if inbox.stopped() {
                return Ok(());
            }
            // Anthropic's own API summarises on its side (see `messages.rs`).
            let full = window > 0 && session.context_tokens() as f64 > window as f64 * COMPACT_AT;
            if full && !self.client.first_party && !compacted {
                compacted = true;
                self.compact(session, inbox, emit)?;
            }
            let completion = match self.respond(session, &specs, inbox, emit) {
                Ok(Some(completion)) => completion,
                Ok(None) => return Ok(()),
                Err(e) if e.is_context_overflow() && !compacted => {
                    compacted = true;
                    self.compact(session, inbox, emit)?;
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
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
                    model: self.model.clone(),
                    usage: completion.usage,
                    cost,
                    problem: problem.clone(),
                },
            );
            emit(session, AgentEvent::Context(context_parts(&fixed, session.context_tokens())));
            if !finished {
                return Err(problem);
            }
            if calls.is_empty() && !completion.paused {
                // What the user sent meanwhile is answered in this turn.
                if Self::take_follow_ups(session, inbox, emit) {
                    continue;
                }
                return Ok(());
            }
            // ponytail: calls run one after another; run the read-only ones
            // of a reply in parallel if agents start making many at once.
            for call in &calls {
                let (output, is_error) = match self.execute(call, &remote, inbox, session, emit) {
                    Ok(output) => (output, false),
                    Err(output) => (output, true),
                };
                emit(session, AgentEvent::ToolResult { id: call.call_id.clone(), output, is_error });
            }
            // What the user sent while the tools ran goes after their results.
            Self::take_follow_ups(session, inbox, emit);
        }
        Err(format!("Stopped after {MAX_ROUNDS} steps in one turn. Say \"continue\" to let it go on."))
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
        let input = history(&session.entries, self.client.speaks_messages(&self.model));
        let request = ResponseRequest {
            model: &self.model,
            instructions: Some(&self.system),
            reasoning: self.effort.as_deref(),
            input: &input,
            tools: specs,
            tool_choice: None,
            web: self.web,
        };
        let mut failures = 0;
        loop {
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
                    let id = format!("server-{server_tools}-{}", crate::session::new_id());
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
            // Only a reply that showed nothing yet is asked for again.
            let wait = RETRIES.get(failures).copied().filter(|_| error.is_retryable() && !written);
            let Some(wait) = wait else {
                emit(session, ended(&error.to_string()));
                return Err(error);
            };
            failures += 1;
            let wait = error.retry_after().unwrap_or(wait);
            // The empty reply it started is taken back.
            if let Some(Entry::Reply(reply)) = session.entries.last()
                && reply.blocks.is_empty()
            {
                session.entries.pop();
            }
            emit(session, AgentEvent::Status(format!("{error}. Trying again in {}s…", wait.as_secs().max(1))));
            if !inbox.pause(wait) {
                return Ok(None);
            }
            emit(session, AgentEvent::Status(String::new()));
        }
    }

    /// Decides whether `call` may run, asks if it must, and runs it: one
    /// of the app's own tools, or one of an MCP server's (`remote`).
    /// `Err` is the failure or refusal the model reads.
    fn execute(
        &self,
        call: &ToolCall,
        remote: &[McpTool],
        inbox: &Inbox,
        session: &mut Session,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
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

    /// Asks the model for a summary of everything before the latest prompt
    /// and continues from it.
    fn compact(
        &self,
        session: &mut Session,
        inbox: &Inbox,
        emit: &mut dyn FnMut(&mut Session, AgentEvent),
    ) -> Result<(), String> {
        let prompt =
            session.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(session.entries.len());
        let Some(earlier) = session.entries.get(..prompt).filter(|earlier| !earlier.is_empty()) else {
            return Err("This prompt alone does not fit the model's context.".to_owned());
        };
        let mut input = history(earlier, false);
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
            Ok(_) => Err("The conversation is too long for this model, and it could not be summarised.".to_owned()),
            Err(e) => Err(format!("The conversation is too long for this model, and summarising it failed: {e}")),
        }
    }
}

/// The event closing a reply that failed with `problem`.
fn ended(problem: &str) -> AgentEvent {
    AgentEvent::ReplyEnded {
        raw: Vec::new(),
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

/// The request input for `entries`, from the latest summary on. For Claude
/// (`messages`), replies go back as the blocks they arrived as; elsewhere as
/// text and calls. Every call is answered, as both APIs require.
fn history(entries: &[Entry], messages: bool) -> Vec<InputItem> {
    let start = entries.iter().rposition(|e| matches!(e, Entry::Summary { .. })).unwrap_or(0);
    let mut input = Vec::new();
    for entry in entries.get(start..).unwrap_or_default() {
        match entry {
            Entry::Summary { text } => {
                input.push(InputItem::text(Role::User, format!("Summary of the conversation so far:\n\n{text}")));
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
                let replayed = messages && !reply.raw.is_empty();
                if replayed {
                    input.push(InputItem::Blocks(reply.raw.clone()));
                }
                let mut said = String::new();
                let mut outputs = Vec::new();
                for block in &reply.blocks {
                    match block {
                        Block::Text { text } => said.push_str(text),
                        Block::Thinking { .. } => {}
                        Block::Tool(call) => {
                            // Among replayed blocks, only our own calls await an answer.
                            let ours = |block: &Value| block["type"] == "tool_use" && block["id"] == call.id.as_str();
                            if replayed && !reply.raw.iter().any(ours) {
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
            history(&session.entries, false)
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
        let claude = history(&entries, true);
        assert_eq!(claude[0], InputItem::text(Role::User, "Summary of the conversation so far:\n\nEarlier."));
        assert_eq!(claude[1], InputItem::text(Role::User, "(on a call)\n\nhi"));
        assert_eq!(claude[2], InputItem::Blocks(raw));
        assert_eq!(
            claude[3],
            InputItem::ToolOutput { call_id: "t1".into(), output: INTERRUPTED.into(), is_error: true }
        );
        assert_eq!(claude.len(), 4, "the server's own tool is not answered");

        let other = history(&entries, false);
        assert_eq!(other[2], InputItem::text(Role::Assistant, "Looking."));
        assert!(matches!(&other[3], InputItem::ToolCall(call) if call.call_id == "server-1"));
        assert!(matches!(&other[6], InputItem::ToolOutput { call_id, .. } if call_id == "t1"));
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
        for messages in [true, false] {
            let input = history(&session.entries, messages);
            let n = input.len();
            assert!(matches!(&input[n - 2], InputItem::ToolOutput { call_id, .. } if call_id == "t1"));
            let InputItem::Message { role: Role::User, text } = &input[n - 1] else { panic!("the follow-up is last") };
            assert_eq!(text, &format!("{FOLLOW_UP}\n\nAlso check the tests."));
        }
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
            engine.execute(call, &[], &inbox, &mut session, &mut |_, event| {
                asked += usize::from(matches!(event, AgentEvent::Permission(_)));
            })
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
        std::fs::remove_dir_all(dir).unwrap();
    }
}
