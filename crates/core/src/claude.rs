//! The Claude Agent engine: an agent that thinks through the Claude Code
//! binary the user installed and signed in to themselves.
//!
//! AtroxaBot starts that binary as published and talks to it with the Claude
//! Agent SDK's protocol (the `atroxabot-claude-agent` crate). It never reads,
//! stores or changes how the binary signs in: whatever the user set up in
//! Claude Code is what runs, and its usage is theirs. One process serves one
//! session for as long as it is open, so a follow-up message reuses the
//! context (and the prompt cache) of the one before it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atroxabot_claude_agent as sdk;
use serde_json::Value;

use crate::agent::{Agent, Autonomy};
use crate::attach;
use crate::config::Connection;
use crate::engine::{AgentEvent, Ask, FollowUp, LimitWindow, Limits, Verdict};
use crate::responses::Usage;
#[cfg(test)]
use crate::session::TodoState;
use crate::session::{Session, ToolUse};
use crate::tools;

/// Where the user's Claude Code binary is, if it is installed.
#[must_use]
pub fn find_claude() -> Option<PathBuf> {
    sdk::find_cli()
}

/// How long Claude Code gets to say what it offers.
const INFO_TIMEOUT: Duration = Duration::from_secs(40);

/// What Claude Code offers for the Claude account `connection`: its models
/// (the default first), the account it is signed in to, and its version.
/// Starts the binary without sending it a message, so nothing is used; call
/// it off the UI thread.
///
/// # Errors
/// The binary is not installed ([`sdk::Error::NotFound`]), could not be
/// started, or did not answer.
pub fn claude_info(connection: &Connection) -> Result<sdk::Info, sdk::Error> {
    sdk::info(&sdk::Options { env: account_env(connection), ..sdk::Options::default() }, INFO_TIMEOUT)
}

/// A command to run in a terminal the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    /// The program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment variables to add to the app's own.
    pub env: Vec<(String, String)>,
}

/// The command that signs the user in to the Claude account `connection`:
/// Claude Code's own `auth login`, which opens Anthropic's sign-in in the
/// browser. The app only starts it, in a terminal the user sees. `None`
/// when Claude Code is not installed.
#[must_use]
pub fn claude_login(connection: &Connection) -> Option<TerminalCommand> {
    // The folder must exist for Claude Code to keep the sign-in in it.
    if !connection.config_dir.is_empty() {
        let _ = std::fs::create_dir_all(&connection.config_dir);
    }
    let args = vec!["auth".to_owned(), "login".to_owned()];
    Some(TerminalCommand { program: sdk::find_cli()?, args, env: account_env(connection) })
}

/// The environment that selects a Claude account: nothing for Claude
/// Code's own default, and otherwise `CLAUDE_CONFIG_DIR`, the folder
/// Anthropic documents for keeping several accounts apart. It names where
/// Claude Code keeps an account; what is in it is Claude Code's business.
fn account_env(connection: &Connection) -> Vec<(String, String)> {
    if connection.config_dir.is_empty() {
        Vec::new()
    } else {
        vec![("CLAUDE_CONFIG_DIR".to_owned(), connection.config_dir.clone())]
    }
}

/// A running Claude Agent session.
pub struct ClaudeAgent {
    session: sdk::Session,
    /// What the user may still approve "for the session", by request id:
    /// the permission rules the binary suggested for it.
    suggestions: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    /// The follow-ups sent and not taken in yet, by the id they were sent under.
    follow_ups: Arc<Mutex<HashMap<String, FollowUp>>>,
}

impl std::fmt::Debug for ClaudeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgent").finish_non_exhaustive()
    }
}

impl ClaudeAgent {
    /// Starts the binary for `agent` on the Claude account `connection`, in
    /// the folder of `session`, resuming the session's context when this
    /// account has it. `mcp` is the `--mcp-config` JSON of the app's own MCP
    /// servers, if any. `emit` is called on a reader thread with everything
    /// that happens.
    ///
    /// # Errors
    /// The binary is not installed or could not be started.
    pub fn connect(
        agent: &Agent,
        connection: &Connection,
        session: &Session,
        mcp: Option<String>,
        mut emit: impl FnMut(AgentEvent) + Send + 'static,
    ) -> Result<Self, sdk::Error> {
        let suggestions: Arc<Mutex<HashMap<String, Vec<Value>>>> = Arc::default();
        let pending = Arc::clone(&suggestions);
        // The binary's running total includes what a resumed session cost before.
        let mut mapper = Mapper { total: session.cost(), ..Mapper::default() };
        let follow_ups = Arc::clone(&mapper.follow_ups);
        let mut options = options(agent, connection, session);
        options.mcp_config = mcp;
        let session = sdk::Session::connect(&options, move |event| {
            if let sdk::Event::Permission(request) = &event
                && let Ok(mut pending) = pending.lock()
            {
                pending.insert(request.request_id.clone(), request.suggestions.clone());
            }
            for event in mapper.map(event) {
                emit(event);
            }
        })?;
        Ok(Self { session, suggestions, follow_ups })
    }

    /// Sends the user's message with the `files` that go with it (pictures
    /// as content, the rest named by path); the reply arrives as events.
    ///
    /// # Errors
    /// The process has ended.
    pub fn send(&self, text: &str, files: &[String]) -> Result<(), sdk::Error> {
        self.session.send_content(&content(text, files))
    }

