//! The transport that drives the user's installed OpenAI Codex CLI through `codex app-server`.
//!
//! [`Session::connect`] starts the Codex binary the user has installed, exactly as published, and talks to it
//! over newline-delimited JSON on its standard input and output (JSON-RPC without the `jsonrpc` field). A
//! reader thread does the handshake and opens a thread (a conversation); everything the binary says then arrives
//! as an [`Event`]. The [`Session`] handle sends the user's input, interrupts, and answers approvals and
//! questions.
//!
//! The crate never touches authentication: the binary inherits the parent environment (plus [`Options::env`])
//! and is signed in however the user set it up with `codex login`. A request of the binary for fresh tokens is
//! refused.
//!
//! Blocking I/O and std threads only; the single dependency is `serde_json`.

mod cli;
mod machine;
mod parse;
mod session;

use std::fmt;
use std::path::PathBuf;

pub use cli::{cli_version, find_cli};
pub use serde_json::Value;
pub use session::{Session, info};

/// How to launch the Codex binary. `Options::default()` is a plain session in the current directory.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The native Codex binary to run; `None` discovers it with [`find_cli`].
    pub cli_path: Option<PathBuf>,
    /// Environment variables added to the inherited environment, e.g. `CODEX_HOME` for another account.
    pub env: Vec<(String, String)>,
    /// Working directory of the session; `None` inherits the parent's.
    pub cwd: Option<PathBuf>,
    /// The model; `None` leaves the choice to Codex's configuration.
    pub model: Option<String>,
    /// Reasoning effort (`low`, `medium`, `high`, ... as the model lists them), sent with every turn when set.
    pub effort: Option<String>,
    /// When Codex asks before it acts.
    pub approval: Approval,
    /// What commands may touch without asking.
    pub sandbox: Sandbox,
    /// Instructions for the agent, sent as the thread's `developerInstructions`.
    pub instructions: Option<String>,
    /// A thread id to continue (`thread/resume`); `None` starts a new thread. When the thread cannot be
    /// resumed a new one is started instead, after an [`Event::Warning`].
    pub resume: Option<String>,
    /// Configuration overrides: each becomes a `-c key=value` argument of `codex app-server`, the value read
    /// as TOML.
    pub config: Vec<(String, String)>,
    /// The caller's version, reported to Codex as `clientInfo.version`; empty reports this crate's.
    pub client_version: String,
}

/// When Codex asks for approval before running something.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Approval {
    /// Ask for everything but commands known to be safe.
    Untrusted,
    /// The model decides when to ask.
    #[default]
    OnRequest,
    /// Never ask: what the sandbox refuses fails.
    Never,
}

impl Approval {
    /// Codex's spelling of the policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }
}

/// What the commands Codex runs may touch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Sandbox {
    /// Read files only.
    ReadOnly,
    /// Also write inside the working directory.
    #[default]
    WorkspaceWrite,
    /// No sandbox.
    FullAccess,
}

impl Sandbox {
    /// Codex's spelling of the mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "danger-full-access",
        }
    }
}

/// One piece of what the user sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Text.
    Text(String),
    /// A picture in a file Codex reads itself.
    LocalImage(PathBuf),
}

/// The answer to an [`Event::Approval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Go ahead, this once.
    Accept,
    /// Go ahead, and do not ask again for the like of it in this session.
    AcceptForSession,
    /// Do not; the agent carries on and may try something else.
    Decline,
    /// Do not, and stop the turn.
    Cancel,
}

/// What Codex offers for an account, from [`info`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    /// The binary's version, e.g. `0.167.1`.
    pub version: String,
    /// The folder holding the account's configuration and conversations (`CODEX_HOME`).
    pub codex_home: String,
    /// The account the binary is signed in to; `None` when it is not.
    pub account: Option<Account>,
    /// Whether the configured model provider needs a sign-in.
    pub requires_auth: bool,
    /// The models it offers.
    pub models: Vec<Model>,
    /// Where the plan's limits stand; `None` when the binary could not say (it cannot when signed out).
    pub limits: Option<RateLimits>,
}

