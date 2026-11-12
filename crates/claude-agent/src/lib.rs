//! A Rust port of the Claude Agent SDK's transport and control protocol.
//!
//! [`Session::connect`] starts the Claude Code binary the user has installed, exactly as published, and talks to it
//! over newline-delimited JSON on its standard input and output (`--input-format stream-json --output-format
//! stream-json`). Everything the binary says arrives as an [`Event`] on a reader thread; the [`Session`] handle
//! sends user messages, interrupts and permission decisions.
//!
//! The crate never touches authentication: the binary inherits the parent environment (minus `CLAUDECODE`, so a
//! launch from inside a Claude Code session works, plus [`Options::env`]) and signs in however the user set it up.
//!
//! Blocking I/O and std threads only; the single dependency is `serde_json`.

mod cli;
mod parse;
mod session;

use std::fmt;
use std::path::PathBuf;

pub use cli::{cli_version, find_cli};
pub use serde_json::Value;
pub use session::{Decision, Session, info, mcp_servers, plan_usage};

/// How to launch the Claude Code binary. `Options::default()` is a plain session in the current directory.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The Claude Code binary to run; `None` discovers it with [`find_cli`].
    pub cli_path: Option<PathBuf>,
    /// Working directory of the session; `None` inherits the parent's.
    pub cwd: Option<PathBuf>,
    /// The system prompt.
    pub system_prompt: SystemPrompt,
    /// Model alias or full name (`--model`).
    pub model: Option<String>,
    /// Effort level (`--effort`): `low`, `medium`, `high`, `xhigh` or `max`.
    pub effort: Option<String>,
    /// Starting permission mode (`--permission-mode`).
    pub permission_mode: Option<PermissionMode>,
    /// Tool rules to allow without asking (`--allowedTools`).
    pub allowed_tools: Vec<String>,
    /// Tool rules to deny (`--disallowedTools`).
    pub disallowed_tools: Vec<String>,
    /// Session id (or name) to resume (`--resume`).
    pub resume: Option<String>,
    /// When resuming, continue under a new session id (`--fork-session`).
    pub fork_session: bool,
    /// Maximum agentic turns per user message (`--max-turns`).
    pub max_turns: Option<u32>,
    /// Extra directories tools may access (`--add-dir`).
    pub add_dirs: Vec<PathBuf>,
    /// Plugins to load for this session only, each a folder (`--plugin-dir`).
    pub plugin_dirs: Vec<PathBuf>,
    /// MCP servers as JSON text or a path to a JSON file (`--mcp-config`).
    pub mcp_config: Option<String>,
    /// Use only the servers from [`Options::mcp_config`] (`--strict-mcp-config`).
    pub strict_mcp_config: bool,
    /// Which settings files to load: `user`, `project`, `local` (`--setting-sources`). `None` keeps the default.
    pub setting_sources: Option<Vec<String>>,
    /// Stream the reply as [`Event::TextDelta`] / [`Event::ThinkingDelta`] (`--include-partial-messages`).
    pub include_partial_messages: bool,
    /// After each turn, have the binary guess what the user will write next (`--prompt-suggestions`): it
    /// arrives as an [`Event::PromptSuggestion`]. Each guess is a small request of its own on the account,
    /// mostly read from the prompt cache; the binary skips it when it would cost more (early in a session,
    /// a cold cache, an account near its limit or on extra usage) and when its guesses go unused.
    pub prompt_suggestions: bool,
    /// Route permission prompts to this process (`--permission-prompt-tool stdio`): each arrives as an
    /// [`Event::Permission`] and waits for [`Session::respond`].
    pub ask_permissions: bool,
    /// Environment variables added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Further flags, without the leading `--`, each with an optional value. `bare` is refused: it disables
    /// subscription sign-in.
    pub extra_args: Vec<(String, Option<String>)>,
}

/// Which system prompt the session runs with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SystemPrompt {
    /// Claude Code's own system prompt (no flag).
    #[default]
    Preset,
    /// Claude Code's system prompt with this text appended (`--append-system-prompt`).
    Append(String),
    /// This text instead of Claude Code's system prompt (`--system-prompt`).
    Custom(String),
}

/// How the binary decides whether a tool may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// Ask for anything not already allowed.
    Default,
    /// Accept file edits without asking.
    AcceptEdits,
    /// Plan only; nothing is changed.
    Plan,
    /// Skip every permission check.
    BypassPermissions,
    /// Never ask: deny whatever is not already allowed.
    DontAsk,
    /// Let a classifier approve or deny.
    Auto,
}