    /// Sends a message the user wrote while the turn runs. Claude Code
    /// takes it in after the tool calls in progress (reported as an
    /// [`AgentEvent::FollowUp`]), or answers it in a turn of its own right
    /// after this one ([`AgentEvent::Queued`]).
    ///
    /// # Errors
    /// The process has ended.
    pub fn follow_up(&self, message: FollowUp) -> Result<(), sdk::Error> {
        let uuid = wire_uuid(&message.id);
        let content = content(&message.text, &message.files);
        if let Ok(mut waiting) = self.follow_ups.lock() {
            waiting.insert(uuid.clone(), message);
        }
        self.session.send_message(&content, Some(&uuid))
    }

    /// Stops the turn in progress; the session stays usable. Follow-ups not
    /// taken in yet are dropped when `cancel_queued`, and answered next otherwise.
    pub fn interrupt(&self, cancel_queued: bool) {
        // Fails only if the process is gone, which ends the turn anyway.
        let _ = self.session.interrupt(cancel_queued);
    }

    /// Answers the approval `id`.
    pub fn decide(&self, id: &str, verdict: Verdict) {
        let suggested = self.suggestions.lock().ok().and_then(|mut pending| pending.remove(id)).unwrap_or_default();
        let decision = match verdict {
            Verdict::Allow => sdk::Decision::Allow { updated_input: None, updated_permissions: Vec::new() },
            // The binary's own suggested rules are what "always" means to it.
            Verdict::AllowAlways => sdk::Decision::Allow { updated_input: None, updated_permissions: suggested },
            // The user's answers to a question ride in the tool's input.
            Verdict::Answer(input) => sdk::Decision::Allow {
                updated_input: serde_json::from_str(&input).ok(),
                updated_permissions: Vec::new(),
            },
            Verdict::Deny(why) if why.trim().is_empty() => {
                sdk::Decision::Deny { message: "The user declined this.".to_owned(), interrupt: false }
            }
            Verdict::Deny(why) => sdk::Decision::Deny { message: why, interrupt: false },
        };
        // Fails if the request was withdrawn or the process is gone.
        let _ = self.session.respond(id, decision);
    }

    /// Ends the process. Also happens when the value is dropped.
    pub fn close(&self) {
        self.session.close();
    }
}

/// A message's content: pictures as blocks, then the words with the paths
/// of the other files.
fn content(text: &str, files: &[String]) -> Value {
    let (images, named) = attach::split(files);
    let text = attach::with_paths(text, &named);
    if images.is_empty() {
        return text.into();
    }
    let picture = |image: &attach::Image| {
        let source = serde_json::json!({ "type": "base64", "media_type": image.media_type, "data": image.data });
        serde_json::json!({ "type": "image", "source": source })
    };
    let mut content: Vec<Value> = images.iter().map(picture).collect();
    if !text.trim().is_empty() {
        content.push(serde_json::json!({ "type": "text", "text": text }));
    }
    content.into()
}

/// The UUID a follow-up is sent under, made from the app's id for it (hex
/// digits, padded): Claude Code replays the message under the same one.
fn wire_uuid(id: &str) -> String {
    let digits: String = id.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_lowercase()).collect();
    let tail = digits.get(digits.len().saturating_sub(30)..).unwrap_or_default();
    let d = format!("{tail:0>30}");
    format!("{}-{}-4{}-8{}-{}", &d[..8], &d[8..12], &d[12..15], &d[15..18], &d[18..])
}

/// How the binary is started for `agent` on `connection` in `session`.
fn options(agent: &Agent, connection: &Connection, session: &Session) -> sdk::Options {
    let some = |text: &str| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
    let (permission_mode, ask_permissions) = match agent.autonomy {
        Autonomy::Ask => (sdk::PermissionMode::Default, true),
        Autonomy::Edits => (sdk::PermissionMode::AcceptEdits, true),
        // What its classifier leaves open is asked of the user.
        Autonomy::Auto => (sdk::PermissionMode::Auto, true),
        // Nothing needs approval, but a question to the user still has to reach them.
        Autonomy::Full => (sdk::PermissionMode::BypassPermissions, true),
        // Whatever would need approval is denied; reading still works.
        Autonomy::ReadOnly => (sdk::PermissionMode::DontAsk, false),
    };
    let folder = session.folder();
    // Another account keeps its own history: only the one that has this
    // session's context can resume it.
    let resume = some(&session.remote).filter(|_| session.account == connection.id);
    sdk::Options {
        // The agent's own prompt, not Claude Code's: its identity and how it
        // works here. Claude Code still adds the project's CLAUDE.md, the
        // skills and the environment (date, system, folder) to the conversation.
        system_prompt: sdk::SystemPrompt::Custom(format!("{}{CLAUDE_TOOLS}", agent.system_prompt(&folder))),
        cwd: Some(folder).filter(|folder| !folder.as_os_str().is_empty()),
        model: some(&agent.model),
        effort: some(&agent.effort),
        permission_mode: Some(permission_mode),
        resume,
        add_dirs: agent.memory_dir().into_iter().collect(),
        include_partial_messages: true,
        ask_permissions,
        // Claude Code gives a session an app starts its task list tools only when they are named here.
        allowed_tools: TASK_TOOLS.map(str::to_owned).into(),
        env: account_env(connection),
        extra_args: vec![
            // Show a summary of the reasoning rather than nothing.
            ("thinking-display".to_owned(), Some("summarized".to_owned())),
            // Say when a message sent during a turn is taken in.
            ("replay-user-messages".to_owned(), None),
        ],
        ..sdk::Options::default()
    }
}

