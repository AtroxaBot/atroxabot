//! The transport that drives the user's installed Antigravity CLI (`agy`) through `agy acp`.
//!
//! [`Session::connect`] starts the Antigravity binary the user has installed, exactly as published, and talks
//! to it in the Agent Client Protocol: JSON-RPC 2.0, a message a line, on its standard input and output. A
//! reader thread does the handshake and opens a session (a conversation); everything the binary says then
//! arrives as an [`Event`]. The [`Session`] handle sends the user's input, stops a turn, and answers approvals
//! and questions.
//!
//! The crate never touches authentication: the binary inherits the parent environment (plus [`Options::env`])
//! and is signed in however the user set it up with `agy`. When the binary says a sign-in is needed the
//! session ends with [`Event::SignInRequired`]; signing in is `agy acp --login` ([`login_args`]), started by
//! the caller in a terminal the user sees.
//!
//! Blocking I/O and std threads only; the single dependency is `serde_json`.

mod cli;
mod machine;
mod parse;
mod session;

use std::fmt;
use std::path::PathBuf;

pub use cli::{cli_version, find_cli, login_args, profile_env};
pub use serde_json::Value;
pub use session::{Session, info};

/// How to launch the Antigravity binary. `Options::default()` is a plain session in the current directory.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The native `agy` binary to run; `None` discovers it with [`find_cli`].
    pub cli_path: Option<PathBuf>,
    /// Environment variables added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Working directory of the session; `None` uses the parent's.
    pub cwd: Option<PathBuf>,
    /// The folder of another account than the one `agy` uses by itself: the binary keeps that account's
    /// sign-in, settings and conversations there. `None` is the user's own `~/.gemini`.
    pub profile: Option<PathBuf>,
    /// The model; `None` leaves the choice to Antigravity.
    pub model: Option<String>,
    /// How hard the model thinks, one of the levels the model lists; `None` leaves it as it is.
    pub effort: Option<String>,
    /// Instructions for the agent, added to Antigravity's own system prompt where the binary takes them.
    pub instructions: Option<String>,
    /// A session id to continue; `None` starts a new session. When the session cannot be continued a new one
    /// is started instead, after an [`Event::Warning`].
    pub resume: Option<String>,
    /// MCP servers the agent may use in this session.
    pub mcp_servers: Vec<McpServer>,
    /// The sign-in method to ask the binary to use (`oauth-personal` is a Google account) when it says a
    /// session needs one. That lets it use the sign-in it already has; it never shows this crate a credential.
    /// `None` never asks: a session that needs a sign-in ends with [`Event::SignInRequired`].
    pub authenticate: Option<String>,
    /// The caller's version, reported to the binary as `clientInfo.version`; empty reports this crate's.
    pub client_version: String,
}

/// An MCP server given to the agent for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The name its tools are known under.
    pub name: String,
    /// How it is reached.
    pub transport: McpTransport,
}

/// How an [`McpServer`] is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A program the binary starts and talks to over its standard streams.
    Stdio {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Environment variables for it.
        env: Vec<(String, String)>,
    },
    /// A server reached over HTTP. Left out, with an [`Event::Warning`], where the binary does not take those.
    Http {
        /// Its address.
        url: String,
        /// Headers sent with every request.
        headers: Vec<(String, String)>,
    },
}

/// One piece of what the user sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Text.
    Text(String),
    /// A picture in a file. It is sent along where the binary takes pictures and the file is one it reads
    /// (PNG, JPEG, WebP or BMP of at most ten megabytes); otherwise the agent is told the file's path.
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
    /// Do not, and leave the question unanswered, as stopping the turn does.
    Cancel,
}

/// What Antigravity offers for an account, from [`info`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    /// The binary's version, e.g. `1.5.12`.
    pub version: String,
    /// Whether the binary is signed in. When it is not, the rest is empty.
    pub signed_in: bool,
    /// The models the account may use and the one chosen.
    pub catalog: Catalog,
    /// How hard each model can think, by model id, for the models that say.
    pub efforts: Vec<(String, Vec<Effort>)>,
    /// The slash commands the binary takes as a message (`compact`, `goal`, ...), when it said in time.
    pub commands: Vec<SlashCommand>,
}