/// The account the binary is signed in to. This is what the binary reports about itself; this crate never
/// reads its credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// How it is signed in: `chatgpt`, `apiKey`, `amazonBedrock`, `gcp`, ...
    pub kind: String,
    /// The account's email address; empty when not said (an API key has none).
    pub email: String,
    /// The plan, e.g. `plus` or `pro`; empty when not said.
    pub plan: String,
}

/// A model the binary offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    /// What [`Options::model`] takes.
    pub id: String,
    /// Its name for display.
    pub name: String,
    /// One line on it.
    pub description: String,
    /// The reasoning efforts it takes, in Codex's order.
    pub efforts: Vec<String>,
    /// The effort it runs with when none is chosen.
    pub default_effort: String,
    /// Whether it is the model Codex picks by itself.
    pub is_default: bool,
    /// Whether Codex leaves it out of its own model picker.
    pub hidden: bool,
}

/// Where the account's plan limits stand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimits {
    /// The short window (five hours on a subscription).
    pub primary: Option<RateWindow>,
    /// The long window (a week).
    pub secondary: Option<RateWindow>,
    /// The plan; empty when not said (an update during a turn may leave it out).
    pub plan: String,
    /// Whether a limit is reached.
    pub reached: bool,
}

/// One window of a plan's limits.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RateWindow {
    /// How much of it is used, `0..=100`.
    pub used_percent: f64,
    /// How long the window is, in minutes.
    pub window_minutes: Option<u64>,
    /// When it resets, in Unix seconds.
    pub resets_at: Option<u64>,
}