/// Claude Code's tools for keeping a task list, which [`Session::todos`] reads back.
const TASK_TOOLS: [&str; 4] = ["TaskCreate", "TaskUpdate", "TaskList", "TaskGet"];

/// What an agent on a Claude account is told about Claude Code's own tools,
/// and about the reminders Claude Code adds to the conversation.
const CLAUDE_TOOLS: &str = "\n\n# Your tools\n\n\
    - When a decision is the user's to make, or they ask you to interview them, ask with the AskUserQuestion tool: \
    a few clear options per question, rather than guessing or asking in prose.\n\
    - For broad searches and for pieces of work that do not depend on each other, start subagents with the Agent \
    tool, several at once, each with a complete brief.\n\
    - Use a skill when one fits the task.\n\
    - For work of three or more steps, keep a task list with TaskCreate and TaskUpdate: mark a task in progress \
    when you start it and completed as soon as it is done. The user sees the list.\n\n\
    The application adds system reminders to this conversation. Treat them as context from the application, not as \
    messages from the user. A message introduced as one \"the user sent while you were working\" is the user's: \
    take it into account and carry on with your task.";

/// A count of tokens, short: `840`, `39.6k`, `1.2M`.
fn compact(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1e6)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1e3)
    } else {
        count.to_string()
    }
}

/// One line on a subagent: what it is doing, or how it ended and what that took.
fn activity(task: &sdk::Task) -> String {
    let calls = if task.tool_uses == 1 { "1 tool call".to_owned() } else { format!("{} tool calls", task.tool_uses) };
    // An end reported without numbers (one sent to the background, say) says only how it ended.
    let took = if task.tool_uses == 0 && task.total_tokens == 0 {
        String::new()
    } else {
        format!(" · {calls} · {} tokens · {}s", compact(task.total_tokens), task.duration_ms / 1000)
    };
    match task.state {
        sdk::TaskState::Started => "Starting…".to_owned(),
        sdk::TaskState::Running if task.description.is_empty() => calls,
        sdk::TaskState::Running => format!("{} · {calls}", task.description),
        sdk::TaskState::Completed => format!("Done{took}"),
        sdk::TaskState::Failed => format!("Failed{took}"),
        sdk::TaskState::Stopped => format!("Stopped{took}"),
    }
}

/// Turns the binary's events into the app's, keeping what a turn needs
/// remembered between them.
#[derive(Debug, Default)]
struct Mapper {
    /// Whether a reply is being written.
    replying: bool,
    /// Whether text or thinking streamed since the last complete message,
    /// which then repeats it and must not be shown twice.
    streamed: bool,
    /// Whether the turn has shown anything at all.
    shown: bool,
    /// The model of the latest message.
    model: String,
    /// The latest message's prompt size: how full the context is.
    context: sdk::Usage,
    /// The session's cost so far, as the binary last reported it.
    total: f64,
    /// The subagents (by the call that started each) whose words streamed
    /// since their last complete message, which then repeats them.
    sub_streamed: HashSet<String>,
    /// The calls that started a subagent. Claude Code reports a command
    /// or a monitor sent to the background as a task too, and those are
    /// no subagents: only these calls are told how their task is doing.
    agents: HashSet<String>,
    /// The follow-ups sent and not taken in yet, by the id they were sent
    /// under; shared with [`ClaudeAgent::follow_up`].
    follow_ups: Arc<Mutex<HashMap<String, FollowUp>>>,
}

impl Mapper {
    /// Remembers the call `id` if it starts a subagent.
    fn note_agent(&mut self, id: &str, name: &str) {
        if matches!(name, "Agent" | "Task") {
            self.agents.insert(id.to_owned());
        }
    }

