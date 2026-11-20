//! The session's side of the protocol as a state machine: lines of the binary and commands of the caller go
//! in, events and lines to write come out. It does no I/O, so tests drive it directly.

use std::collections::HashMap;
use std::mem;

use serde_json::{Value, json};

use crate::parse::{self, Abilities, Asked, Choice, Incoming, Refusal, Settings, field, opt_text, text};
use crate::{Decision, Error, Event, McpTransport, Options, TokenUsage, Tool, TurnStatus};

/// The name this client gives itself.
const CLIENT_NAME: &str = "atroxabot";
/// The client's name for display.
const CLIENT_TITLE: &str = "AtroxaBot";
/// The version of the Agent Client Protocol spoken here.
const PROTOCOL: u64 = 1;
/// JSON-RPC's code for a method the receiver does not have.
const METHOD_NOT_FOUND: i64 = -32601;
/// The mode a session works in when the one chosen is not on offer: the binary asks before it acts.
const DEFAULT_MODE: &str = "default";
/// What a session that needs a sign-in ends with.
pub(crate) const NOT_SIGNED_IN: &str = "Antigravity is not signed in";
/// What comes before and after what was said in a conversation the binary could not continue.
const CATCH_UP: &str = "(You are taking over a conversation. What was said so far:)";
const CATCH_UP_END: &str = "(Now the user says:)";

/// One piece of a message, read and ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    /// Text.
    Text(String),
    /// A picture: its media type, its bytes in base64 and the file it came from.
    Image {
        /// E.g. `image/png`.
        mime: &'static str,
        /// The file's bytes, in base64.
        data: String,
        /// The file, named instead where the binary takes no pictures.
        path: String,
    },
}

/// What the user sends, and the id the caller knows it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    /// Its pieces, in order.
    pub parts: Vec<Part>,
    /// Comes back in [`Event::UserMessage`].
    pub client_id: Option<String>,
}

impl Message {
    /// The message's text, without its pictures.
    fn text(&self) -> String {
        let texts = self.parts.iter().filter_map(|part| match part {
            Part::Text(text) => Some(text.as_str()),
            Part::Image { .. } => None,
        });
        texts.collect::<Vec<_>>().join("\n")
    }

    /// The message as the protocol's content blocks.
    fn blocks(&self, pictures: bool) -> Value {
        let block = |part: &Part| match part {
            Part::Text(text) => json!({"type": "text", "text": text}),
            Part::Image { mime, data, .. } if pictures => json!({"type": "image", "data": data, "mimeType": mime}),
            Part::Image { path, .. } => json!({"type": "text", "text": format!("[picture: {path}]")}),
        };
        self.parts.iter().map(block).collect()
    }
}

/// Something the caller wants done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Give the agent this message.
    Send(Message),
    /// Give the agent this message in the turn that runs, or not at all.
    Steer(Message),
    /// Stop the running turn.
    Interrupt,
}

/// What came of a line of the binary or a command of the caller.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Output {
    /// For the caller, in order.
    pub events: Vec<Event>,
    /// For the binary, in order.
    pub lines: Vec<Value>,
    /// The session cannot go on: the process is to be ended and this reported as its end.
    pub fatal: Option<String>,
}

/// What a request of ours waits for.
#[derive(Debug)]
enum Awaiting {
    /// The answer to `initialize`.
    Initialize,
    /// The answer to `authenticate`.
    Authenticate,
    /// The answer to `session/new`, or to `session/resume` or `session/load` when `resume` is set.
    Open {
        /// Whether it continues a session.
        resume: bool,
    },
    /// The answer to setting this model.
    Model(String),
    /// The answer to setting this level of thinking.
    Effort(String),
    /// The answer to setting this mode.
    Mode(String),
    /// The answer to `session/prompt`: the agent is done with that message.
    Prompt,
}

/// A request of the binary the caller has not answered yet.
#[derive(Debug)]
struct Pending {
    /// The id to answer under.
    id: Value,
    /// The answers on offer.
    choices: Vec<Choice>,
    /// Whether it is a question to the user, not a tool call to approve.
    question: bool,
}

/// The session's state.
#[derive(Debug)]
pub(crate) struct Machine {
    /// What the session is opened and run with.
    options: Options,
    /// The folder the session works in.
    cwd: String,
    next_id: u64,
    /// Our requests not yet answered.
    awaiting: HashMap<u64, Awaiting>,
    /// What the handshake said the binary can do.
    can: Abilities,
    /// The session, once it is open.
    session: Option<String>,
    /// The handshake failed or the process ended: nothing is sent any more.
    closed: bool,
    /// The models and levels on offer, and how they are set.
    settings: Settings,
    /// The model last set here; what the session opened with until then.
    model: String,
    /// A request that sets the model or the level is on its way: messages wait for it.
    tuning: bool,
    /// The models and levels the binary would not take. They are not asked for again until the caller chooses
    /// anew.
    refused: Vec<String>,
    /// The modes the binary would not take: not asked for again.
    no_mode: Vec<String>,
    /// A sign-in was asked of the binary already.
    authenticated: bool,
    /// A session is being continued by having its history told again, which is not reported.
    loading: bool,
    /// The conversation to continue could not be: the next message says what was said in it.
    catching_up: bool,
    /// A turn runs.
    turn: bool,
    /// The messages the agent has not answered yet. A turn with none is one the agent began by itself.
    prompts: usize,
    /// The running turn is being stopped.
    stopping: bool,
    /// Why the agent stopped, as the last answer to a message said.
    stop_reason: String,
    /// Why the turn failed.
    failure: Option<String>,
    /// The tokens the turn took so far.
    usage: Option<TokenUsage>,
    /// What waits: anything until the session is open, then messages (for a model being set, or for the end
    /// of a turn that is being stopped).
    held: Vec<Command>,
    /// The binary's requests the caller has not answered yet, by their key.
    asked: HashMap<String, Pending>,
    /// The tool calls known, by id: an update says only what changed.
    tools: HashMap<String, Tool>,
}

impl Machine {
    pub(crate) fn new(options: &Options, cwd: String) -> Self {
        Self {
            options: options.clone(),
            cwd,
            next_id: 1,
            awaiting: HashMap::new(),
            can: Abilities::default(),
            session: None,
            closed: false,
            settings: Settings::default(),
            model: String::new(),
            tuning: false,
            refused: Vec::new(),
            no_mode: Vec::new(),
            authenticated: false,
            loading: false,
            catching_up: false,
            turn: false,
            prompts: 0,
            stopping: false,
            stop_reason: String::new(),
            failure: None,
            usage: None,
            held: Vec::new(),
            asked: HashMap::new(),
            tools: HashMap::new(),
        }
    }

    /// The `initialize` request that opens the conversation with the binary.
    pub(crate) fn hello(&mut self) -> Value {
        let params = initialize_params(&self.options);
        self.request("initialize", params, Awaiting::Initialize)
    }

    /// Takes a line the binary wrote.
    pub(crate) fn receive(&mut self, line: &str) -> Output {
        let mut out = Output::default();
        if self.closed {
            return out;
        }
        match parse::parse_line(line) {
            Incoming::Nothing => {}
            // Asked to sign in, it shows Google's page and answers once the user is through with it.
            Incoming::SignInLink if self.awaiting.values().any(|asked| matches!(asked, Awaiting::Authenticate)) => {
                out.events.push(Event::SigningIn);
            }
            Incoming::SignInLink => self.signed_out(&mut out),
            Incoming::Response { id, outcome } => {
                if let Some(awaiting) = self.awaiting.remove(&id) {
                    self.answered(awaiting, outcome, &mut out);
                }
            }
            Incoming::Notification { method, params } => {
                if method == "session/update" {
                    self.updated(&params, &mut out);
                }
            }
            Incoming::Request { id, method, params } => self.requested(id, &method, &params, &mut out),
        }
        out
    }

    /// The binary waits for a sign-in in a browser, as a line outside the protocol said.
    pub(crate) fn signed_out(&mut self, out: &mut Output) {
        if !self.closed {
            self.closed = true;
            out.events.push(Event::SignInRequired);
            out.fatal = Some(NOT_SIGNED_IN.to_owned());
        }
    }

    /// The messages sent from now on are answered by `model` at `effort`; `None` leaves that as it is.
    pub(crate) fn retune(&mut self, model: Option<String>, effort: Option<String>) {
        (self.options.model, self.options.effort) = (model, effort);
        self.refused.clear();
    }