impl PermissionMode {
    /// The Claude Code binary's spelling of the mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
        }
    }

    /// Parses the binary's spelling (as in [`Init::permission_mode`]). `manual` is its newer name for `default`.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "default" | "manual" => Self::Default,
            "acceptEdits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "bypassPermissions" => Self::BypassPermissions,
            "dontAsk" => Self::DontAsk,
            "auto" => Self::Auto,
            _ => return None,
        })
    }
}

/// What the binary says about itself in answer to `initialize`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    /// The models it offers, its default first.
    pub models: Vec<ModelInfo>,
    /// The account it is signed in to; empty fields when it is not.
    pub account: Account,
    /// The binary's version.
    pub version: String,
    /// The slash commands and skills a message can start with, as `/name`.
    pub commands: Vec<SlashCommand>,
    /// The whole answer, for fields this crate does not model.
    pub raw: Value,
}

/// A slash command or skill the binary offers: sending `/name arguments` as a message runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlashCommand {
    /// Its name, without the slash.
    pub name: String,
    /// What it does.
    pub description: String,
    /// What it takes after its name, e.g. `<file>`; often empty.
    pub argument_hint: String,
}

/// Where the account's plan limits stand, from a `rate_limit_event`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimit {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    /// The window that decides right now, e.g. `five_hour`.
    pub kind: String,
    /// When that window resets, in Unix seconds; 0 when not said.
    pub resets_at: u64,
    /// Every window the binary reports, by name.
    pub windows: Vec<RateWindow>,
}

/// One window of a plan's limits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateWindow {
    /// Its name: `five_hour`, `seven_day`, ...
    pub name: String,
    /// How much of it is used, `0..=1`.
    pub utilization: f64,
    /// When it resets, in Unix seconds; 0 when not said.
    pub resets_at: u64,
    /// `allowed`, `allowed_warning` or `rejected`; empty when not said (a [`PlanUsage`] never says).
    pub status: String,
}

/// The account's plan usage as Claude Code's `/usage` shows it, from [`Session::plan_usage`] or [`plan_usage`].
/// Claude Code fetches it from Anthropic itself, with its own sign-in. Anthropic calls the request experimental
/// and says its shape may change: whatever is missing or mistyped is left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanUsage {
    /// The claude.ai plan (`pro`, `max`, `team`, `enterprise`); empty for an API key or a cloud provider.
    pub subscription: String,
    /// Whether plan limits apply to how the binary signs in: not for an API key, Bedrock or Vertex.
    pub available: bool,
    /// Where the plan's limits stand; `None` when they do not apply, or Claude Code could not get them (its
    /// usage endpoint did not answer and it kept nothing recent).
    pub limits: Option<PlanLimits>,
    /// The whole answer.
    pub raw: Value,
}

/// Where a plan's limits stand, from a [`PlanUsage`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanLimits {
    /// The windows of the whole plan, by Claude Code's names for them: `five_hour`, `seven_day`, and where the
    /// plan has them `seven_day_opus`, `seven_day_sonnet` and `seven_day_oauth_apps`. A window without a
    /// number is left out.
    pub windows: Vec<RateWindow>,
    /// The weekly windows of single models (`model_scoped`), each named as Anthropic labels its model
    /// (`Fable`). `None` when nothing is known of them: Claude Code answered from what it kept rather than
    /// asking. Empty when the account has none.
    pub models: Option<Vec<RateWindow>>,
}

/// How a subagent is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskState {
    /// It was started.
    #[default]
    Started,
    /// It moved on: another tool call.
    Running,
    /// It finished.
    Completed,
    /// It failed.
    Failed,
    /// It was stopped.
    Stopped,
}

/// News of a subagent, from `system`/`task_started`, `task_progress` and `task_notification`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Task {
    /// The tool call that started it, matching a [`Block::ToolUse`] and the `parent_tool_use_id` of its messages.
    pub tool_use_id: String,
    /// How it is doing.
    pub state: TaskState,
    /// What it was started for, or (while running) what it is doing now.
    pub description: String,
    /// Its kind, e.g. `general-purpose` or `Explore`.
    pub subagent_type: String,
    /// What sort of task it is, from `task_started` only: `local_agent` (a subagent), but also
    /// `local_bash` (a command sent to the background), `monitor_mcp` and others that are no subagent.
    pub task_type: String,
    /// The tool it called last.
    pub last_tool: String,
    /// Tool calls it made so far.
    pub tool_uses: u64,
    /// Tokens it used so far.
    pub total_tokens: u64,
    /// How long it has run.
    pub duration_ms: u64,
    /// Its final words, once it ended.
    pub summary: String,
}

