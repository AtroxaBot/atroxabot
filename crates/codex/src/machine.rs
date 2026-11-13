//! The session's side of the protocol as a state machine: lines of the binary and commands of the caller go
//! in, events and lines to write come out. It does no I/O, so tests drive it directly.

use std::collections::HashMap;
use std::mem;

use serde_json::{Map, Value, json};

use crate::parse::{self, Asked, Incoming, Refusal, Reply, field, opt_text, text};
use crate::{Decision, Error, Event, Input, Options, TurnStatus};

/// The name this client gives itself; Codex puts it in its user agent.
const CLIENT_NAME: &str = "atroxabot";
/// The client's name for display.
const CLIENT_TITLE: &str = "AtroxaBot";
/// JSON-RPC's code for a method the receiver does not have.
const METHOD_NOT_FOUND: i64 = -32601;

/// What the user sends: the `input` of a turn and the id the caller knows it by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    /// Codex's `UserInput`s.
    pub input: Value,
    /// Sent as `clientUserMessageId`; it comes back in [`Event::UserMessage`].
    pub client_id: Option<String>,
}

impl Message {
    pub(crate) fn new(input: &[Input], client_id: Option<&str>) -> Self {
        let input = input.iter().map(|part| match part {
            Input::Text(text) => json!({"type": "text", "text": text, "text_elements": []}),
            Input::LocalImage(path) => json!({"type": "localImage", "path": path.to_string_lossy()}),
        });
        Self { input: input.collect(), client_id: client_id.map(str::to_owned) }
    }
}

/// Something the caller wants done.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// Give the agent this input.
    Send(Message),
    /// Give the agent this input in the turn that runs, or not at all.
    Steer(Message),
    /// Stop the running turn.
    Interrupt,
    /// Summarise the conversation.
    Compact,
    /// Any other request.
    Call {
        /// Its method.
        method: String,
        /// Its parameters; `null` sends none.
        params: Value,
    },
}

/// What came of a line of the binary.
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
    /// The answer to `thread/resume`.
    Resume,
    /// The answer to `thread/start`.
    Start,
    /// The answer to `turn/start`: the turn's id.
    TurnStart(Message),
    /// The answer to `turn/steer`; refused, the message starts a turn of its own.
    Steer(Message),
    /// The answer to a `turn/steer` that is not tried again: refused, the message is dropped.
    Steered,
    /// The answer to `turn/interrupt`; a refusal says the turn had ended already.
    Interrupt,
    /// The answer to a request of the caller's; a refusal is reported under this name.
    Call(String),
}

/// Whether the running turn is being stopped. Messages wait meanwhile, for the turn after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// It is not.
    No,
    /// It is to be, once Codex announces it: until `turn/started` there is nothing to interrupt.
    Wanted,
    /// The interrupt is on its way.
    Asked,
}

/// The session's state.
#[derive(Debug)]
pub(crate) struct Machine {
    /// What threads and turns are started with.
    options: Options,
    next_id: u64,
    /// Our requests not yet answered.
    awaiting: HashMap<u64, Awaiting>,
    /// The binary's version, from the handshake.
    version: String,
    /// The session's thread, once it is open.
    thread: Option<String>,
    /// The handshake failed or the process ended: nothing is sent any more.
    closed: bool,
    /// The running turn of the session's thread.
    active: Option<String>,
    /// Whether Codex announced that turn (`turn/started`), and not only answered the request that began it.
    announced: bool,
    /// A `turn/start` of ours is not answered yet: the turn's id is still to come.
    starting: bool,
    /// The turn that ended last. Its end may overtake the answer to the `turn/start` that began it.
    ended: Option<String>,
    /// The running turn takes no input (it compacts or reviews).
    unsteerable: bool,
    /// Whether the running turn is being stopped.
    stop: Stop,
    /// What waits: anything until the thread is open, then only messages (for the id of the turn being
    /// started, or for the end of a turn that is being stopped or takes no input).
    held: Vec<Command>,
    /// The binary's requests the caller has not answered yet, by their key: the id to echo and the kind of
    /// answer expected.
    asked: HashMap<String, (Value, Reply)>,
}

impl Machine {
    pub(crate) fn new(options: &Options) -> Self {
        Self {
            options: options.clone(),
            next_id: 1,
            awaiting: HashMap::new(),
            version: String::new(),
            thread: None,
            closed: false,
            active: None,
            announced: false,
            starting: false,
            ended: None,
            unsteerable: false,
            stop: Stop::No,
            held: Vec::new(),
            asked: HashMap::new(),
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
            Incoming::Response { id, outcome } => {
                if let Some(awaiting) = self.awaiting.remove(&id) {
                    self.answered(awaiting, outcome, &mut out);
                }
            }
            Incoming::Notification { method, params } => self.notified(&method, params, &mut out),
            Incoming::Request { id, method, params } => self.requested(id, &method, params, &mut out),
        }
        out
    }

    /// The turns started from now on think with `model` at `effort`; `None` leaves that to Codex.
    pub(crate) fn retune(&mut self, model: Option<String>, effort: Option<String>) {
        (self.options.model, self.options.effort) = (model, effort);
    }

    /// Takes a command of the caller's and returns the lines to write for it: none while it has to wait.
    pub(crate) fn command(&mut self, command: Command) -> Vec<Value> {
        let mut lines = Vec::new();
        if self.closed {
            return lines;
        }
        // Behind what already waits, so that messages keep their order.
        self.held.push(command);
        if self.thread.is_some() {
            self.flush(&mut lines);
        }
        lines
    }

    /// The line that answers an approval request with `decision`.
    pub(crate) fn respond(&mut self, request_id: &str, decision: Decision) -> Result<Value, Error> {
        let unknown = || Error::UnknownRequest(request_id.to_owned());
        let (id, reply) = self.asked.get(request_id).ok_or_else(unknown)?;
        let result = match reply {
            Reply::Decision => json!({"decision": match decision {
                Decision::Accept => "accept",
                Decision::AcceptForSession => "acceptForSession",
                Decision::Decline => "decline",
                Decision::Cancel => "cancel",
            }}),
            Reply::Review => json!({"decision": match decision {
                Decision::Accept => json!("approved"),
                Decision::AcceptForSession => json!("approved_for_session"),
                Decision::Decline => json!({"denied": {"rejection": "The user declined."}}),
                Decision::Cancel => json!("abort"),
            }}),
            Reply::Permissions(wanted) => permissions_granted(wanted, decision),
            Reply::Answers => return Err(unknown()),
        };
        let line = json!({"id": id, "result": result});
        self.asked.remove(request_id);
        Ok(line)
    }