/// Something the Codex binary reported. Delivered to the callback given to [`Session::connect`].
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The thread is open: messages can be answered. `resumed` is false when a new thread was started, also
    /// when resuming failed.
    Ready {
        /// The thread's id; pass it as [`Options::resume`] to continue the conversation later.
        thread_id: String,
        /// The model in use.
        model: String,
        /// The reasoning effort in use, when Codex says.
        effort: Option<String>,
        /// The sandbox in force: `readOnly`, `workspaceWrite`, `dangerFullAccess` or `externalSandbox`.
        sandbox: String,
        /// Whether the thread of [`Options::resume`] was continued.
        resumed: bool,
        /// The binary's version.
        version: String,
    },
    /// A turn began.
    TurnStarted {
        /// The turn's id.
        turn_id: String,
    },
    /// A user message entered the conversation: the prompt of a turn, or input steered into it.
    UserMessage {
        /// The `client_id` it was sent with ([`Session::send`]).
        client_id: Option<String>,
        /// Its text.
        text: String,
    },
    /// Streamed text of the reply.
    TextDelta {
        /// The [`Item::AgentMessage`] it belongs to.
        item_id: String,
        /// The new text.
        text: String,
    },
    /// Streamed reasoning: its summary, or the reasoning itself where a model gives that.
    ReasoningDelta {
        /// The [`Item::Reasoning`] it belongs to.
        item_id: String,
        /// The new text.
        text: String,
    },
    /// The reasoning summary starts a new paragraph.
    ReasoningBreak {
        /// The [`Item::Reasoning`] it belongs to.
        item_id: String,
    },
    /// Streamed output of a command or a file change.
    OutputDelta {
        /// The [`Item::Command`] or [`Item::FileChange`] it belongs to.
        item_id: String,
        /// The new output.
        text: String,
    },
    /// An item (a message, a command, a tool call, ...) began; it is repeated, complete, as
    /// [`Event::ItemCompleted`].
    ItemStarted(Item),
    /// An item is complete.
    ItemCompleted(Item),
    /// The agent's plan for the turn, whole, each time it changes.
    Plan {
        /// Why the plan changed.
        explanation: Option<String>,
        /// Its steps, in order.
        steps: Vec<PlanStep>,
    },
    /// Token counts of the thread.
    TokenUsage {
        /// The latest request to the model.
        last: TokenUsage,
        /// Everything so far.
        total: TokenUsage,
        /// The model's context window, when known.
        context_window: Option<u64>,
    },
    /// Where the account's plan limits stand. Sent when they change.
    RateLimits(RateLimits),
    /// Codex asks whether something may happen. Answer with [`Session::respond`].
    Approval(ApprovalRequest),
    /// The agent asks the user something. Answer with [`Session::answer`].
    Question(QuestionRequest),
    /// A request to the client was settled: answered here or elsewhere, or withdrawn. It may name a request
    /// this crate answered by itself and never reported.
    RequestResolved {
        /// The request, as in [`ApprovalRequest::request_id`] and [`QuestionRequest::request_id`].
        request_id: String,
    },
    /// Something went wrong in a turn. With `will_retry` the turn goes on; without, it ends as failed.
    Error {
        /// What went wrong.
        message: String,
        /// More on it, e.g. the server's answer.
        details: Option<String>,
        /// Codex's name for the kind of error, e.g. `usageLimitExceeded`, `unauthorized`,
        /// `httpConnectionFailed`; empty when not said.
        kind: String,
        /// The HTTP status behind it, when there is one.
        http_status: Option<u16>,
        /// Whether Codex tries again by itself.
        will_retry: bool,
    },
    /// Something the user should know that is not a failure, from Codex or from this crate (a request that was
    /// refused, a conversation that could not be resumed).
    Warning(String),
    /// The conversation was summarised to make room.
    Compacted,
    /// Codex answered with another model than the one asked for.
    ModelRerouted {
        /// The model asked for.
        from: String,
        /// The model that answers.
        to: String,
    },
    /// A turn ended. A turn that could not be started at all ends here too, as failed and with an empty
    /// `turn_id`.
    TurnCompleted {
        /// The turn's id.
        turn_id: String,
        /// How it ended.
        status: TurnStatus,
        /// Why it failed.
        error: Option<String>,
    },
    /// Anything above that belongs to another thread than the session's own: a sub-agent's. Requests
    /// ([`Event::Approval`], [`Event::Question`], [`Event::RequestResolved`]) are never wrapped, they name
    /// their thread themselves.
    Sub {
        /// The thread it belongs to.
        thread_id: String,
        /// What happened there.
        event: Box<Event>,
    },
    /// A line the binary wrote to its standard error.
    Stderr(String),
    /// The process ended; nothing follows.
    Closed {
        /// The exit code; `None` when a signal ended the process.
        exit_code: Option<i32>,
        /// Set for a failed start, a non-zero exit or a broken pipe.
        error: Option<String>,
    },
}

