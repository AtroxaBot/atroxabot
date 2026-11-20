//! The Antigravity engine: an agent that thinks through the Antigravity CLI
//! (`agy`) the user installed and signed in to themselves.
//!
//! AtroxaBot starts that binary as published and talks to it in the Agent
//! Client Protocol of `agy acp` (the `atroxabot-antigravity` crate). It never
//! reads, stores or changes how the binary signs in, and never asks it to
//! sign in: the user does that with Antigravity's own `agy acp --login`, in a
//! terminal, and its usage is theirs. One process serves one session for as
//! long as it is open; the conversation itself is Antigravity's, resumed by
//! its id.
//!
//! Antigravity has no sandbox or permission mode a client sets: it asks about
//! a tool call, and the agent's autonomy answers what it settles here
//! ([`Policy`]); the rest is the user's to decide.
//!
//! The binary unpacks itself each time it starts, which takes a while. So
//! what an account offers is asked once and kept (`antigravity.json`), and
//! read anew only when the user asks for that.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use atroxabot_antigravity as sdk;
use serde_json::{Map, Value, json};

use crate::agent::{Agent, Autonomy};
use crate::attach;
use crate::claude::TerminalCommand;
use crate::config::{Config, Connection, McpServer, write_private};
use crate::engine::{AgentEvent, Ask, FollowUp, Verdict};
use crate::responses::Usage;
use crate::session::{Session, ToolUse};

/// How long Antigravity gets to say what it offers: it unpacks itself first, and each model is asked in turn.
const INFO_TIMEOUT: Duration = Duration::from_secs(150);
/// The file what the accounts offer is kept in, in the app's folder.
const KNOWN_FILE: &str = "antigravity.json";
/// What a session says while its Antigravity starts.
const STARTING: &str = "Starting Antigravity: it takes up to a minute";
/// What a session that needs a sign-in is told.
const SIGN_IN: &str = "Antigravity is not signed in. Sign in under Settings, Providers.";

/// Where the user's Antigravity binary is, if it is installed.
#[must_use]
pub fn find_antigravity() -> Option<PathBuf> {
    sdk::find_cli()
}

/// Google's own commands that install the Antigravity CLI (on Windows, then
/// elsewhere): they fetch the native binary from Google, check it and put it
/// where [`find_antigravity`] looks.
pub const ANTIGRAVITY_INSTALL: (&str, &str) = (
    "irm https://antigravity.google/cli/install.ps1 | iex",
    "curl -fsSL https://antigravity.google/cli/install.sh | bash",
);

/// The command that installs Antigravity: Google's own installer, which the
/// app only starts, in a terminal the user sees. Nothing of an account goes
/// with it: Antigravity is installed once for all of them.
#[must_use]
pub fn antigravity_install() -> TerminalCommand {
    TerminalCommand::shell_line(ANTIGRAVITY_INSTALL.0, ANTIGRAVITY_INSTALL.1)
}

/// The command that signs the user in to the Antigravity account
/// `connection`: Antigravity's own `agy acp --login`, which shows Google's
/// sign-in. The app only starts it, in a terminal the user sees. `None` when
/// Antigravity is not installed.
#[must_use]
pub fn antigravity_login(connection: &Connection) -> Option<TerminalCommand> {
    let program = sdk::find_cli()?;
    let profile = profile(connection);
    // The folder must exist for Antigravity to keep the sign-in in it.
    if let Some(profile) = &profile {
        let _ = std::fs::create_dir_all(profile);
    }
    let args = sdk::login_args(profile.as_deref()).iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let env = profile.as_deref().map(sdk::profile_env).unwrap_or_default();
    Some(TerminalCommand { program, args, env })
}

/// The folder that selects an Antigravity account: none for Antigravity's
/// own default, and otherwise where it keeps that account's sign-in, settings
/// and conversations. What is in it is Antigravity's business.
fn profile(connection: &Connection) -> Option<PathBuf> {
    Some(connection.config_dir.trim()).filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

/// What Antigravity offers for the account `connection`: its models, how
/// hard each can think, its version and whether it is signed in. What was
/// learned before is said again without starting anything; `fresh` reads it
/// anew, which starts the binary (without sending it a message, so nothing is
/// used) and takes it a while. One account at a time. Call it off the UI
/// thread.
///
/// # Errors
/// The binary is not installed ([`sdk::Error::NotFound`]), could not be
/// started, or did not answer.
pub fn antigravity_info(connection: &Connection, fresh: bool) -> Result<sdk::Info, sdk::Error> {
    /// Held while the binary is asked: several accounts start one Antigravity after another.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    if sdk::find_cli().is_none() {
        return Err(sdk::Error::NotFound);
    }
    let known = Config::dir().ok().map(|dir| dir.join(KNOWN_FILE));
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    if !fresh && let Some(info) = known.as_deref().and_then(|file| known_info(file, &connection.id)) {
        return Ok(info);
    }
    // It leaves folders of its own where it is started: not in a project of the user's.
    let scratch = std::env::temp_dir().join("atroxabot-antigravity");
    let _ = std::fs::create_dir_all(&scratch);
    let options = sdk::Options {
        cwd: Some(scratch),
        profile: profile(connection),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        ..sdk::Options::default()
    };
    let info = sdk::info(&options, INFO_TIMEOUT)?;
    if let Some(file) = &known {
        // An account that is signed out is asked again the next time.
        keep_info(file, &connection.id, Some(&info).filter(|info| info.signed_in));
    }
    Ok(info)
}

/// What is kept of the account `id` in `file`, if anything.
fn known_info(file: &Path, id: &str) -> Option<sdk::Info> {
    let kept: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    info_of(kept.get(id)?)
}

/// Keeps `info` of the account `id` in `file`, or forgets the account with `None`.
fn keep_info(file: &Path, id: &str, info: Option<&sdk::Info>) {
    /// Held while the file is read and written again.
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let read = || serde_json::from_slice::<Map<String, Value>>(&std::fs::read(file).ok()?).ok();
    let mut kept = read().unwrap_or_default();
    match info {
        Some(info) => drop(kept.insert(id.to_owned(), info_json(info))),
        None if kept.remove(id).is_none() => return,
        None => {}
    }
    // Losing it costs one slow start, no more.
    let _ = write_private(file, Value::Object(kept).to_string().as_bytes());
}