    /// The line that answers a question request: for each question's id, the answers given.
    pub(crate) fn answer(&mut self, request_id: &str, answers: &[(String, Vec<String>)]) -> Result<Value, Error> {
        let unknown = || Error::UnknownRequest(request_id.to_owned());
        let (id, reply) = self.asked.get(request_id).ok_or_else(unknown)?;
        if *reply != Reply::Answers {
            return Err(unknown());
        }
        let answers: Map<String, Value> =
            answers.iter().map(|(question, answers)| (question.clone(), json!({"answers": answers}))).collect();
        let line = json!({"id": id, "result": {"answers": answers}});
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

    /// Sends what waits, in the order it came, as far as it can be sent now. The thread is open.
    fn flush(&mut self, lines: &mut Vec<Value>) {
        let mut waiting = Vec::new();
        for command in mem::take(&mut self.held) {
            match command {
                // Behind a message that waits, the later ones wait too.
                Command::Send(message) if !waiting.is_empty() => waiting.push(Command::Send(message)),
                Command::Send(message) => waiting.extend(self.send(message, lines).map(Command::Send)),
                // The id of the turn being started is on its way: with it the message joins that turn.
                Command::Steer(message) if self.starting && self.stop == Stop::No => {
                    waiting.push(Command::Steer(message));
                }
                Command::Steer(message) => self.steer(&message, lines),
                Command::Interrupt => self.interrupt(lines),
                Command::Compact => {
                    let params = json!({"threadId": self.thread});
                    lines.push(self.request("thread/compact/start", params, Awaiting::Call("compacting".into())));
                }
                Command::Call { method, params } => {
                    let awaiting = Awaiting::Call(method.clone());
                    lines.push(self.request(&method, params, awaiting));
                }
            }
        }
        self.held = waiting;
    }

    /// Gives the agent a message: into the running turn, or as the start of one. Hands it back when it has to
    /// wait.
    fn send(&mut self, message: Message, lines: &mut Vec<Value>) -> Option<Message> {
        // With the id of the turn being started the message can join it; a turn that is being stopped or takes
        // no input has to end first.
        if self.starting || self.unsteerable || self.stop != Stop::No {
            return Some(message);
        }
        let Some(turn) = self.active.clone() else {
            self.start_turn(message, lines);
            return None;
        };
        let mut params = json!({"threadId": self.thread, "expectedTurnId": turn, "input": message.input});
        if let Some(client_id) = &message.client_id {
            params["clientUserMessageId"] = client_id.as_str().into();
        }
        lines.push(self.request("turn/steer", params, Awaiting::Steer(message)));
        None
    }

    /// Puts a message into the running turn. With none running, or one that is being stopped or takes no
    /// input, the message is dropped: the caller sees no [`Event::UserMessage`] for it and sends it again.
    fn steer(&mut self, message: &Message, lines: &mut Vec<Value>) {
        let Some(turn) = self.active.clone().filter(|_| !self.unsteerable && self.stop == Stop::No) else { return };
        let mut params = json!({"threadId": self.thread, "expectedTurnId": turn, "input": message.input});
        if let Some(client_id) = &message.client_id {
            params["clientUserMessageId"] = client_id.as_str().into();
        }
        lines.push(self.request("turn/steer", params, Awaiting::Steered));
    }

    /// Starts a turn with `message`. Sent while a turn runs, Codex adds the message to that turn instead.
    fn start_turn(&mut self, message: Message, lines: &mut Vec<Value>) {
        let mut params = Map::new();
        params.insert("threadId".into(), self.thread.clone().unwrap_or_default().into());
        params.insert("input".into(), message.input.clone());
        if let Some(client_id) = &message.client_id {
            params.insert("clientUserMessageId".into(), client_id.as_str().into());
        }
        if let Some(model) = filled(self.options.model.as_ref()) {
            params.insert("model".into(), model.into());
        }
        if let Some(effort) = filled(self.options.effort.as_ref()) {
            params.insert("effort".into(), effort.into());
        }
        // Without it some models keep their reasoning to themselves.
        params.insert("summary".into(), "auto".into());
        self.starting = true;
        lines.push(self.request("turn/start", params.into(), Awaiting::TurnStart(message)));
    }

    /// Stops the running turn, now or as soon as it can be. With no turn running there is nothing to stop.
    fn interrupt(&mut self, lines: &mut Vec<Value>) {
        match self.active.clone() {
            Some(turn) if self.announced => {
                if self.stop != Stop::Asked {
                    self.stop = Stop::Asked;
                    let params = json!({"threadId": self.thread, "turnId": turn});
                    lines.push(self.request("turn/interrupt", params, Awaiting::Interrupt));
                }
            }
            Some(_) => self.stop = Stop::Wanted,
            None if self.starting => self.stop = Stop::Wanted,
            None => {}
        }
    }

    /// The binary answered a request of ours.
    fn answered(&mut self, awaiting: Awaiting, outcome: Result<Value, Refusal>, out: &mut Output) {
        match (awaiting, outcome) {
            (Awaiting::Initialize, Ok(result)) => {
                self.version = parse::version_of(&text(&result, "userAgent"));
                out.lines.push(json!({"method": "initialized"}));
                let mut params = thread_params(&self.options);
                let line = match filled(self.options.resume.as_ref()) {
                    Some(thread) => {
                        params.insert("threadId".into(), thread.into());
                        // The earlier turns are the caller's to keep; the whole history would make a huge line.
                        params.insert("excludeTurns".into(), true.into());
                        self.request("thread/resume", params.into(), Awaiting::Resume)
                    }
                    None => self.request("thread/start", params.into(), Awaiting::Start),
                };
                out.lines.push(line);
            }
            (Awaiting::Initialize, Err(refusal)) => {
                self.fail(format!("Codex refused to start: {}", refusal.message), out);
            }
            (Awaiting::Resume, Ok(result)) => self.opened(&result, true, out),
            (Awaiting::Resume, Err(refusal)) => {
                let warning = format!("The earlier conversation could not be resumed: {}", refusal.message);
                out.events.push(Event::Warning(warning));
                let params = thread_params(&self.options);
                out.lines.push(self.request("thread/start", params.into(), Awaiting::Start));
            }
            (Awaiting::Start, Ok(result)) => self.opened(&result, false, out),
            (Awaiting::Start, Err(refusal)) => {
                self.fail(format!("Codex could not start a conversation: {}", refusal.message), out);
            }
            (Awaiting::TurnStart(_), Ok(result)) => {
                self.starting = false;
                let turn = text(field(&result, "turn"), "id");
                // The turn may be over already, its end having overtaken this answer; or announced already.
                if !turn.is_empty()
                    && self.ended.as_deref() != Some(turn.as_str())
                    && self.active.as_deref() != Some(turn.as_str())
                {
                    self.active = Some(turn);
                    self.announced = false;
                }
                if self.active.is_none() {
                    self.stop = Stop::No;
                }
                self.flush(&mut out.lines);
            }
            (Awaiting::TurnStart(message), Err(refusal)) => {
                self.starting = false;
                if self.active.is_none() {
                    self.stop = Stop::No;
                }
                if refusal.not_steerable {
                    // A turn that takes no input runs (its start may not even be announced yet): the message
                    // starts a turn of its own once that one ends.
                    self.unsteerable = true;
                    self.held.insert(0, Command::Send(message));
                } else if self.active.is_some() {
                    // The running turn goes on and will end by itself; only this message is lost.
                    out.events.push(Event::Warning(format!("A message could not be sent: {}", refusal.message)));
                } else {
                    // No `turn/completed` follows a turn that never began.
                    let status = TurnStatus::Failed;
                    out.events.push(Event::TurnCompleted {
                        turn_id: String::new(),
                        status,
                        error: Some(refusal.message),
                    });
                }
                self.flush(&mut out.lines);
            }
            (Awaiting::Steer(message), Err(refusal)) => {
                if refusal.not_steerable {
                    self.unsteerable = true;
                    self.held.insert(0, Command::Send(message));
                } else if self.starting || self.stop != Stop::No {
                    // It came before whatever waits now, and goes first once that can go.
                    self.held.insert(0, Command::Send(message));
                } else {
                    // The turn ended meanwhile: the message starts the next one. Not steered again, whatever
                    // runs by now, so that a message Codex will not take is not offered for ever.
                    self.start_turn(message, &mut out.lines);
                }
            }
            (Awaiting::Interrupt, Err(_)) => {
                // There was nothing to stop: what waited for the turn's end need not wait any longer.
                self.stop = Stop::No;
                self.flush(&mut out.lines);
            }
            (Awaiting::Call(method), Err(refusal)) => {
                out.events.push(Event::Warning(format!("{method} failed: {}", refusal.message)));
            }
            // A steer that is not tried again: taken, the message shows as a user message; refused, it is gone.
            (Awaiting::Steered, _) | (Awaiting::Steer(_) | Awaiting::Interrupt | Awaiting::Call(_), Ok(_)) => {}
        }
    }

    /// The thread is open: says so, and sends what waited for it.
    fn opened(&mut self, result: &Value, resumed: bool, out: &mut Output) {
        let thread = text(field(result, "thread"), "id");
        if thread.is_empty() {
            self.fail("Codex opened a conversation without an id".to_owned(), out);
            return;
        }
        self.thread = Some(thread.clone());
        out.events.push(Event::Ready {
            thread_id: thread,
            model: text(result, "model"),
            effort: opt_text(result, "reasoningEffort"),
            sandbox: text(field(result, "sandbox"), "type"),
            resumed,
            version: self.version.clone(),
        });
        self.flush(&mut out.lines);
    }

    fn fail(&mut self, why: String, out: &mut Output) {
        self.close();
        out.fatal = Some(why);
    }

    /// The binary sent news.
    fn notified(&mut self, method: &str, params: Value, out: &mut Output) {
        let Some((thread, event)) = parse::notification(method, params) else { return };
        if let Event::RequestResolved { request_id } = &event {
            // Settled elsewhere, or withdrawn: it can no longer be answered here. Never wrapped, since the
            // request was not either.
            self.asked.remove(request_id);
            out.events.push(event);
            return;
        }
        // Before the thread is open there is no other it could be about.
        if let (Some(thread), Some(own)) = (thread, &self.thread)
            && thread != *own
        {
            out.events.push(Event::Sub { thread_id: thread, event: Box::new(event) });
            return;
        }
        let (started, ended) = match &event {
            Event::TurnStarted { turn_id } => (Some(turn_id.clone()), None),
            Event::TurnCompleted { turn_id, .. } => (None, Some(turn_id.clone())),
            _ => (None, None),
        };
        out.events.push(event);
        if let Some(turn) = started {
            self.active = Some(turn);
            self.announced = true;
            if self.stop == Stop::Wanted {
                self.stop = Stop::No;
                self.interrupt(&mut out.lines);
            }
        }
        if let Some(turn) = ended {
            if self.active.as_ref() == Some(&turn) {
                self.active = None;
                self.announced = false;
            }
            self.ended = Some(turn);
            // Whatever kept messages waiting is over, also when this is not the turn taken for running:
            // better a message offered once too often than one that waits for ever.
            self.unsteerable = false;
            self.stop = Stop::No;
            self.flush(&mut out.lines);
        }
    }

    /// The binary asks something: the caller is asked in turn, or the answer is given at once.
    fn requested(&mut self, id: Value, method: &str, params: Value, out: &mut Output) {
        let key = parse::request_key(&id);
        match parse::request(key.clone(), method, params) {
            Asked::Approval(request, reply) => {
                self.asked.insert(key, (id, reply));
                out.events.push(Event::Approval(request));
            }
            Asked::Question(request) => {
                self.asked.insert(key, (id, Reply::Answers));
                out.events.push(Event::Question(request));
            }
            Asked::Elicitation => {
                out.lines.push(json!({"id": id, "result": {"action": "decline", "content": null, "_meta": null}}));
            }
            // Answered so that Codex never waits on us; this also refuses to hand it tokens.
            Asked::Unsupported => out.lines.push(json!({
                "id": id,
                "error": {"code": METHOD_NOT_FOUND, "message": "not supported by this client"},
            })),
        }
    }
}

/// A request as it is written: `params` of `null` are left out, as the methods that take none want it.
pub(crate) fn request_line(method: &str, id: u64, params: Value) -> Value {
    let mut request = Map::new();
    request.insert("method".into(), method.into());
    request.insert("id".into(), id.into());
    if !params.is_null() {
        request.insert("params".into(), params);
    }
    request.into()
}

/// `value` unless it is missing or empty.
fn filled(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|value| !value.is_empty())
}