/// The models an account may use, and what the session runs with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// The models, in Antigravity's order.
    pub models: Vec<Model>,
    /// The model in use; empty when the binary does not say.
    pub model: String,
    /// How hard the model in use can think; empty when it has no such choice. Antigravity says this only for
    /// the model in use.
    pub efforts: Vec<Effort>,
    /// The level in use; empty when there is no choice.
    pub effort: String,
}

/// A model the binary offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    /// What [`Options::model`] takes.
    pub id: String,
    /// Its name for display.
    pub name: String,
    /// One line on it; empty when not said.
    pub description: String,
}

/// A level of thinking a model offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effort {
    /// What [`Options::effort`] takes.
    pub id: String,
    /// Its name for display.
    pub name: String,
}

/// A command the binary takes as a message that starts with a slash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlashCommand {
    /// The command, without the slash.
    pub name: String,
    /// What it does.
    pub description: String,
    /// What to write after it, when it takes something.
    pub hint: String,
}

/// Something the Antigravity binary reported. Delivered to the callback given to [`Session::connect`].
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The session is open: messages can be answered. `resumed` is false when a new session was started, also
    /// when continuing one failed.
    Ready {
        /// The session's id; pass it as [`Options::resume`] to continue the conversation later.
        session_id: String,
        /// Whether the session of [`Options::resume`] was continued.
        resumed: bool,
        /// The binary's version.
        version: String,
        /// The models on offer and the one in use, before [`Options::model`] is applied.
        catalog: Catalog,
    },
    /// The models on offer or the one in use changed, or a model said how hard it can think.
    Catalog(Catalog),
    /// The slash commands the binary takes, whole, each time they change.
    Commands(Vec<SlashCommand>),
    /// A turn began: the agent works on a message, or went on by itself after something it had started in the
    /// background finished.
    TurnStarted,
    /// A user message was given to the agent: the prompt of a turn, or input put into the running one.
    UserMessage {
        /// The `client_id` it was sent with ([`Session::send`]).
        client_id: Option<String>,
        /// Its text.
        text: String,
    },
    /// Streamed text of the reply.
    TextDelta {
        /// The message it belongs to, where the binary says.
        message_id: Option<String>,
        /// The new text.
        text: String,
    },
    /// Streamed thinking.
    ReasoningDelta {
        /// The new text.
        text: String,
    },
    /// A tool call, whole, when it begins and each time something about it changes.
    Tool(Tool),
    /// The agent's plan for the turn, whole, each time it changes.
    Plan {
        /// Its steps, in order.
        steps: Vec<PlanStep>,
    },
    /// How full the model's context is.
    Context {
        /// Tokens in it.
        used: u64,
        /// Tokens it holds.
        size: u64,
    },
    /// The binary asks whether something may happen. Answer with [`Session::respond`].
    Approval(ApprovalRequest),
    /// The agent asks the user to choose. Answer with [`Session::answer`].
    Question(QuestionRequest),
    /// A request was settled without the caller's answer: stopping the turn withdrew it.
    RequestResolved {
        /// The request, as in [`ApprovalRequest::request_id`] and [`QuestionRequest::request_id`].
        request_id: String,
    },
    /// The binary is not signed in. The session ends; [`Event::Closed`] follows.
    SignInRequired,
    /// Something the user should know that is not a failure, from the binary or from this crate (a request
    /// that was refused, a conversation that could not be continued).
    Warning(String),
    /// A turn ended.
    TurnCompleted {
        /// How it ended.
        status: TurnStatus,
        /// Why the agent stopped, in the protocol's words: `end_turn`, `max_tokens`, `max_turn_requests`,
        /// `refusal` (which Antigravity says when the account's quota is used up) or `cancelled`; empty when a
        /// request failed instead.
        stop_reason: String,
        /// Why it failed.
        error: Option<String>,
        /// The tokens the turn took, where the binary says.
        usage: Option<TokenUsage>,
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

/// A tool call of the agent, as far as it is known.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tool {
    /// The call's id.
    pub id: String,
    /// What the binary calls it, e.g. `Running run_command`.
    pub title: String,
    /// What kind of thing it does.
    pub kind: ToolKind,
    /// How it is doing.
    pub status: ToolStatus,
    /// The command line, for a command.
    pub command: Option<String>,
    /// Where the command runs.
    pub cwd: Option<String>,
    /// What it printed or returned, as far as it is known; a very long output keeps its end.
    pub output: Option<String>,
    /// The command's exit code.
    pub exit_code: Option<i64>,
    /// The changes to files it makes.
    pub diffs: Vec<Diff>,
    /// The files it touches, each with a line where the binary says.
    pub locations: Vec<(String, Option<u64>)>,
    /// The tool's input, as the binary gave it; `null` when it did not.
    pub input: Value,
    /// Whether it is a tool of an MCP server.
    pub mcp: bool,
}