/// A piece of a conversation, from Codex's `ThreadItem`.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// Text of the reply.
    AgentMessage {
        /// The item's id.
        id: String,
        /// The text.
        text: String,
        /// `commentary`, `partial_answer` or `final_answer`, where the model says.
        phase: Option<String>,
    },
    /// Reasoning.
    Reasoning {
        /// The item's id.
        id: String,
        /// The summary, a paragraph each.
        summary: Vec<String>,
        /// The reasoning itself, where a model gives it.
        content: Vec<String>,
    },
    /// A command run in the shell.
    Command {
        /// The item's id.
        id: String,
        /// The command line.
        command: String,
        /// Where it runs.
        cwd: String,
        /// How it is doing.
        status: ItemStatus,
        /// What it printed, once it ended.
        output: Option<String>,
        /// Its exit code.
        exit_code: Option<i64>,
        /// How long it ran.
        duration_ms: Option<u64>,
    },
    /// Changes to files.
    FileChange {
        /// The item's id.
        id: String,
        /// The files changed.
        changes: Vec<FileChange>,
        /// How it is doing.
        status: ItemStatus,
    },
    /// A call of an MCP server's tool.
    McpToolCall {
        /// The item's id.
        id: String,
        /// The server.
        server: String,
        /// The tool.
        tool: String,
        /// The tool's input.
        arguments: Value,
        /// How it is doing.
        status: ItemStatus,
        /// What it returned: the text parts joined, or the JSON when there are none.
        result: Option<String>,
        /// Why it failed.
        error: Option<String>,
    },
    /// A call of a tool the client offers.
    ToolCall {
        /// The item's id.
        id: String,
        /// The tool.
        tool: String,
        /// The tool's input.
        arguments: Value,
        /// How it is doing.
        status: ItemStatus,
        /// The text it returned.
        output: Option<String>,
    },
    /// A web search.
    WebSearch {
        /// The item's id.
        id: String,
        /// What was searched for.
        query: String,
        /// What exactly was done (`search`, `open_page`, `find_in_page`), as Codex reports it.
        action: Option<Value>,
    },
    /// The agent works with other agents: starts one, sends it input, waits for it, closes it.
    Collab {
        /// The item's id.
        id: String,
        /// `spawnAgent`, `sendInput`, `wait`, `closeAgent`, ...
        tool: String,
        /// How it is doing.
        status: ItemStatus,
        /// What the other agent was told.
        prompt: Option<String>,
        /// The threads of the agents addressed; their events arrive as [`Event::Sub`].
        receivers: Vec<String>,
        /// The agents' state as last known: thread id, status (`running`, `completed`, `errored`, ...) and
        /// message.
        states: Vec<(String, String, Option<String>)>,
    },
    /// A plan the agent proposes.
    Plan {
        /// The item's id.
        id: String,
        /// The plan.
        text: String,
    },
    /// The agent looked at a picture.
    ImageView {
        /// The item's id.
        id: String,
        /// The picture's file.
        path: String,
    },
    /// The conversation is summarised to make room.
    Compaction {
        /// The item's id.
        id: String,
    },
    /// A message of the user. A session reports these as [`Event::UserMessage`].
    UserMessage {
        /// The item's id.
        id: String,
        /// The `client_id` it was sent with.
        client_id: Option<String>,
        /// Its text.
        text: String,
    },
    /// Any other item, as received.
    Other {
        /// The item's id.
        id: String,
        /// Its `type`.
        kind: String,
        /// The whole item.
        raw: Value,
    },
}

impl Item {
    /// The item's id, which its deltas name.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::AgentMessage { id, .. }
            | Self::Reasoning { id, .. }
            | Self::Command { id, .. }
            | Self::FileChange { id, .. }
            | Self::McpToolCall { id, .. }
            | Self::ToolCall { id, .. }
            | Self::WebSearch { id, .. }
            | Self::Collab { id, .. }
            | Self::Plan { id, .. }
            | Self::ImageView { id, .. }
            | Self::Compaction { id }
            | Self::UserMessage { id, .. }
            | Self::Other { id, .. } => id,
        }
    }
}

/// How an item is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ItemStatus {
    /// It runs (also when Codex does not say).
    #[default]
    InProgress,
    /// It is done.
    Completed,
    /// It failed.
    Failed,
    /// The user declined it.
    Declined,
    /// It was stopped.
    Interrupted,
}

/// One file of an [`Item::FileChange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The file.
    pub path: String,
    /// What happens to it.
    pub kind: ChangeKind,
    /// Where it moves to, when an update renames it.
    pub move_path: Option<String>,
    /// The change as a unified diff (the content, for a new file).
    pub diff: String,
}

/// What a [`FileChange`] does to its file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// The file is new.
    Add,
    /// The file goes.
    Delete,
    /// The file changes (also when Codex does not say).
    #[default]
    Update,
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    /// What is to be done.
    pub step: String,
    /// How far it is.
    pub status: PlanStatus,
}

/// How far a [`PlanStep`] is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PlanStatus {
    /// Not begun.
    #[default]
    Pending,
    /// Being done.
    InProgress,
    /// Done.
    Completed,
}