/// The parameters of `initialize`.
pub(crate) fn initialize_params(options: &Options) -> Value {
    let version = if options.client_version.is_empty() { env!("CARGO_PKG_VERSION") } else { &options.client_version };
    json!({
        "clientInfo": {"name": CLIENT_NAME, "title": CLIENT_TITLE, "version": version},
        // The experimental API carries questions to the user and what sub-agents do. No attestation: this is
        // not one of OpenAI's own clients.
        "capabilities": {"experimentalApi": true, "requestAttestation": false},
    })
}

/// What `thread/start` and `thread/resume` both take.
fn thread_params(options: &Options) -> Map<String, Value> {
    let mut params = Map::new();
    if let Some(cwd) = &options.cwd {
        params.insert("cwd".into(), cwd.to_string_lossy().into());
    }
    params.insert("approvalPolicy".into(), options.approval.as_str().into());
    params.insert("sandbox".into(), options.sandbox.as_str().into());
    if let Some(instructions) = filled(options.instructions.as_ref()) {
        params.insert("developerInstructions".into(), instructions.into());
    }
    if let Some(model) = filled(options.model.as_ref()) {
        params.insert("model".into(), model.into());
    }
    params
}

/// The answer to a permissions request: what is granted out of what was `wanted`, and for how long.
fn permissions_granted(wanted: &Value, decision: Decision) -> Value {
    let scope = match decision {
        Decision::Accept => "turn",
        Decision::AcceptForSession => "session",
        Decision::Decline | Decision::Cancel => return json!({"permissions": {}, "scope": "turn"}),
    };
    // Only the parts asked for: a `null` part is not a grant Codex accepts.
    let granted: Map<String, Value> = ["network", "fileSystem"]
        .into_iter()
        .filter_map(|part| {
            wanted.get(part).filter(|value| !value.is_null()).map(|value| (part.to_owned(), value.clone()))
        })
        .collect();
    json!({"permissions": granted, "scope": scope})
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Approval, ApprovalKind, Sandbox};

    const THREAD: &str = "01a1cb92-b7b0-75c2-8488-611c6d419a4a";
    const TURN: &str = "01a1cb92-b7d1-7f63-afd5-752fc44d7bdf";
    const USER_AGENT: &str = "atroxabot/0.167.1 (Windows 10.0.26200; x86_64) xterm-256color (atroxabot; 0.1.0)";

    fn text_message(text: &str, client_id: Option<&str>) -> Command {
        Command::Send(Message::new(&[Input::Text(text.to_owned())], client_id))
    }

    fn input(text: &str) -> Value {
        json!([{"type": "text", "text": text, "text_elements": []}])
    }

    /// Feeds the machine a message of the binary.
    fn receive(machine: &mut Machine, message: &Value) -> Output {
        machine.receive(&message.to_string())
    }

    fn ok(machine: &mut Machine, id: u64, result: &Value) -> Output {
        receive(machine, &json!({"id": id, "result": result}))
    }

    fn refuse(machine: &mut Machine, id: u64, message: &str) -> Output {
        receive(machine, &json!({"id": id, "error": {"code": -32600, "message": message}}))
    }

    fn notify(machine: &mut Machine, method: &str, params: &Value) -> Output {
        receive(machine, &json!({"method": method, "params": params, "emittedAtMs": 1_794_416_752_678_u64}))
    }

    fn thread_result(thread: &str) -> Value {
        json!({
            "thread": {"id": thread, "preview": "", "status": {"type": "idle"}, "turns": []},
            "model": "gpt-6.1-sol",
            "modelProvider": "openai",
            "approvalPolicy": "on-request",
            "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false},
            "reasoningEffort": "medium",
        })
    }

    fn turn(id: &str, status: &str) -> Value {
        json!({"threadId": THREAD, "turn": {"id": id, "items": [], "status": status, "error": null}})
    }

    /// A machine whose thread is open; its next request has id 3.
    fn ready() -> Machine {
        let mut machine = Machine::new(&Options::default());
        machine.hello();
        ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        let out = ok(&mut machine, 2, &thread_result(THREAD));
        assert!(matches!(out.events[..], [Event::Ready { .. }]));
        machine
    }

    /// A machine with a turn running that was started by "ping"; its next request has id 4.
    fn running() -> Machine {
        let mut machine = ready();
        assert_eq!(machine.command(text_message("ping", None)).len(), 1);
        ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}}));
        notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        machine
    }

    #[test]
    fn handshake() {
        let options = Options {
            cwd: Some(PathBuf::from("work")),
            model: Some("gpt-6.1-sol".into()),
            approval: Approval::Untrusted,
            sandbox: Sandbox::ReadOnly,
            instructions: Some("You are Scout.".into()),
            client_version: "0.1.0".into(),
            ..Options::default()
        };
        let mut machine = Machine::new(&options);
        let hello = json!({"method": "initialize", "id": 1, "params": {
            "clientInfo": {"name": "atroxabot", "title": "AtroxaBot", "version": "0.1.0"},
            "capabilities": {"experimentalApi": true, "requestAttestation": false},
        }});
        assert_eq!(machine.hello(), hello);

        let out =
            ok(&mut machine, 1, &json!({"userAgent": USER_AGENT, "codexHome": "C:\\home", "platformOs": "windows"}));
        let start = json!({"method": "thread/start", "id": 2, "params": {
            "cwd": "work", "approvalPolicy": "untrusted", "sandbox": "read-only",
            "developerInstructions": "You are Scout.", "model": "gpt-6.1-sol",
        }});
        assert_eq!(out, Output { lines: vec![json!({"method": "initialized"}), start], ..Output::default() });

        let out = ok(&mut machine, 2, &thread_result(THREAD));
        let ready = Event::Ready {
            thread_id: THREAD.into(),
            model: "gpt-6.1-sol".into(),
            effort: Some("medium".into()),
            sandbox: "workspaceWrite".into(),
            resumed: false,
            version: "0.167.1".into(),
        };
        assert_eq!(out, Output { events: vec![ready], ..Output::default() });
    }

    #[test]
    fn default_options_send_only_the_policies() {
        let mut machine = Machine::new(&Options::default());
        assert_eq!(machine.hello()["params"]["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
        let out = ok(&mut machine, 1, &json!({}));
        let params = json!({"approvalPolicy": "on-request", "sandbox": "workspace-write"});
        assert_eq!(out.lines[1], json!({"method": "thread/start", "id": 2, "params": params}));
        assert_eq!(Sandbox::FullAccess.as_str(), "danger-full-access");
        assert_eq!(Approval::Never.as_str(), "never");
    }

    #[test]
    fn resuming() {
        let options = Options { resume: Some(THREAD.into()), ..Options::default() };
        let mut machine = Machine::new(&options);
        machine.hello();
        let out = ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        let params = json!({"threadId": THREAD, "excludeTurns": true, "approvalPolicy": "on-request",
            "sandbox": "workspace-write"});
        assert_eq!(out.lines[1], json!({"method": "thread/resume", "id": 2, "params": params}));
        let out = ok(&mut machine, 2, &thread_result(THREAD));
        assert!(matches!(&out.events[..], [Event::Ready { resumed: true, thread_id, .. }] if thread_id == THREAD));
    }

    #[test]
    fn a_thread_that_cannot_be_resumed_is_replaced_by_a_new_one() {
        let options = Options { resume: Some("gone".into()), ..Options::default() };
        let mut machine = Machine::new(&options);
        machine.hello();
        ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        let out = refuse(&mut machine, 2, "no rollout found for thread id gone");
        let warning = "The earlier conversation could not be resumed: no rollout found for thread id gone";
        assert_eq!(out.events, [Event::Warning(warning.into())]);
        let params = json!({"approvalPolicy": "on-request", "sandbox": "workspace-write"});
        assert_eq!(out.lines, [json!({"method": "thread/start", "id": 3, "params": params})]);
        assert_eq!(out.fatal, None);
        let out = ok(&mut machine, 3, &thread_result("new-thread"));
        assert!(
            matches!(&out.events[..], [Event::Ready { resumed: false, thread_id, .. }] if thread_id == "new-thread")
        );
    }

    #[test]
    fn a_failed_handshake_ends_the_session() {
        let mut machine = Machine::new(&Options::default());
        machine.hello();
        let out = refuse(&mut machine, 1, "unsupported client");
        assert_eq!(out.fatal.as_deref(), Some("Codex refused to start: unsupported client"));
        assert!(out.events.is_empty() && out.lines.is_empty());
        // Nothing is sent or reported after that.
        assert!(machine.command(text_message("ping", None)).is_empty());
        assert_eq!(notify(&mut machine, "turn/started", &turn(TURN, "inProgress")), Output::default());

        let mut machine = Machine::new(&Options::default());
        machine.hello();
        ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        let out = refuse(&mut machine, 2, "no such model");
        assert_eq!(out.fatal.as_deref(), Some("Codex could not start a conversation: no such model"));

        let mut machine = Machine::new(&Options::default());
        machine.hello();
        ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        let out = ok(&mut machine, 2, &json!({"thread": {}}));
        assert_eq!(out.fatal.as_deref(), Some("Codex opened a conversation without an id"));
    }

    #[test]
    fn what_is_sent_early_waits_for_the_thread() {
        let options = Options { model: Some("gpt-6.1-sol".into()), effort: Some("low".into()), ..Options::default() };
        let mut machine = Machine::new(&options);
        machine.hello();
        assert!(machine.command(text_message("ping", Some("client-1"))).is_empty());
        assert!(machine.command(Command::Call { method: "skills/list".into(), params: json!({}) }).is_empty());
        assert!(machine.command(Command::Compact).is_empty());
        assert!(machine.command(text_message("more", None)).is_empty());
        assert_eq!(ok(&mut machine, 1, &json!({"userAgent": USER_AGENT})).lines.len(), 2);

        let out = ok(&mut machine, 2, &thread_result(THREAD));
        let start = json!({"method": "turn/start", "id": 3, "params": {
            "threadId": THREAD, "input": input("ping"), "clientUserMessageId": "client-1",
            "model": "gpt-6.1-sol", "effort": "low", "summary": "auto",
        }});
        let call = json!({"method": "skills/list", "id": 4, "params": {}});
        let compact = json!({"method": "thread/compact/start", "id": 5, "params": {"threadId": THREAD}});
        // In order; the second message waits for the turn's id.
        assert_eq!(out.lines, [start, call, compact]);

        let out = ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}}));
        let steer = json!({"method": "turn/steer", "id": 6, "params": {
            "threadId": THREAD, "expectedTurnId": TURN, "input": input("more"),
        }});
        assert_eq!(out, Output { lines: vec![steer], ..Output::default() });
    }

    #[test]
    fn another_model_or_effort_goes_with_the_next_turn() {
        let options = Options { model: Some("gpt-6.1-sol".into()), effort: Some("low".into()), ..Options::default() };
        let mut machine = Machine::new(&options);
        machine.hello();
        ok(&mut machine, 1, &json!({"userAgent": USER_AGENT}));
        ok(&mut machine, 2, &thread_result(THREAD));
        let started = |machine: &mut Machine| {
            let lines = machine.command(text_message("ping", None));
            (lines[0]["method"].clone(), lines[0]["params"]["model"].clone(), lines[0]["params"]["effort"].clone())
        };
        machine.retune(Some("gpt-6-astra".into()), Some("high".into()));
        assert_eq!(started(&mut machine), (json!("turn/start"), json!("gpt-6-astra"), json!("high")));
        // Left to Codex, neither is named.
        let mut machine = ready();
        machine.retune(None, None);
        assert_eq!(started(&mut machine), (json!("turn/start"), Value::Null, Value::Null));
    }

    #[test]
    fn a_message_starts_a_turn_or_joins_the_running_one() {
        let mut machine = ready();
        let lines = machine.command(text_message("ping", None));
        let params = json!({"threadId": THREAD, "input": input("ping"), "summary": "auto"});
        assert_eq!(lines, [json!({"method": "turn/start", "id": 3, "params": params})]);
        ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}}));
        let out = notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        assert_eq!(out.events, [Event::TurnStarted { turn_id: TURN.into() }]);

        let lines = machine.command(text_message("more", Some("client-2")));
        let params = json!({"threadId": THREAD, "expectedTurnId": TURN, "input": input("more"),
            "clientUserMessageId": "client-2"});
        assert_eq!(lines, [json!({"method": "turn/steer", "id": 4, "params": params})]);
        assert_eq!(ok(&mut machine, 4, &json!({"turnId": TURN})), Output::default());

        let out = notify(&mut machine, "turn/completed", &turn(TURN, "completed"));
        let completed = Event::TurnCompleted { turn_id: TURN.into(), status: TurnStatus::Completed, error: None };
        assert_eq!(out, Output { events: vec![completed], ..Output::default() });
        // The next message starts a turn again.
        assert_eq!(machine.command(text_message("again", None))[0]["method"], "turn/start");
    }

    #[test]
    fn images_are_sent_by_path() {
        let message = Message::new(&[Input::Text("look".into()), Input::LocalImage(PathBuf::from("shot.png"))], None);
        let expected =
            json!([{"type": "text", "text": "look", "text_elements": []}, {"type": "localImage", "path": "shot.png"}]);
        assert_eq!(message.input, expected);
    }

    #[test]
    fn a_message_for_the_running_turn_only_never_starts_one() {
        let steer = |text: &str| Command::Steer(Message::new(&[Input::Text(text.to_owned())], Some("c1")));
        let mut machine = ready();
        // No turn runs: there is nothing to put it into.
        assert!(machine.command(steer("too early")).is_empty());
        // A turn is being started: it waits for the turn's id, then joins the turn.
        assert_eq!(machine.command(text_message("go", None))[0]["method"], "turn/start");
        assert!(machine.command(steer("wait for me")).is_empty());
        let out = ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}}));
        let params = json!({"threadId": THREAD, "expectedTurnId": TURN, "input": input("wait for me"),
            "clientUserMessageId": "c1"});
        assert_eq!(out.lines, [json!({"method": "turn/steer", "id": 4, "params": params})]);
        // Refused (the turn ended meanwhile): it is dropped, and no turn is started for it.
        assert_eq!(refuse(&mut machine, 4, "no active turn to steer"), Output::default());
        // While the turn is being stopped it is dropped too, and nothing of it is left for the next turn.
        notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        assert_eq!(machine.command(Command::Interrupt)[0]["method"], "turn/interrupt");
        assert!(machine.command(steer("too late")).is_empty());
        let ended = notify(&mut machine, "turn/completed", &turn(TURN, "interrupted"));
        assert!(ended.lines.is_empty(), "{:?}", ended.lines);
        // A turn that takes no input drops it as well.
        let mut machine = running();
        machine.unsteerable = true;
        assert!(machine.command(steer("not now")).is_empty());
        assert!(notify(&mut machine, "turn/completed", &turn(TURN, "completed")).lines.is_empty());
    }

    #[test]
    fn a_steer_that_comes_too_late_starts_a_turn() {
        let mut machine = running();
        let lines = machine.command(text_message("late", Some("client-9")));
        assert_eq!(lines[0]["method"], "turn/steer");
        notify(&mut machine, "turn/completed", &turn(TURN, "completed"));
        let out = refuse(&mut machine, 4, "no active turn to steer");
        let params =
            json!({"threadId": THREAD, "input": input("late"), "clientUserMessageId": "client-9", "summary": "auto"});
        assert_eq!(
            out,
            Output { lines: vec![json!({"method": "turn/start", "id": 5, "params": params})], ..Output::default() }
        );
    }

    #[test]
    fn a_steer_refused_before_the_turns_end_is_known_starts_a_turn_too() {
        let mut machine = running();
        machine.command(text_message("late", None));
        let out = refuse(&mut machine, 4, "expected active turn id `x` but found `y`");
        assert_eq!(out.lines[0]["method"], "turn/start");
        // While that start is on its way further messages wait for its turn.
        assert!(machine.command(text_message("later", None)).is_empty());
        let out = ok(&mut machine, 5, &json!({"turn": {"id": "turn-2"}}));
        assert_eq!(out.lines[0]["method"], "turn/steer");
        assert_eq!(out.lines[0]["params"]["expectedTurnId"], "turn-2");
        assert_eq!(out.lines[0]["params"]["input"], input("later"));
    }

    #[test]
    fn a_turn_that_cannot_be_started_ends_as_failed() {
        let mut machine = ready();
        machine.command(text_message("ping", None));
        assert!(machine.command(text_message("second", None)).is_empty());
        let out = refuse(&mut machine, 3, "model not available");
        let failed = Event::TurnCompleted {
            turn_id: String::new(),
            status: TurnStatus::Failed,
            error: Some("model not available".into()),
        };
        assert_eq!(out.events, [failed]);
        // The message that waited for that turn starts one of its own.
        assert_eq!(out.lines.len(), 1);
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["input"]),
            (&json!("turn/start"), &input("second"))
        );
    }

    #[test]
    fn a_message_lost_while_a_turn_runs_does_not_end_that_turn() {
        let mut machine = running();
        machine.command(text_message("late", None));
        // The steer is refused, and so is the start that follows it, while the turn still runs.
        refuse(&mut machine, 4, "invalid input");
        let out = refuse(&mut machine, 5, "invalid input");
        assert_eq!(out.events, [Event::Warning("A message could not be sent: invalid input".into())]);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn interrupting() {
        // Nothing runs: nothing to stop.
        let mut machine = ready();
        assert!(machine.command(Command::Interrupt).is_empty());

        // Codex refuses to interrupt a turn it has not announced yet: the interrupt waits for `turn/started`.
        machine.command(text_message("ping", None));
        assert!(machine.command(Command::Interrupt).is_empty());
        assert_eq!(ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}})), Output::default());
        let out = notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        let params = json!({"threadId": THREAD, "turnId": TURN});
        assert_eq!(out.lines, [json!({"method": "turn/interrupt", "id": 4, "params": params})]);
        // Asked once is enough.
        assert!(machine.command(Command::Interrupt).is_empty());
        assert_eq!(ok(&mut machine, 4, &json!({})), Output::default());
        let out = notify(&mut machine, "turn/completed", &turn(TURN, "interrupted"));
        assert!(matches!(out.events[..], [Event::TurnCompleted { status: TurnStatus::Interrupted, .. }]));
        assert!(out.lines.is_empty());
        assert!(machine.command(Command::Interrupt).is_empty());

        // A turn that runs is stopped at once.
        let mut machine = running();
        let lines = machine.command(Command::Interrupt);
        assert_eq!(lines, [json!({"method": "turn/interrupt", "id": 4, "params": params})]);
    }

    #[test]
    fn an_interrupt_before_the_turn_is_known_stops_it_once_it_is_announced() {
        let mut machine = ready();
        machine.command(text_message("ping", None));
        // The announcement may come before the answer: either way the id is known by then.
        machine.command(Command::Interrupt);
        let out = notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        assert_eq!(out.lines.len(), 1);
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["turnId"]),
            (&json!("turn/interrupt"), &json!(TURN))
        );
        assert_eq!(ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}})), Output::default());

        // A turn that could not be started leaves nothing to stop.
        let mut machine = ready();
        machine.command(text_message("ping", None));
        machine.command(Command::Interrupt);
        refuse(&mut machine, 3, "model not available");
        assert_eq!(machine.command(text_message("again", None))[0]["method"], "turn/start");
    }

    #[test]
    fn what_is_sent_while_a_turn_is_stopped_starts_the_next_one() {
        let mut machine = running();
        machine.command(Command::Interrupt);
        assert!(machine.command(text_message("instead", None)).is_empty());
        assert!(machine.command(text_message("and more", None)).is_empty());
        ok(&mut machine, 4, &json!({}));
        let out = notify(&mut machine, "turn/completed", &turn(TURN, "interrupted"));
        assert_eq!(out.lines.len(), 1);
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["input"]),
            (&json!("turn/start"), &input("instead"))
        );
        let out = ok(&mut machine, 5, &json!({"turn": {"id": "turn-2"}}));
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["input"]),
            (&json!("turn/steer"), &input("and more"))
        );

        // A message that waited for the id of a turn stopped before it was known waits for the next turn too.
        let mut machine = ready();
        machine.command(text_message("ping", None));
        machine.command(text_message("more", None));
        machine.command(Command::Interrupt);
        assert_eq!(ok(&mut machine, 3, &json!({"turn": {"id": TURN}})), Output::default());
        assert_eq!(
            notify(&mut machine, "turn/started", &turn(TURN, "inProgress")).lines[0]["method"],
            "turn/interrupt"
        );
        let out = notify(&mut machine, "turn/completed", &turn(TURN, "interrupted"));
        assert_eq!((&out.lines[0]["method"], &out.lines[0]["params"]["input"]), (&json!("turn/start"), &input("more")));

        // An interrupt that finds nothing to stop keeps nothing waiting.
        let mut machine = running();
        machine.command(Command::Interrupt);
        assert!(machine.command(text_message("instead", None)).is_empty());
        let out = refuse(&mut machine, 4, "no active turn to interrupt");
        assert!(out.events.is_empty());
        assert_eq!(out.lines[0]["method"], "turn/steer");
    }

    #[test]
    fn a_turn_that_ends_before_its_start_is_answered_is_not_taken_for_running() {
        let mut machine = ready();
        machine.command(text_message("ping", None));
        notify(&mut machine, "turn/started", &turn(TURN, "inProgress"));
        notify(&mut machine, "turn/completed", &turn(TURN, "failed"));
        ok(&mut machine, 3, &json!({"turn": {"id": TURN, "status": "inProgress"}}));
        assert_eq!(machine.command(text_message("again", None))[0]["method"], "turn/start");
    }

    #[test]
    fn messages_wait_for_a_turn_that_takes_no_input() {
        let mut machine = ready();
        let lines = machine.command(Command::Compact);
        assert_eq!(lines, [json!({"method": "thread/compact/start", "id": 3, "params": {"threadId": THREAD}})]);
        // Sent before the compaction's turn is announced, as a start that Codex refuses.
        assert_eq!(machine.command(text_message("first", None))[0]["method"], "turn/start");
        ok(&mut machine, 3, &json!({}));
        let out = receive(
            &mut machine,
            &json!({"id": 4, "error": {"code": -32603,
            "message": "failed to submit turn input: ActiveTurnNotSteerable { turn_kind: Compact }"}}),
        );
        assert_eq!(out, Output::default());
        notify(&mut machine, "turn/started", &turn("compact-turn", "inProgress"));
        // Further ones are not even tried.
        assert!(machine.command(text_message("second", None)).is_empty());

        let out = notify(&mut machine, "turn/completed", &turn("compact-turn", "completed"));
        assert!(matches!(out.events[..], [Event::TurnCompleted { .. }]));
        assert_eq!(out.lines.len(), 1);
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["input"]),
            (&json!("turn/start"), &input("first"))
        );
        let out = ok(&mut machine, 5, &json!({"turn": {"id": TURN}}));
        assert_eq!(
            (&out.lines[0]["method"], &out.lines[0]["params"]["input"]),
            (&json!("turn/steer"), &input("second"))
        );
    }

    #[test]
    fn a_steer_into_a_compaction_waits_for_its_end() {
        let mut machine = ready();
        notify(&mut machine, "turn/started", &turn("compact-turn", "inProgress"));
        assert_eq!(machine.command(text_message("hello", None))[0]["method"], "turn/steer");
        let out = receive(
            &mut machine,
            &json!({"id": 3, "error": {"code": -32600, "message": "cannot steer a compact turn",
            "data": {"message": "cannot steer a compact turn",
                "codexErrorInfo": {"activeTurnNotSteerable": {"turnKind": "compact"}}}}}),
        );
        assert_eq!(out, Output::default());
        let out = notify(&mut machine, "turn/completed", &turn("compact-turn", "completed"));
        assert_eq!(out.lines[0]["method"], "turn/start");
    }

    #[test]
    fn other_requests_report_only_their_failure() {
        let mut machine = ready();
        let lines = machine.command(Command::Call { method: "account/rateLimits/read".into(), params: Value::Null });
        assert_eq!(lines, [json!({"method": "account/rateLimits/read", "id": 3})]);
        let out = refuse(&mut machine, 3, "codex account authentication required to read rate limits");
        let warning = "account/rateLimits/read failed: codex account authentication required to read rate limits";
        assert_eq!(out.events, [Event::Warning(warning.into())]);

        machine.command(Command::Call { method: "skills/list".into(), params: json!({"cwds": []}) });
        assert_eq!(ok(&mut machine, 4, &json!({"data": []})), Output::default());
        machine.command(Command::Compact);
        assert_eq!(refuse(&mut machine, 5, "busy").events, [Event::Warning("compacting failed: busy".into())]);
        // An answer to nothing, or a second one, is dropped.
        assert_eq!(ok(&mut machine, 4, &json!({})), Output::default());
        assert_eq!(refuse(&mut machine, 99, "x"), Output::default());
    }

    #[test]
    fn other_threads_are_reported_as_sub_agents() {
        let mut machine = running();
        let delta = |thread: &str| json!({"threadId": thread, "turnId": "u", "itemId": "i1", "delta": "hi"});
        let text = || Event::TextDelta { item_id: "i1".into(), text: "hi".into() };
        assert_eq!(notify(&mut machine, "item/agentMessage/delta", &delta(THREAD)).events, [text()]);
        let sub = Event::Sub { thread_id: "sub-thread".into(), event: Box::new(text()) };
        assert_eq!(notify(&mut machine, "item/agentMessage/delta", &delta("sub-thread")).events, [sub]);

        // A sub-agent's turns do not move the session's own.
        let ended = json!({"threadId": "sub-thread", "turn": {"id": TURN, "status": "completed", "error": null}});
        let out = notify(&mut machine, "turn/completed", &ended);
        assert!(
            matches!(&out.events[..], [Event::Sub { event, .. }] if matches!(**event, Event::TurnCompleted { .. }))
        );
        let started = json!({"threadId": "sub-thread", "turn": {"id": "sub-turn", "status": "inProgress"}});
        notify(&mut machine, "turn/started", &started);
        let lines = machine.command(text_message("more", None));
        assert_eq!(lines[0]["params"]["expectedTurnId"], TURN);

        // News without a thread is the session's.
        let limits = json!({"rateLimits": {"primary": {"usedPercent": 10}, "secondary": null}});
        let out = notify(&mut machine, "account/rateLimits/updated", &limits);
        assert!(matches!(out.events[..], [Event::RateLimits(_)]));
        // What is not modelled is not reported, whatever the thread.
        assert_eq!(
            notify(&mut machine, "thread/status/changed", &json!({"threadId": "sub-thread"})),
            Output::default()
        );
    }

    #[test]
    fn approvals_are_answered_in_their_own_format() {
        let mut machine = running();
        let ask = |machine: &mut Machine, id: Value, method: &str| {
            let params = json!({"threadId": THREAD, "turnId": TURN, "itemId": "i1", "conversationId": THREAD,
                "callId": "i1", "command": "cargo test", "cwd": "/w"});
            let out = receive(machine, &json!({"method": method, "id": id, "params": params}));
            assert!(out.lines.is_empty());
            match &out.events[..] {
                [Event::Approval(request)] => request.clone(),
                other => panic!("{other:?}"),
            }
        };

        let request = ask(&mut machine, json!(7), "item/commandExecution/requestApproval");
        assert_eq!((request.request_id.as_str(), request.kind), ("7", ApprovalKind::Command));
        let line = machine.respond("7", Decision::Accept).unwrap();
        // The id goes back as the number it was.
        assert_eq!(line, json!({"id": 7, "result": {"decision": "accept"}}));
        assert!(matches!(machine.respond("7", Decision::Accept), Err(Error::UnknownRequest(id)) if id == "7"));

        let request = ask(&mut machine, json!("srv-8"), "item/fileChange/requestApproval");
        assert_eq!((request.request_id.as_str(), request.kind), ("srv-8", ApprovalKind::FileChange));
        let line = machine.respond("srv-8", Decision::AcceptForSession).unwrap();
        assert_eq!(line, json!({"id": "srv-8", "result": {"decision": "acceptForSession"}}));

        for (id, decision, word) in [(9, Decision::Decline, "decline"), (10, Decision::Cancel, "cancel")] {
            ask(&mut machine, json!(id), "item/commandExecution/requestApproval");
            let line = machine.respond(&id.to_string(), decision).unwrap();
            assert_eq!(line, json!({"id": id, "result": {"decision": word}}));
        }

        let legacy = [
            (Decision::Accept, json!("approved")),
            (Decision::AcceptForSession, json!("approved_for_session")),
            (Decision::Decline, json!({"denied": {"rejection": "The user declined."}})),
            (Decision::Cancel, json!("abort")),
        ];
        for (decision, word) in legacy {
            for method in ["execCommandApproval", "applyPatchApproval"] {
                ask(&mut machine, json!(11), method);
                assert_eq!(machine.respond("11", decision).unwrap(), json!({"id": 11, "result": {"decision": word}}));
            }
        }
    }

    #[test]
    fn permissions_are_granted_as_asked() {
        let mut machine = running();
        let ask = |machine: &mut Machine, permissions: Value| {
            let params = json!({"threadId": THREAD, "turnId": TURN, "itemId": "i1", "cwd": "/w", "reason": null,
                "permissions": permissions});
            let out =
                receive(machine, &json!({"method": "item/permissions/requestApproval", "id": 12, "params": params}));
            assert!(matches!(&out.events[..], [Event::Approval(request)] if request.kind == ApprovalKind::Permissions));
        };
        let network = json!({"network": {"enabled": true}, "fileSystem": null});
        ask(&mut machine, network.clone());
        let line = machine.respond("12", Decision::Accept).unwrap();
        let granted = json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"});
        assert_eq!(line, json!({"id": 12, "result": granted}));

        let both = json!({"network": {"enabled": true}, "fileSystem": {"read": null, "write": ["/other"]}});
        ask(&mut machine, both.clone());
        let line = machine.respond("12", Decision::AcceptForSession).unwrap();
        assert_eq!(line, json!({"id": 12, "result": {"permissions": both, "scope": "session"}}));

        for decision in [Decision::Decline, Decision::Cancel] {
            ask(&mut machine, network.clone());
            let line = machine.respond("12", decision).unwrap();
            assert_eq!(line, json!({"id": 12, "result": {"permissions": {}, "scope": "turn"}}));
        }
        // A request that names no permissions grants none.
        ask(&mut machine, json!("junk"));
        let line = machine.respond("12", Decision::Accept).unwrap();
        assert_eq!(line, json!({"id": 12, "result": {"permissions": {}, "scope": "turn"}}));
    }

    #[test]
    fn questions_are_answered_by_question() {
        let mut machine = running();
        let question = json!({"id": "db", "header": "Database", "question": "Which?", "isOther": false,
            "isSecret": false, "options": [{"label": "SQLite", "description": "a file"}]});
        let params =
            json!({"threadId": THREAD, "turnId": TURN, "itemId": "i1", "questions": [question], "isBlocking": true});
        let out = receive(&mut machine, &json!({"method": "item/tool/requestUserInput", "id": 13, "params": params}));
        assert!(matches!(&out.events[..], [Event::Question(request)] if request.request_id == "13"));

        // Each kind of request takes its own kind of answer.
        assert!(matches!(machine.respond("13", Decision::Accept), Err(Error::UnknownRequest(_))));
        let answers = [("db".to_owned(), vec!["SQLite".to_owned()]), ("other".to_owned(), Vec::new())];
        let line = machine.answer("13", &answers).unwrap();
        let expected = json!({"answers": {"db": {"answers": ["SQLite"]}, "other": {"answers": []}}});
        assert_eq!(line, json!({"id": 13, "result": expected}));
        assert!(matches!(machine.answer("13", &answers), Err(Error::UnknownRequest(_))));

        let params = json!({"threadId": THREAD, "turnId": TURN, "itemId": "i2", "command": "ls"});
        receive(&mut machine, &json!({"method": "item/commandExecution/requestApproval", "id": 14, "params": params}));
        assert!(matches!(machine.answer("14", &answers), Err(Error::UnknownRequest(_))));
        assert!(machine.respond("14", Decision::Decline).is_ok());
    }

    #[test]
    fn requests_this_client_does_not_serve_are_answered_at_once() {
        let mut machine = running();
        let params =
            json!({"threadId": THREAD, "turnId": null, "serverName": "docs", "mode": "form", "message": "Name?"});
        let out =
            receive(&mut machine, &json!({"method": "mcpServer/elicitation/request", "id": 15, "params": params}));
        let declined = json!({"id": 15, "result": {"action": "decline", "content": null, "_meta": null}});
        assert_eq!(out, Output { lines: vec![declined], ..Output::default() });

        let methods =
            ["item/tool/call", "account/chatgptAuthTokens/refresh", "attestation/generate", "some/new/request"];
        for (id, method) in [json!(16), json!("srv-17"), json!(18), json!(19)].into_iter().zip(methods) {
            let out = receive(&mut machine, &json!({"method": method, "id": id, "params": {"reason": "unauthorized"}}));
            let refused = json!({"id": id, "error": {"code": -32601, "message": "not supported by this client"}});
            assert_eq!(out, Output { lines: vec![refused], ..Output::default() }, "{method}");
        }
        // Nothing of it waits for the caller.
        assert!(matches!(machine.respond("16", Decision::Accept), Err(Error::UnknownRequest(_))));
    }

    #[test]
    fn a_request_settled_elsewhere_can_no_longer_be_answered() {
        let mut machine = running();
        let params = json!({"threadId": "sub-thread", "turnId": TURN, "itemId": "i1", "command": "ls"});
        let out = receive(
            &mut machine,
            &json!({"method": "item/commandExecution/requestApproval", "id": 20, "params": params}),
        );
        // A sub-agent's request is asked like any other; it names its thread.
        assert!(matches!(&out.events[..], [Event::Approval(request)] if request.thread_id == "sub-thread"));
        let out = notify(&mut machine, "serverRequest/resolved", &json!({"threadId": "sub-thread", "requestId": 20}));
        assert_eq!(out.events, [Event::RequestResolved { request_id: "20".into() }]);
        assert!(matches!(machine.respond("20", Decision::Accept), Err(Error::UnknownRequest(_))));
    }

    #[test]
    fn hostile_lines_change_nothing() {
        let mut machine = running();
        let lines = [
            "",
            "not json",
            "{",
            "[1,2,3]",
            "{\"id\":\"3\",\"result\":{}}",
            "{\"id\":4,\"result\":\"text\"}",
            "{\"method\":\"turn/completed\"}",
            "{\"method\":\"turn/completed\",\"params\":[]}",
            "{\"method\":\"item/started\",\"params\":{\"item\":7}}",
            "{\"method\":null,\"params\":{}}",
        ];
        for line in lines {
            let out = machine.receive(line);
            assert!(out.lines.is_empty() && out.fatal.is_none(), "{line}");
        }
        // The turn still runs: a turn that ends without an id is not it.
        assert_eq!(machine.command(text_message("more", None))[0]["method"], "turn/steer");
    }

    #[test]
    fn a_closed_machine_is_silent() {
        let mut machine = running();
        machine.close();
        assert!(machine.command(text_message("more", None)).is_empty());
        assert_eq!(notify(&mut machine, "turn/completed", &turn(TURN, "completed")), Output::default());
    }
}
