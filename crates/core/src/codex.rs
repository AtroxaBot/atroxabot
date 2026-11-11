//! The Codex engine: an agent that thinks through the Codex CLI the user
//! installed and signed in to themselves.
//!
//! AtroxaBot starts that binary as published and talks to its app server
//! (the `atroxabot-codex` crate), the interface OpenAI gives clients of
//! Codex. It never reads, stores or changes how the binary signs in: the
//! user signs in with Codex's own `codex login`, in a terminal, and its
//! usage is theirs. One process serves one session for as long as it is
//! open; the conversation itself is Codex's (a thread), resumed by its id.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use atroxabot_codex as sdk;
use serde_json::{Value, json};

use crate::agent::{Agent, Autonomy};
use crate::attach;
use crate::claude::TerminalCommand;
use crate::config::{Connection, McpServer};
use crate::engine::{AgentEvent, Ask, FollowUp, LimitWindow, Limits, Verdict};
use crate::responses::Usage;
use crate::session::{Session, ToolUse};

/// How long Codex gets to say what it offers.
const INFO_TIMEOUT: Duration = Duration::from_secs(40);
/// Most of a command's streamed output kept until the command ends and says it whole.
const OUTPUT_KEPT: usize = 200_000;

/// Where the user's Codex binary is, if it is installed.
#[must_use]
pub fn find_codex() -> Option<PathBuf> {
    sdk::find_cli()
}

/// What Codex offers for the account `connection`: its models, who is
/// signed in, its version and where the plan's limits stand. Starts the
/// binary without sending it a message, so nothing is used; call it off
/// the UI thread.
///
/// # Errors
/// The binary is not installed ([`sdk::Error::NotFound`]), could not be
/// started, or did not answer.
pub fn codex_info(connection: &Connection) -> Result<sdk::Info, sdk::Error> {
    sdk::info(&sdk::Options { env: account_env(connection), ..sdk::Options::default() }, INFO_TIMEOUT)
}

/// The command that signs the user in to the Codex account `connection`:
/// Codex's own `login`, which opens OpenAI's sign-in in the browser. The app
/// only starts it, in a terminal the user sees. `None` when Codex is not
/// installed.
#[must_use]
pub fn codex_login(connection: &Connection) -> Option<TerminalCommand> {
    // The folder must exist for Codex to keep the sign-in in it.
    if !connection.config_dir.is_empty() {
        let _ = std::fs::create_dir_all(&connection.config_dir);
    }
    Some(TerminalCommand { program: sdk::find_cli()?, args: vec!["login".to_owned()], env: account_env(connection) })
}

/// The environment that selects a Codex account: nothing for Codex's own
/// default, and otherwise `CODEX_HOME`, the folder Codex keeps an account's
/// sign-in, settings and conversations in. What is in it is Codex's business.
fn account_env(connection: &Connection) -> Vec<(String, String)> {
    if connection.config_dir.is_empty() {
        Vec::new()
    } else {
        vec![("CODEX_HOME".to_owned(), connection.config_dir.clone())]
    }
}

/// Where an account's plan limits stand, as the app keeps them.
#[must_use]
pub fn codex_limits(limits: &sdk::RateLimits) -> Limits {
    let name = |window: &sdk::RateWindow, fallback: &str| match window.window_minutes {
        Some(300) => "five_hour".to_owned(),
        Some(10_080) => "seven_day".to_owned(),
        Some(minutes) if minutes > 0 && minutes % 1440 == 0 => format!("{}_day", minutes / 1440),
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => format!("{}_hour", minutes / 60),
        Some(minutes) if minutes > 0 => format!("{minutes}_minute"),
        _ => fallback.to_owned(),
    };
    let window = |window: &sdk::RateWindow, fallback: &str| LimitWindow {
        name: name(window, fallback),
        used: (window.used_percent / 100.0).clamp(0.0, 1.0),
        resets: window.resets_at.unwrap_or(0),
    };
    let windows: Vec<LimitWindow> =
        [(limits.primary.as_ref(), "short_window"), (limits.secondary.as_ref(), "long_window")]
            .into_iter()
            .filter_map(|(found, fallback)| Some(window(found?, fallback)))
            .collect();
    // The window that is full says when the limit lifts.
    let lifts = windows.iter().filter(|window| window.used >= 1.0).map(|window| window.resets).max().unwrap_or(0);
    Limits {
        at: crate::session::unix_now(),
        status: if limits.reached { "rejected" } else { "allowed" }.to_owned(),
        resets: if limits.reached { lifts } else { 0 },
        windows,
    }
}

/// What the engine and the app's side of it both know.
#[derive(Debug, Default)]
struct Shared {
    /// The follow-ups handed to Codex and not taken in yet, by the id they were sent under.
    follow_ups: HashMap<String, FollowUp>,
    /// The questions Codex asked and waits on, by request: each question's id and its words.
    questions: HashMap<String, Vec<(String, String)>>,
    /// A message was sent and its turn has not ended.
    awaiting: bool,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running Codex session.
pub struct CodexAgent {
    session: sdk::Session,
    shared: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for CodexAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAgent").finish_non_exhaustive()
    }
}

impl CodexAgent {
    /// Starts Codex for `agent` on the account `connection`, in the folder
    /// of `session`, resuming the session's conversation when this account
    /// has it. `mcp` are the app's own MCP servers, which Codex is given for
    /// this run. `emit` is called on a reader thread with everything that
    /// happens.
    ///
    /// # Errors
    /// The binary is not installed or could not be started.
    pub fn connect(
        agent: &Agent,
        connection: &Connection,
        session: &Session,
        mcp: &[McpServer],
        mut emit: impl FnMut(AgentEvent) + Send + 'static,
    ) -> Result<Self, sdk::Error> {
        let options = options(agent, connection, session, mcp);
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let mut mapper =
            Mapper { asked_resume: options.resume.is_some(), shared: Arc::clone(&shared), ..Mapper::default() };
        let session = sdk::Session::connect(&options, move |event| {
            for event in mapper.map(event) {
                emit(event);
            }
        })?;
        // The skills every agent has: Codex is shown where they are, for this run.
        let library = crate::skills::Library::open().ok().filter(|library| !library.skills().is_empty());
        if let Some(library) = library {
            let roots = json!({ "extraRoots": [library.skills_dir().display().to_string()] });
            // An older Codex that does not know the request says so in a warning; the session works without.
            let _ = session.call("skills/extraRoots/set", roots);
        }
        Ok(Self { session, shared })
    }

    /// Sends the user's message with the `files` that go with it (pictures
    /// for Codex to look at, the rest named by path); the reply arrives as events.
    ///
    /// # Errors
    /// The process has ended.
    pub fn send(&self, text: &str, files: &[String]) -> Result<(), sdk::Error> {
        lock(&self.shared).awaiting = true;
        self.session.send(&input(text, files), None)
    }

    /// Sends a message the user wrote while the turn runs: Codex takes it
    /// into the turn (reported as an [`AgentEvent::FollowUp`]). One it does
    /// not take in before the turn ends is not reported, and the app sends
    /// it again.
    ///
    /// # Errors
    /// The process has ended.
    pub fn follow_up(&self, message: FollowUp) -> Result<(), sdk::Error> {
        let (id, input) = (message.id.clone(), input(&message.text, &message.files));
        lock(&self.shared).follow_ups.insert(id.clone(), message);
        self.session.steer(&input, &id)
    }