    fn map(&mut self, event: sdk::Event) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match event {
            sdk::Event::Init(init) => out.push(AgentEvent::Remote(init.session_id)),
            sdk::Event::TextDelta { text, parent_tool_use_id: None } => {
                self.begin(&mut out);
                self.streamed = true;
                out.push(AgentEvent::Text(text));
            }
            sdk::Event::ThinkingDelta { text, parent_tool_use_id: None } if !text.is_empty() => {
                self.begin(&mut out);
                self.streamed = true;
                out.push(AgentEvent::Thinking(text));
            }
            // What a subagent says stays inside its tool call.
            sdk::Event::Assistant { blocks, model, parent_tool_use_id: None, usage, error } => {
                self.begin(&mut out);
                self.model = model;
                self.context = usage;
                for block in blocks {
                    match block {
                        sdk::Block::Text(text) if !self.streamed => out.push(AgentEvent::Text(text)),
                        sdk::Block::Thinking(text) if !self.streamed && !text.is_empty() => {
                            out.push(AgentEvent::Thinking(text));
                        }
                        sdk::Block::ToolUse { id, name, input } => {
                            self.note_agent(&id, &name);
                            let call = ToolUse { id, name, input: input.to_string(), ..ToolUse::default() };
                            out.push(AgentEvent::ToolCall(call));
                        }
                        _ => {}
                    }
                }
                self.streamed = false;
                if let Some(error) = error {
                    out.push(AgentEvent::Status(error));
                }
            }
            // What a subagent says and does goes inside the tool call that started it.
            sdk::Event::TextDelta { text, parent_tool_use_id: Some(parent) } if !text.is_empty() => {
                self.sub_streamed.insert(parent.clone());
                out.push(AgentEvent::Sub { parent, event: Box::new(AgentEvent::Text(text)) });
            }
            sdk::Event::ThinkingDelta { text, parent_tool_use_id: Some(parent) } if !text.is_empty() => {
                self.sub_streamed.insert(parent.clone());
                out.push(AgentEvent::Sub { parent, event: Box::new(AgentEvent::Thinking(text)) });
            }
            sdk::Event::Assistant { blocks, parent_tool_use_id: Some(parent), .. } => {
                let streamed = self.sub_streamed.remove(&parent);
                for block in blocks {
                    let event = match block {
                        sdk::Block::Text(_) | sdk::Block::Thinking(_) if streamed => continue,
                        sdk::Block::Text(text) if !text.trim().is_empty() => AgentEvent::Text(text),
                        sdk::Block::Thinking(text) if !text.trim().is_empty() => AgentEvent::Thinking(text),
                        sdk::Block::ToolUse { id, name, input } => {
                            self.note_agent(&id, &name);
                            AgentEvent::ToolCall(ToolUse { id, name, input: input.to_string(), ..ToolUse::default() })
                        }
                        _ => continue,
                    };
                    out.push(AgentEvent::Sub { parent: parent.clone(), event: Box::new(event) });
                }
            }
            sdk::Event::Task(task) if !task.tool_use_id.is_empty() => {
                // A task of a subagent's kind is one, whatever its call is named.
                let kind = task.task_type.as_str();
                if kind.contains("agent") || kind.contains("teammate") {
                    self.agents.insert(task.tool_use_id.clone());
                }
                if self.agents.contains(&task.tool_use_id) {
                    out.push(AgentEvent::Activity { text: activity(&task), id: task.tool_use_id });
                }
            }
            sdk::Event::RateLimit(limit) => {
                let window =
                    |w: sdk::RateWindow| LimitWindow { name: w.name, used: w.utilization, resets: w.resets_at };
                out.push(AgentEvent::Limits(Limits {
                    at: crate::session::unix_now(),
                    status: limit.status,
                    resets: limit.resets_at,
                    windows: limit.windows.into_iter().map(window).collect(),
                }));
            }
            sdk::Event::Status(status) => {
                let busy = status.as_deref() == Some("compacting");
                out.push(AgentEvent::Status(
                    if busy { "Summarising the conversation to make room…" } else { "" }.to_owned(),
                ));
            }
            sdk::Event::Compacted { pre_tokens, post_tokens, .. } => {
                let sizes = if post_tokens > 0 {
                    format!(" · {} → {} tokens", compact(pre_tokens), compact(post_tokens))
                } else {
                    String::new()
                };
                out.push(AgentEvent::Note(format!("Earlier messages were summarised to make room{sizes}")));
            }
            // A follow-up the turn takes in now: the model reads it after the tool results before it.
            sdk::Event::User { replay: true, uuid, parent_tool_use_id: None, .. } => {
                let taken = self.follow_ups.lock().ok().and_then(|mut waiting| waiting.remove(&uuid));
                // Claude Code says itself that it came while the agent worked. Its answer is a
                // reply after it, or (when the turn it was sent in had ended) a turn of its own.
                if let Some(FollowUp { id, text, files }) = taken {
                    out.push(AgentEvent::FollowUp { id, text, note: String::new(), files });
                }
            }
            // Tool results carry ids of their own, a subagent's too.
            sdk::Event::User { blocks, .. } => {
                for block in blocks {
                    if let sdk::Block::ToolResult { tool_use_id, content, is_error } = block {
                        out.push(AgentEvent::ToolResult { id: tool_use_id, output: content, is_error });
                    }
                }
            }
            sdk::Event::Permission(request) => {
                let reason = request.decision_reason.or(request.description).or(request.title).unwrap_or_default();
                out.push(AgentEvent::Permission(Ask {
                    id: request.request_id,
                    // A question is known by the tool's own name, whatever it is shown as.
                    tool: if request.tool_name == "AskUserQuestion" {
                        request.tool_name
                    } else {
                        request.display_name.unwrap_or(request.tool_name)
                    },
                    summary: tools::summary(&request.input),
                    input: request.input.to_string(),
                    reason,
                }));
            }
            sdk::Event::PermissionCancelled { request_id } => out.push(AgentEvent::PermissionGone(request_id)),
            sdk::Event::Result(result) => {
                self.begin(&mut out);
                let said = result.result.filter(|text| !text.trim().is_empty());
                // A turn that showed nothing (a command's output, an error) still says something.
                if !self.shown
                    && let Some(text) = &said
                {
                    out.push(AgentEvent::Text(text.clone()));
                }
                let problem = match (result.is_error, said) {
                    (false, _) => None,
                    (true, Some(text)) if self.shown => Some(text),
                    (true, _) => Some(format!("The turn ended with an error ({}).", result.subtype)),
                };
                // The binary reports the session's running total.
                let total = result.total_cost_usd.unwrap_or(self.total);
                let cost = (total - self.total).max(0.0);
                self.total = total.max(self.total);
                let context = self.context;
                let usage = Usage {
                    input_tokens: context
                        .input_tokens
                        .saturating_add(context.cache_read_input_tokens)
                        .saturating_add(context.cache_creation_input_tokens),
                    output_tokens: result.usage.output_tokens,
                    cached_tokens: context.cache_read_input_tokens,
                    cache_write_tokens: context.cache_creation_input_tokens,
                };
                out.push(AgentEvent::ReplyEnded {
                    raw: Vec::new(),
                    model: std::mem::take(&mut self.model),
                    usage,
                    cost,
                    problem: problem.clone().unwrap_or_default(),
                });
                if result.context_window > 0 {
                    out.push(AgentEvent::Window(result.context_window));
                }
                out.push(AgentEvent::Status(String::new()));
                if result.queued_turn_count > 0 {
                    out.push(AgentEvent::Queued(result.queued_turn_count));
                }
                out.push(AgentEvent::TurnEnded { problem });
                (self.replying, self.shown, self.streamed) = (false, false, false);
            }
            sdk::Event::Retry { attempt, max_retries, delay_ms, error } => {
                let wait = (delay_ms / 1000).max(1);
                out.push(AgentEvent::Status(format!("{error}. Trying again in {wait}s ({attempt}/{max_retries})…")));
            }
            sdk::Event::Closed { exit_code, error } => {
                let why = error.or_else(|| exit_code.filter(|code| *code != 0).map(|code| format!("exit code {code}")));
                if self.replying {
                    let problem = format!(
                        "The Claude Agent stopped unexpectedly{}.",
                        why.as_ref().map_or_else(String::new, |why| format!(": {why}"))
                    );
                    out.push(AgentEvent::ReplyEnded {
                        raw: Vec::new(),
                        model: String::new(),
                        usage: Usage::default(),
                        cost: 0.0,
                        problem: problem.clone(),
                    });
                    out.push(AgentEvent::TurnEnded { problem: Some(problem) });
                    self.replying = false;
                }
                out.push(AgentEvent::Disconnected(why));
            }
            // Subagent chatter, hook and status frames, stderr lines.
            _ => {}
        }
        self.shown |= out
            .iter()
            .any(|event| matches!(event, AgentEvent::Text(_) | AgentEvent::Thinking(_) | AgentEvent::ToolCall(_)));
        out
    }

    /// Starts the turn's reply if nothing did yet. The whole turn is one
    /// reply: the binary keeps the context, so nothing is replayed from it.
    fn begin(&mut self, out: &mut Vec<AgentEvent>) {
        if !self.replying {
            self.replying = true;
            out.push(AgentEvent::ReplyStarted);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_subagents_hear_of_their_tasks() {
        let mut mapper = Mapper::default();
        let calls = |calls: &[(&str, &str)]| sdk::Event::Assistant {
            blocks: calls
                .iter()
                .map(|(id, name)| sdk::Block::ToolUse { id: (*id).into(), name: (*name).into(), input: json!({}) })
                .collect(),
            model: "m".into(),
            parent_tool_use_id: None,
            usage: sdk::Usage::default(),
            error: None,
        };
        let task = |id: &str, kind: &str, state: sdk::TaskState| {
            sdk::Event::Task(sdk::Task {
                tool_use_id: id.into(),
                task_type: kind.into(),
                state,
                ..sdk::Task::default()
            })
        };
        let news = |out: Vec<AgentEvent>| -> Vec<String> {
            out.into_iter()
                .filter_map(|e| if let AgentEvent::Activity { id, .. } = e { Some(id) } else { None })
                .collect()
        };
        mapper.map(calls(&[("b1", "Bash"), ("m1", "Monitor"), ("a1", "Agent")]));
        // A command and a monitor sent to the background are tasks, but no subagents.
        for (id, kind) in [("b1", "local_bash"), ("m1", "monitor_mcp"), ("a1", "local_agent")] {
            let mut said = news(mapper.map(task(id, kind, sdk::TaskState::Started)));
            said.extend(news(mapper.map(task(id, "", sdk::TaskState::Completed))));
            let expected: &[&str] = if id == "a1" { &["a1", "a1"] } else { &[] };
            assert_eq!(said, expected, "{id}");
        }
        // A call of another name is a subagent when its task says so.
        assert_eq!(news(mapper.map(task("r1", "remote_agent", sdk::TaskState::Started))), ["r1"]);
        assert_eq!(news(mapper.map(task("r1", "", sdk::TaskState::Completed))), ["r1"]);
    }

    #[test]
    fn subagents_limits_and_compaction_are_mapped() {
        let mut mapper = Mapper::default();
        let said = |parent: Option<&str>, blocks: Vec<sdk::Block>| sdk::Event::Assistant {
            blocks,
            model: "m".into(),
            parent_tool_use_id: parent.map(str::to_owned),
            usage: sdk::Usage::default(),
            error: None,
        };
        let call = |id: &str, name: &str| sdk::Block::ToolUse { id: id.into(), name: name.into(), input: json!({}) };
        // The agent starts a subagent, which speaks and calls a tool of its own.
        let started = mapper.map(said(None, vec![call("a1", "Agent")]));
        assert!(matches!(&started[..], [AgentEvent::ReplyStarted, AgentEvent::ToolCall(tool)] if tool.id == "a1"));
        let inner = mapper.map(said(Some("a1"), vec![sdk::Block::Text("Looking.".into()), call("g1", "Glob")]));
        assert!(matches!(&inner[..], [
            AgentEvent::Sub { parent, event: first },
            AgentEvent::Sub { event: second, .. },
        ] if parent == "a1" && **first == AgentEvent::Text("Looking.".into()) && matches!(&**second, AgentEvent::ToolCall(tool) if tool.id == "g1")));
        let result = sdk::Block::ToolResult { tool_use_id: "g1".into(), content: "hello.txt".into(), is_error: false };
        let answered = mapper.map(sdk::Event::User {
            blocks: vec![result],
            parent_tool_use_id: Some("a1".into()),
            uuid: String::new(),
            replay: false,
        });
        assert!(matches!(&answered[..], [AgentEvent::ToolResult { id, .. }] if id == "g1"));

        let task = sdk::Task {
            tool_use_id: "a1".into(),
            state: sdk::TaskState::Running,
            description: "Finding *".into(),
            tool_uses: 1,
            ..sdk::Task::default()
        };
        assert_eq!(
            mapper.map(sdk::Event::Task(task.clone())),
            [AgentEvent::Activity { id: "a1".into(), text: "Finding * · 1 tool call".into() }]
        );
        let done = sdk::Task {
            state: sdk::TaskState::Completed,
            tool_uses: 3,
            total_tokens: 28_841,
            duration_ms: 3_100,
            ..task
        };
        assert_eq!(activity(&done), "Done · 3 tool calls · 28.8k tokens · 3s");
        let failed = sdk::Task { state: sdk::TaskState::Failed, tool_uses: 0, total_tokens: 0, ..done };
        assert_eq!(activity(&failed), "Failed", "no row of zeros when the end came without numbers");

        let window =
            sdk::RateWindow { name: "five_hour".into(), utilization: 0.1, resets_at: 9, status: "allowed".into() };
        let limit =
            sdk::RateLimit { status: "allowed".into(), kind: "five_hour".into(), resets_at: 9, windows: vec![window] };
        let limits = mapper.map(sdk::Event::RateLimit(limit));
        assert!(
            matches!(&limits[..], [AgentEvent::Limits(limits)] if limits.windows[0].name == "five_hour" && limits.resets == 9)
        );

        assert_eq!(mapper.map(sdk::Event::Status(Some("compacting".into()))).len(), 1);
        let compacted = mapper.map(sdk::Event::Compacted { manual: true, pre_tokens: 39_577, post_tokens: 11_092 });
        assert_eq!(
            compacted,
            [AgentEvent::Note("Earlier messages were summarised to make room · 39.6k → 11.1k tokens".into())]
        );
        // The summary Claude Code replays to itself is not the user's message, nor a tool result.
        let replay = sdk::Event::User {
            blocks: vec![sdk::Block::Text("This session is being continued".into())],
            parent_tool_use_id: None,
            uuid: String::new(),
            replay: false,
        };
        assert!(mapper.map(replay).is_empty());
    }

    fn result(is_error: bool, text: Option<&str>, total: f64) -> sdk::Event {
        sdk::Event::Result(sdk::TurnResult {
            subtype: if is_error { "error_during_execution" } else { "success" }.into(),
            is_error,
            result: text.map(str::to_owned),
            session_id: "s".into(),
            duration_ms: 1,
            num_turns: 1,
            total_cost_usd: Some(total),
            usage: sdk::Usage { output_tokens: 9, ..sdk::Usage::default() },
            permission_denials: 0,
            context_window: 200_000,
            queued_turn_count: 0,
            raw: Value::Null,
        })
    }

    fn assistant(blocks: Vec<sdk::Block>) -> sdk::Event {
        let usage = sdk::Usage {
            input_tokens: 2,
            output_tokens: 1,
            cache_read_input_tokens: 30,
            cache_creation_input_tokens: 4,
        };
        sdk::Event::Assistant { blocks, model: "claude-x".into(), parent_tool_use_id: None, usage, error: None }
    }

    #[test]
    fn a_streamed_turn_is_one_reply_shown_once() {
        let mut mapper = Mapper { total: 1.0, ..Mapper::default() };
        let delta = |text: &str| sdk::Event::TextDelta { text: text.into(), parent_tool_use_id: None };
        assert_eq!(mapper.map(delta("Hel")), [AgentEvent::ReplyStarted, AgentEvent::Text("Hel".into())]);
        assert_eq!(mapper.map(delta("lo")), [AgentEvent::Text("lo".into())]);
        assert!(mapper.map(assistant(vec![sdk::Block::Text("Hello".into())])).is_empty(), "already streamed");
        let call = sdk::Block::ToolUse { id: "t".into(), name: "Read".into(), input: json!({ "file_path": "a" }) };
        let called = mapper.map(assistant(vec![call]));
        assert!(
            matches!(&called[0], AgentEvent::ToolCall(call) if call.name == "Read" && call.input.contains("file_path"))
        );
        let answered = sdk::Block::ToolResult { tool_use_id: "t".into(), content: "x".into(), is_error: false };
        let answered = mapper.map(sdk::Event::User {
            blocks: vec![answered],
            parent_tool_use_id: None,
            uuid: String::new(),
            replay: false,
        });
        assert_eq!(answered, [AgentEvent::ToolResult { id: "t".into(), output: "x".into(), is_error: false }]);
        // A message that did not stream is shown from its blocks.
        assert_eq!(mapper.map(assistant(vec![sdk::Block::Text("Done.".into())])), [AgentEvent::Text("Done.".into())]);

        let ended = mapper.map(result(false, Some("Done."), 1.25));
        let AgentEvent::ReplyEnded { usage, cost, problem, model, .. } = &ended[0] else { panic!("the reply ends") };
        assert_eq!((usage.input_tokens, usage.output_tokens, usage.cached_tokens), (36, 9, 30));
        assert!((cost - 0.25).abs() < 1e-9 && problem.is_empty() && model == "claude-x");
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: None }));
        // The next turn starts a new reply.
        assert_eq!(mapper.map(delta("x"))[0], AgentEvent::ReplyStarted);
    }

    #[test]
    fn silent_and_failed_turns_still_say_something() {
        let mut mapper = Mapper::default();
        let quiet = mapper.map(result(false, Some("Compacted."), 0.0));
        assert_eq!(quiet[..2], [AgentEvent::ReplyStarted, AgentEvent::Text("Compacted.".into())]);
        let failed = mapper.map(result(true, None, 0.0));
        assert!(
            matches!(failed.last(), Some(AgentEvent::TurnEnded { problem: Some(text) }) if text.contains("error_during_execution"))
        );

        // A subagent's words stream into its own call, and its whole message does not repeat them.
        let inner = sdk::Event::TextDelta { text: "inner".into(), parent_tool_use_id: Some("t".into()) };
        let sub = |event| AgentEvent::Sub { parent: "t".into(), event: Box::new(event) };
        assert_eq!(mapper.map(inner), [sub(AgentEvent::Text("inner".into()))]);
        let whole = |text: &str| sdk::Event::Assistant {
            blocks: vec![sdk::Block::Text(text.into())],
            model: "m".into(),
            parent_tool_use_id: Some("t".into()),
            usage: sdk::Usage::default(),
            error: None,
        };
        assert!(mapper.map(whole("inner")).is_empty(), "already streamed");
        assert_eq!(mapper.map(whole("Next.")), [sub(AgentEvent::Text("Next.".into()))], "this one did not stream");
        let _ = mapper.map(sdk::Event::TextDelta { text: "x".into(), parent_tool_use_id: None });
        let closed = mapper.map(sdk::Event::Closed { exit_code: Some(1), error: None });
        assert!(matches!(&closed[1], AgentEvent::TurnEnded { problem: Some(text) } if text.contains("exit code 1")));
        assert_eq!(closed[2], AgentEvent::Disconnected(Some("exit code 1".into())));
        assert_eq!(
            mapper.map(sdk::Event::Closed { exit_code: Some(0), error: None }),
            [AgentEvent::Disconnected(None)]
        );
    }

    #[test]
    fn follow_ups_are_reported_when_claude_code_takes_them_in() {
        // As Claude Code 2.1.322 does it: the replay comes right after the tool results it follows.
        let mut mapper = Mapper::default();
        let follow_up = |id: &str, text: &str| FollowUp { id: id.into(), text: text.into(), files: Vec::new() };
        for message in
            [follow_up("18e6a58ab5fe8230", "Also run the tests."), follow_up("18e6a58ab5fe8231", "Then stop.")]
        {
            let uuid = wire_uuid(&message.id);
            mapper.follow_ups.lock().unwrap().insert(uuid, message);
        }
        let replayed = |uuid: &str, text: &str| sdk::Event::User {
            blocks: vec![sdk::Block::Text(text.into())],
            parent_tool_use_id: None,
            uuid: uuid.into(),
            replay: true,
        };
        let _ =
            mapper.map(assistant(vec![sdk::Block::ToolUse { id: "t".into(), name: "Bash".into(), input: json!({}) }]));
        // The prompt that started the turn is replayed too, and is not a follow-up.
        assert!(mapper.map(replayed("11111111-1111-4111-8111-111111111111", "Build it.")).is_empty());
        let taken = mapper.map(replayed(&wire_uuid("18e6a58ab5fe8230"), "Also run the tests."));
        assert_eq!(
            taken,
            [AgentEvent::FollowUp {
                id: "18e6a58ab5fe8230".into(),
                text: "Also run the tests.".into(),
                note: String::new(),
                files: Vec::new()
            }]
        );
        assert!(mapper.map(replayed(&wire_uuid("18e6a58ab5fe8230"), "Also run the tests.")).is_empty(), "once");
        // The turn ends with one still waiting: Claude Code answers it next, in a turn of its own.
        let mut ended = result(false, Some("Built."), 0.0);
        if let sdk::Event::Result(result) = &mut ended {
            result.queued_turn_count = 1;
        }
        let ended = mapper.map(ended);
        assert_eq!(ended[ended.len() - 2..], [AgentEvent::Queued(1), AgentEvent::TurnEnded { problem: None }]);
        let next = mapper.map(replayed(&wire_uuid("18e6a58ab5fe8231"), "Then stop."));
        assert!(matches!(&next[..], [AgentEvent::FollowUp { id, .. }] if id == "18e6a58ab5fe8231"));
        assert_eq!(
            mapper.map(sdk::Event::TextDelta { text: "OK".into(), parent_tool_use_id: None })[0],
            AgentEvent::ReplyStarted
        );

        // The id Claude Code is given is a UUID, the same for the same follow-up, another for another.
        assert_eq!(wire_uuid("18e6a58ab5fe8230"), "00000000-0000-4001-88e6-a58ab5fe8230");
        assert_ne!(wire_uuid("18e6a58ab5fe8231"), wire_uuid("18e6a58ab5fe8230"));
        // Whatever the id holds, the shape stays a UUID's.
        for id in ["", "x-?", &"f".repeat(64)] {
            assert_eq!(wire_uuid(id).split('-').map(str::len).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        }
    }

    #[test]
    fn options_follow_the_agent_account_and_session() {
        let agent =
            Agent { model: " opus ".into(), autonomy: Autonomy::ReadOnly, memory: false, ..Agent::new("Scout") };
        let own = Connection::claude_agent();
        let session = Session {
            workdir: "/work".into(),
            remote: "abc".into(),
            account: own.id.clone(),
            ..Session::new(&agent.id)
        };
        let options = options(&agent, &own, &session);
        assert_eq!(
            (options.model.as_deref(), options.resume.as_deref(), options.effort),
            (Some("opus"), Some("abc"), None)
        );
        assert_eq!((options.permission_mode, options.ask_permissions), (Some(sdk::PermissionMode::DontAsk), false));
        assert_eq!(options.cwd.as_deref(), Some(std::path::Path::new("/work")));
        assert!(
            matches!(&options.system_prompt, sdk::SystemPrompt::Custom(text) if text.starts_with("You are Scout.") && text.contains("`/work`"))
        );
        assert!(options.env.is_empty(), "Claude Code's own account: its environment is left alone");
        assert_eq!(options.allowed_tools, TASK_TOOLS, "the task list's tools are asked for by name");

        // Another account runs in its own folder and cannot resume the first one's session.
        let work = Connection { id: "w".into(), config_dir: "/accounts/w".into(), ..Connection::claude_agent() };
        let other = super::options(&Agent { autonomy: Autonomy::Auto, ..agent }, &work, &session);
        assert_eq!(other.env, [("CLAUDE_CONFIG_DIR".to_owned(), "/accounts/w".to_owned())]);
        assert_eq!(
            (other.resume, other.permission_mode, other.ask_permissions),
            (None, Some(sdk::PermissionMode::Auto), true)
        );
    }
}