    /// Takes a command of the caller's: what to write for it and what to report, both nothing while it has to
    /// wait.
    pub(crate) fn command(&mut self, command: Command) -> Output {
        let mut out = Output::default();
        if self.closed {
            return out;
        }
        // Behind what already waits, so that messages keep their order.
        self.held.push(command);
        self.flush(&mut out);
        out
    }

    /// Answers an approval request with `decision`.
    pub(crate) fn respond(&mut self, request_id: &str, decision: Decision) -> Result<Value, Error> {
        let unknown = || Error::UnknownRequest(request_id.to_owned());
        let pending = self.asked.get(request_id).filter(|pending| !pending.question).ok_or_else(unknown)?;
        // The nearest the binary offers: it may have no way to allow, or to refuse, just once.
        let kinds: &[&str] = match decision {
            Decision::Accept => &["allow_once", "allow_always"],
            Decision::AcceptForSession => &["allow_always", "allow_once"],
            Decision::Decline => &["reject_once", "reject_always"],
            Decision::Cancel => &[],
        };
        let chosen = kinds.iter().find_map(|kind| pending.choices.iter().find(|choice| choice.kind == *kind));
        let line = outcome_line(&pending.id, chosen.map(|choice| choice.id.as_str()));
        self.asked.remove(request_id);
        Ok(line)
    }

    /// Answers a question with one of its choices, named by its id or by its text; `None` leaves it unanswered.
    pub(crate) fn answer(&mut self, request_id: &str, choice: Option<&str>) -> Result<Value, Error> {
        let unknown = || Error::UnknownRequest(request_id.to_owned());
        let pending = self.asked.get(request_id).filter(|pending| pending.question).ok_or_else(unknown)?;
        let chosen = match choice {
            None => None,
            Some(choice) => {
                let by_id = pending.choices.iter().find(|offered| offered.id == choice);
                let mut named = pending.choices.iter().filter(|offered| offered.name == choice);
                // A text that two choices share names neither.
                let by_name = named.next().filter(|_| named.next().is_none());
                Some(by_id.or(by_name).ok_or_else(unknown)?)
            }
        };
        let line = outcome_line(&pending.id, chosen.map(|choice| choice.id.as_str()));
        self.asked.remove(request_id);
        Ok(line)
    }