    /// Stops the turn in progress; the session stays usable.
    pub fn interrupt(&self) {
        // Fails only if the process is gone, which ends the turn anyway.
        let _ = self.session.interrupt();
    }

    /// Answers the approval or the question `id`.
    pub fn decide(&self, id: &str, verdict: Verdict) {
        let asked = lock(&self.shared).questions.remove(id);
        // Fails if the request was withdrawn or the process is gone.
        let _ = match (asked, verdict) {
            (Some(asked), Verdict::Answer(input)) => {
                let given = serde_json::from_str::<Value>(&input).unwrap_or(Value::Null);
                let answers: Vec<(String, Vec<String>)> = asked
                    .into_iter()
                    .filter_map(|(question, words)| {
                        let answer = given.get("answers")?.get(&words)?.as_str()?.to_owned();
                        Some((question, vec![answer]))
                    })
                    .collect();
                self.session.answer(id, &answers)
            }
            // Dismissed: no answers, and the agent carries on without.
            (Some(_), _) => self.session.answer(id, &[]),
            (None, Verdict::Allow | Verdict::Answer(_)) => self.session.respond(id, sdk::Decision::Accept),
            (None, Verdict::AllowAlways) => self.session.respond(id, sdk::Decision::AcceptForSession),
            (None, Verdict::Deny(_)) => self.session.respond(id, sdk::Decision::Decline),
        };
    }

    /// Ends the process. Also happens when the value is dropped.
    pub fn close(&self) {
        self.session.close();
    }
}

/// A message as Codex takes it: the words (with the paths of files that
/// are not pictures), then the pictures, which Codex reads itself.
fn input(text: &str, files: &[String]) -> Vec<sdk::Input> {
    let picture = |file: &&String| {
        let small =
            std::fs::metadata(file.as_str()).is_ok_and(|meta| meta.is_file() && meta.len() <= attach::MAX_IMAGE);
        small && attach::media_type(file).is_some()
    };
    let (pictures, named): (Vec<&String>, Vec<&String>) = files.iter().partition(picture);
    let named: Vec<&str> = named.into_iter().map(String::as_str).collect();
    let text = attach::with_paths(text, &named);
    let mut input = Vec::new();
    if !text.trim().is_empty() || pictures.is_empty() {
        input.push(sdk::Input::Text(text));
    }
    input.extend(pictures.into_iter().map(|file| sdk::Input::LocalImage(PathBuf::from(file))));
    input
}

/// A string as TOML writes it.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The app's MCP servers as Codex configuration: `mcp_servers.<name>.…`
/// settings for this run only, each a key and its value in TOML. Nothing is
/// written to the account's own configuration.
#[must_use]
pub fn codex_mcp_config(servers: &[McpServer]) -> Vec<(String, String)> {
    let table = |pairs: &[(String, String)]| {
        let pairs: Vec<String> =
            pairs.iter().map(|(name, value)| format!("{} = {}", toml_string(name), toml_string(value))).collect();
        format!("{{ {} }}", pairs.join(", "))
    };
    let mut config = Vec::new();
    for server in servers.iter().filter(|server| server.enabled) {
        // A name is part of a setting's key: letters, digits, dashes and underscores only.
        let name: String = server
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        if name.is_empty() {
            continue;
        }
        let key = |setting: &str| format!("mcp_servers.{name}.{setting}");
        let pairs = server.pairs();
        if !server.url.trim().is_empty() {
            config.push((key("url"), toml_string(server.url.trim())));
            if !pairs.is_empty() {
                config.push((key("http_headers"), table(&pairs)));
            }
        } else if let Some((command, args)) = atroxabot_mcp::split_command(&server.command) {
            let args: Vec<String> = args.iter().map(|arg| toml_string(arg)).collect();
            config.push((key("command"), toml_string(&command)));
            config.push((key("args"), format!("[{}]", args.join(", "))));
            if !pairs.is_empty() {
                config.push((key("env"), table(&pairs)));
            }
        }
    }
    config
}