/// Token counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input tokens, cached ones included.
    pub input: u64,
    /// Input tokens read from the cache.
    pub cached_input: u64,
    /// Input tokens written to the cache.
    pub cache_write_input: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Output tokens spent on reasoning.
    pub reasoning_output: u64,
    /// Input and output together.
    pub total: u64,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnStatus {
    /// The agent finished.
    Completed,
    /// It was stopped.
    Interrupted,
    /// It failed.
    Failed,
}

/// What an [`ApprovalRequest`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalKind {
    /// To run a command.
    Command,
    /// To change files.
    FileChange,
    /// More room in the sandbox: network access, or files outside the working directory.
    Permissions,
}

/// Codex asks whether something may happen.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// Identifies the request to [`Session::respond`].
    pub request_id: String,
    /// What is asked for.
    pub kind: ApprovalKind,
    /// The item it is about: an [`Item::Command`] or [`Item::FileChange`] already started.
    pub item_id: String,
    /// The thread that asks: the session's own, or a sub-agent's.
    pub thread_id: String,
    /// The command to run.
    pub command: Option<String>,
    /// Where it would run.
    pub cwd: Option<String>,
    /// Why Codex asks.
    pub reason: Option<String>,
    /// A folder the agent wants to write under for the rest of the session.
    pub grant_root: Option<String>,
    /// The host a command wants to reach, when the question is about the network.
    pub host: Option<String>,
    /// The whole request (for a permissions request, `permissions` says what is wanted).
    pub raw: Value,
}

/// The agent asks the user something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    /// Identifies the request to [`Session::answer`].
    pub request_id: String,
    /// The tool call that asks.
    pub item_id: String,
    /// The thread that asks: the session's own, or a sub-agent's.
    pub thread_id: String,
    /// The questions.
    pub questions: Vec<Question>,
}

/// One question of a [`QuestionRequest`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Question {
    /// Names the question in the answer.
    pub id: String,
    /// A short title.
    pub header: String,
    /// The question.
    pub question: String,
    /// Whether an answer in the user's own words is welcome besides the options.
    pub is_other: bool,
    /// Whether the answer is a secret and should not be shown as typed.
    pub is_secret: bool,
    /// The answers to choose from, each a label and a description; empty for a free answer.
    pub options: Vec<(String, String)>,
}

/// Why a call into this crate failed.
#[derive(Debug)]
pub enum Error {
    /// No Codex binary was found.
    NotFound,
    /// The path names a `.cmd`/`.bat` script. Windows runs those through `cmd.exe`, which cannot be given
    /// arguments safely, so only a native `codex.exe` is run.
    BatchScript(PathBuf),
    /// An option was refused; the text says which and why.
    InvalidOption(String),
    /// The binary could not be started.
    Spawn(std::io::Error),
    /// Writing to the binary, or starting a reader thread, failed.
    Io(std::io::Error),
    /// The session is closed.
    Closed,
    /// [`Session::respond`] or [`Session::answer`] named a request that is not pending (unknown, already
    /// answered, or settled elsewhere) or that takes the other kind of answer.
    UnknownRequest(String),
    /// The binary did not report a version; the text says what happened.
    Version(String),
    /// The binary did not say what it offers; the text says what happened.
    NoInfo(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => {
                f.write_str("Codex not found: install it (npm install -g @openai/codex) or put `codex` on PATH")
            }
            Self::BatchScript(path) => {
                write!(f, "refusing to run batch script {}: use the native codex.exe", path.display())
            }
            Self::InvalidOption(why) => write!(f, "invalid option: {why}"),
            Self::Spawn(error) => write!(f, "failed to start the Codex binary: {error}"),
            Self::Io(error) => write!(f, "failed to talk to the Codex binary: {error}"),
            Self::Closed => f.write_str("the session is closed"),
            Self::UnknownRequest(id) => write!(f, "no pending request {id}"),
            Self::Version(why) => write!(f, "could not read the Codex version: {why}"),
            Self::NoInfo(why) => write!(f, "Codex did not report its models and account: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