/// What kind of thing a [`Tool`] does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolKind {
    /// Reads a file.
    Read,
    /// Changes a file.
    Edit,
    /// Deletes a file.
    Delete,
    /// Moves a file.
    Move,
    /// Searches.
    Search,
    /// Runs a command.
    Execute,
    /// Thinks.
    Think,
    /// Fetches something from the network.
    Fetch,
    /// Has other agents work: their steps are not reported, only what they return.
    Subagent,
    /// Anything else (also when the binary does not say).
    #[default]
    Other,
}

/// How a [`Tool`] is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolStatus {
    /// Not begun: its input is still streaming, or it waits for approval.
    #[default]
    Pending,
    /// It runs.
    InProgress,
    /// It is done.
    Completed,
    /// It failed, or was not allowed.
    Failed,
}

impl ToolStatus {
    /// Whether nothing more is to come of the call.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// One file changed by a [`Tool`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// The file.
    pub path: String,
    /// What is replaced; `None` for a new file. It may be a part of the file only.
    pub old_text: Option<String>,
    /// What it becomes.
    pub new_text: String,
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
    /// Output tokens, thinking included.
    pub output: u64,
    /// Output tokens spent on thinking.
    pub reasoning_output: u64,
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

/// The binary asks whether a tool call may happen.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// Identifies the request to [`Session::respond`].
    pub request_id: String,
    /// The call it is about, as in the [`Event::Tool`] just before.
    pub tool: Tool,
    /// Whether the binary offers to stop asking for the like of it ([`Decision::AcceptForSession`]).
    pub can_remember: bool,
    /// What the binary warns of when it is told to stop asking.
    pub warning: Option<String>,
}

/// The agent asks the user to choose one of a few answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    /// Identifies the request to [`Session::answer`].
    pub request_id: String,
    /// The question.
    pub question: String,
    /// The answers to choose from, each an id and its text. Nothing else is taken for an answer.
    pub options: Vec<(String, String)>,
}

/// Why a call into this crate failed.
#[derive(Debug)]
pub enum Error {
    /// No Antigravity binary was found.
    NotFound,
    /// The path names a `.cmd`/`.bat` script. Windows runs those through `cmd.exe`, which cannot be given
    /// arguments safely, so only a native `agy.exe` is run.
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
    /// answered, or withdrawn), that takes the other kind of answer, or an answer the question does not offer.
    UnknownRequest(String),
    /// The binary did not report a version; the text says what happened.
    Version(String),
    /// The binary did not say what it offers; the text says what happened.
    NoInfo(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("Antigravity not found: install its CLI or put `agy` on PATH"),
            Self::BatchScript(path) => {
                write!(f, "refusing to run batch script {}: use the native agy.exe", path.display())
            }
            Self::InvalidOption(why) => write!(f, "invalid option: {why}"),
            Self::Spawn(error) => write!(f, "failed to start the Antigravity binary: {error}"),
            Self::Io(error) => write!(f, "failed to talk to the Antigravity binary: {error}"),
            Self::Closed => f.write_str("the session is closed"),
            Self::UnknownRequest(id) => write!(f, "no pending request {id}"),
            Self::Version(why) => write!(f, "could not read the Antigravity version: {why}"),
            Self::NoInfo(why) => write!(f, "Antigravity did not report its models: {why}"),
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