/// A model the binary offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// What `--model` takes: an alias (`default`, `opus`) or a full name.
    pub value: String,
    /// The model the alias stands for now, e.g. `claude-opus-5-5`.
    pub resolved: String,
    /// Its short name, e.g. `Opus`.
    pub display_name: String,
    /// One line on it, e.g. `Opus 5.5 · Best for everyday, complex tasks`.
    pub description: String,
    /// The effort levels it takes, weakest first; empty when it takes none.
    pub efforts: Vec<String>,
    /// Whether the `auto` permission mode works with it.
    pub auto_mode: bool,
}

/// The account the binary is signed in to. This is what the binary reports about itself; this crate never
/// reads its credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// The account's email address; empty when not signed in.
    pub email: String,
    /// The organization it belongs to.
    pub organization: String,
    /// The plan, e.g. `Claude Max`.
    pub subscription: String,
}

/// Something the Claude Code binary reported. Delivered to the callback given to [`Session::connect`].
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The binary's answer to `initialize`, right after connect and before any message: the models it offers
    /// and the account it is signed in to.
    Ready(Info),
    /// `system`/`init`: the session's settings. Arrives once the first user message is being processed, not at
    /// connect, and may be repeated on later turns.
    Init(Init),
    /// Streamed text of the reply (needs [`Options::include_partial_messages`]).
    TextDelta {
        /// The new text.
        text: String,
        /// Set when the text comes from a subagent: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
    },
    /// Streamed thinking (needs [`Options::include_partial_messages`]). The text is empty when the model's
    /// thinking is not displayed.
    ThinkingDelta {
        /// The new thinking text.
        text: String,
        /// Set when the thinking comes from a subagent: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
    },
    /// The model began a message (`message_start`; needs [`Options::include_partial_messages`]). The earliest
    /// sign of a reply, also of a turn the binary starts by itself (when a task sent to the background ends).
    MessageStart {
        /// Set inside a subagent: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
    },
    /// The model began thinking (a thinking block's `content_block_start`). Summarised thinking arrives only once
    /// the block is complete, so until then this is all there is to show.
    ThinkingStarted {
        /// Set inside a subagent: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
    },
    /// The answer to [`Session::mcp_status`]: the session's MCP servers and how each is doing.
    McpStatus(Vec<McpServerStatus>),
    /// The answer to [`Session::context_usage`]: what the context window holds.
    ContextUsage(ContextUsage),
    /// The answer to [`Session::plan_usage`]: where the account's plan limits stand.
    PlanUsage(PlanUsage),
    /// A complete piece of an assistant message. The binary sends one of these per content block, each repeating
    /// the message's `usage`, so add up [`TurnResult::usage`] rather than these.
    Assistant {
        /// The message's content.
        blocks: Vec<Block>,
        /// The model that wrote it.
        model: String,
        /// Set when a subagent wrote it: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
        /// Token counts of this message.
        usage: Usage,
        /// Set when the message stands in for an API failure, e.g. `rate_limit` or `authentication_failed`.
        error: Option<String>,
    },
    /// A user-role message echoed by the binary: tool results and subagent prompts, and (with
    /// `--replay-user-messages`) the caller's own messages at the moment a turn takes them in.
    User {
        /// The message's content.
        blocks: Vec<Block>,
        /// Set inside a subagent: the id of the tool call that started it.
        parent_tool_use_id: Option<String>,
        /// The message's id: for a replayed message, the `uuid` it was sent with.
        uuid: String,
        /// Whether this is one of the caller's messages, replayed (`isReplay`).
        replay: bool,
    },
    /// The binary asks whether a tool may run (`can_use_tool`). Answer with [`Session::respond`].
    Permission(PermissionRequest),
    /// The binary withdrew a pending [`Event::Permission`]; it can no longer be answered.
    PermissionCancelled {
        /// The withdrawn request.
        request_id: String,
    },
    /// A turn ended.
    Result(TurnResult),
    /// `system`/`api_retry`: an API call failed and will be retried.
    Retry {
        /// Which attempt failed, counting from 1.
        attempt: u64,
        /// How many attempts will be made.
        max_retries: u64,
        /// Wait before the next attempt.
        delay_ms: u64,
        /// What went wrong, e.g. `rate_limit (HTTP 429)`.
        error: String,
    },
    /// `rate_limit_event`: where the account's plan limits stand. Sent when they change.
    RateLimit(RateLimit),
    /// `prompt_suggestion`: what the user may write next (needs [`Options::prompt_suggestions`]). Arrives a
    /// moment after a turn's [`Event::Result`], and not after every turn.
    PromptSuggestion(String),
    /// A subagent started, moved on or ended.
    Task(Task),
    /// `system`/`status`: what the binary is busy with besides the model (`compacting`), `None` once it is done.
    Status(Option<String>),
    /// `system`/`session_state_changed`: the binary is `running` a turn (one it started by itself too, when a
    /// task sent to the background ended), is `idle`, or waits on the caller (`requires_action`). Sent because
    /// [`Session::connect`] asks for it, as the official SDK does.
    SessionState(String),
    /// `system`/`compact_boundary`: the conversation was summarised to make room.
    Compacted {
        /// Whether the user asked for it (`/compact`) rather than the context filling up.
        manual: bool,
        /// The context's size before, in tokens.
        pre_tokens: u64,
        /// Its size after; 0 when not said.
        post_tokens: u64,
    },
    /// Any other message: other `system` subtypes (hook events, ...), stream events that are not text or thinking
    /// deltas (`kind` is `stream_event`, `subtype` the event's type), answers to control requests this crate
    /// does not model (`kind` is `control_response`, `subtype` `success`; a bare acknowledgement is not
    /// reported), and top-level types this crate does not model.
    Other {
        /// The message's `type`.
        kind: String,
        /// The message's `subtype`, or empty.
        subtype: String,
        /// The whole message.
        raw: Value,
    },
    /// A line the binary wrote to its standard error.
    Stderr(String),
    /// A control request of ours failed, or a line could not be parsed or was too long.
    Error(String),
    /// The process ended; nothing follows.
    Closed {
        /// The exit code; `None` when a signal ended the process.
        exit_code: Option<i32>,
        /// Set for a non-zero exit or a broken pipe.
        error: Option<String>,
    },
}