/// What an account offers, as it is kept.
fn info_json(info: &sdk::Info) -> Value {
    let levels = |levels: &[sdk::Effort]| -> Vec<Value> {
        levels.iter().map(|level| json!({ "id": level.id, "name": level.name })).collect()
    };
    let models: Vec<Value> = (info.catalog.models.iter())
        .map(|model| json!({ "id": model.id, "name": model.name, "description": model.description }))
        .collect();
    let efforts: Vec<Value> =
        info.efforts.iter().map(|(model, of)| json!({ "model": model, "levels": levels(of) })).collect();
    let commands: Vec<Value> = (info.commands.iter())
        .map(|command| json!({ "name": command.name, "description": command.description, "hint": command.hint }))
        .collect();
    json!({
        "version": info.version,
        "model": info.catalog.model,
        "effort": info.catalog.effort,
        "models": models,
        "efforts": efforts,
        "commands": commands,
    })
}

/// What was kept of an account; `None` for anything but what [`info_json`] writes.
fn info_of(kept: &Value) -> Option<sdk::Info> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let list = |value: &Value, key: &str| value.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let models: Vec<sdk::Model> = list(kept, "models")
        .iter()
        .map(|model| sdk::Model {
            id: text(model, "id"),
            name: text(model, "name"),
            description: text(model, "description"),
        })
        .filter(|model| !model.id.is_empty())
        .collect();
    if models.is_empty() {
        return None;
    }
    let efforts: Vec<(String, Vec<sdk::Effort>)> = list(kept, "efforts")
        .iter()
        .map(|of| {
            let levels = list(of, "levels");
            let levels = levels.iter().map(|level| sdk::Effort { id: text(level, "id"), name: text(level, "name") });
            (text(of, "model"), levels.filter(|level| !level.id.is_empty()).collect())
        })
        .collect();
    let model = text(kept, "model");
    let in_use = efforts.iter().find(|(of, _)| *of == model).map(|(_, levels)| levels.clone()).unwrap_or_default();
    let commands = list(kept, "commands")
        .iter()
        .map(|command| sdk::SlashCommand {
            name: text(command, "name"),
            description: text(command, "description"),
            hint: text(command, "hint"),
        })
        .filter(|command| !command.name.is_empty())
        .collect();
    Some(sdk::Info {
        version: text(kept, "version"),
        signed_in: true,
        catalog: sdk::Catalog { models, model, efforts: in_use, effort: text(kept, "effort") },
        efforts,
        commands,
    })
}

/// What the agent's autonomy settles of what Antigravity asks, without a card.
#[derive(Debug, Default)]
struct Policy {
    autonomy: Autonomy,
    /// The folder the session works in.
    folder: PathBuf,
    /// Where the agent keeps its notes, which are its own to write too.
    memory: Option<PathBuf>,
}

impl Policy {
    /// The answer to a request about `tool`, where the autonomy gives one; `None` asks the user.
    fn settles(&self, tool: &sdk::Tool) -> Option<sdk::Decision> {
        use sdk::ToolKind as Kind;
        // What a server's tool does is that server's word: only the user, or full autonomy, lets it.
        let looks = matches!(tool.kind, Kind::Read | Kind::Search | Kind::Think) && !tool.mcp;
        let edits = matches!(tool.kind, Kind::Edit | Kind::Delete | Kind::Move) && !tool.mcp;
        match self.autonomy {
            Autonomy::Full => Some(sdk::Decision::AcceptForSession),
            _ if looks => Some(sdk::Decision::Accept),
            Autonomy::ReadOnly => Some(sdk::Decision::Decline),
            Autonomy::Edits | Autonomy::Auto if edits && self.within(tool) => Some(sdk::Decision::Accept),
            Autonomy::Ask | Autonomy::Edits | Autonomy::Auto => None,
        }
    }

    /// Whether every file `tool` touches is the agent's own: in its folder, or among its notes.
    fn within(&self, tool: &sdk::Tool) -> bool {
        let changed = tool.diffs.iter().map(|diff| diff.path.as_str());
        let mut touched = changed.chain(tool.locations.iter().map(|(path, _)| path.as_str())).peekable();
        touched.peek().is_some() && touched.all(|path| self.owns(Path::new(path)))
    }

    fn owns(&self, path: &Path) -> bool {
        // One that climbs out of where it starts is not judged here.
        if path.components().any(|part| matches!(part, Component::ParentDir)) {
            return false;
        }
        // Relative to the folder the binary was started in.
        path.is_relative()
            || under(&self.folder, path)
            || self.memory.as_deref().is_some_and(|notes| under(notes, path))
    }
}

/// Whether `path` lies in the folder `root`, as their names say.
fn under(root: &Path, path: &Path) -> bool {
    let same = |a: &OsStr, b: &OsStr| if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b };
    let mut inner = path.components();
    !root.as_os_str().is_empty()
        && root.components().all(|part| inner.next().is_some_and(|other| same(part.as_os_str(), other.as_os_str())))
}

/// What the engine and the app's side of it both know.
#[derive(Debug, Default)]
struct Shared {
    /// The follow-ups handed to Antigravity and not taken in yet, by the id they were sent under.
    follow_ups: HashMap<String, FollowUp>,
    /// The questions Antigravity asked and waits on, by request: each one's words.
    questions: HashMap<String, String>,
    /// A message was sent and its turn has not ended.
    awaiting: bool,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running Antigravity session.
pub struct AntigravityAgent {
    session: Arc<sdk::Session>,
    shared: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for AntigravityAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntigravityAgent").finish_non_exhaustive()
    }
}