    /// The process ended: nothing waits any more.
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.awaiting.clear();
        self.asked.clear();
        self.held.clear();
    }

    /// A request of ours, registered so that its answer finds its way.
    fn request(&mut self, method: &str, params: Value, awaiting: Awaiting) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.awaiting.insert(id, awaiting);
        request_line(method, id, params)
    }

    /// The session cannot go on.
    fn fail(&mut self, why: String, out: &mut Output) {
        self.closed = true;
        out.fatal = Some(why);
    }

    /// Opens the session: continues the one asked for where the binary can, starts one otherwise.
    fn open(&mut self, out: &mut Output) {
        let mut servers = Vec::new();
        for server in &self.options.mcp_servers {
            let pairs = |pairs: &[(String, String)]| -> Vec<Value> {
                pairs.iter().map(|(name, value)| json!({"name": name, "value": value})).collect()
            };
            match &server.transport {
                McpTransport::Stdio { command, args, env } => {
                    servers.push(json!({"name": server.name, "command": command, "args": args, "env": pairs(env)}));
                }
                McpTransport::Http { url, headers } if self.can.http_mcp => {
                    servers.push(json!({"type": "http", "name": server.name, "url": url, "headers": pairs(headers)}));
                }
                McpTransport::Http { .. } => {
                    let name = &server.name;
                    out.events.push(Event::Warning(format!(
                        "this Antigravity takes no MCP servers over HTTP: {name} is left out"
                    )));
                }
            }
        }
        let mut params = json!({"cwd": self.cwd, "mcpServers": servers});
        if let Some(instructions) = filled(self.options.instructions.as_ref()).filter(|_| self.can.instructions) {
            params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": instructions}}});
        }
        let Some(session) = filled(self.options.resume.as_ref()) else {
            out.lines.push(self.request("session/new", params, Awaiting::Open { resume: false }));
            return;
        };
        if !self.can.resume && !self.can.load {
            self.options.resume = None;
            self.catching_up = true;
            out.events.push(Event::Warning(
                "this Antigravity cannot continue a conversation: a new one is started".to_owned(),
            ));
            return self.open(out);
        }
        params["sessionId"] = session.into();
        // Continuing is preferred to loading: loading has the binary tell the whole history again.
        let method = if self.can.resume { "session/resume" } else { "session/load" };
        self.loading = !self.can.resume;
        out.lines.push(self.request(method, params, Awaiting::Open { resume: true }));
    }

    /// Takes the answer to a request of ours.
    fn answered(&mut self, awaiting: Awaiting, outcome: Result<Value, Refusal>, out: &mut Output) {
        match (awaiting, outcome) {
            (Awaiting::Initialize, Ok(answer)) => {
                self.can = parse::abilities(&answer);
                self.open(out);
            }
            (Awaiting::Initialize, Err(refusal)) => {
                self.fail(format!("Antigravity refused the handshake: {}", refusal.message), out);
            }
            (Awaiting::Authenticate, Ok(_)) => self.open(out),
            (Awaiting::Authenticate | Awaiting::Open { .. }, Err(refusal)) if refusal.needs_sign_in() => {
                self.loading = false;
                self.sign_in(out);
            }
            (Awaiting::Authenticate, Err(refusal)) => {
                self.fail(format!("Antigravity could not use its sign-in: {}", refusal.message), out);
            }
            (Awaiting::Open { resume }, Ok(answer)) => self.opened(resume, &answer, out),
            (Awaiting::Open { resume: true }, Err(refusal)) => {
                self.loading = false;
                self.options.resume = None;
                self.catching_up = true;
                let why = refusal.message;
                out.events.push(Event::Warning(format!(
                    "the conversation could not be continued ({why}): a new one is started"
                )));
                self.open(out);
            }
            (Awaiting::Open { resume: false }, Err(refusal)) => {
                self.fail(format!("Antigravity could not start a session: {}", refusal.message), out);
            }
            (Awaiting::Model(model), outcome) => {
                self.tuning = false;
                match outcome {
                    Ok(answer) => {
                        self.model.clone_from(&model);
                        self.settings.catalog.model = model;
                        self.settings.update(field(&answer, "configOptions"));
                        out.events.push(Event::Catalog(self.settings.catalog.clone()));
                    }
                    Err(refusal) => {
                        let why = refusal.message;
                        out.events.push(Event::Warning(format!("Antigravity did not take the model {model}: {why}")));
                        self.refused.push(model);
                    }
                }
                self.flush(out);
            }
            (Awaiting::Effort(effort), outcome) => {
                self.tuning = false;
                match outcome {
                    Ok(answer) => {
                        self.settings.catalog.effort.clone_from(&effort);
                        self.settings.update(field(&answer, "configOptions"));
                        if self.settings.catalog.effort != effort {
                            // It said yes and kept its own: asking again would go on for ever.
                            self.refused.push(effort);
                        }
                        out.events.push(Event::Catalog(self.settings.catalog.clone()));
                    }
                    Err(refusal) => {
                        let why = refusal.message;
                        out.events.push(Event::Warning(format!("Antigravity did not take the level {effort}: {why}")));
                        self.refused.push(effort);
                    }
                }
                self.flush(out);
            }
            (Awaiting::Mode(mode), outcome) => {
                self.tuning = false;
                match outcome {
                    Ok(answer) => {
                        self.settings.mode.clone_from(&mode);
                        self.settings.update(field(&answer, "configOptions"));
                        if self.settings.mode != mode {
                            // It said yes and kept its own: asking again would go on for ever.
                            self.no_mode.push(mode);
                        }
                        out.events.push(Event::Mode(self.settings.mode.clone()));
                    }
                    Err(refusal) => {
                        let why = refusal.message;
                        out.events.push(Event::Warning(format!("Antigravity did not take the mode {mode}: {why}")));
                        self.no_mode.push(mode);
                    }
                }
                self.flush(out);
            }
            (Awaiting::Prompt, outcome) => self.prompted(outcome, out),
        }
    }

    /// The binary says a session needs a sign-in: it is asked to use the one it has, once, where the caller
    /// allows that.
    fn sign_in(&mut self, out: &mut Output) {
        let method = filled(self.options.authenticate.as_ref()).filter(|_| !self.authenticated);
        let Some(method) = method else { return self.signed_out(out) };
        self.authenticated = true;
        out.lines.push(self.request("authenticate", json!({"methodId": method}), Awaiting::Authenticate));
    }

    /// The session is open.
    fn opened(&mut self, resumed: bool, answer: &Value, out: &mut Output) {
        self.loading = false;
        let asked = self.options.resume.clone().filter(|_| resumed);
        let Some(session) = opt_text(answer, "sessionId").or(asked) else {
            return self.fail("Antigravity opened a session without naming it".to_owned(), out);
        };
        self.settings = Settings::of(answer);
        self.model.clone_from(&self.settings.catalog.model);
        out.events.push(Event::Ready {
            session_id: session.clone(),
            resumed,
            version: self.can.version.clone(),
            catalog: self.settings.catalog.clone(),
        });
        if !self.settings.mode.is_empty() {
            out.events.push(Event::Mode(self.settings.mode.clone()));
        }
        self.session = Some(session);
        self.flush(out);
    }

    /// Sets the model, the level and the mode the caller chose, where they are not in force: one request at
    /// a time, the level after the model, whose levels it is one of. Says whether a request went out.
    fn tune(&mut self, out: &mut Output) -> bool {
        let session = self.session.clone().unwrap_or_default();
        let model = filled(self.options.model.as_ref()).filter(|model| *model != self.model);
        if let Some(model) = model.filter(|model| !self.refused.contains(model)) {
            let (method, params) = match &self.settings.model_option {
                Some(option) => {
                    ("session/set_config_option", json!({"sessionId": session, "configId": option, "value": model}))
                }
                None => ("session/set_model", json!({"sessionId": session, "modelId": model})),
            };
            out.lines.push(self.request(method, params, Awaiting::Model(model)));
            self.tuning = true;
            return true;
        }
        let catalog = &self.settings.catalog;
        let effort = filled(self.options.effort.as_ref())
            .filter(|effort| *effort != catalog.effort && catalog.efforts.iter().any(|level| level.id == *effort));
        let wanted = effort.filter(|effort| !self.refused.contains(effort)).zip(self.settings.effort_option.clone());
        if let Some((effort, option)) = wanted {
            let params = json!({"sessionId": session, "configId": option, "value": effort});
            out.lines.push(self.request("session/set_config_option", params, Awaiting::Effort(effort)));
            self.tuning = true;
            return true;
        }
        // The mode chosen where the session offers it, and the one that asks first where it does not.
        let offered = |mode: &String| self.settings.modes.contains(mode);
        let mode = filled(self.options.mode.as_ref())
            .map(|mode| if offered(&mode) { mode } else { DEFAULT_MODE.to_owned() })
            .filter(|mode| *mode != self.settings.mode && offered(mode) && !self.no_mode.contains(mode));
        if let Some(mode) = mode {
            let (method, params) = match &self.settings.mode_option {
                Some(option) => {
                    ("session/set_config_option", json!({"sessionId": session, "configId": option, "value": mode}))
                }
                None => ("session/set_mode", json!({"sessionId": session, "modeId": mode})),
            };
            out.lines.push(self.request(method, params, Awaiting::Mode(mode)));
            self.tuning = true;
            return true;
        }
        false
    }

    /// Sends what waits, in the order it came, as far as it can be sent now.
    fn flush(&mut self, out: &mut Output) {
        if self.session.is_none() || self.closed {
            return;
        }
        let mut waiting = Vec::new();
        for command in mem::take(&mut self.held) {
            match command {
                Command::Interrupt => self.interrupt(out),
                // Behind a message that waits, the later ones wait too.
                Command::Send(message) if !waiting.is_empty() || self.stopping || self.tuning => {
                    waiting.push(Command::Send(message));
                }
                // A turn of its own starts with the model and the level chosen.
                Command::Send(message) if !self.turn && self.tune(out) => waiting.push(Command::Send(message)),
                Command::Send(message) => self.prompt(&message, out),
                Command::Steer(message) if self.turn && !self.stopping => self.prompt(&message, out),
                // Nothing runs that could take it: the caller sees no `UserMessage` and sends it again.
                Command::Steer(_) => {}
            }
        }
        self.held = waiting;
    }

    /// Gives the agent a message. While a turn runs the binary puts it into that turn.
    fn prompt(&mut self, message: &Message, out: &mut Output) {
        let mut blocks = message.blocks(self.can.images);
        // The conversation it was to continue is gone: the first message says what was said in it.
        let earlier = filled(self.options.catch_up.as_ref()).filter(|_| mem::take(&mut self.catching_up));
        if let (Some(earlier), Some(blocks)) = (earlier, blocks.as_array_mut()) {
            let told = format!("{CATCH_UP}\n\n{earlier}\n\n{CATCH_UP_END}");
            blocks.insert(0, json!({"type": "text", "text": told}));
        }
        let params = json!({"sessionId": self.session, "prompt": blocks});
        out.lines.push(self.request("session/prompt", params, Awaiting::Prompt));
        self.prompts += 1;
        self.begin(out);
        out.events.push(Event::UserMessage { client_id: message.client_id.clone(), text: message.text() });
    }

    /// A turn runs from here on, if none did.
    fn begin(&mut self, out: &mut Output) {
        if self.turn {
            return;
        }
        self.turn = true;
        (self.stop_reason, self.failure, self.usage) = (String::new(), None, None);
        // What ended in an earlier turn is not heard of again; a command still running may be.
        self.tools.retain(|_, tool| !tool.status.is_final());
        out.events.push(Event::TurnStarted);
    }

    /// The running turn ended.
    fn end(&mut self, status: TurnStatus, error: Option<String>, out: &mut Output) {
        (self.turn, self.stopping, self.prompts) = (false, false, 0);
        self.failure = None;
        out.events.push(Event::TurnCompleted {
            status,
            stop_reason: mem::take(&mut self.stop_reason),
            error,
            usage: self.usage.take(),
        });
        self.flush(out);
    }

    /// The agent is done with a message. The turn ends with the last of them.
    fn prompted(&mut self, outcome: Result<Value, Refusal>, out: &mut Output) {
        self.prompts = self.prompts.saturating_sub(1);
        match outcome {
            Ok(answer) => {
                self.stop_reason = text(&answer, "stopReason");
                if let Some(usage) = parse::usage(field(&answer, "usage")) {
                    let total = self.usage.get_or_insert_default();
                    total.input = total.input.saturating_add(usage.input);
                    total.cached_input = total.cached_input.saturating_add(usage.cached_input);
                    total.cache_write_input = total.cache_write_input.saturating_add(usage.cache_write_input);
                    total.output = total.output.saturating_add(usage.output);
                    total.reasoning_output = total.reasoning_output.saturating_add(usage.reasoning_output);
                }
            }
            Err(refusal) if refusal.needs_sign_in() => self.failure = Some(NOT_SIGNED_IN.to_owned()),
            // The answer to a message another took the place of: the turn goes on under that one.
            Err(_) if self.prompts > 0 => {}
            Err(refusal) => self.failure = Some(refusal.message),
        }
        if self.prompts > 0 || !self.turn {
            return;
        }
        let (status, error) = if self.stopping || self.stop_reason == "cancelled" {
            (TurnStatus::Interrupted, None)
        } else if let Some(failure) = self.failure.take() {
            (TurnStatus::Failed, Some(failure))
        } else if self.stop_reason == "refusal" {
            // The only thing the binary stops for in this way.
            (TurnStatus::Failed, Some("the account's Antigravity quota for this period is used up".to_owned()))
        } else {
            let cut = match self.stop_reason.as_str() {
                "max_tokens" => Some("the reply was cut off: it reached the most a model may say at once"),
                "max_turn_requests" => Some("the agent stopped: it took as many steps as one turn may"),
                _ => None,
            };
            out.events.extend(cut.map(|cut| Event::Warning(cut.to_owned())));
            (TurnStatus::Completed, None)
        };
        self.end(status, error, out);
    }

    /// Stops the running turn. With none running, what the agent left running in the background is stopped.
    fn interrupt(&mut self, out: &mut Output) {
        let running = self.tools.values().any(|tool| !tool.status.is_final());
        if self.stopping || !(self.turn || running) {
            return;
        }
        out.lines.push(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": self.session}}));
        // The protocol wants what is still asked answered as withdrawn.
        let mut asked: Vec<(String, Pending)> = self.asked.drain().collect();
        asked.sort_by(|a, b| a.0.cmp(&b.0));
        for (request_id, pending) in asked {
            out.lines.push(outcome_line(&pending.id, None));
            out.events.push(Event::RequestResolved { request_id });
        }
        if !self.turn {
            return;
        }
        if self.prompts == 0 {
            // No message is open whose answer would end it.
            self.end(TurnStatus::Interrupted, None, out);
        } else {
            self.stopping = true;
        }
    }

    /// Takes a `session/update`.
    fn updated(&mut self, params: &Value, out: &mut Output) {
        // Before the session is open there is only the history of one being loaded.
        let ours = self.session.as_deref().is_some_and(|session| text(params, "sessionId") == session);
        if !ours || self.loading {
            return;
        }
        let update = field(params, "update");
        match text(update, "sessionUpdate").as_str() {
            "agent_message_chunk" => {
                if let Some(text) = parse::block_text(field(update, "content")).filter(|text| !text.is_empty()) {
                    self.begin(out);
                    out.events.push(Event::TextDelta { message_id: opt_text(update, "messageId"), text });
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = parse::block_text(field(update, "content")).filter(|text| !text.is_empty()) {
                    self.begin(out);
                    out.events.push(Event::ReasoningDelta { text });
                }
            }
            kind @ ("tool_call" | "tool_call_update") => {
                let known = opt_text(update, "toolCallId").and_then(|id| self.tools.get(&id).cloned());
                let Some(tool) = parse::tool(known, update) else { return };
                // A call that begins is the agent at work; one that only ends may be all that comes.
                if kind == "tool_call" {
                    self.begin(out);
                }
                self.tools.insert(tool.id.clone(), tool.clone());
                out.events.push(Event::Tool(tool));
            }
            "plan" => out.events.push(Event::Plan { steps: parse::plan(update) }),
            "available_commands_update" => out.events.push(Event::Commands(parse::commands(update))),
            "config_option_update" => {
                let (before, mode) = (self.settings.catalog.clone(), self.settings.mode.clone());
                self.settings.update(field(update, "configOptions"));
                if self.settings.catalog != before {
                    out.events.push(Event::Catalog(self.settings.catalog.clone()));
                }
                if self.settings.mode != mode {
                    out.events.push(Event::Mode(self.settings.mode.clone()));
                }
            }
            "current_mode_update" => {
                let mode = opt_text(update, "currentModeId").filter(|mode| *mode != self.settings.mode);
                if let Some(mode) = mode {
                    self.settings.mode.clone_from(&mode);
                    out.events.push(Event::Mode(mode));
                }
            }
            "usage_update" => {
                let count = |key: &str| field(update, key).as_u64().unwrap_or_default();
                let (used, size) = (count("used"), count("size"));
                if used > 0 || size > 0 {
                    out.events.push(Event::Context { used, size });
                }
                // Every reply of the model ends with one; nothing else ends a turn the agent began by itself.
                if self.turn && self.prompts == 0 && self.asked.is_empty() {
                    self.end(TurnStatus::Completed, None, out);
                }
            }
            _ => {}
        }
    }

    /// Takes a request of the binary.
    fn requested(&mut self, id: Value, method: &str, params: &Value, out: &mut Output) {
        if method != "session/request_permission" {
            // Files and terminals are the binary's own business here: none was offered in the handshake.
            let message = format!("{CLIENT_NAME} does not offer {method}");
            out.lines
                .push(json!({"jsonrpc": "2.0", "id": id, "error": {"code": METHOD_NOT_FOUND, "message": message}}));
            return;
        }
        let key = match &id {
            Value::String(id) => id.clone(),
            id => id.to_string(),
        };
        let tools = &self.tools;
        let asked = parse::asked(&key, params, |call| tools.get(call).cloned());
        // Nothing to choose from, or a turn that is being stopped: it is answered as withdrawn.
        let Some((asked, choices)) = asked.filter(|_| !self.stopping && !self.loading) else {
            out.lines.push(outcome_line(&id, None));
            return;
        };
        self.begin(out);
        let question = matches!(asked, Asked::Question(_));
        self.asked.insert(key, Pending { id, choices, question });
        match asked {
            Asked::Approval(request) => {
                self.tools.insert(request.tool.id.clone(), request.tool.clone());
                out.events.push(Event::Tool(request.tool.clone()));
                out.events.push(Event::Approval(*request));
            }
            Asked::Question(request) => out.events.push(Event::Question(request)),
        }
    }
}

/// The value of an option, when it is set to something.
fn filled(option: Option<&String>) -> Option<String> {
    option.filter(|value| !value.trim().is_empty()).cloned()
}

/// A request line.
pub(crate) fn request_line(method: &str, id: u64, params: Value) -> Value {
    let mut line = json!({"jsonrpc": "2.0", "id": id, "method": method});
    line["params"] = params;
    line
}

/// The answer to a request for permission: the choice made, or none.
fn outcome_line(id: &Value, choice: Option<&str>) -> Value {
    let outcome = match choice {
        Some(choice) => json!({"outcome": "selected", "optionId": choice}),
        None => json!({"outcome": "cancelled"}),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
}

/// The parameters of `initialize`: who is calling, and that the binary reads files and runs commands itself.
pub(crate) fn initialize_params(options: &Options) -> Value {
    let version = match options.client_version.trim() {
        "" => env!("CARGO_PKG_VERSION"),
        version => version,
    };
    json!({
        "protocolVersion": PROTOCOL,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "clientInfo": {"name": CLIENT_NAME, "title": CLIENT_TITLE, "version": version},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, McpServer, ToolKind, ToolStatus};

    const CAN: &str = r#"{"protocolVersion":1,"agentCapabilities":{"loadSession":true,
        "promptCapabilities":{"image":true},"mcpCapabilities":{"http":true},
        "sessionCapabilities":{"resume":{}},"_meta":{"antigravity":{"systemPrompt":{"append":true}}}},
        "agentInfo":{"version":"1.5.12"}}"#;

    fn machine(options: &Options) -> Machine {
        Machine::new(options, "/work".to_owned())
    }

    /// The binary's answer to our request `id`.
    fn answer(machine: &mut Machine, id: u64, result: &str) -> Output {
        machine.receive(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{}}}", result.replace('\n', " ")))
    }

    fn refuse(machine: &mut Machine, id: u64, code: i64, message: &str) -> Output {
        machine.receive(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string())
    }

    fn update(machine: &mut Machine, update: Value) -> Output {
        let mut params = json!({"sessionId": "s1"});
        params["update"] = update;
        machine.receive(&json!({"method": "session/update", "params": params}).to_string())
    }

    fn options_json() -> Value {
        json!([
            {"id": "model", "category": "model", "type": "select", "currentValue": "flash", "options": [
                {"value": "flash", "name": "Flash"}, {"value": "opus", "name": "Opus"}]},
            {"id": "thinking", "category": "thought_level", "type": "select", "currentValue": "low", "options": [
                {"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}
        ])
    }

    /// A machine whose session `s1` is open: requests 1 and 2 are spent.
    fn open(options: &Options) -> Machine {
        let mut machine = machine(options);
        machine.hello();
        answer(&mut machine, 1, CAN);
        let setup = json!({"sessionId": "s1", "configOptions": options_json()}).to_string();
        let out = answer(&mut machine, 2, &setup);
        assert!(matches!(out.events.first(), Some(Event::Ready { .. })), "{out:?}");
        machine
    }

    fn say(text: &str, client_id: &str) -> Command {
        Command::Send(Message { parts: vec![Part::Text(text.to_owned())], client_id: Some(client_id.to_owned()) })
    }

    fn methods(out: &Output) -> Vec<String> {
        out.lines.iter().map(|line| text(line, "method")).collect()
    }

    #[test]
    fn the_handshake_opens_a_session_and_reports_it() {
        let options = Options {
            instructions: Some("Be brief.".into()),
            client_version: "0.2.0".into(),
            mcp_servers: vec![
                McpServer {
                    name: "files".into(),
                    transport: McpTransport::Stdio {
                        command: "srv".into(),
                        args: vec!["--x".into()],
                        env: vec![("K".into(), "V".into())],
                    },
                },
                McpServer {
                    name: "web".into(),
                    transport: McpTransport::Http {
                        url: "http://h/mcp".into(),
                        headers: vec![("A".into(), "B".into())],
                    },
                },
            ],
            ..Options::default()
        };
        let mut machine = machine(&options);
        let hello = machine.hello();
        assert_eq!(text(&hello, "method"), "initialize");
        assert_eq!(hello["params"]["protocolVersion"], 1);
        assert_eq!(
            hello["params"]["clientInfo"],
            json!({"name": "atroxabot", "title": "AtroxaBot", "version": "0.2.0"})
        );
        assert_eq!(hello["params"]["clientCapabilities"]["terminal"], false);

        let out = answer(&mut machine, 1, CAN);
        assert_eq!(methods(&out), ["session/new"]);
        let params = &out.lines[0]["params"];
        assert_eq!(params["cwd"], "/work");
        assert_eq!(params["_meta"]["antigravity"]["systemPrompt"]["append"], "Be brief.");
        assert_eq!(
            params["mcpServers"],
            json!([
                {"name": "files", "command": "srv", "args": ["--x"], "env": [{"name": "K", "value": "V"}]},
                {"type": "http", "name": "web", "url": "http://h/mcp", "headers": [{"name": "A", "value": "B"}]}
            ])
        );

        let out = answer(&mut machine, 2, &json!({"sessionId": "s1", "configOptions": options_json()}).to_string());
        let Some(Event::Ready { session_id, resumed, version, catalog }) = out.events.first() else {
            panic!("{out:?}")
        };
        assert_eq!((session_id.as_str(), *resumed, version.as_str()), ("s1", false, "1.5.12"));
        assert_eq!((catalog.model.as_str(), catalog.effort.as_str(), catalog.models.len()), ("flash", "low", 2));
        assert!(out.lines.is_empty() && out.fatal.is_none());
    }

    #[test]
    fn what_the_binary_cannot_take_is_left_out() {
        let options = Options {
            instructions: Some("Be brief.".into()),
            mcp_servers: vec![McpServer {
                name: "web".into(),
                transport: McpTransport::Http { url: "http://h/mcp".into(), headers: Vec::new() },
            }],
            ..Options::default()
        };
        let mut machine = machine(&options);
        machine.hello();
        let out = answer(&mut machine, 1, "{}");
        assert_eq!(out.lines[0]["params"], json!({"cwd": "/work", "mcpServers": []}));
        assert!(matches!(out.events.as_slice(), [Event::Warning(warning)] if warning.contains("web")));
    }

    #[test]
    fn a_refused_handshake_ends_the_session() {
        let mut machine = machine(&Options::default());
        machine.hello();
        let out = refuse(&mut machine, 1, -32600, "unsupported protocol");
        assert_eq!(out.fatal.as_deref(), Some("Antigravity refused the handshake: unsupported protocol"));
        // Nothing is taken after that.
        assert_eq!(machine.command(say("hi", "m1")), Output::default());
        assert_eq!(answer(&mut machine, 2, "{}"), Output::default());
    }

    #[test]
    fn a_session_that_needs_a_sign_in_ends_saying_so() {
        let mut machine = machine(&Options::default());
        machine.hello();
        answer(&mut machine, 1, CAN);
        let out = refuse(&mut machine, 2, -32000, "Authentication required");
        assert_eq!(out.events, [Event::SignInRequired]);
        assert_eq!(out.fatal.as_deref(), Some(NOT_SIGNED_IN));
        assert!(out.lines.is_empty());
    }

    #[test]
    fn the_binary_is_asked_once_to_use_the_sign_in_it_has() {
        let options = Options { authenticate: Some("oauth-personal".into()), ..Options::default() };
        let mut signed_in = machine(&options);
        signed_in.hello();
        answer(&mut signed_in, 1, CAN);
        let out = refuse(&mut signed_in, 2, -32000, "Authentication required");
        assert_eq!(methods(&out), ["authenticate"]);
        assert_eq!(out.lines[0]["params"], json!({"methodId": "oauth-personal"}));
        assert!(out.events.is_empty() && out.fatal.is_none());
        let out = answer(&mut signed_in, 3, "{}");
        assert_eq!(methods(&out), ["session/new"]);
        let out = answer(&mut signed_in, 4, "{\"sessionId\":\"s1\"}");
        assert!(matches!(out.events.as_slice(), [Event::Ready { .. }]));

        // Without a sign-in to use it says so, in one of two ways, or prints a link and waits.
        for (code, message) in [(-32000, "Authentication required"), (-32603, "No browser is available here.")] {
            let mut signed_out = machine(&options);
            signed_out.hello();
            answer(&mut signed_out, 1, CAN);
            refuse(&mut signed_out, 2, -32000, "Authentication required");
            let out = refuse(&mut signed_out, 3, code, message);
            assert_eq!((out.events, out.fatal.as_deref()), (vec![Event::SignInRequired], Some(NOT_SIGNED_IN)));
        }
        let mut waiting = machine(&options);
        waiting.hello();
        answer(&mut waiting, 1, CAN);
        refuse(&mut waiting, 2, -32000, "Authentication required");
        let link = "Open the following link to authenticate the ACP server: https://accounts.google.com/x";
        // Asked to sign in it shows Google's page, and the session opens once the user is through with it.
        let out = waiting.receive(link);
        assert_eq!((out.events, out.fatal.as_deref()), (vec![Event::SigningIn], None));
        assert_eq!(methods(&answer(&mut waiting, 3, "{}")), ["session/new"]);
        // A link nobody asked for is a session that cannot open.
        let mut unasked = machine(&Options::default());
        unasked.hello();
        let out = unasked.receive(link);
        assert_eq!((out.events, out.fatal.as_deref()), (vec![Event::SignInRequired], Some(NOT_SIGNED_IN)));
        // Asked again after the sign-in was used, it is not asked a second time.
        let mut again = machine(&options);
        again.hello();
        answer(&mut again, 1, CAN);
        refuse(&mut again, 2, -32000, "Authentication required");
        answer(&mut again, 3, "{}");
        let out = refuse(&mut again, 4, -32000, "Authentication required");
        assert_eq!(out.fatal.as_deref(), Some(NOT_SIGNED_IN));
    }

    #[test]
    fn a_conversation_is_continued_or_started_anew() {
        let options = Options { resume: Some("old".into()), ..Options::default() };
        let mut machine = machine(&options);
        machine.hello();
        let out = answer(&mut machine, 1, CAN);
        assert_eq!(methods(&out), ["session/resume"]);
        assert_eq!(out.lines[0]["params"]["sessionId"], "old");
        // The answer need not name the session again.
        let out = answer(&mut machine, 2, "{}");
        assert!(
            matches!(out.events.as_slice(), [Event::Ready { session_id, resumed: true, .. }] if session_id == "old")
        );

        let options = Options { catch_up: Some("User: hello\n\nYou: hi".into()), ..options };
        let mut gone = super::Machine::new(&options, "/work".to_owned());
        gone.hello();
        answer(&mut gone, 1, CAN);
        let out = refuse(&mut gone, 2, -32603, "session not found");
        assert!(matches!(out.events.as_slice(), [Event::Warning(warning)] if warning.contains("session not found")));
        assert_eq!(methods(&out), ["session/new"]);
        assert!(out.lines[0]["params"].get("sessionId").is_none());
        let out = answer(&mut gone, 3, "{\"sessionId\":\"new\"}");
        assert!(
            matches!(out.events.as_slice(), [Event::Ready { session_id, resumed: false, .. }] if session_id == "new")
        );
        // The one started in its place is told what was said, with its first message only.
        let out = gone.command(say("and now?", "m1"));
        let told = out.lines[0]["params"]["prompt"].as_array().expect("the blocks").clone();
        assert_eq!(told.len(), 2);
        let earlier = text(&told[0], "text");
        assert!(earlier.starts_with(CATCH_UP) && earlier.contains("You: hi") && earlier.ends_with(CATCH_UP_END));
        assert_eq!(told[1], json!({"type": "text", "text": "and now?"}));
        answer(&mut gone, 4, r#"{"stopReason":"end_turn"}"#);
        let out = gone.command(say("again", "m2"));
        assert_eq!(out.lines[0]["params"]["prompt"], json!([{"type": "text", "text": "again"}]));
    }

    #[test]
    fn a_conversation_that_is_continued_is_told_nothing_again() {
        let options =
            Options { resume: Some("old".into()), catch_up: Some("User: hello".into()), ..Options::default() };
        let mut machine = machine(&options);
        machine.hello();
        answer(&mut machine, 1, CAN);
        answer(&mut machine, 2, "{}");
        let out = machine.command(say("and now?", "m1"));
        assert_eq!(out.lines[0]["params"]["prompt"], json!([{"type": "text", "text": "and now?"}]));
    }

    #[test]
    fn a_loaded_history_is_not_reported_again() {
        let options = Options { resume: Some("s1".into()), ..Options::default() };
        let mut machine = machine(&options);
        machine.hello();
        let out = answer(&mut machine, 1, r#"{"agentCapabilities":{"loadSession":true}}"#);
        assert_eq!(methods(&out), ["session/load"]);
        let replayed = update(
            &mut machine,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old"}}),
        );
        assert_eq!(replayed, Output::default());
        let out = answer(&mut machine, 2, "{}");
        assert!(matches!(out.events.as_slice(), [Event::Ready { resumed: true, .. }]));
        let live = update(
            &mut machine,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "new"}}),
        );
        assert_eq!(live.events.len(), 2);

        // A binary that can do neither starts anew, and says so.
        let mut neither = super::Machine::new(&options, "/work".to_owned());
        neither.hello();
        let out = answer(&mut neither, 1, "{}");
        assert_eq!(methods(&out), ["session/new"]);
        assert!(matches!(out.events.as_slice(), [Event::Warning(_)]));
    }

    #[test]
    fn a_turn_runs_from_the_message_to_its_answer() {
        let mut machine = open(&Options::default());
        let out = machine.command(say("hello", "m1"));
        assert_eq!(methods(&out), ["session/prompt"]);
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "hello"}]}));
        assert_eq!(
            out.events,
            [Event::TurnStarted, Event::UserMessage { client_id: Some("m1".into()), text: "hello".into() }]
        );

        let out = update(
            &mut machine,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}}),
        );
        assert_eq!(out.events, [Event::ReasoningDelta { text: "hm".into() }]);
        let chunk = json!({"sessionUpdate": "agent_message_chunk", "messageId": "a1", "content": {"type": "text", "text": "Hi"}});
        let out = update(&mut machine, chunk);
        assert_eq!(out.events, [Event::TextDelta { message_id: Some("a1".into()), text: "Hi".into() }]);
        let out = update(
            &mut machine,
            json!({"sessionUpdate": "plan", "entries": [{"content": "Greet", "status": "completed"}]}),
        );
        assert!(matches!(out.events.as_slice(), [Event::Plan { steps }] if steps.len() == 1));
        // The reply's closing count does not end a turn that a message is open in.
        let out = update(&mut machine, json!({"sessionUpdate": "usage_update", "used": 1200, "size": 1_000_000}));
        assert_eq!(out.events, [Event::Context { used: 1200, size: 1_000_000 }]);

        let done = r#"{"stopReason":"end_turn","usage":{"inputTokens":10,"cachedReadTokens":90,"outputTokens":5,"thoughtTokens":2}}"#;
        let out = answer(&mut machine, 3, done);
        let usage = TokenUsage { input: 100, cached_input: 90, cache_write_input: 0, output: 7, reasoning_output: 2 };
        let ended = Event::TurnCompleted {
            status: TurnStatus::Completed,
            stop_reason: "end_turn".into(),
            error: None,
            usage: Some(usage),
        };
        assert_eq!(out.events, [ended]);
    }

    #[test]
    fn a_message_sent_meanwhile_joins_the_turn_which_ends_with_the_last_answer() {
        let mut machine = open(&Options::default());
        machine.command(say("first", "m1"));
        let out = machine.command(say("and this", "m2"));
        assert_eq!(methods(&out), ["session/prompt"]);
        assert_eq!(out.events, [Event::UserMessage { client_id: Some("m2".into()), text: "and this".into() }]);
        let out = answer(&mut machine, 3, r#"{"stopReason":"end_turn","usage":{"inputTokens":1,"outputTokens":1}}"#);
        assert!(out.events.is_empty());
        let out = answer(&mut machine, 4, r#"{"stopReason":"end_turn","usage":{"inputTokens":2,"outputTokens":2}}"#);
        let Some(Event::TurnCompleted { status: TurnStatus::Completed, usage: Some(usage), .. }) = out.events.first()
        else {
            panic!("{out:?}")
        };
        assert_eq!((usage.input, usage.output), (3, 3));
    }

    #[test]
    fn steering_needs_a_turn_to_steer() {
        let mut machine = open(&Options::default());
        let steer =
            |text: &str| Command::Steer(Message { parts: vec![Part::Text(text.into())], client_id: Some("f".into()) });
        assert_eq!(machine.command(steer("too early")), Output::default());
        machine.command(say("go", "m1"));
        let out = machine.command(steer("faster"));
        assert_eq!(methods(&out), ["session/prompt"]);
        assert_eq!(out.events, [Event::UserMessage { client_id: Some("f".into()), text: "faster".into() }]);
    }

    #[test]
    fn what_is_asked_before_the_session_opens_waits_for_it() {
        let mut machine = machine(&Options::default());
        machine.hello();
        assert_eq!(machine.command(say("early", "m1")), Output::default());
        answer(&mut machine, 1, CAN);
        let out = answer(&mut machine, 2, "{\"sessionId\":\"s1\"}");
        assert_eq!(methods(&out), ["session/prompt"]);
        assert!(matches!(out.events.as_slice(), [Event::Ready { .. }, Event::TurnStarted, Event::UserMessage { .. }]));
    }

    #[test]
    fn the_model_and_level_chosen_are_set_before_the_first_message() {
        let options = Options { model: Some("opus".into()), effort: Some("high".into()), ..Options::default() };
        let mut machine = open(&options);
        let out = machine.command(say("hi", "m1"));
        assert_eq!(methods(&out), ["session/set_config_option"]);
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "configId": "model", "value": "opus"}));
        assert!(out.events.is_empty());
        // The new model lists its own levels; the one chosen is among them.
        let mut listed = options_json();
        listed[0]["currentValue"] = "opus".into();
        listed[1]["currentValue"] = "low".into();
        let out = answer(&mut machine, 3, &json!({"configOptions": listed}).to_string());
        assert!(matches!(out.events.as_slice(), [Event::Catalog(Catalog { model, .. })] if model == "opus"));
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "configId": "thinking", "value": "high"}));
        let out = answer(&mut machine, 4, "{}");
        assert!(matches!(&out.events[0], Event::Catalog(Catalog { effort, .. }) if effort == "high"));
        assert_eq!(methods(&out), ["session/prompt"]);
        // Nothing is set again for the next message.
        answer(&mut machine, 5, r#"{"stopReason":"end_turn"}"#);
        assert_eq!(methods(&machine.command(say("again", "m2"))), ["session/prompt"]);
    }

    /// A machine whose session offers the modes `default` and `auto` and is in `current`.
    fn open_in(options: &Options, current: &str) -> Machine {
        let mut machine = machine(options);
        machine.hello();
        answer(&mut machine, 1, CAN);
        let mut listed = options_json();
        listed.as_array_mut().unwrap().push(json!({"id": "mode", "category": "mode", "type": "select",
            "currentValue": current, "options": [{"value": "default"}, {"value": "auto"}]}));
        let out = answer(&mut machine, 2, &json!({"sessionId": "s1", "configOptions": listed}).to_string());
        assert_eq!(out.events.last(), Some(&Event::Mode(current.to_owned())), "{out:?}");
        machine
    }

    #[test]
    fn the_mode_chosen_is_set_where_the_session_offers_it() {
        let auto = Options { mode: Some("auto".into()), ..Options::default() };
        let mut machine = open_in(&auto, "default");
        let out = machine.command(say("hi", "m1"));
        assert_eq!(methods(&out), ["session/set_config_option"]);
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "configId": "mode", "value": "auto"}));
        let out = answer(&mut machine, 3, "{}");
        assert_eq!(out.events.first(), Some(&Event::Mode("auto".into())));
        assert_eq!(methods(&out), ["session/prompt"]);
        answer(&mut machine, 4, r#"{"stopReason":"end_turn"}"#);
        assert_eq!(methods(&machine.command(say("again", "m2"))), ["session/prompt"], "set once");

        // A session left in `auto` is put back for an agent that asks first.
        let asks = Options { mode: Some("default".into()), ..Options::default() };
        let mut machine = open_in(&asks, "auto");
        let out = machine.command(say("hi", "m1"));
        assert_eq!(out.lines[0]["params"]["value"], "default");
        // The binary says itself when the mode changes.
        answer(&mut machine, 3, "{}");
        let out = update(&mut machine, json!({"sessionUpdate": "current_mode_update", "currentModeId": "auto"}));
        assert_eq!(out.events, [Event::Mode("auto".into())]);

        // One that is not offered is not asked for: the session stays where it asks first.
        let mut machine = open(&auto);
        assert_eq!(methods(&machine.command(say("hi", "m1"))), ["session/prompt"]);
        // A refusal is said once, and the message goes on.
        let mut machine = open_in(&auto, "default");
        machine.command(say("hi", "m1"));
        let out = refuse(&mut machine, 3, -32602, "Auto mode is not available");
        assert!(matches!(&out.events[0], Event::Warning(said) if said.contains("did not take the mode auto")));
        assert_eq!(methods(&out), ["session/prompt"]);
        answer(&mut machine, 4, r#"{"stopReason":"end_turn"}"#);
        machine.retune(None, None);
        assert_eq!(methods(&machine.command(say("again", "m2"))), ["session/prompt"], "not asked again");
    }

    #[test]
    fn an_older_binary_takes_its_mode_the_older_way() {
        let options = Options { mode: Some("auto".into()), ..Options::default() };
        let mut machine = machine(&options);
        machine.hello();
        answer(&mut machine, 1, CAN);
        let setup = json!({"sessionId": "s1", "modes": {"currentModeId": "default",
            "availableModes": [{"id": "default"}, {"id": "auto"}]}});
        answer(&mut machine, 2, &setup.to_string());
        let out = machine.command(say("hi", "m1"));
        assert_eq!(methods(&out), ["session/set_mode"]);
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "modeId": "auto"}));
    }

    #[test]
    fn a_message_put_in_the_place_of_another_decides_how_the_turn_ends() {
        let mut machine = open(&Options::default());
        machine.command(say("hi", "m1"));
        machine
            .command(Command::Steer(Message { parts: vec![Part::Text("wait".into())], client_id: Some("m2".into()) }));
        // The first is answered as broken off, in whatever way: the turn goes on under the second.
        assert!(refuse(&mut machine, 3, -32603, "superseded").events.is_empty());
        let out = answer(&mut machine, 4, r#"{"stopReason":"end_turn"}"#);
        assert!(matches!(
            out.events.as_slice(),
            [Event::TurnCompleted { status: TurnStatus::Completed, error: None, .. }]
        ));
    }

    #[test]
    fn a_model_the_binary_refuses_is_said_once_and_the_message_goes_on() {
        let options = Options { model: Some("gone".into()), effort: Some("unheard".into()), ..Options::default() };
        let mut machine = open(&options);
        machine.command(say("hi", "m1"));
        let out = refuse(&mut machine, 3, -32602, "unknown model");
        assert!(
            matches!(&out.events[0], Event::Warning(warning) if warning.contains("gone") && warning.contains("unknown model"))
        );
        // A level the model does not list is not asked for at all.
        assert_eq!(methods(&out), ["session/prompt"]);
        answer(&mut machine, 4, r#"{"stopReason":"end_turn"}"#);
        assert_eq!(methods(&machine.command(say("again", "m2"))), ["session/prompt"]);
        // Until the caller chooses anew.
        answer(&mut machine, 5, r#"{"stopReason":"end_turn"}"#);
        machine.retune(Some("gone".into()), None);
        assert_eq!(methods(&machine.command(say("once more", "m3"))), ["session/set_config_option"]);
    }

    #[test]
    fn an_older_binary_takes_its_model_the_older_way() {
        let options = Options { model: Some("b".into()), ..Options::default() };
        let mut machine = machine(&options);
        machine.hello();
        answer(&mut machine, 1, CAN);
        let setup =
            r#"{"sessionId":"s1","models":{"currentModelId":"a","availableModels":[{"modelId":"a"},{"modelId":"b"}]}}"#;
        answer(&mut machine, 2, setup);
        let out = machine.command(say("hi", "m1"));
        assert_eq!(methods(&out), ["session/set_model"]);
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1", "modelId": "b"}));
        let out = answer(&mut machine, 3, "{}");
        assert!(matches!(&out.events[0], Event::Catalog(Catalog { model, .. }) if model == "b"));
        assert_eq!(methods(&out), ["session/prompt"]);
    }

    #[test]
    fn tool_calls_are_reported_whole_each_time() {
        let mut machine = open(&Options::default());
        machine.command(say("run it", "m1"));
        let begun = json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Running run_command",
            "status": "in_progress", "rawInput": {"CommandLine": "ls"}});
        let out = update(&mut machine, begun);
        let Some(Event::Tool(tool)) = out.events.first() else { panic!("{out:?}") };
        assert_eq!(
            (tool.kind, tool.status, tool.command.as_deref()),
            (ToolKind::Execute, ToolStatus::InProgress, Some("ls"))
        );
        let done = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "rawOutput": {"output": "a\nb", "exitCode": 0}});
        let out = update(&mut machine, done);
        let Some(Event::Tool(tool)) = out.events.first() else { panic!("{out:?}") };
        assert_eq!((tool.title.as_str(), tool.command.as_deref()), ("Running run_command", Some("ls")));
        assert_eq!(
            (tool.output.as_deref(), tool.exit_code, tool.status),
            (Some("a\nb"), Some(0), ToolStatus::Completed)
        );
    }

    fn permission(id: Value, call: &str) -> String {
        let mut line = json!({"jsonrpc": "2.0", "method": "session/request_permission", "params": {"sessionId": "s1",
            "toolCall": {"toolCallId": call, "title": "Run run_command?", "rawInput": {"CommandLine": "rm x"}},
            "options": [
                {"optionId": "y", "name": "Allow", "kind": "allow_once"},
                {"optionId": "a", "name": "Always", "kind": "allow_always"},
                {"optionId": "n", "name": "Deny", "kind": "reject_once"}]}});
        line["id"] = id;
        line.to_string()
    }

    #[test]
    fn an_approval_is_answered_with_the_choice_the_binary_offers_for_it() {
        for (decision, outcome) in [
            (Decision::Accept, json!({"outcome": "selected", "optionId": "y"})),
            (Decision::AcceptForSession, json!({"outcome": "selected", "optionId": "a"})),
            (Decision::Decline, json!({"outcome": "selected", "optionId": "n"})),
            (Decision::Cancel, json!({"outcome": "cancelled"})),
        ] {
            let mut machine = open(&Options::default());
            machine.command(say("go", "m1"));
            let out = machine.receive(&permission(json!(41), "t1"));
            let [Event::Tool(tool), Event::Approval(request)] = out.events.as_slice() else { panic!("{out:?}") };
            assert_eq!((request.request_id.as_str(), request.can_remember), ("41", true));
            assert_eq!(tool, &request.tool);
            // A question's answer is not an approval's.
            assert!(matches!(machine.answer("41", Some("y")), Err(Error::UnknownRequest(_))));
            let line = machine.respond("41", decision).unwrap();
            assert_eq!(line, json!({"jsonrpc": "2.0", "id": 41, "result": {"outcome": outcome}}));
            assert!(matches!(machine.respond("41", decision), Err(Error::UnknownRequest(_))));
        }
    }

    #[test]
    fn a_question_takes_one_of_its_choices_by_id_or_by_text() {
        let ask = json!({"jsonrpc": "2.0", "id": "q1", "method": "session/request_permission", "params": {
            "toolCall": {"toolCallId": "interaction_1", "title": "Which?"},
            "options": [{"optionId": "a", "name": "Apples"}, {"optionId": "b", "name": "Pears"}, {"optionId": "c", "name": "Pears"}]}})
        .to_string();
        let selected = |id: &str| json!({"jsonrpc": "2.0", "id": "q1", "result": {"outcome": {"outcome": "selected", "optionId": id}}});
        let mut machine = open(&Options::default());
        machine.command(say("go", "m1"));
        let out = machine.receive(&ask);
        assert!(
            matches!(out.events.as_slice(), [Event::Question(question)] if question.request_id == "q1" && question.options.len() == 3)
        );
        assert!(matches!(machine.respond("q1", Decision::Accept), Err(Error::UnknownRequest(_))));
        // Words of the user's own, or a text two choices share, answer nothing: the question stays.
        assert!(matches!(machine.answer("q1", Some("Plums")), Err(Error::UnknownRequest(_))));
        assert!(matches!(machine.answer("q1", Some("Pears")), Err(Error::UnknownRequest(_))));
        assert_eq!(machine.answer("q1", Some("Apples")).unwrap(), selected("a"));
        machine.receive(&ask);
        assert_eq!(machine.answer("q1", Some("c")).unwrap(), selected("c"));
        machine.receive(&ask);
        let dismissed = machine.answer("q1", None).unwrap();
        assert_eq!(dismissed["result"]["outcome"], json!({"outcome": "cancelled"}));
    }

    #[test]
    fn stopping_withdraws_what_is_asked_and_the_turn_ends_as_stopped() {
        let mut machine = open(&Options::default());
        machine.command(say("go", "m1"));
        machine.receive(&permission(json!(7), "t1"));
        let out = machine.command(Command::Interrupt);
        assert_eq!(methods(&out)[0], "session/cancel");
        assert_eq!(out.lines[0]["params"], json!({"sessionId": "s1"}));
        assert_eq!(out.lines[1], json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "cancelled"}}}));
        assert_eq!(out.events, [Event::RequestResolved { request_id: "7".into() }]);
        // Stopping twice asks once; what is asked or sent meanwhile is withdrawn or waits.
        assert_eq!(machine.command(Command::Interrupt), Output::default());
        let out = machine.receive(&permission(json!(8), "t2"));
        assert_eq!(out.lines[0]["result"]["outcome"], json!({"outcome": "cancelled"}));
        assert!(out.events.is_empty());
        assert_eq!(machine.command(say("next", "m2")), Output::default());
        let out = answer(&mut machine, 3, r#"{"stopReason":"cancelled"}"#);
        assert!(matches!(&out.events[0], Event::TurnCompleted { status: TurnStatus::Interrupted, error: None, .. }));
        // The message that waited starts the next turn.
        assert_eq!(methods(&out), ["session/prompt"]);
        assert!(matches!(out.events[1..], [Event::TurnStarted, Event::UserMessage { .. }]));
    }

    #[test]
    fn a_turn_fails_with_what_the_binary_says() {
        let mut machine = open(&Options::default());
        machine.command(say("go", "m1"));
        let out = refuse(&mut machine, 3, -32603, "upstream unavailable");
        let failed = Event::TurnCompleted {
            status: TurnStatus::Failed,
            stop_reason: String::new(),
            error: Some("upstream unavailable".into()),
            usage: None,
        };
        assert_eq!(out.events, [failed]);

        machine.command(say("again", "m2"));
        let out = answer(&mut machine, 4, r#"{"stopReason":"refusal"}"#);
        assert!(
            matches!(&out.events[0], Event::TurnCompleted { status: TurnStatus::Failed, stop_reason, error: Some(error), .. }
            if stop_reason == "refusal" && error.contains("quota"))
        );

        machine.command(say("long", "m3"));
        let out = answer(&mut machine, 5, r#"{"stopReason":"max_tokens"}"#);
        assert!(matches!(
            out.events.as_slice(),
            [Event::Warning(_), Event::TurnCompleted { status: TurnStatus::Completed, .. }]
        ));

        machine.command(say("late", "m4"));
        let out = refuse(&mut machine, 6, -32000, "Authentication required");
        assert!(matches!(&out.events[0], Event::TurnCompleted { error: Some(error), .. } if error == NOT_SIGNED_IN));
    }

    #[test]
    fn the_agent_going_on_by_itself_is_a_turn_that_ends_with_its_reply() {
        let mut machine = open(&Options::default());
        machine.command(say("start the build", "m1"));
        let command = json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Running run_command",
            "status": "in_progress", "rawInput": {"CommandLine": "make"}});
        update(&mut machine, command);
        answer(&mut machine, 3, r#"{"stopReason":"end_turn"}"#);
        // The command it left running ends: that alone is no turn.
        let out = update(
            &mut machine,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
        );
        assert!(matches!(out.events.as_slice(), [Event::Tool(tool)] if tool.command.as_deref() == Some("make")));
        // It says something about it, which is, and ends with the count that closes every reply.
        let out = update(
            &mut machine,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Built."}}),
        );
        assert!(matches!(out.events.as_slice(), [Event::TurnStarted, Event::TextDelta { .. }]));
        let out = update(&mut machine, json!({"sessionUpdate": "usage_update", "used": 10, "size": 100}));
        assert!(matches!(
            out.events.as_slice(),
            [Event::Context { .. }, Event::TurnCompleted { status: TurnStatus::Completed, usage: None, .. }]
        ));

        // Such a turn is stopped at once: no answer would end it.
        update(
            &mut machine,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "and"}}),
        );
        let out = machine.command(Command::Interrupt);
        assert_eq!(methods(&out), ["session/cancel"]);
        assert!(matches!(out.events.as_slice(), [Event::TurnCompleted { status: TurnStatus::Interrupted, .. }]));
        // With nothing running there is nothing to stop.
        assert_eq!(machine.command(Command::Interrupt), Output::default());
    }

    #[test]
    fn a_command_left_running_can_be_stopped_between_turns() {
        let mut machine = open(&Options::default());
        machine.command(say("serve", "m1"));
        update(
            &mut machine,
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "status": "in_progress", "kind": "execute"}),
        );
        answer(&mut machine, 3, r#"{"stopReason":"end_turn"}"#);
        let out = machine.command(Command::Interrupt);
        assert_eq!(methods(&out), ["session/cancel"]);
        assert!(out.events.is_empty());
    }

    #[test]
    fn what_was_not_offered_is_refused_and_other_sessions_are_not_heard() {
        let mut machine = open(&Options::default());
        let out =
            machine.receive(r#"{"jsonrpc":"2.0","id":5,"method":"fs/read_text_file","params":{"path":"/etc/passwd"}}"#);
        assert_eq!(out.lines[0]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(out.lines[0]["id"], 5);
        assert!(out.events.is_empty());
        let other = json!({"method": "session/update", "params": {"sessionId": "s2",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}}});
        assert_eq!(machine.receive(&other.to_string()), Output::default());
        assert_eq!(machine.receive("{\"method\":\"session/update\"}"), Output::default());
        assert_eq!(machine.receive("not json"), Output::default());
        // A request for permission that offers nothing is answered as withdrawn.
        let empty =
            r#"{"id":6,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"t"},"options":[]}}"#;
        let out = machine.receive(empty);
        assert_eq!(out.lines[0]["result"]["outcome"], json!({"outcome": "cancelled"}));
    }

    #[test]
    fn the_binary_says_what_it_offers_as_it_changes() {
        let mut machine = open(&Options::default());
        let out = update(
            &mut machine,
            json!({"sessionUpdate": "available_commands_update",
            "availableCommands": [{"name": "compact", "description": "Summarise"}]}),
        );
        assert!(matches!(out.events.as_slice(), [Event::Commands(commands)] if commands[0].name == "compact"));
        // The same options again are no news.
        let same =
            update(&mut machine, json!({"sessionUpdate": "config_option_update", "configOptions": options_json()}));
        assert!(same.events.is_empty());
        let mut changed = options_json();
        changed[1]["currentValue"] = "high".into();
        let out = update(&mut machine, json!({"sessionUpdate": "config_option_update", "configOptions": changed}));
        assert!(matches!(out.events.as_slice(), [Event::Catalog(Catalog { effort, .. })] if effort == "high"));
    }

    #[test]
    fn a_picture_is_sent_along_where_the_binary_takes_them() {
        let message = Message {
            parts: vec![
                Part::Text("look".into()),
                Part::Image { mime: "image/png", data: "QUJD".into(), path: "/tmp/a.png".into() },
            ],
            client_id: None,
        };
        assert_eq!(
            message.blocks(true),
            json!([{"type": "text", "text": "look"}, {"type": "image", "data": "QUJD", "mimeType": "image/png"}])
        );
        assert_eq!(
            message.blocks(false),
            json!([{"type": "text", "text": "look"}, {"type": "text", "text": "[picture: /tmp/a.png]"}])
        );
        assert_eq!(message.text(), "look");
    }
}