/// An MCP server of a session and how it is doing, from [`Session::mcp_status`]. A Claude account's claude.ai
/// connectors are among them (`source` `claudeai`, names starting `claude.ai `).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerStatus {
    /// Its name.
    pub name: String,
    /// `connected`, `pending`, `needs-auth`, `failed` or `disabled`.
    pub status: String,
    /// Where it is configured: `claudeai`, `user`, `project`, `local`, `dynamic` (`--mcp-config`) and others.
    pub source: String,
    /// Why it failed, when it did.
    pub error: String,
    /// How many tools it offers.
    pub tools: usize,
}

/// What the context window holds, as Claude Code's `/context` shows it, from [`Session::context_usage`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsage {
    /// What it holds, by kind, in Claude Code's order.
    pub categories: Vec<ContextCategory>,
    /// The tokens in use.
    pub total: u64,
    /// How many the window takes.
    pub max: u64,
}

/// One kind of thing the context window holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCategory {
    /// Claude Code's key for it: `system_prompt`, `system_tools`, `mcp_tools`, `mcp_instructions`,
    /// `memory_files`, `skills`, `messages`, `free_space` and others.
    pub id: String,
    /// Its name as Claude Code shows it ("System prompt", "MCP tools", …).
    pub name: String,
    /// The tokens it takes.
    pub tokens: u64,
    /// `used` (in the context), `deferred` (tools loaded only when searched for, not in it), `buffer`
    /// (kept free for summarising) or `free`.
    pub kind: String,
}

/// The session's settings, from `system`/`init`.
#[derive(Debug, Clone, PartialEq)]
pub struct Init {
    /// The session id; pass it as [`Options::resume`] to continue the conversation later.
    pub session_id: String,
    /// The model in use.
    pub model: String,
    /// The session's working directory.
    pub cwd: String,
    /// Names of the available tools.
    pub tools: Vec<String>,
    /// MCP servers as `(name, status)`.
    pub mcp_servers: Vec<(String, String)>,
    /// The permission mode, in the binary's spelling (see [`PermissionMode::from_key`]).
    pub permission_mode: String,
    /// Where the binary says its API credential came from (`apiKeySource`); `none` for a claude.ai sign-in.
    pub api_key_source: String,
    /// The binary's version.
    pub claude_code_version: String,
    /// The whole message.
    pub raw: Value,
}