impl AntigravityAgent {
    /// Starts Antigravity for `agent` on the account `connection`, in the
    /// folder of `session`, resuming the session's conversation when this
    /// account has it. `mcp` are the app's own MCP servers, which Antigravity
    /// is given for this run. `emit` is called on a reader thread with
    /// everything that happens.
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
        let (prompt, memory) = session.account_prompt(agent);
        let options = options(agent, connection, session, mcp, prompt);
        // What it is told of the agent's notes stays with the session: a later start says the same.
        if let Some(memory) = memory {
            emit(AgentEvent::Memory(memory));
        }
        // It unpacks itself before it answers: the wait is said, until the session is open.
        emit(AgentEvent::Status(STARTING.to_owned()));
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let policy = Policy { autonomy: agent.autonomy, folder: session.folder(), memory: agent.memory_dir() };
        let mut mapper = Mapper {
            asked_resume: options.resume.is_some(),
            account: connection.id.clone(),
            shared: Arc::clone(&shared),
            ..Mapper::default()
        };
        // The reader thread answers what the autonomy settles: it is handed the session once there is one.
        let handle: Arc<OnceLock<Weak<sdk::Session>>> = Arc::default();
        let own = Arc::clone(&handle);
        let session = sdk::Session::connect(&options, move |event| {
            if let sdk::Event::Approval(request) = &event
                && let Some(decision) = policy.settles(&request.tool)
                && let Some(session) = own.get().and_then(Weak::upgrade)
                && session.respond(&request.request_id, decision).is_ok()
            {
                return;
            }
            for event in mapper.map(event) {
                emit(event);
            }
        })?;
        let session = Arc::new(session);
        let _ = handle.set(Arc::downgrade(&session));
        Ok(Self { session, shared })
    }

    /// Sends the user's message with the `files` that go with it (pictures
    /// for the model to look at, the rest named by path); the reply arrives
    /// as events.
    ///
    /// # Errors
    /// The process has ended.
    pub fn send(&self, text: &str, files: &[String]) -> Result<(), sdk::Error> {
        lock(&self.shared).awaiting = true;
        self.session.send(&input(text, files), None)
    }

    /// Sends a message the user wrote while the turn runs: Antigravity puts
    /// it into the turn (reported as an [`AgentEvent::FollowUp`]). One it
    /// does not take in before the turn ends is not reported, and the app
    /// sends it again.
    ///
    /// # Errors
    /// The process has ended.
    pub fn follow_up(&self, message: FollowUp) -> Result<(), sdk::Error> {
        let (id, input) = (message.id.clone(), input(&message.text, &message.files));
        lock(&self.shared).follow_ups.insert(id.clone(), message);
        self.session.steer(&input, &id)
    }

    /// Has it answer with `model` at `effort` from its next turn on (empty
    /// for Antigravity's own choice): it sets both as a turn starts, so
    /// nothing is started again.
    pub fn retune(&self, model: &str, effort: &str) {
        let some = |text: &str| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
        self.session.retune(some(model), some(effort));
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
            (Some(question), Verdict::Answer(given)) => {
                let given = serde_json::from_str::<Value>(&given).unwrap_or(Value::Null);
                let answer = given.get("answers").and_then(|answers| answers.get(&question)).and_then(Value::as_str);
                let taken = self.session.answer(id, answer);
                match (taken, answer) {
                    // Words of the user's own are none of its choices: it is told so, and reads them next.
                    (Err(sdk::Error::UnknownRequest(_)), Some(words)) if !words.trim().is_empty() => {
                        let said = [sdk::Input::Text(words.to_owned())];
                        self.session
                            .answer(id, None)
                            .and_then(|()| self.session.steer(&said, &crate::session::new_id()))
                    }
                    (taken, _) => taken,
                }
            }
            // Dismissed: no answer, and the agent carries on without.
            (Some(_), _) => self.session.answer(id, None),
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

/// A message as Antigravity takes it: the words (with the paths of files
/// that are not pictures), then the pictures.
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

/// The app's MCP servers as Antigravity takes them, for this run only.
fn mcp_servers(servers: &[McpServer]) -> Vec<sdk::McpServer> {
    let server = |server: &McpServer| {
        let name = server.name.trim();
        let transport = if server.url.trim().is_empty() {
            let (command, args) = atroxabot_mcp::split_command(&server.command)?;
            sdk::McpTransport::Stdio { command, args, env: server.pairs() }
        } else {
            sdk::McpTransport::Http { url: server.url.trim().to_owned(), headers: server.pairs() }
        };
        (!name.is_empty()).then(|| sdk::McpServer { name: name.to_owned(), transport })
    };
    servers.iter().filter(|server| server.enabled).filter_map(server).collect()
}

/// How Antigravity is started for `agent` on `connection` in `session`, with `prompt` for the agent's own
/// instructions ([`Session::account_prompt`]).
fn options(
    agent: &Agent,
    connection: &Connection,
    session: &Session,
    mcp: &[McpServer],
    prompt: String,
) -> sdk::Options {
    let some = |text: &str| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
    let profile = profile(connection);
    if let Some(profile) = &profile {
        let _ = std::fs::create_dir_all(profile);
    }
    let folder = session.folder();
    sdk::Options {
        cli_path: None,
        env: Vec::new(),
        cwd: Some(folder).filter(|folder| !folder.as_os_str().is_empty()),
        profile,
        model: some(&agent.model),
        effort: some(&agent.effort),
        // The agent's own instructions, on top of Antigravity's: who it is and how it works here.
        instructions: Some(prompt),
        // Another account keeps its own conversations: only the one that has this one can resume it.
        resume: some(&session.remote).filter(|_| session.account == connection.id),
        mcp_servers: mcp_servers(mcp),
        // The binary is never asked to sign in: that is the user's, in a terminal.
        authenticate: None,
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// A tool call of Antigravity's as the app shows one: the name the app knows the like of by, and its input.
fn call_of(tool: &sdk::Tool) -> (String, Value) {
    use sdk::ToolKind as Kind;
    let given = |key: &str| tool.input.get(key).and_then(Value::as_str).map(str::to_owned);
    let file = || {
        let changed = tool.diffs.first().map(|diff| diff.path.clone());
        changed.or_else(|| tool.locations.first().map(|(path, _)| path.clone())).unwrap_or_default()
    };
    // What it is given says most; where it gives nothing, what it calls itself does.
    let or_given = |shaped: Value| if tool.input.is_object() { tool.input.clone() } else { shaped };
    if tool.mcp {
        return (tool.title.clone(), tool.input.clone());
    }
    match tool.kind {
        Kind::Execute => {
            let command = tool.command.clone().or_else(|| given("command")).unwrap_or_else(|| tool.title.clone());
            ("Bash".to_owned(), json!({ "command": command, "cwd": tool.cwd }))
        }
        Kind::Edit | Kind::Delete | Kind::Move if !tool.diffs.is_empty() => changes(&tool.diffs),
        Kind::Edit => ("Edit".to_owned(), json!({ "file_path": file() })),
        Kind::Delete => ("Delete".to_owned(), json!({ "file_path": file() })),
        Kind::Move => ("Move".to_owned(), json!({ "file_path": file() })),
        Kind::Read => ("Read".to_owned(), json!({ "file_path": file() })),
        Kind::Search => ("Search".to_owned(), or_given(json!({ "pattern": tool.title }))),
        Kind::Fetch => ("WebFetch".to_owned(), or_given(json!({ "url": tool.title }))),
        Kind::Think => ("Think".to_owned(), json!({ "text": tool.title })),
        Kind::Subagent => {
            let prompt = given("prompt").or_else(|| given("task")).unwrap_or_default();
            ("Agent".to_owned(), json!({ "description": tool.title, "prompt": prompt }))
        }
        Kind::Other => (tool.title.clone(), tool.input.clone()),
    }
}

/// The changes to files as the conversation reads them: one file's as what is replaced by what, a new
/// file's as what is written, several files' each under its name.
fn changes(diffs: &[sdk::Diff]) -> (String, Value) {
    let first = diffs.first().map(|diff| diff.path.clone()).unwrap_or_default();
    if diffs.iter().any(|diff| diff.path != first) {
        let file = |diff: &sdk::Diff| {
            let lines = |text: &str, sign: char| {
                text.lines().fold(String::new(), |mut said, line| {
                    said.push(sign);
                    said.push_str(line);
                    said.push('\n');
                    said
                })
            };
            match &diff.old_text {
                None => json!({ "path": diff.path, "kind": "add", "diff": diff.new_text }),
                Some(old) => {
                    let diff_text = format!("@@\n{}{}", lines(old, '-'), lines(&diff.new_text, '+'));
                    json!({ "path": diff.path, "kind": "update", "diff": diff_text })
                }
            }
        };
        let files: Vec<Value> = diffs.iter().map(file).collect();
        return ("Edit".to_owned(), json!({ "file_path": first, "changes": files }));
    }
    if let [sdk::Diff { old_text: None, new_text, .. }] = diffs {
        return ("Write".to_owned(), json!({ "file_path": first, "content": new_text }));
    }
    let edits: Vec<Value> = diffs
        .iter()
        .map(|diff| json!({ "old_string": diff.old_text.clone().unwrap_or_default(), "new_string": diff.new_text }))
        .collect();
    ("Edit".to_owned(), json!({ "file_path": first, "edits": edits }))
}

/// Turns Antigravity's events into the app's, keeping what a turn needs
/// remembered between them.
#[derive(Debug, Default)]
struct Mapper {
    /// Whether a reply is being written.
    replying: bool,
    /// The session asked to continue a conversation Antigravity has.
    asked_resume: bool,
    /// The session is open.
    ready: bool,
    /// The account it runs on, whose kept answers a sign-out makes stale.
    account: String,
    /// The model that answers.
    model: String,
    /// The tokens the model's context holds and how many it takes, as last said.
    context: (u64, u64),
    /// The message the text last written belongs to, while text is the latest thing written.
    text_of: Option<String>,
    /// The calls reported so far, and those of them answered.
    calls: HashSet<String>,
    answered: HashSet<String>,
    /// How many plans the turn made so far.
    plans: u64,
    /// The binary said it is not signed in.
    signed_out: bool,
    shared: Arc<Mutex<Shared>>,
}

impl Mapper {
    /// Starts the turn's reply if nothing did yet. The whole turn is one
    /// reply: Antigravity keeps the conversation, so nothing is replayed from it.
    fn begin(&mut self, out: &mut Vec<AgentEvent>) {
        if !self.replying {
            self.replying = true;
            out.push(AgentEvent::ReplyStarted);
        }
    }

    /// What is known of a tool call. It is reported once it runs (an edit
    /// once it says what it changes), is asked about (`asked`), or is done;
    /// and answered when it is done.
    fn tool(&mut self, tool: &sdk::Tool, asked: bool, out: &mut Vec<AgentEvent>) {
        use sdk::ToolKind as Kind;
        let done = tool.status.is_final();
        let edit = matches!(tool.kind, Kind::Edit | Kind::Delete | Kind::Move) && !tool.mcp;
        let runs = tool.status != sdk::ToolStatus::Pending && (!edit || !tool.diffs.is_empty());
        if !self.calls.contains(&tool.id) {
            if !(runs || asked || done) {
                return;
            }
            self.begin(out);
            self.text_of = None;
            self.calls.insert(tool.id.clone());
            let (name, input) = call_of(tool);
            out.push(AgentEvent::ToolCall(ToolUse {
                id: tool.id.clone(),
                name,
                input: input.to_string(),
                ..ToolUse::default()
            }));
        }
        if done && self.answered.insert(tool.id.clone()) {
            let failed = tool.status == sdk::ToolStatus::Failed || tool.exit_code.is_some_and(|code| code != 0);
            let output = tool.output.clone().unwrap_or_default();
            out.push(AgentEvent::ToolResult { id: tool.id.clone(), output, is_error: failed });
        }
    }

    /// Ends the turn: its reply, what it used, and why it failed if it did.
    fn end(&mut self, problem: Option<String>, usage: Option<sdk::TokenUsage>, out: &mut Vec<AgentEvent>) {
        self.begin(out);
        let spent = usage.unwrap_or_default();
        let (held, size) = self.context;
        // What the context holds is what the next request reads: the turn's own count where it says none.
        let usage = Usage {
            input_tokens: if held > 0 { held } else { spent.input },
            output_tokens: spent.output,
            cached_tokens: spent.cached_input,
            cache_write_tokens: spent.cache_write_input,
        };
        out.push(AgentEvent::ReplyEnded {
            wire: crate::Wire::Messages,
            raw: Vec::new(),
            model: self.model.clone(),
            usage,
            cost: 0.0,
            problem: problem.clone().unwrap_or_default(),
        });
        if size > 0 {
            out.push(AgentEvent::Window(size));
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
        self.calls.clear();
        self.answered.clear();
    }

    fn map(&mut self, event: sdk::Event) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match event {
            sdk::Event::Ready { session_id, resumed, catalog, .. } => {
                self.ready = true;
                self.model = catalog.model;
                out.push(AgentEvent::Remote(session_id));
                out.push(AgentEvent::Status(String::new()));
                if self.asked_resume && !resumed {
                    let note = "Antigravity no longer has the earlier conversation: it starts over from here.";
                    out.push(AgentEvent::Note(note.to_owned()));
                }
            }
            sdk::Event::Catalog(catalog) => self.model = catalog.model,
            sdk::Event::Commands(commands) => {
                let line = |text: &str| text.lines().next().unwrap_or_default().chars().take(160).collect::<String>();
                let commands = commands.into_iter().map(|command| (command.name, line(&command.description)));
                out.push(AgentEvent::Commands(commands.collect()));
            }
            sdk::Event::TurnStarted => {
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
            sdk::Event::TextDelta { message_id, text } => {
                self.begin(&mut out);
                // A new message starts a new paragraph.
                let of = message_id.unwrap_or_default();
                if self.text_of.as_deref().is_some_and(|last| last != of) {
                    out.push(AgentEvent::Text("\n\n".to_owned()));
                }
                self.text_of = Some(of);
                out.push(AgentEvent::Text(text));
            }
            sdk::Event::ReasoningDelta { text } => {
                self.begin(&mut out);
                self.text_of = None;
                out.push(AgentEvent::Thinking(text));
            }
            sdk::Event::Tool(tool) => self.tool(&tool, false, &mut out),
            // The agent's plan is its task list: the app reads it from a call of that name.
            sdk::Event::Plan { steps } => {
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
                self.begin(&mut out);
                self.text_of = None;
                self.plans += 1;
                let id = format!("plan-{}-{}", crate::session::new_id(), self.plans);
                let input = json!({ "todos": todos }).to_string();
                out.push(AgentEvent::ToolCall(ToolUse {
                    id: id.clone(),
                    name: "TodoWrite".to_owned(),
                    input,
                    ..ToolUse::default()
                }));
                out.push(AgentEvent::ToolResult { id, output: "The plan was updated.".to_owned(), is_error: false });
            }
            sdk::Event::Context { used, size } => self.context = (used, size),
            sdk::Event::Approval(request) => {
                self.tool(&request.tool, true, &mut out);
                let (tool, input) = call_of(&request.tool);
                out.push(AgentEvent::Permission(Ask {
                    id: request.request_id,
                    summary: crate::tools::summary(&input),
                    tool,
                    input: input.to_string(),
                    reason: request.warning.unwrap_or_default(),
                    call: request.tool.id,
                    always: request.can_remember,
                    ..Ask::default()
                }));
            }
            // A question, in the shape the app's question card reads.
            sdk::Event::Question(request) => {
                let options: Vec<Value> =
                    (request.options.iter()).map(|(_, text)| json!({ "label": text, "description": "" })).collect();
                let question =
                    json!({ "question": request.question, "header": "", "options": options, "multiSelect": false });
                lock(&self.shared).questions.insert(request.request_id.clone(), request.question.clone());
                self.begin(&mut out);
                out.push(AgentEvent::Permission(Ask {
                    id: request.request_id,
                    tool: "AskUserQuestion".to_owned(),
                    input: json!({ "questions": [question] }).to_string(),
                    summary: request.question,
                    ..Ask::default()
                }));
            }
            sdk::Event::RequestResolved { request_id } => {
                lock(&self.shared).questions.remove(&request_id);
                out.push(AgentEvent::PermissionGone(request_id));
            }
            sdk::Event::SignInRequired => {
                self.signed_out = true;
                // What was kept of the account says it is signed in: it is asked anew the next time.
                if let Ok(dir) = Config::dir()
                    && !self.account.is_empty()
                {
                    keep_info(&dir.join(KNOWN_FILE), &self.account, None);
                }
            }
            // That a conversation could not be continued is said when the session opens, in the app's words.
            sdk::Event::Warning(text) if !self.ready && self.asked_resume && text.contains("continue") => {}
            sdk::Event::Warning(text) => out.push(AgentEvent::Note(sentence(&text))),
            sdk::Event::TurnCompleted { status, stop_reason, error, usage } => {
                let problem = match status {
                    // Being stopped is what was asked for.
                    sdk::TurnStatus::Completed | sdk::TurnStatus::Interrupted => None,
                    sdk::TurnStatus::Failed => {
                        // The only way Antigravity says an account ran out: the app waits for it or goes on elsewhere.
                        if stop_reason == "refusal" {
                            out.push(AgentEvent::LimitReached);
                        }
                        let said = error.filter(|said| !said.trim().is_empty());
                        Some(said.map_or_else(
                            || "The turn failed.".to_owned(),
                            |said| if said.contains("not signed in") { SIGN_IN.to_owned() } else { sentence(&said) },
                        ))
                    }
                };
                self.end(problem, usage, &mut out);
            }
            sdk::Event::Closed { exit_code, error } => {
                let why = if self.signed_out {
                    Some(SIGN_IN.to_owned())
                } else {
                    error.or_else(|| exit_code.filter(|code| *code != 0).map(|code| format!("exit code {code}")))
                };
                // A message it never answered still gets an end, and a reason.
                if self.replying || lock(&self.shared).awaiting {
                    let problem = match &why {
                        Some(why) if self.signed_out => why.clone(),
                        Some(why) => format!("Antigravity stopped unexpectedly: {why}."),
                        None => "Antigravity stopped unexpectedly.".to_owned(),
                    };
                    self.end(Some(problem), None, &mut out);
                }
                out.push(AgentEvent::Disconnected(why));
            }
            // Its own prompt coming back, and what the binary writes to its standard error.
            sdk::Event::UserMessage { .. } | sdk::Event::Stderr(_) => {}
        }
        out
    }
}

/// `text` as a sentence of the app's: it begins with a capital and ends with a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim();
    let mut letters = text.chars();
    let Some(first) = letters.next() else { return String::new() };
    let mut said: String = first.to_uppercase().chain(letters).collect();
    if !said.ends_with(['.', '!', '?']) {
        said.push('.');
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asks the Antigravity installed here what the user's own account offers, and prints it. It sends
    /// no message, so nothing of the account's quota is used; it starts the binary, which takes a while.
    #[test]
    #[ignore = "live: needs the Antigravity CLI installed (and, to list models, signed in)"]
    fn live_antigravity_says_what_it_offers() {
        let scratch = std::env::temp_dir().join("atroxabot-antigravity");
        std::fs::create_dir_all(&scratch).unwrap();
        let options = sdk::Options { cwd: Some(scratch), ..sdk::Options::default() };
        let started = std::time::Instant::now();
        let info = sdk::info(&options, INFO_TIMEOUT).expect("Antigravity answers");
        eprintln!("answered in {:.1?}: version {}, signed in: {}", started.elapsed(), info.version, info.signed_in);
        eprintln!("model in use: {} at {}", info.catalog.model, info.catalog.effort);
        for model in &info.catalog.models {
            let levels = info.efforts.iter().find(|(of, _)| *of == model.id).map(|(_, levels)| levels.as_slice());
            let levels: Vec<&str> = levels.unwrap_or_default().iter().map(|level| level.id.as_str()).collect();
            eprintln!("  {} ({}): {levels:?}", model.id, model.name);
        }
        let commands: Vec<&str> = info.commands.iter().map(|command| command.name.as_str()).collect();
        eprintln!("commands: {commands:?}");
        assert!(!info.version.is_empty(), "the handshake names a version");
        assert!(!info.signed_in || !info.catalog.models.is_empty(), "a signed-in account has models");
    }

    fn tool(id: &str, kind: sdk::ToolKind, status: sdk::ToolStatus) -> sdk::Tool {
        sdk::Tool { id: id.into(), title: "Running a tool".into(), kind, status, ..sdk::Tool::default() }
    }

    fn completed(status: sdk::TurnStatus, stop_reason: &str, error: Option<&str>) -> sdk::Event {
        sdk::Event::TurnCompleted {
            status,
            stop_reason: stop_reason.into(),
            error: error.map(str::to_owned),
            usage: Some(sdk::TokenUsage { input: 900, cached_input: 700, output: 40, ..sdk::TokenUsage::default() }),
        }
    }

    #[test]
    fn a_turn_is_one_reply_with_its_calls_and_what_it_used() {
        let mut mapper = Mapper::default();
        let catalog = sdk::Catalog { model: "gemini-3-pro".into(), ..sdk::Catalog::default() };
        let ready = sdk::Event::Ready { session_id: "s1".into(), resumed: false, version: "1.5.12".into(), catalog };
        assert_eq!(mapper.map(ready), [AgentEvent::Remote("s1".into()), AgentEvent::Status(String::new())]);
        assert_eq!(mapper.map(sdk::Event::TurnStarted), [AgentEvent::Working, AgentEvent::ReplyStarted]);
        // Its own prompt comes back, and is not a follow-up.
        assert!(mapper.map(sdk::Event::UserMessage { client_id: None, text: "Run the tests".into() }).is_empty());
        let text = |of: &str, text: &str| sdk::Event::TextDelta { message_id: Some(of.into()), text: text.into() };
        assert_eq!(mapper.map(text("m1", "I'll run ")), [AgentEvent::Text("I'll run ".into())]);
        assert_eq!(mapper.map(text("m1", "them.")), [AgentEvent::Text("them.".into())]);

        // A command: nothing while its input still streams, a call once it runs, its answer when it ends.
        let mut run = tool("c1", sdk::ToolKind::Execute, sdk::ToolStatus::Pending);
        assert!(mapper.map(sdk::Event::Tool(run.clone())).is_empty());
        (run.status, run.command) = (sdk::ToolStatus::InProgress, Some("cargo test".into()));
        let started = mapper.map(sdk::Event::Tool(run.clone()));
        let [AgentEvent::ToolCall(call)] = &started[..] else { panic!("a call: {started:?}") };
        assert_eq!((call.id.as_str(), call.name.as_str()), ("c1", "Bash"));
        assert!(call.input.contains("cargo test"));
        assert!(mapper.map(sdk::Event::Tool(run.clone())).is_empty(), "said once");
        (run.status, run.output, run.exit_code) = (sdk::ToolStatus::Completed, Some("ok\n".into()), Some(0));
        assert_eq!(
            mapper.map(sdk::Event::Tool(run.clone())),
            [AgentEvent::ToolResult { id: "c1".into(), output: "ok\n".into(), is_error: false }]
        );
        assert!(mapper.map(sdk::Event::Tool(run)).is_empty(), "answered once");
        // One that is only heard of as it ends is a call and its answer; a command that failed says so.
        let failed = sdk::Tool { exit_code: Some(1), ..tool("c2", sdk::ToolKind::Execute, sdk::ToolStatus::Completed) };
        let failed = mapper.map(sdk::Event::Tool(failed));
        assert!(matches!(&failed[..], [AgentEvent::ToolCall(_), AgentEvent::ToolResult { is_error: true, .. }]));

        // After a call the next message needs no break; another message after text does.
        assert_eq!(mapper.map(text("m2", "All")), [AgentEvent::Text("All".into())]);
        assert_eq!(
            mapper.map(text("m3", "Done.")),
            [AgentEvent::Text("\n\n".into()), AgentEvent::Text("Done.".into())]
        );

        mapper.map(sdk::Event::Context { used: 12_000, size: 1_000_000 });
        let ended = mapper.map(completed(sdk::TurnStatus::Completed, "end_turn", None));
        let AgentEvent::ReplyEnded { usage, model, problem, .. } = &ended[0] else { panic!("the reply ends") };
        assert_eq!((usage.input_tokens, usage.cached_tokens, usage.output_tokens), (12_000, 700, 40));
        assert!(model == "gemini-3-pro" && problem.is_empty());
        assert_eq!(ended[1], AgentEvent::Window(1_000_000));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: None }));
        // The next turn starts a new reply; being stopped is no failure.
        assert_eq!(mapper.map(sdk::Event::TurnStarted)[1], AgentEvent::ReplyStarted);
        let ended = mapper.map(completed(sdk::TurnStatus::Interrupted, "cancelled", None));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: None }));
    }

    #[test]
    fn edits_plans_approvals_and_questions_are_mapped() {
        let mut mapper = Mapper::default();
        mapper.map(sdk::Event::TurnStarted);
        // An edit waits until it says what it changes, so that the conversation can show it.
        let mut edit = tool("e1", sdk::ToolKind::Edit, sdk::ToolStatus::InProgress);
        assert!(mapper.map(sdk::Event::Tool(edit.clone())).is_empty());
        edit.diffs = vec![sdk::Diff { path: "src/a.rs".into(), old_text: Some("a".into()), new_text: "b".into() }];
        let started = mapper.map(sdk::Event::Tool(edit));
        let [AgentEvent::ToolCall(call)] = &started[..] else { panic!("a call: {started:?}") };
        assert_eq!(call.name, "Edit");
        let input: Value = serde_json::from_str(&call.input).unwrap();
        assert_eq!(crate::tools::summary(&input), "src/a.rs");
        assert_eq!(input["edits"][0], json!({ "old_string": "a", "new_string": "b" }));
        // A new file is written whole; several files go each under its name.
        let new = sdk::Diff { path: "b.md".into(), old_text: None, new_text: "# B\n".into() };
        assert_eq!(
            changes(std::slice::from_ref(&new)),
            ("Write".into(), json!({ "file_path": "b.md", "content": "# B\n" }))
        );
        let other = sdk::Diff { path: "c.md".into(), old_text: Some("x\ny".into()), new_text: "z".into() };
        let (name, both) = changes(&[new, other]);
        assert_eq!(name, "Edit");
        assert_eq!(both["changes"][0], json!({ "path": "b.md", "kind": "add", "diff": "# B\n" }));
        assert_eq!(both["changes"][1], json!({ "path": "c.md", "kind": "update", "diff": "@@\n-x\n-y\n+z\n" }));

        // What is asked about shows as a call at once, and as a card that names it.
        let mut run = tool("c1", sdk::ToolKind::Execute, sdk::ToolStatus::Pending);
        run.command = Some("rm -rf target".into());
        assert!(mapper.map(sdk::Event::Tool(run.clone())).is_empty());
        let request = sdk::ApprovalRequest {
            request_id: "7".into(),
            tool: run,
            can_remember: true,
            warning: Some("It will not ask again".into()),
        };
        let asked = mapper.map(sdk::Event::Approval(request));
        let [AgentEvent::ToolCall(call), AgentEvent::Permission(ask)] = &asked[..] else { panic!("asked: {asked:?}") };
        assert_eq!((call.id.as_str(), ask.call.as_str(), ask.tool.as_str()), ("c1", "c1", "Bash"));
        assert_eq!((ask.id.as_str(), ask.summary.as_str(), ask.always), ("7", "rm -rf target", true));
        assert_eq!(
            mapper.map(sdk::Event::RequestResolved { request_id: "7".into() }),
            [AgentEvent::PermissionGone("7".into())]
        );

        // The plan is the task list the side panel shows.
        let step = |step: &str, status| sdk::PlanStep { step: step.into(), status };
        let plan = sdk::Event::Plan {
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

        let request = sdk::QuestionRequest {
            request_id: "9".into(),
            question: "Which format?".into(),
            options: vec![("o1".into(), "JSON".into()), ("o2".into(), "YAML".into())],
        };
        let asked = mapper.map(sdk::Event::Question(request));
        let [AgentEvent::Permission(ask)] = &asked[..] else { panic!("a question") };
        assert_eq!((ask.tool.as_str(), ask.summary.as_str()), ("AskUserQuestion", "Which format?"));
        let input: Value = serde_json::from_str(&ask.input).unwrap();
        assert_eq!(input["questions"][0]["options"][1]["label"], "YAML");
        assert_eq!(lock(&mapper.shared).questions["9"], "Which format?");
    }

    #[test]
    fn follow_ups_failures_and_the_end_of_the_process() {
        let mut mapper = Mapper { asked_resume: true, ..Mapper::default() };
        // A conversation Antigravity lost is said once, in the app's words.
        assert!(mapper.map(sdk::Event::Warning("the conversation could not be continued (gone)".into())).is_empty());
        let ready = sdk::Event::Ready {
            session_id: "s2".into(),
            resumed: false,
            version: String::new(),
            catalog: sdk::Catalog::default(),
        };
        assert!(
            matches!(&mapper.map(ready)[..], [AgentEvent::Remote(_), AgentEvent::Status(_), AgentEvent::Note(note)] if note.contains("starts over"))
        );
        assert_eq!(
            mapper.map(sdk::Event::Warning("Antigravity did not take the model x: no".into())),
            [AgentEvent::Note("Antigravity did not take the model x: no.".into())]
        );

        mapper.map(sdk::Event::TurnStarted);
        let follow_up = FollowUp { id: "f1".into(), text: "Also the docs".into(), files: Vec::new() };
        lock(&mapper.shared).follow_ups.insert("f1".into(), follow_up);
        let taken = mapper.map(sdk::Event::UserMessage { client_id: Some("f1".into()), text: "Also the docs".into() });
        assert!(matches!(&taken[..], [AgentEvent::FollowUp { id, text, .. }] if id == "f1" && text == "Also the docs"));

        // Out of quota: the app is told, to wait for it or go on elsewhere.
        lock(&mapper.shared).follow_ups.insert("f2".into(), FollowUp::default());
        let spent = "the account's Antigravity quota for this period is used up";
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, "refusal", Some(spent)));
        assert_eq!(ended[0], AgentEvent::LimitReached);
        let problem = "The account's Antigravity quota for this period is used up.";
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some(problem.into()) }));
        assert!(lock(&mapper.shared).follow_ups.is_empty(), "what was not taken in goes again from the app");
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, "", Some("Antigravity is not signed in")));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some(SIGN_IN.into()) }));
        let ended = mapper.map(completed(sdk::TurnStatus::Failed, "", None));
        assert_eq!(ended.last(), Some(&AgentEvent::TurnEnded { problem: Some("The turn failed.".into()) }));

        // A message the process never answered still gets an end, and a reason.
        lock(&mapper.shared).awaiting = true;
        let closed = mapper.map(sdk::Event::Closed { exit_code: Some(3), error: None });
        let problem = "Antigravity stopped unexpectedly: exit code 3.";
        assert!(closed.contains(&AgentEvent::TurnEnded { problem: Some(problem.into()) }));
        assert_eq!(closed.last(), Some(&AgentEvent::Disconnected(Some("exit code 3".into()))));
        let closed = mapper.map(sdk::Event::Closed { exit_code: Some(0), error: None });
        assert_eq!(closed, [AgentEvent::Disconnected(None)]);
    }

    #[test]
    fn the_autonomy_settles_what_it_can() {
        let folder = std::env::temp_dir().join("atroxabot-agy-work");
        let inside = folder.join("src").join("a.rs").display().to_string();
        let outside = std::env::temp_dir().join("elsewhere").join("a.rs").display().to_string();
        let policy = |autonomy| Policy { autonomy, folder: folder.clone(), memory: None };
        let edit = |path: &str| sdk::Tool {
            diffs: vec![sdk::Diff { path: path.into(), old_text: None, new_text: "x".into() }],
            ..tool("e", sdk::ToolKind::Edit, sdk::ToolStatus::Pending)
        };
        let run = tool("c", sdk::ToolKind::Execute, sdk::ToolStatus::Pending);
        let read = tool("r", sdk::ToolKind::Read, sdk::ToolStatus::Pending);
        let (accept, decline) = (Some(sdk::Decision::Accept), Some(sdk::Decision::Decline));

        // Asking first: looking is free, the rest is the user's.
        let ask = policy(Autonomy::Ask);
        assert_eq!((ask.settles(&read), ask.settles(&edit(&inside)), ask.settles(&run)), (accept, None, None));
        // Editing freely: its own folder only, and never a command.
        for level in [Autonomy::Edits, Autonomy::Auto] {
            let edits = policy(level);
            assert_eq!(edits.settles(&edit(&inside)), accept);
            assert_eq!(edits.settles(&edit("src/a.rs")), accept, "relative to its folder");
            assert_eq!(edits.settles(&edit(&outside)), None);
            assert_eq!(edits.settles(&edit("../a.rs")), None);
            assert_eq!(edits.settles(&tool("e", sdk::ToolKind::Edit, sdk::ToolStatus::Pending)), None, "names no file");
            assert_eq!(edits.settles(&run), None);
        }
        // Its notes are its own too.
        let notes = std::env::temp_dir().join("atroxabot-agy-notes");
        let keeps = Policy { autonomy: Autonomy::Edits, folder: folder.clone(), memory: Some(notes.clone()) };
        assert_eq!(keeps.settles(&edit(&notes.join("notes.md").display().to_string())), accept);
        // Looking only: whatever else is asked is refused. Full autonomy: nothing is asked twice.
        let looks = policy(Autonomy::ReadOnly);
        assert_eq!(
            (looks.settles(&read), looks.settles(&edit(&inside)), looks.settles(&run)),
            (accept, decline, decline)
        );
        assert_eq!(policy(Autonomy::Full).settles(&run), Some(sdk::Decision::AcceptForSession));
        // A server's tool is never taken at its word.
        let served = sdk::Tool { mcp: true, ..read };
        assert_eq!((ask.settles(&served), looks.settles(&served)), (None, decline));
    }

    #[test]
    fn what_an_account_offers_is_kept_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("atroxabot-agy-known-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(KNOWN_FILE);
        let level = |id: &str| sdk::Effort { id: id.into(), name: id.to_uppercase() };
        let info = sdk::Info {
            version: "1.5.12".into(),
            signed_in: true,
            catalog: sdk::Catalog {
                models: vec![
                    sdk::Model { id: "pro".into(), name: "Pro".into(), description: "The large one".into() },
                    sdk::Model { id: "flash".into(), name: "Flash".into(), description: String::new() },
                ],
                model: "pro".into(),
                efforts: vec![level("low"), level("high")],
                effort: "high".into(),
            },
            efforts: vec![("pro".into(), vec![level("low"), level("high")]), ("flash".into(), vec![level("low")])],
            commands: vec![sdk::SlashCommand {
                name: "compact".into(),
                description: "Summarise".into(),
                hint: String::new(),
            }],
        };
        assert_eq!(known_info(&file, "a"), None);
        keep_info(&file, "a", Some(&info));
        keep_info(&file, "b", Some(&info));
        assert_eq!(known_info(&file, "a"), Some(info.clone()));
        keep_info(&file, "a", None);
        assert_eq!((known_info(&file, "a"), known_info(&file, "b")), (None, Some(info)));
        // A file that says something else is no answer.
        std::fs::write(&file, r#"{"b": {"models": "many"}, "c": 3}"#).unwrap();
        assert_eq!((known_info(&file, "b"), known_info(&file, "c")), (None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn servers_and_the_installer_are_given_as_they_are() {
        let server = |name: &str, command: &str, url: &str, enabled| McpServer {
            name: name.into(),
            command: command.into(),
            url: url.into(),
            vars: "TOKEN=abc".into(),
            enabled,
        };
        let given = mcp_servers(&[
            server("files", "npx -y files-server", "", true),
            server("remote", "", "https://mcp.example.com", true),
            server("off", "x", "", false),
            server("", "x", "", true),
        ]);
        let pairs = vec![("TOKEN".to_owned(), "abc".to_owned())];
        let stdio = sdk::McpTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "files-server".into()],
            env: pairs.clone(),
        };
        let http = sdk::McpTransport::Http { url: "https://mcp.example.com".into(), headers: pairs };
        assert_eq!(
            given,
            [
                sdk::McpServer { name: "files".into(), transport: stdio },
                sdk::McpServer { name: "remote".into(), transport: http }
            ]
        );
        let installer = antigravity_install();
        assert!(installer.env.is_empty(), "installed once for every account");
        if cfg!(windows) {
            assert_eq!(installer.args[2], "irm https://antigravity.google/cli/install.ps1 | iex");
        } else {
            assert_eq!(installer.args[1], "curl -fsSL https://antigravity.google/cli/install.sh | bash");
        }
        assert_eq!(sentence("the reply was cut off"), "The reply was cut off.");
    }
}