#[cfg(test)]
mod live {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// One real turn through the user's Claude Code installation: checks the
    /// flags this engine passes and the events it maps against the binary.
    /// It uses the owner's Claude usage, so it only runs when asked for:
    /// `cargo test -p atroxabot-core live -- --ignored`.
    #[test]
    #[ignore = "starts the real Claude Code binary and uses the owner's Claude usage"]
    fn a_real_turn_answers() {
        let dir = std::env::temp_dir().join(format!("atroxabot-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = Agent {
            instructions: "Reply with exactly the word: pong".into(),
            autonomy: Autonomy::ReadOnly,
            memory: false,
            model: "haiku".into(),
            ..Agent::new("Echo")
        };
        let (events, inbox) = mpsc::channel();
        let session = Session { workdir: dir.display().to_string(), ..Session::new(&agent.id) };
        let own = Connection::claude_agent();
        let info = claude_info(&own).unwrap();
        assert!(!info.models.is_empty() && !info.version.is_empty(), "the binary says what it offers");
        let live = ClaudeAgent::connect(&agent, &own, &session, None, move |event| drop(events.send(event))).unwrap();
        live.send("ping", &[]).unwrap();
        let (mut said, mut remote, mut ended) = (String::new(), String::new(), None);
        while ended.is_none() {
            match inbox.recv_timeout(Duration::from_secs(180)).expect("the turn ends in time") {
                AgentEvent::Text(text) => said.push_str(&text),
                AgentEvent::Remote(id) => remote = id,
                AgentEvent::TurnEnded { problem } => ended = Some(problem),
                AgentEvent::Disconnected(why) => panic!("the binary stopped: {why:?}"),
                _ => {}
            }
        }
        live.close();
        assert_eq!(ended, Some(None), "the turn ended well");
        assert!(said.to_lowercase().contains("pong"), "said {said:?}");
        assert!(!remote.is_empty(), "the session can be resumed");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A real turn in which the agent keeps a task list: checks that the
    /// binary offers its task tools to a session started here, even to an
    /// agent that may only read, and that the list is read back from them.
    #[test]
    #[ignore = "starts the real Claude Code binary and uses the owner's Claude usage"]
    fn a_real_turn_keeps_a_task_list() {
        let dir = std::env::temp_dir().join(format!("atroxabot-tasks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = Agent { autonomy: Autonomy::ReadOnly, memory: false, model: "haiku".into(), ..Agent::new("Echo") };
        let (events, inbox) = mpsc::channel();
        let mut session = Session { workdir: dir.display().to_string(), ..Session::new(&agent.id) };
        let own = Connection::claude_agent();
        let live = ClaudeAgent::connect(&agent, &own, &session, None, move |event| drop(events.send(event))).unwrap();
        let ask = "With your task tools, make two tasks, 'Read the file' and 'Fix the bug', and mark the first \
            one completed. Do nothing else, then say done.";
        live.send(ask, &[]).unwrap();
        loop {
            let event = inbox.recv_timeout(Duration::from_secs(180)).expect("the turn ends in time");
            session.apply(&event);
            match event {
                AgentEvent::TurnEnded { .. } => break,
                AgentEvent::Disconnected(why) => panic!("the binary stopped: {why:?}"),
                _ => {}
            }
        }
        live.close();
        let todos: Vec<_> = session.todos().into_iter().map(|todo| (todo.subject, todo.state)).collect();
        let expected = [("Read the file", TodoState::Done), ("Fix the bug", TodoState::Pending)];
        assert_eq!(todos, expected.map(|(subject, state)| (subject.to_owned(), state)), "{:#?}", session.entries);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// Asks the real binary what it offers. Nothing is sent to a model, so
    /// it uses nothing: `cargo test -p atroxabot-core probe -- --ignored`.
    #[test]
    #[ignore = "starts the real Claude Code binary (without using any usage)"]
    fn claude_code_says_what_it_offers() {
        let info = claude_info(&Connection::claude_agent()).unwrap();
        assert!(info.models.len() > 1 && info.models[0].value == "default", "{:?}", info.models);
        assert!(info.models.iter().all(|model| !model.resolved.is_empty()));
        assert!(!info.version.is_empty());
    }
}