/// A request to run a tool, from `can_use_tool`.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    /// Identifies the request to [`Session::respond`].
    pub request_id: String,
    /// The tool to run.
    pub tool_name: String,
    /// The tool's input.
    pub input: Value,
    /// The tool call's id, matching a [`Block::ToolUse`].
    pub tool_use_id: Option<String>,
    /// A full prompt sentence, e.g. "Claude wants to read foo.txt".
    pub title: Option<String>,
    /// A short name for the tool action.
    pub display_name: Option<String>,
    /// A subtitle describing the action.
    pub description: Option<String>,
    /// Why the binary is asking.
    pub decision_reason: Option<String>,
    /// The path that triggered the request, if one did.
    pub blocked_path: Option<String>,
    /// Permission updates the binary suggests, usable as `updated_permissions` in [`Decision::Allow`].
    pub suggestions: Vec<Value>,
}

/// The end of a turn, from a `result` message.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    /// `success`, or an error kind such as `error_max_turns` or `error_during_execution`.
    pub subtype: String,
    /// Whether the turn failed.
    pub is_error: bool,
    /// The final reply text (or the error text of a failed API call).
    pub result: Option<String>,
    /// The session id.
    pub session_id: String,
    /// Wall-clock time of the turn.
    pub duration_ms: u64,
    /// Agentic turns taken.
    pub num_turns: u64,
    /// What the session has cost so far, as the binary estimates it.
    pub total_cost_usd: Option<f64>,
    /// Token counts of the turn.
    pub usage: Usage,
    /// How many tool calls were denied during the turn.
    pub permission_denials: usize,
    /// The context window of the model that answered (the largest, if several did); 0 when not said.
    pub context_window: u64,
    /// How many of the caller's messages still wait to be answered, each in a turn of its own that follows
    /// at once (`queued_turn_count`); 0 when not said.
    pub queued_turn_count: u64,
    /// The whole message.
    pub raw: Value,
}

/// One piece of a message's content.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Text.
    Text(String),
    /// Thinking.
    Thinking(String),
    /// A tool call.
    ToolUse {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
        /// The tool's input.
        input: Value,
    },
    /// The outcome of a tool call.
    ToolResult {
        /// The call this answers.
        tool_use_id: String,
        /// The result as text; non-text parts appear as `[type]`.
        content: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Any other block, as received.
    Other(Value),
}

/// Token counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation_input_tokens: u64,
}

/// Why a call into this crate failed.
#[derive(Debug)]
pub enum Error {
    /// No Claude Code binary was found.
    NotFound,
    /// The path names a `.cmd`/`.bat` script. Windows runs those through `cmd.exe`, which cannot be given
    /// arguments safely, so only a native `claude.exe` is run.
    BatchScript(PathBuf),
    /// An option was refused; the text says which and why.
    InvalidOption(String),
    /// The binary could not be started.
    Spawn(std::io::Error),
    /// Writing to the binary, or starting a reader thread, failed.
    Io(std::io::Error),
    /// The session is closed.
    Closed,
    /// [`Session::respond`] named a request that is not pending: unknown, already answered, or withdrawn.
    UnknownRequest(String),
    /// The binary did not report a version; the text says what happened.
    Version(String),
    /// The binary did not say what it offers; the text says what happened.
    NoInfo(String),
    /// The binary refused a request of this crate's, or answered it in a shape this crate does not know: a
    /// version that dropped or changed it. The text says which.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound if cfg!(windows) => f.write_str(
                "Claude Code not found: install the native claude.exe (irm https://claude.ai/install.ps1 | iex); \
                 npm's claude.cmd shim is not supported",
            ),
            Self::NotFound => f.write_str("Claude Code not found: install it or put `claude` on PATH"),
            Self::BatchScript(path) => {
                write!(f, "refusing to run batch script {}: use a native claude.exe", path.display())
            }
            Self::InvalidOption(why) => write!(f, "invalid option: {why}"),
            Self::Spawn(error) => write!(f, "failed to start the Claude Code binary: {error}"),
            Self::Io(error) => write!(f, "failed to talk to the Claude Code binary: {error}"),
            Self::Closed => f.write_str("the session is closed"),
            Self::UnknownRequest(id) => write!(f, "no pending permission request {id}"),
            Self::Version(why) => write!(f, "could not read the Claude Code version: {why}"),
            Self::NoInfo(why) => write!(f, "Claude Code did not report its models and account: {why}"),
            Self::Unsupported(why) => write!(f, "Claude Code does not answer the request as expected: {why}"),
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