/// How Codex is started for `agent` on `connection` in `session`.
fn options(agent: &Agent, connection: &Connection, session: &Session, mcp: &[McpServer]) -> sdk::Options {
    let some = |text: &str| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
    let (approval, sandbox) = match agent.autonomy {
        // Whatever writes, runs or goes online is asked about.
        Autonomy::Ask => (sdk::Approval::Untrusted, sdk::Sandbox::ReadOnly),
        // Its folder is its own; what reaches beyond it is asked about.
        Autonomy::Edits | Autonomy::Auto => (sdk::Approval::OnRequest, sdk::Sandbox::WorkspaceWrite),
        Autonomy::Full => (sdk::Approval::Never, sdk::Sandbox::FullAccess),
        // Nothing is asked, so what the sandbox refuses simply fails.
        Autonomy::ReadOnly => (sdk::Approval::Never, sdk::Sandbox::ReadOnly),
    };
    let folder = session.folder();
    let mut config = codex_mcp_config(mcp);
    // Its notes are outside its folder: the sandbox lets it write them.
    if let Some(memory) = agent.memory_dir() {
        let roots = format!("[{}]", toml_string(&memory.display().to_string()));
        config.push(("sandbox_workspace_write.writable_roots".to_owned(), roots));
    }
    sdk::Options {
        cli_path: None,
        env: account_env(connection),
        // The agent's own instructions, on top of Codex's: who it is and how it works here.
        instructions: Some(format!("{}{}", agent.system_prompt(&folder), session.worktree_note())),
        cwd: Some(folder).filter(|folder| !folder.as_os_str().is_empty()),
        model: some(&agent.model),
        effort: some(&agent.effort),
        approval,
        sandbox,
        // Another account keeps its own conversations: only the one that has this one can resume it.
        resume: some(&session.remote).filter(|_| session.account == connection.id),
        config,
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// A sub-agent Codex started: the call that started it, and what it said last.
#[derive(Debug, Default)]
struct SubAgent {
    parent: String,
    said: String,
    /// Whether the call was answered: its agent is done.
    done: bool,
}

/// Turns Codex's events into the app's, keeping what a turn needs
/// remembered between them.
#[derive(Debug, Default)]
struct Mapper {
    /// Whether a reply is being written.
    replying: bool,
    /// The session asked to continue a conversation Codex has.
    asked_resume: bool,
    /// The model that answers.
    model: String,
    /// The latest request's tokens, and the thread's so far.
    last: sdk::TokenUsage,
    total: sdk::TokenUsage,
    /// The thread's output tokens when the turn began: the turn's are what was added since.
    output_before: u64,
    /// The model's context window, as last said.
    window: u64,
    /// The messages and reasoning that streamed, which their complete item then repeats.
    streamed: HashSet<String>,
    /// The message the text last written belongs to, while text is the latest thing written.
    text_of: Option<String>,
    /// The calls reported so far, so that none is reported twice.
    calls: HashSet<String>,
    /// What each command printed so far, until it ends and says it whole.
    printed: HashMap<String, String>,
    /// What each file change is, for the approval that asks about it.
    changes: HashMap<String, (String, String)>,
    /// The sub-agents, by their thread.
    agents: HashMap<String, SubAgent>,
    /// How many plans the turn made so far.
    plans: u64,
    /// The turn's context was summarised: said once.
    compacted: bool,
    /// Why the turn is failing, as last reported.
    problem: Option<String>,
    shared: Arc<Mutex<Shared>>,
}

/// One line on what a web search did.
fn search_done(action: Option<&Value>) -> String {
    let field = |key: &str| action.and_then(|action| action.get(key)).and_then(Value::as_str).unwrap_or_default();
    match (field("url"), field("pattern")) {
        ("", _) => "Searched the web.".to_owned(),
        (url, "") => format!("Opened {url}"),
        (url, pattern) => format!("Looked for \"{pattern}\" in {url}"),
    }
}

/// What a failed turn is called: the user's to fix where it can be.
fn failure(kind: &str, http_status: Option<u16>, message: &str, details: Option<&str>) -> String {
    if kind == "unauthorized" || http_status == Some(401) {
        return "Codex is not signed in. Sign in under Settings, Providers.".to_owned();
    }
    match details.map(str::trim).filter(|details| !details.is_empty() && !message.contains(details)) {
        Some(details) => format!("{message} ({details})"),
        None => message.to_owned(),
    }
}

impl Mapper {
    /// Starts the turn's reply if nothing did yet. The whole turn is one
    /// reply: Codex keeps the conversation, so nothing is replayed from it.
    fn begin(&mut self, out: &mut Vec<AgentEvent>) {
        if !self.replying {
            self.replying = true;
            out.push(AgentEvent::ReplyStarted);
        }
    }

    /// More of the reply's text, of the message `item`: a new message
    /// starts a new paragraph.
    fn text(&mut self, item: &str, text: String, out: &mut Vec<AgentEvent>) {
        self.begin(out);
        if self.text_of.as_deref().is_some_and(|of| of != item) {
            out.push(AgentEvent::Text("\n\n".to_owned()));
        }
        self.text_of = Some(item.to_owned());
        out.push(AgentEvent::Text(text));
    }

    /// Reports the call `id` of `tool` with `input`, once.
    fn call(&mut self, id: &str, tool: &str, input: &Value, out: &mut Vec<AgentEvent>) {
        if self.calls.insert(id.to_owned()) {
            self.begin(out);
            self.text_of = None;
            let call =
                ToolUse { id: id.to_owned(), name: tool.to_owned(), input: input.to_string(), ..ToolUse::default() };
            out.push(AgentEvent::ToolCall(call));
        }
    }

    /// What a thread item adds: `done` when it is complete.
    fn item(&mut self, item: sdk::Item, done: bool, out: &mut Vec<AgentEvent>) {
        let failed = |status: sdk::ItemStatus| {
            matches!(status, sdk::ItemStatus::Failed | sdk::ItemStatus::Declined | sdk::ItemStatus::Interrupted)
        };
        let result = |id: String, output: String, is_error: bool| AgentEvent::ToolResult { id, output, is_error };
        match item {
            // A message that streamed was written already. A plan it proposes is words of its reply too.
            sdk::Item::AgentMessage { id, text, .. } | sdk::Item::Plan { id, text } => {
                if done && !self.streamed.remove(&id) && !text.trim().is_empty() {
                    self.text(&id, text, out);
                }
            }
            sdk::Item::Reasoning { id, summary, content } => {
                let whole = if summary.iter().any(|part| !part.trim().is_empty()) { summary } else { content };
                let whole = whole.join("\n\n");
                if done && !self.streamed.remove(&id) && !whole.trim().is_empty() {
                    self.begin(out);
                    self.text_of = None;
                    out.push(AgentEvent::Thinking(whole));
                }
            }
            sdk::Item::Command { id, command, cwd, status, output, exit_code, .. } => {
                self.call(&id, "Bash", &json!({ "command": command, "cwd": cwd }), out);
                if done {
                    let printed = self.printed.remove(&id);
                    let said = output.or(printed).unwrap_or_default();
                    let said = if status == sdk::ItemStatus::Declined && said.is_empty() {
                        "The user declined this.".to_owned()
                    } else {
                        said
                    };
                    out.push(result(id, said, failed(status) || exit_code.is_some_and(|code| code != 0)));
                }
            }
            sdk::Item::FileChange { id, changes, status } => {
                let files: Vec<Value> = changes
                    .iter()
                    .map(|change| {
                        let kind = match change.kind {
                            sdk::ChangeKind::Add => "add",
                            sdk::ChangeKind::Delete => "delete",
                            sdk::ChangeKind::Update => "update",
                        };
                        json!({ "path": change.path, "kind": kind, "moved_to": change.move_path, "diff": change.diff })
                    })
                    .collect();
                let first = changes.first().map(|change| change.path.clone()).unwrap_or_default();
                let input = json!({ "file_path": first, "changes": files });
                self.changes.insert(id.clone(), (first, input.to_string()));
                self.call(&id, "Edit", &input, out);
                if done {
                    let printed = self.printed.remove(&id).filter(|printed| !printed.trim().is_empty());
                    let said = printed.unwrap_or_else(|| match (status, changes.len()) {
                        (sdk::ItemStatus::Declined, _) => "The user declined this.".to_owned(),
                        (status, _) if failed(status) => "The change could not be applied.".to_owned(),
                        (_, 1) => "Changed 1 file.".to_owned(),
                        (_, count) => format!("Changed {count} files."),
                    });
                    out.push(result(id, said, failed(status)));
                }
            }
            sdk::Item::McpToolCall { id, server, tool, arguments, status, result: answer, error } => {
                self.call(&id, &format!("mcp__{server}__{tool}"), &arguments, out);
                if done {
                    let is_error = error.is_some() || failed(status);
                    out.push(result(id, error.or(answer).unwrap_or_default(), is_error));
                }
            }
            sdk::Item::ToolCall { id, tool, arguments, status, output } => {
                self.call(&id, &tool, &arguments, out);
                if done {
                    out.push(result(id, output.unwrap_or_default(), failed(status)));
                }
            }
            sdk::Item::WebSearch { id, query, action } => {
                self.call(&id, "WebSearch", &json!({ "query": query }), out);
                if done {
                    out.push(result(id, search_done(action.as_ref()), false));
                }
            }
            sdk::Item::ImageView { id, path } => {
                self.call(&id, "Read", &json!({ "file_path": path }), out);
                if done {
                    out.push(result(id, String::new(), false));
                }
            }
            // Starting another agent is the app's subagent: what that one says and does goes inside this call,
            // which is answered when it is done.
            sdk::Item::Collab { id, tool, status, prompt, receivers, .. } if tool == "spawnAgent" => {
                let prompt = prompt.unwrap_or_default();
                let about: String = prompt.lines().next().unwrap_or_default().chars().take(80).collect();
                self.call(&id, "Agent", &json!({ "description": about, "prompt": prompt }), out);
                for thread in receivers {
                    self.agents.entry(thread).or_insert_with(|| SubAgent { parent: id.clone(), ..SubAgent::default() });
                }
                if done && failed(status) {
                    out.push(result(id.clone(), "The agent could not be started.".to_owned(), true));
                    self.agents.values_mut().filter(|agent| agent.parent == id).for_each(|agent| agent.done = true);
                }
            }
            sdk::Item::Collab { id, tool, status, receivers, states, .. } => {
                self.call(&id, &tool, &json!({ "agents": receivers }), out);
                if done {
                    let lines: Vec<String> = states
                        .into_iter()
                        .map(|(_, state, message)| match message.filter(|message| !message.trim().is_empty()) {
                            Some(message) => format!("{state}: {message}"),
                            None => state,
                        })
                        .collect();
                    out.push(result(id, lines.join("\n"), failed(status)));
                }
            }
            sdk::Item::Compaction { .. } => self.summarised(out),
            // The user's own words are reported apart; what else there is has no place in a conversation.
            sdk::Item::UserMessage { .. } | sdk::Item::Other { .. } => {}
        }
    }

    /// The conversation was summarised to make room: said once a turn.
    fn summarised(&mut self, out: &mut Vec<AgentEvent>) {
        if !std::mem::replace(&mut self.compacted, true) {
            out.push(AgentEvent::Note("Earlier messages were summarised to make room".to_owned()));
        }
    }

    /// What a sub-agent's thread reports goes inside the call that started it.
    fn sub(&mut self, thread: &str, event: sdk::Event, out: &mut Vec<AgentEvent>) {
        let Some(parent) = self.agents.get(thread).map(|agent| agent.parent.clone()) else { return };
        let done = matches!(event, sdk::Event::ItemCompleted(_));
        let inside = |event: AgentEvent| AgentEvent::Sub { parent: parent.clone(), event: Box::new(event) };
        let activity = |text: &str| AgentEvent::Activity { id: parent.clone(), text: text.to_owned() };
        match event {
            sdk::Event::TurnStarted { .. } => out.push(activity("Working…")),
            sdk::Event::TextDelta { text, .. } => {
                if let Some(agent) = self.agents.get_mut(thread) {
                    agent.said.push_str(&text);
                }
                out.push(inside(AgentEvent::Text(text)));
            }
            sdk::Event::ReasoningDelta { text, .. } => out.push(inside(AgentEvent::Thinking(text))),
            sdk::Event::ItemStarted(item) | sdk::Event::ItemCompleted(item) => {
                // Its calls are told apart from the agent's own by their ids, which Codex makes unique.
                let mut own = Vec::new();
                if let sdk::Item::AgentMessage { text, .. } = &item
                    && let Some(agent) = self.agents.get_mut(thread)
                    && !text.trim().is_empty()
                {
                    agent.said.clone_from(text);
                }
                let (was_replying, text_of) = (self.replying, self.text_of.take());
                self.replying = true;
                self.item(item, done, &mut own);
                (self.replying, self.text_of) = (was_replying, text_of);
                for event in own {
                    match event {
                        AgentEvent::ToolCall(_) | AgentEvent::Thinking(_) => out.push(inside(event)),
                        // A result finds its call wherever it is.
                        AgentEvent::ToolResult { .. } => out.push(event),
                        _ => {}
                    }
                }
            }
            sdk::Event::TurnCompleted { status, error, .. } => {
                let Some(agent) = self.agents.get_mut(thread) else { return };
                agent.done = true;
                let failed = status == sdk::TurnStatus::Failed;
                let said = std::mem::take(&mut agent.said);
                let output = if failed { error.unwrap_or(said) } else { said };
                out.push(AgentEvent::ToolResult { id: parent.clone(), output, is_error: failed });
                out.push(activity(match status {
                    sdk::TurnStatus::Completed => "Done",
                    sdk::TurnStatus::Interrupted => "Stopped",
                    sdk::TurnStatus::Failed => "Failed",
                }));
            }
            _ => {}
        }
    }

    /// Ends the turn: its reply, what it used, and why it failed if it did.
    fn end(&mut self, problem: Option<String>, out: &mut Vec<AgentEvent>) {
        self.begin(out);
        // A sub-agent Codex never said the end of is not left running for ever.
        for agent in self.agents.values_mut().filter(|agent| !agent.done) {
            agent.done = true;
            let output = std::mem::take(&mut agent.said);
            out.push(AgentEvent::ToolResult { id: agent.parent.clone(), output, is_error: false });
        }
        let spoken = self.total.output.saturating_sub(self.output_before);
        let usage = Usage {
            input_tokens: self.last.input,
            output_tokens: if spoken > 0 { spoken } else { self.last.output },
            cached_tokens: self.last.cached_input,
            cache_write_tokens: self.last.cache_write_input,
        };
        out.push(AgentEvent::ReplyEnded {
            raw: Vec::new(),
            model: self.model.clone(),
            usage,
            cost: 0.0,
            problem: problem.clone().unwrap_or_default(),
        });
        if self.window > 0 {
            out.push(AgentEvent::Window(self.window));
        }
        out.push(AgentEvent::Status(String::new()));
        out.push(AgentEvent::TurnEnded { problem });
        // What was handed over and not taken in goes again from the app.
        let mut shared = lock(&self.shared);
        shared.follow_ups.clear();
        shared.awaiting = false;
        drop(shared);
        self.replying = false;
        self.text_of = None;
        self.compacted = false;
        self.problem = None;
        self.last = sdk::TokenUsage::default();
        self.streamed.clear();
        self.calls.clear();
        self.printed.clear();
        self.changes.clear();
    }

    fn map(&mut self, event: sdk::Event) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match event {
            sdk::Event::Ready { thread_id, model, resumed, .. } => {
                self.model = model;
                out.push(AgentEvent::Remote(thread_id));
                if self.asked_resume && !resumed {
                    let note = "Codex no longer has the earlier conversation: it starts over from here.";
                    out.push(AgentEvent::Note(note.to_owned()));
                }
            }
            sdk::Event::TurnStarted { .. } => {
                self.output_before = self.total.output;
                out.push(AgentEvent::Working);
                self.begin(&mut out);
            }
            // A message handed over during the turn is taken in: the model reads it here.
            sdk::Event::UserMessage { client_id: Some(id), .. } => {
                let taken = lock(&self.shared).follow_ups.remove(&id);
                if let Some(FollowUp { id, text, files }) = taken {
                    self.text_of = None;
                    out.push(AgentEvent::FollowUp { id, text, note: String::new(), files });
                }
            }
            sdk::Event::TextDelta { item_id, text } => {
                self.streamed.insert(item_id.clone());
                self.text(&item_id, text, &mut out);
            }
            sdk::Event::ReasoningDelta { item_id, text } => {
                self.begin(&mut out);
                self.streamed.insert(item_id);
                self.text_of = None;
                out.push(AgentEvent::Thinking(text));
            }
            sdk::Event::ReasoningBreak { item_id } if self.streamed.contains(&item_id) => {
                out.push(AgentEvent::Thinking("\n\n".to_owned()));
            }
            sdk::Event::OutputDelta { item_id, text } => {
                let printed = self.printed.entry(item_id).or_default();
                if printed.len() < OUTPUT_KEPT {
                    printed.push_str(&text);
                }
            }
            sdk::Event::ItemStarted(item) => self.item(item, false, &mut out),
            sdk::Event::ItemCompleted(item) => self.item(item, true, &mut out),
            // The agent's plan is its task list: the app reads it from a call of that name.
            sdk::Event::Plan { steps, .. } => {
                let todos: Vec<Value> = steps
                    .iter()
                    .map(|step| {
                        let status = match step.status {
                            sdk::PlanStatus::Pending => "pending",
                            sdk::PlanStatus::InProgress => "in_progress",
                            sdk::PlanStatus::Completed => "completed",
                        };
                        json!({ "content": step.step, "status": status, "activeForm": step.step })
                    })
                    .collect();
                self.plans += 1;
                let id = format!("plan-{}-{}", crate::session::new_id(), self.plans);
                self.call(&id, "TodoWrite", &json!({ "todos": todos }), &mut out);
                out.push(AgentEvent::ToolResult { id, output: "The plan was updated.".to_owned(), is_error: false });
            }
            sdk::Event::TokenUsage { last, total, context_window } => {
                (self.last, self.total) = (last, total);
                self.window = context_window.unwrap_or(self.window);
            }
            sdk::Event::RateLimits(limits) => out.push(AgentEvent::Limits(codex_limits(&limits))),
            sdk::Event::Approval(request) => {
                let known = self.changes.get(&request.item_id).cloned();
                let reason = request.reason.clone().unwrap_or_default();
                let (tool, summary, input) = match request.kind {
                    sdk::ApprovalKind::Command => {
                        let command = request.command.clone().unwrap_or_default();
                        let input = json!({ "command": command, "cwd": request.cwd, "host": request.host });
                        ("Bash", if command.is_empty() { request.host.unwrap_or_default() } else { command }, input)
                    }
                    sdk::ApprovalKind::FileChange => {
                        let (file, input) = known.unwrap_or_default();
                        let input = serde_json::from_str(&input)
                            .unwrap_or_else(|_| json!({ "file_path": request.grant_root, "reason": reason }));
                        ("Edit", file, input)
                    }
                    sdk::ApprovalKind::Permissions => {
                        let wanted = request.raw.get("permissions").cloned().unwrap_or(Value::Null);
                        ("Permissions", reason.clone(), json!({ "permissions": wanted, "cwd": request.cwd }))
                    }
                };
                out.push(AgentEvent::Permission(Ask {
                    id: request.request_id,
                    tool: tool.to_owned(),
                    input: input.to_string(),
                    summary,
                    reason,
                }));
            }
            // A question, in the shape the app's question card reads.
            sdk::Event::Question(request) => {
                let asked: Vec<(String, String)> =
                    request.questions.iter().map(|q| (q.id.clone(), q.question.clone())).collect();
                let questions: Vec<Value> = request
                    .questions
                    .iter()
                    .map(|asked| {
                        let options: Vec<Value> = asked
                            .options
                            .iter()
                            .map(|(label, description)| json!({ "label": label, "description": description }))
                            .collect();
                        json!({ "question": asked.question, "header": asked.header, "options": options,
                                "multiSelect": false })
                    })
                    .collect();
                let summary = asked.first().map(|(_, words)| words.clone()).unwrap_or_default();
                lock(&self.shared).questions.insert(request.request_id.clone(), asked);
                out.push(AgentEvent::Permission(Ask {
                    id: request.request_id,
                    tool: "AskUserQuestion".to_owned(),
                    input: json!({ "questions": questions }).to_string(),
                    summary,
                    reason: String::new(),
                }));
            }
            sdk::Event::RequestResolved { request_id } => {
                lock(&self.shared).questions.remove(&request_id);
                out.push(AgentEvent::PermissionGone(request_id));
            }
            // Codex tries again by itself: a passing note. What ends the turn is said when it ends.
            sdk::Event::Error { message, will_retry: true, .. } => out.push(AgentEvent::Status(message)),
            sdk::Event::Error { message, details, kind, http_status, .. } => {
                if kind == "usageLimitExceeded" {
                    out.push(AgentEvent::LimitReached);
                }
                self.problem = Some(failure(&kind, http_status, &message, details.as_deref()));
            }
            sdk::Event::Warning(text) if self.replying => out.push(AgentEvent::Status(text)),
            sdk::Event::Compacted => self.summarised(&mut out),
            sdk::Event::ModelRerouted { from, to } => {
                out.push(AgentEvent::Note(format!("Codex answered with {to} instead of {from}.")));
                self.model = to;
            }
            sdk::Event::TurnCompleted { status, error, .. } => {
                let problem = match status {
                    // Being stopped is what was asked for.
                    sdk::TurnStatus::Completed | sdk::TurnStatus::Interrupted => None,
                    sdk::TurnStatus::Failed => {
                        let said = self.problem.take().or(error).filter(|said| !said.trim().is_empty());
                        Some(said.map_or_else(
                            || "The turn failed.".to_owned(),
                            |said| {
                                if said.contains("401 Unauthorized") {
                                    failure("unauthorized", None, "", None)
                                } else {
                                    said
                                }
                            },
                        ))
                    }
                };
                self.end(problem, &mut out);
            }
            sdk::Event::Sub { thread_id, event } => self.sub(&thread_id, *event, &mut out),
            sdk::Event::Closed { exit_code, error } => {
                let why = error.or_else(|| exit_code.filter(|code| *code != 0).map(|code| format!("exit code {code}")));
                // A message it never answered still gets an end, and a reason.
                if self.replying || lock(&self.shared).awaiting {
                    let problem = format!(
                        "Codex stopped unexpectedly{}.",
                        why.as_ref().map_or_else(String::new, |why| format!(": {why}"))
                    );
                    self.end(Some(problem), &mut out);
                }
                out.push(AgentEvent::Disconnected(why));
            }
            // Its own user message, warnings between turns, stderr lines.
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(item: &str, text: &str) -> sdk::Event {
        sdk::Event::TextDelta { item_id: item.into(), text: text.into() }
    }

    fn command(id: &str, status: sdk::ItemStatus, output: Option<&str>, exit_code: Option<i64>) -> sdk::Item {
        sdk::Item::Command {
            id: id.into(),
            command: "cargo test".into(),
            cwd: "/work".into(),
            status,
            output: output.map(str::to_owned),
            exit_code,
            duration_ms: None,
        }
    }

    fn completed(status: sdk::TurnStatus, error: Option<&str>) -> sdk::Event {
        sdk::Event::TurnCompleted { turn_id: "t".into(), status, error: error.map(str::to_owned) }
    }

    fn tokens(input: u64, cached: u64, output: u64) -> sdk::TokenUsage {
        sdk::TokenUsage { input, cached_input: cached, output, total: input + output, ..sdk::TokenUsage::default() }
    }

    #[test]
    fn a_turn_is_one_reply_with_its_calls_and_what_it_used() {
        let mut mapper = Mapper::default();
        let ready = sdk::Event::Ready {
            thread_id: "th".into(),
            model: "gpt-6.1-sol".into(),
            effort: None,
            sandbox: "workspaceWrite".into(),
            resumed: false,
            version: "0.167.1".into(),
        };
        assert_eq!(mapper.map(ready), [AgentEvent::Remote("th".into())]);
        assert_eq!(
            mapper.map(sdk::Event::TurnStarted { turn_id: "t".into() }),
            [AgentEvent::Working, AgentEvent::ReplyStarted]
        );
        // Its own prompt comes back as an item, and is not a follow-up.
        assert!(mapper.map(sdk::Event::UserMessage { client_id: None, text: "Run the tests".into() }).is_empty());
        assert_eq!(mapper.map(delta("m1", "I'll run ")), [AgentEvent::Text("I'll run ".into())]);
        assert_eq!(mapper.map(delta("m1", "them.")), [AgentEvent::Text("them.".into())]);
        let whole = sdk::Item::AgentMessage { id: "m1".into(), text: "I'll run them.".into(), phase: None };
        assert!(mapper.map(sdk::Event::ItemCompleted(whole)).is_empty(), "already streamed");

        // A command: reported when it starts, answered when it ends, with what it printed.
        let started = mapper.map(sdk::Event::ItemStarted(command("c1", sdk::ItemStatus::InProgress, None, None)));
        let [AgentEvent::ToolCall(call)] = &started[..] else { panic!("a call: {started:?}") };
        assert_eq!((call.id.as_str(), call.name.as_str()), ("c1", "Bash"));
        assert!(call.input.contains("cargo test"));
        assert!(mapper.map(sdk::Event::OutputDelta { item_id: "c1".into(), text: "ok\n".into() }).is_empty());
        let ended = mapper.map(sdk::Event::ItemCompleted(command("c1", sdk::ItemStatus::Completed, None, Some(0))));
        assert_eq!(ended, [AgentEvent::ToolResult { id: "c1".into(), output: "ok\n".into(), is_error: false }]);
        let failed = sdk::Event::ItemCompleted(command("c2", sdk::ItemStatus::Completed, Some("boom"), Some(1)));
        let failed = mapper.map(failed);
        assert!(
            matches!(&failed[..], [AgentEvent::ToolCall(_), AgentEvent::ToolResult { is_error: true, output, .. }] if output == "boom")
        );

        // A second message is a new paragraph; one that did not stream is written from its item.
        assert_eq!(mapper.map(delta("m2", "All")), [AgentEvent::Text("All".into())], "after a call: no break");
        let unstreamed = sdk::Item::AgentMessage { id: "m3".into(), text: "Done.".into(), phase: None };
        assert_eq!(
            mapper.map(sdk::Event::ItemCompleted(unstreamed)),
            [AgentEvent::Text("\n\n".into()), AgentEvent::Text("Done.".into())]
        );

        mapper.map(sdk::Event::TokenUsage {
            last: tokens(1_200, 1_000, 40),
            total: tokens(5_000, 4_000, 90),
            context_window: Some(400_000),
        });
        let ended = mapper.map(completed(sdk::TurnStatus::Completed, None));
        let AgentEvent::ReplyEnded { usage, model, problem, cost, .. } = &ended[0] else { panic!("the reply ends") };
        assert_eq!((usage.input_tokens, usage.cached_tokens, usage.output_tokens), (1_200, 1_000, 90));
        assert!(model == "gpt-6.1-sol" && problem.is_empty() && *cost == 0.0);
        assert_eq!(ended[1], AgentEvent::Window(400_000));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: None }));
        // The next turn starts a new reply, and counts only its own output.
        assert_eq!(mapper.map(sdk::Event::TurnStarted { turn_id: "t2".into() })[1], AgentEvent::ReplyStarted);
        mapper.map(sdk::Event::TokenUsage {
            last: tokens(1_300, 1_200, 10),
            total: tokens(6_300, 5_200, 100),
            context_window: None,
        });
        let ended = mapper.map(completed(sdk::TurnStatus::Interrupted, Some("interrupted")));
        assert!(matches!(&ended[0], AgentEvent::ReplyEnded { usage, .. } if usage.output_tokens == 10));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: None }), "being stopped is no failure");
    }

    #[test]
    fn edits_plans_searches_and_reasoning_are_mapped() {
        let mut mapper = Mapper::default();
        mapper.map(sdk::Event::TurnStarted { turn_id: "t".into() });
        assert_eq!(
            mapper.map(sdk::Event::ReasoningDelta { item_id: "r1".into(), text: "Think".into() }),
            [AgentEvent::Thinking("Think".into())]
        );
        assert_eq!(
            mapper.map(sdk::Event::ReasoningBreak { item_id: "r1".into() }),
            [AgentEvent::Thinking("\n\n".into())]
        );
        let whole =
            sdk::Item::Reasoning { id: "r2".into(), summary: vec!["One".into(), "Two".into()], content: vec![] };
        assert_eq!(mapper.map(sdk::Event::ItemCompleted(whole)), [AgentEvent::Thinking("One\n\nTwo".into())]);

        let change = sdk::FileChange {
            path: "src/a.rs".into(),
            kind: sdk::ChangeKind::Update,
            move_path: None,
            diff: "@@ -1 +1 @@\n-a\n+b".into(),
        };
        let edit = |status| sdk::Item::FileChange { id: "f1".into(), changes: vec![change.clone()], status };
        let started = mapper.map(sdk::Event::ItemStarted(edit(sdk::ItemStatus::InProgress)));
        let [AgentEvent::ToolCall(call)] = &started[..] else { panic!("a call") };
        assert_eq!(call.name, "Edit");
        assert_eq!(crate::tools::summary(&serde_json::from_str(&call.input).unwrap()), "src/a.rs");
        // Codex asks before it applies it: the card shows the same change.
        let ask = sdk::ApprovalRequest {
            request_id: "7".into(),
            kind: sdk::ApprovalKind::FileChange,
            item_id: "f1".into(),
            thread_id: "th".into(),
            command: None,
            cwd: None,
            reason: Some("outside the folder".into()),
            grant_root: None,
            host: None,
            raw: Value::Null,
        };
        let asked = mapper.map(sdk::Event::Approval(ask));
        let [AgentEvent::Permission(ask)] = &asked[..] else { panic!("an approval") };
        assert_eq!((ask.id.as_str(), ask.tool.as_str(), ask.summary.as_str()), ("7", "Edit", "src/a.rs"));
        assert!(ask.input.contains("+b") && ask.reason == "outside the folder");
        assert_eq!(
            mapper.map(sdk::Event::RequestResolved { request_id: "7".into() }),
            [AgentEvent::PermissionGone("7".into())]
        );
        let declined = mapper.map(sdk::Event::ItemCompleted(edit(sdk::ItemStatus::Declined)));
        assert_eq!(
            declined,
            [AgentEvent::ToolResult { id: "f1".into(), output: "The user declined this.".into(), is_error: true }]
        );

        // The plan is the task list the side panel shows.
        let step = |step: &str, status| sdk::PlanStep { step: step.into(), status };
        let plan = sdk::Event::Plan {
            explanation: None,
            steps: vec![
                step("Read the file", sdk::PlanStatus::Completed),
                step("Fix the bug", sdk::PlanStatus::InProgress),
            ],
        };
        let mut session = Session::default();
        for event in mapper.map(plan) {
            session.apply(&event);
        }
        let todos: Vec<_> = session.todos().into_iter().map(|todo| (todo.subject, todo.state)).collect();
        assert_eq!(
            todos,
            [
                ("Read the file".to_owned(), crate::session::TodoState::Done),
                ("Fix the bug".to_owned(), crate::session::TodoState::Active)
            ]
        );

        let search =
            sdk::Item::WebSearch { id: "w1".into(), query: "rust".into(), action: Some(json!({"url": "https://x"})) };
        let searched = mapper.map(sdk::Event::ItemCompleted(search));
        assert!(matches!(&searched[..], [AgentEvent::ToolCall(call), AgentEvent::ToolResult { output, .. }]
            if call.name == "WebSearch" && output == "Opened https://x"));
        let tool = sdk::Item::McpToolCall {
            id: "m1".into(),
            server: "files".into(),
            tool: "list".into(),
            arguments: json!({ "path": "." }),
            status: sdk::ItemStatus::Failed,
            result: None,
            error: Some("no such folder".into()),
        };
        let called = mapper.map(sdk::Event::ItemCompleted(tool));
        assert!(
            matches!(&called[..], [AgentEvent::ToolCall(call), AgentEvent::ToolResult { is_error: true, output, .. }]
            if call.name == "mcp__files__list" && output == "no such folder")
        );
        // Summarised once, however Codex says it.
        assert_eq!(mapper.map(sdk::Event::Compacted).len(), 1);
        assert!(mapper.map(sdk::Event::ItemCompleted(sdk::Item::Compaction { id: "x".into() })).is_empty());
    }

    #[test]
    fn follow_ups_questions_and_failures() {
        let mut mapper = Mapper::default();
        mapper.map(sdk::Event::TurnStarted { turn_id: "t".into() });
        let follow_up = FollowUp { id: "f1".into(), text: "Also the docs".into(), files: Vec::new() };
        lock(&mapper.shared).follow_ups.insert("f1".into(), follow_up);
        let taken = mapper.map(sdk::Event::UserMessage { client_id: Some("f1".into()), text: "Also the docs".into() });
        assert!(matches!(&taken[..], [AgentEvent::FollowUp { id, text, .. }] if id == "f1" && text == "Also the docs"));
        assert!(mapper.map(sdk::Event::UserMessage { client_id: Some("f1".into()), text: String::new() }).is_empty());

        let question = sdk::Question {
            id: "q1".into(),
            header: "Format".into(),
            question: "Which format?".into(),
            options: vec![("JSON".into(), "Machine readable".into())],
            ..sdk::Question::default()
        };
        let request = sdk::QuestionRequest {
            request_id: "9".into(),
            item_id: "i".into(),
            thread_id: "th".into(),
            questions: vec![question],
        };
        let asked = mapper.map(sdk::Event::Question(request));
        let [AgentEvent::Permission(ask)] = &asked[..] else { panic!("a question") };
        assert_eq!((ask.tool.as_str(), ask.summary.as_str()), ("AskUserQuestion", "Which format?"));
        let input: Value = serde_json::from_str(&ask.input).unwrap();
        assert_eq!(input["questions"][0]["options"][0]["label"], "JSON");
        assert_eq!(lock(&mapper.shared).questions["9"], [("q1".to_owned(), "Which format?".to_owned())]);

        // Retries are a passing note; what ends the turn is its problem, in words the user can act on.
        let error = |message: &str, will_retry, http_status| sdk::Event::Error {
            message: message.into(),
            details: None,
            kind: "httpConnectionFailed".into(),
            http_status,
            will_retry,
        };
        assert_eq!(
            mapper.map(error("Reconnecting... 2/5", true, None)),
            [AgentEvent::Status("Reconnecting... 2/5".into())]
        );
        assert!(mapper.map(error("unexpected status 401 Unauthorized", false, Some(401))).is_empty());
        lock(&mapper.shared).follow_ups.insert("f2".into(), FollowUp::default());
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, Some("unexpected status 401 Unauthorized")));
        let problem = "Codex is not signed in. Sign in under Settings, Providers.";
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some(problem.into()) }));
        assert!(lock(&mapper.shared).follow_ups.is_empty(), "what was not taken in goes again from the app");
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, None));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some("The turn failed.".into()) }));
        // The plan's limit ends the turn: the app is told, to wait for it or go on elsewhere.
        let spent = sdk::Event::Error {
            message: "You've hit your usage limit.".into(),
            details: None,
            kind: "usageLimitExceeded".into(),
            http_status: Some(429),
            will_retry: false,
        };
        assert_eq!(mapper.map(spent), [AgentEvent::LimitReached]);
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, None));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some("You've hit your usage limit.".into()) }));

        // The process ends: a message it never answered still gets an end.
        lock(&mapper.shared).awaiting = true;
        let closed = mapper.map(sdk::Event::Closed { exit_code: Some(1), error: None });
        assert!(matches!(&closed[..], [AgentEvent::ReplyStarted, AgentEvent::ReplyEnded { problem, .. }, ..]
            if problem == "Codex stopped unexpectedly: exit code 1."));
        assert_eq!(closed.last(), Some(&AgentEvent::Disconnected(Some("exit code 1".into()))));
        assert_eq!(
            mapper.map(sdk::Event::Closed { exit_code: Some(0), error: None }),
            [AgentEvent::Disconnected(None)]
        );

        // A conversation Codex no longer has starts over, and says so.
        let mut resumed = Mapper { asked_resume: true, ..Mapper::default() };
        let ready = sdk::Event::Ready {
            thread_id: "new".into(),
            model: "m".into(),
            effort: None,
            sandbox: String::new(),
            resumed: false,
            version: String::new(),
        };
        assert!(matches!(&resumed.map(ready)[..], [AgentEvent::Remote(id), AgentEvent::Note(_)] if id == "new"));
    }

    #[test]
    fn a_sub_agent_lives_inside_the_call_that_started_it() {
        let mut mapper = Mapper::default();
        mapper.map(sdk::Event::TurnStarted { turn_id: "t".into() });
        let spawn = sdk::Item::Collab {
            id: "a1".into(),
            tool: "spawnAgent".into(),
            status: sdk::ItemStatus::Completed,
            prompt: Some("Find the parser\nand read it".into()),
            receivers: vec!["sub".into()],
            states: Vec::new(),
        };
        let started = mapper.map(sdk::Event::ItemCompleted(spawn));
        let [AgentEvent::ToolCall(call)] = &started[..] else { panic!("one call, not answered yet: {started:?}") };
        assert_eq!(call.name, "Agent");
        assert!(call.input.contains("\"description\":\"Find the parser\""));
        let sub = |event| sdk::Event::Sub { thread_id: "sub".into(), event: Box::new(event) };
        let inside = |event| AgentEvent::Sub { parent: "a1".into(), event: Box::new(event) };
        assert_eq!(mapper.map(sub(delta("m", "Looking."))), [inside(AgentEvent::Text("Looking.".into()))]);
        let ran = mapper.map(sub(sdk::Event::ItemStarted(command("c9", sdk::ItemStatus::InProgress, None, None))));
        assert!(
            matches!(&ran[..], [AgentEvent::Sub { parent, event }] if parent == "a1" && matches!(&**event, AgentEvent::ToolCall(call) if call.id == "c9"))
        );
        let done =
            mapper.map(sub(sdk::Event::ItemCompleted(command("c9", sdk::ItemStatus::Completed, Some("x"), Some(0)))));
        assert_eq!(done, [AgentEvent::ToolResult { id: "c9".into(), output: "x".into(), is_error: false }]);
        let ended = mapper.map(sub(completed(sdk::TurnStatus::Completed, None)));
        assert_eq!(
            ended,
            [
                AgentEvent::ToolResult { id: "a1".into(), output: "Looking.".into(), is_error: false },
                AgentEvent::Activity { id: "a1".into(), text: "Done".into() }
            ]
        );
        // Another thread nobody started is not the agent's.
        let stray = sdk::Event::Sub { thread_id: "other".into(), event: Box::new(delta("m", "x")) };
        assert!(mapper.map(stray).is_empty());
        // One whose end Codex never says ends with the turn.
        let spawn = sdk::Item::Collab {
            id: "a2".into(),
            tool: "spawnAgent".into(),
            status: sdk::ItemStatus::InProgress,
            prompt: None,
            receivers: vec!["sub2".into()],
            states: Vec::new(),
        };
        mapper.map(sdk::Event::ItemStarted(spawn));
        let ended = mapper.map(completed(sdk::TurnStatus::Completed, None));
        assert!(ended.contains(&AgentEvent::ToolResult { id: "a2".into(), output: String::new(), is_error: false }));
    }

    #[test]
    fn options_limits_and_settings_follow_the_agent_and_account() {
        let agent =
            Agent { model: " gpt-6-astra ".into(), autonomy: Autonomy::Ask, memory: false, ..Agent::new("Scout") };
        let own =
            Connection { id: "codex".into(), kind: crate::config::ConnectionKind::Codex, ..Connection::default() };
        let session = Session {
            workdir: "/work".into(),
            remote: "th".into(),
            account: "codex".into(),
            ..Session::new(&agent.id)
        };
        let options = options(&agent, &own, &session, &[]);
        assert_eq!(
            (options.model.as_deref(), options.resume.as_deref(), options.effort),
            (Some("gpt-6-astra"), Some("th"), None)
        );
        assert_eq!((options.approval, options.sandbox), (sdk::Approval::Untrusted, sdk::Sandbox::ReadOnly));
        assert!(options.env.is_empty() && options.config.is_empty(), "Codex's own account: its setup is left alone");
        assert!(options.instructions.as_deref().is_some_and(|text| text.starts_with("You are Scout.")));
        // Another account runs in its own folder and cannot resume the first one's conversation.
        let work = Connection { id: "w".into(), config_dir: "/accounts/w".into(), ..own };
        let full = Agent { autonomy: Autonomy::Full, ..agent };
        let other = super::options(&full, &work, &session, &[]);
        assert_eq!(other.env, [("CODEX_HOME".to_owned(), "/accounts/w".to_owned())]);
        assert_eq!(
            (other.resume, other.approval, other.sandbox),
            (None, sdk::Approval::Never, sdk::Sandbox::FullAccess)
        );

        // The app's MCP servers, as settings for the run.
        let server = |name: &str, command: &str, url: &str, vars: &str| McpServer {
            name: name.into(),
            command: command.into(),
            url: url.into(),
            vars: vars.into(),
            enabled: true,
        };
        let servers = [
            server("my files", "npx -y @scope/server \"C:\\My Notes\"", "", "TOKEN=a\"b"),
            server("tracker", "", "https://mcp.example.com/mcp", "Authorization=Bearer x"),
            McpServer { enabled: false, ..server("off", "run", "", "") },
            server("broken", "run \"unclosed", "", ""),
        ];
        let config: Vec<(String, String)> = codex_mcp_config(&servers);
        let expected = [
            ("mcp_servers.my_files.command", "\"npx\""),
            ("mcp_servers.my_files.args", "[\"-y\", \"@scope/server\", \"C:\\\\My Notes\"]"),
            ("mcp_servers.my_files.env", "{ \"TOKEN\" = \"a\\\"b\" }"),
            ("mcp_servers.tracker.url", "\"https://mcp.example.com/mcp\""),
            ("mcp_servers.tracker.http_headers", "{ \"Authorization\" = \"Bearer x\" }"),
        ];
        assert_eq!(config, expected.map(|(key, value)| (key.to_owned(), value.to_owned())));

        // The plan's windows, named as the app names Claude's.
        let window =
            |used_percent, minutes| sdk::RateWindow { used_percent, window_minutes: minutes, resets_at: Some(9) };
        let limits = sdk::RateLimits {
            primary: Some(window(100.0, Some(300))),
            secondary: Some(window(42.0, Some(10_080))),
            plan: "plus".into(),
            reached: true,
        };
        let limits = codex_limits(&limits);
        assert_eq!((limits.status.as_str(), limits.resets), ("rejected", 9));
        let windows: Vec<(&str, f64)> = limits.windows.iter().map(|w| (w.name.as_str(), w.used)).collect();
        assert_eq!(windows, [("five_hour", 1.0), ("seven_day", 0.42)]);
        let odd = sdk::RateLimits {
            primary: Some(window(5.0, Some(180))),
            secondary: Some(window(5.0, None)),
            ..sdk::RateLimits::default()
        };
        let names: Vec<String> = codex_limits(&odd).windows.into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["3_hour", "long_window"]);
    }

    #[test]
    fn a_message_goes_with_its_pictures_and_named_files() {
        let dir = std::env::temp_dir().join(format!("atroxabot-codex-input-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("shot.png").display().to_string();
        std::fs::write(&picture, [0_u8; 8]).unwrap();
        let files = [picture.clone(), "notes.txt".to_owned(), dir.join("missing.png").display().to_string()];
        let sent = input("Look at this", &files);
        assert!(matches!(&sent[..], [sdk::Input::Text(text), sdk::Input::LocalImage(path)]
            if text.contains("Look at this") && text.contains("notes.txt") && text.contains("missing.png")
                && path.display().to_string() == picture));
        assert_eq!(input("", &[picture]).len(), 1, "a picture alone");
        assert_eq!(input("hi", &[]), [sdk::Input::Text("hi".into())]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// Asks the real binary what it offers. Nothing is sent to a model, so
    /// it uses nothing: `cargo test -p atroxabot-core codex::probe -- --ignored`.
    #[test]
    #[ignore = "starts the real Codex binary (without using any usage)"]
    fn codex_says_what_it_offers() {
        let own = Connection { kind: crate::config::ConnectionKind::Codex, ..Connection::default() };
        let info = codex_info(&own).unwrap();
        assert!(!info.models.is_empty() && !info.version.is_empty(), "{info:?}");
    }
}

#[cfg(test)]
mod live {
    use std::sync::mpsc;

    use super::*;

    /// One real turn through the installed Codex: checks what this engine sends and maps against the binary.
    /// Signed out it ends with the reason; signed in it answers, which uses the owner's plan, so it only runs
    /// when asked for: `cargo test -p atroxabot-core codex::live -- --ignored`.
    #[test]
    #[ignore = "starts the real Codex binary and, when it is signed in, uses the owner's usage"]
    fn a_real_turn_ends() {
        let dir = std::env::temp_dir().join(format!("atroxabot-codex-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = Agent {
            instructions: "Reply with exactly the word: pong".into(),
            autonomy: Autonomy::ReadOnly,
            memory: false,
            ..Agent::new("Echo")
        };
        let own = Connection { kind: crate::config::ConnectionKind::Codex, ..Connection::default() };
        let info = codex_info(&own).unwrap();
        assert!(!info.models.is_empty() && !info.version.is_empty(), "the binary says what it offers");
        let (events, inbox) = mpsc::channel();
        let mut session = Session { workdir: dir.display().to_string(), ..Session::new(&agent.id) };
        let live = CodexAgent::connect(&agent, &own, &session, &[], move |event| drop(events.send(event))).unwrap();
        live.send("ping", &[]).unwrap();
        let mut ended = None;
        while ended.is_none() {
            let event = inbox.recv_timeout(Duration::from_secs(180)).expect("the turn ends in time");
            session.apply(&event);
            match event {
                AgentEvent::TurnEnded { problem } => ended = Some(problem),
                AgentEvent::Disconnected(why) => panic!("the binary stopped: {why:?}"),
                _ => {}
            }
        }
        live.close();
        assert!(!session.remote.is_empty(), "the conversation can be resumed");
        match (info.account.is_some(), ended.flatten()) {
            (true, problem) => assert_eq!(problem, None, "signed in: the turn ended well"),
            (false, problem) => {
                assert_eq!(problem.as_deref(), Some("Codex is not signed in. Sign in under Settings, Providers."));
                let said =
                    matches!(session.entries.last(), Some(crate::Entry::Reply(reply)) if !reply.problem.is_empty());
                assert!(said, "the reply carries the reason: {:?}", session.entries);
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
