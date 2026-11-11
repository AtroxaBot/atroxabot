//! A running Claude Code process: its pipes, reader threads and the control protocol.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::cli::{build_args, command};
use crate::parse::{Parsed, parse_line};
use crate::{Error, Event, Info, McpServerStatus, Options, PermissionMode, find_cli};

/// Longest line accepted from the binary; a longer one is dropped and reported.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// How long [`Session::close`] waits at each step: the binary needs a moment after end of input to flush its
/// session file.
const GRACE: Duration = Duration::from_secs(5);
/// Interval for polling the process and a busy input lock.
const POLL: Duration = Duration::from_millis(20);

type Callback = Box<dyn FnMut(Event) + Send>;

/// The answer to an [`Event::Permission`].
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// The tool may run.
    Allow {
        /// Input to run the tool with; `None` runs it with the input it asked for.
        updated_input: Option<Value>,
        /// Permission updates to apply, e.g. the request's `suggestions` to stop asking for this kind of call.
        updated_permissions: Vec<Value>,
    },
    /// The tool may not run.
    Deny {
        /// Shown to the model as the reason.
        message: String,
        /// Also stop the turn.
        interrupt: bool,
    },
}

/// A connection to a running Claude Code process.
///
/// All methods take `&self` and may be called from any thread; writes are serialized. Dropping the session closes
/// it, which can block for a few seconds (see [`Session::close`]).
pub struct Session {
    shared: Arc<Shared>,
}

/// State shared with the reader threads.
struct Shared {
    /// The binary's standard input; `None` once closed.
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    /// Inputs of the permission requests not yet answered, by request id.
    pending: Mutex<HashMap<String, Value>>,
    /// The caller's callback; dropped after [`Event::Closed`].
    callback: Mutex<Option<Callback>>,
    next_request: AtomicU64,
    ask_permissions: bool,
}

/// The variable the official SDK sets so that the binary reports its session state.
const READS_SESSION_STATE: &str = "CLAUDE_CODE_SDK_READS_SESSION_STATE";

/// Locks `mutex`; a panic in the caller's callback must not take the session down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Session {
    /// Starts the Claude Code binary and sends the `initialize` control request.
    ///
    /// `on_event` receives everything that arrives, on the reader threads (standard output and standard error;
    /// never two calls at once), possibly before this function returns. The last event is [`Event::Closed`],
    /// after which the callback is dropped. It should return quickly and not panic, since nothing else is read
    /// meanwhile; sending from it is fine, [`Session::close`] would stall it for the grace period.
    ///
    /// # Errors
    /// [`Error::NotFound`] when no binary is found, [`Error::BatchScript`] for a `.cmd`/`.bat` path on Windows,
    /// [`Error::InvalidOption`] for a refused option, [`Error::Spawn`] when the binary cannot be started, and
    /// [`Error::Io`] when the first write or a reader thread fails.
    pub fn connect(options: &Options, on_event: impl FnMut(Event) + Send + 'static) -> Result<Self, Error> {
        let path = match &options.cli_path {
            Some(path) => path.clone(),
            None => find_cli().ok_or(Error::NotFound)?,
        };
        let mut command = command(&path)?;
        command.args(build_args(options)?).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // The environment is inherited as is. Only CLAUDECODE goes, as in the SDK, so that a launch from inside
        // a Claude Code session is not taken for a nested one.
        command.env_remove("CLAUDECODE");
        // As the official SDK does (unless the caller says otherwise): the binary then says when it runs a turn,
        // one it starts by itself included, and when it is idle ([`Event::SessionState`]).
        if !options.env.iter().any(|(key, _)| key.eq_ignore_ascii_case(READS_SESSION_STATE)) {
            command.env(READS_SESSION_STATE, "1");
        }
        command.envs(options.env.iter().map(|(key, value)| (key, value)));
        if let Some(cwd) = &options.cwd {
            if !cwd.is_dir() {
                return Err(Error::InvalidOption(format!("working directory does not exist: {}", cwd.display())));
            }
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(Error::Spawn)?;
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let callback: Callback = Box::new(on_event);
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            callback: Mutex::new(Some(callback)),
            next_request: AtomicU64::new(1),
            ask_permissions: options.ask_permissions,
        });
        // From here on an early return drops the session, which stops the process.
        let session = Self { shared };

        let (stderr_open, stderr_closed) = mpsc::channel::<()>();
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("claude-agent-stderr".into()).spawn(move || {
            if let Some(stderr) = stderr {
                shared.read_stderr(stderr);
            }
            drop(stderr_open);
        });
        spawned.map_err(Error::Io)?;
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("claude-agent-stdout".into()).spawn(move || {
            let pipe_error = stdout.and_then(|stdout| shared.read_stdout(stdout));
            shared.finish(pipe_error, &stderr_closed);
        });
        spawned.map_err(Error::Io)?;

        session.request(&json!({"subtype": "initialize", "hooks": null}))?;
        Ok(session)
    }

    /// Sends a user message, starting a turn (or queueing behind the running one).
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn send(&self, text: &str) -> Result<(), Error> {
        self.send_content(&text.into())
    }

    /// Sends a user message whose `content` is what the Messages API takes: a string, or a list of blocks
    /// (`{"type":"text",...}`, `{"type":"image","source":{"type":"base64",...}}`).
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn send_content(&self, content: &Value) -> Result<(), Error> {
        self.send_message(content, None)
    }

    /// Sends a user message, as [`Session::send_content`] does, under the id `uuid` when one is given. A message
    /// sent while a turn runs is taken into that turn after its running tool calls (or starts the next turn if
    /// it ends first); with `--replay-user-messages` the binary replays it, under the same id, at that moment.
    /// Every message is marked as typed by a person (`origin`), which the binary counts in a result's
    /// [`TurnResult::queued_turn_count`].
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn send_message(&self, content: &Value, uuid: Option<&str>) -> Result<(), Error> {
        let mut message = json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "session_id": "default",
            "origin": {"kind": "human"},
        });
        if let Some(uuid) = uuid {
            message["uuid"] = uuid.into();
        }
        self.shared.write(&message)
    }

    /// Asks the binary to stop the running turn. The turn still ends with an [`Event::Result`]. Messages sent
    /// meanwhile and not taken in yet are dropped when `cancel_queued`, and otherwise answered next, each in a
    /// turn of its own (builds before 2.1.219 always answer them).
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn interrupt(&self, cancel_queued: bool) -> Result<(), Error> {
        self.request(&json!({"subtype": "interrupt", "cancel_queued": cancel_queued}))
    }

    /// Changes the permission mode. A refusal arrives as an [`Event::Error`].
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), Error> {
        self.request(&json!({"subtype": "set_permission_mode", "mode": mode.as_str()}))
    }

    /// Asks how the session's MCP servers are doing; the answer arrives as an [`Event::McpStatus`]. Servers
    /// (a Claude account's claude.ai connectors among them) connect in the background after the start, so an
    /// early answer may list none or some still `pending`.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn mcp_status(&self) -> Result<(), Error> {
        self.request(&json!({"subtype": "mcp_status"}))
    }

    /// Asks what the context window holds, by kind, as `/context` shows it; the answer arrives as an
    /// [`Event::ContextUsage`]. Claude Code works it out itself: no message is sent, so nothing is used.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn context_usage(&self) -> Result<(), Error> {
        self.request(&json!({"subtype": "get_context_usage", "detail": "summary"}))
    }

    /// Changes the model for the following turns; `None` returns to the default model.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn set_model(&self, model: Option<&str>) -> Result<(), Error> {
        self.request(&json!({"subtype": "set_model", "model": model}))
    }

    /// Answers the [`Event::Permission`] with this `request_id`.
    ///
    /// # Errors
    /// [`Error::UnknownRequest`] when no such request is pending (already answered, or withdrawn by an
    /// [`Event::PermissionCancelled`]), [`Error::Closed`] after the session closed, [`Error::Io`] when the write
    /// fails.
    pub fn respond(&self, request_id: &str, decision: Decision) -> Result<(), Error> {
        let Some(input) = lock(&self.shared.pending).remove(request_id) else {
            return Err(Error::UnknownRequest(request_id.to_owned()));
        };
        self.shared.write(&decision_response(request_id, decision, input))
    }

    /// Ends the session: closes the binary's input, waits up to five seconds for it to exit by itself, then kills
    /// it. Idempotent, never panics, and returns within about fifteen seconds at worst (normally well under one).
    ///
    /// [`Event::Closed`] is delivered by the reader thread, usually moments after this returns; events already on
    /// their way may still arrive before it.
    pub fn close(&self) {
        let shared = &self.shared;
        // A writer stuck on a full pipe holds the lock; give up on it after the grace period, the kill frees it.
        let start = Instant::now();
        loop {
            match shared.stdin.try_lock() {
                Ok(mut stdin) => *stdin = None,
                Err(TryLockError::Poisoned(poisoned)) => *poisoned.into_inner() = None,
                Err(TryLockError::WouldBlock) if start.elapsed() < GRACE => {
                    thread::sleep(POLL);
                    continue;
                }
                Err(TryLockError::WouldBlock) => {}
            }
            break;
        }
        if !shared.wait_exit(GRACE) {
            // ponytail: std has no SIGTERM, so Unix goes from end-of-input straight to SIGKILL (on Windows
            // terminate and kill are the same call). Add a `kill -TERM` step if a build ever needs it.
            let _ = lock(&shared.child).kill();
            shared.wait_exit(GRACE);
        }
    }

    /// Sends a control request. Its outcome arrives on the reader thread: success is dropped, failure becomes
    /// an [`Event::Error`].
    fn request(&self, request: &Value) -> Result<(), Error> {
        let id = self.shared.next_request.fetch_add(1, Ordering::Relaxed);
        self.shared.write(&json!({"type": "control_request", "request_id": format!("req_{id}"), "request": request}))
    }
}

/// Asks the binary what it offers (its models and the account it is signed in to) without sending it a
/// message, so nothing is used: starts it, waits up to `timeout` for its answer to `initialize`, and stops it.
///
/// # Errors
/// The binary could not be started, ended first, or did not answer in time.
pub fn info(options: &Options, timeout: Duration) -> Result<Info, Error> {
    let (answers, inbox) = mpsc::channel();
    let session = Session::connect(options, move |event| {
        let answer = match event {
            Event::Ready(info) => Ok(info),
            Event::Closed { error, .. } => Err(error.unwrap_or_else(|| "it stopped before answering".to_owned())),
            _ => return,
        };
        // The receiver is gone once an answer was taken.
        let _ = answers.send(answer);
    })?;
    let answer = inbox.recv_timeout(timeout);
    session.close();
    match answer {
        Ok(Ok(info)) => Ok(info),
        Ok(Err(why)) => Err(Error::NoInfo(why)),
        Err(_) => Err(Error::NoInfo("it did not answer in time".to_owned())),
    }
}

/// Asks the binary which MCP servers a session of it gets (a Claude account's claude.ai connectors among them)
/// and how each is doing, without sending it a message, so nothing is used. Asks every second until the list
/// stops changing and nothing is `pending`, or until `timeout`, then stops it.
///
/// # Errors
/// The binary could not be started, ended first, or never answered.
pub fn mcp_servers(options: &Options, timeout: Duration) -> Result<Vec<McpServerStatus>, Error> {
    /// How long one answer may take.
    const ANSWER: Duration = Duration::from_secs(3);
    let (answers, inbox) = mpsc::channel();
    let session = Session::connect(options, move |event| {
        let answer = match event {
            Event::Ready(_) => Ok(None),
            Event::McpStatus(servers) => Ok(Some(servers)),
            Event::Closed { error, .. } => Err(error.unwrap_or_else(|| "it stopped before answering".to_owned())),
            _ => return,
        };
        let _ = answers.send(answer);
    })?;
    let deadline = Instant::now() + timeout;
    let mut latest: Option<Vec<McpServerStatus>> = None;
    let outcome = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(left.min(ANSWER)) {
            Ok(Ok(Some(servers))) => {
                let settled = !servers.is_empty()
                    && servers.iter().all(|server| server.status != "pending")
                    && latest.as_ref() == Some(&servers);
                latest = Some(servers);
                if settled {
                    break Ok(());
                }
                thread::sleep(Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now())));
            }
            Ok(Err(why)) => break Err(why),
            Err(_) if left.is_zero() => break Ok(()),
            // Ready (ask now), or no answer yet (ask again).
            Ok(Ok(None)) | Err(_) => {}
        }
        if Instant::now() >= deadline {
            break Ok(());
        }
        if let Err(e) = session.mcp_status() {
            break Err(e.to_string());
        }
    };
    session.close();
    match (outcome, latest) {
        (Ok(()), Some(servers)) => Ok(servers),
        (Ok(()), None) => Err(Error::NoInfo("it did not answer in time".to_owned())),
        (Err(why), _) => Err(Error::NoInfo(why)),
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    /// Writes one message as a line to the binary's input.
    fn write(&self, message: &Value) -> Result<(), Error> {
        let mut guard = lock(&self.stdin);
        let Some(stdin) = guard.as_mut() else { return Err(Error::Closed) };
        let mut line = message.to_string();
        line.push('\n');
        if let Err(error) = stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()) {
            *guard = None;
            return Err(Error::Io(error));
        }
        Ok(())
    }

    fn emit(&self, event: Event) {
        if let Some(callback) = lock(&self.callback).as_mut() {
            callback(event);
        }
    }

    /// Tells the binary a control request of its own failed, so it does not wait for an answer.
    fn refuse(&self, request_id: &str, why: &str) {
        let response = json!({"subtype": "error", "request_id": request_id, "error": why});
        // A failed write means the process is going away; `Closed` will say so.
        let _ = self.write(&json!({"type": "control_response", "response": response}));
    }

    fn handle(&self, parsed: Parsed) {
        match parsed {
            Parsed::Nothing => {}
            Parsed::Unsupported { request_id, subtype } => {
                self.refuse(&request_id, &format!("Unsupported control request subtype: {subtype}"));
            }
            Parsed::Event(Event::Permission(request)) if !self.ask_permissions => {
                self.refuse(&request.request_id, "canUseTool callback is not provided");
            }
            Parsed::Event(Event::Permission(request)) => {
                lock(&self.pending).insert(request.request_id.clone(), request.input.clone());
                self.emit(Event::Permission(request));
            }
            Parsed::Event(Event::PermissionCancelled { request_id }) => {
                // The binary also cancels requests we refused; only report the ones the caller has seen.
                if lock(&self.pending).remove(&request_id).is_some() {
                    self.emit(Event::PermissionCancelled { request_id });
                }
            }
            Parsed::Event(event) => self.emit(event),
        }
    }

    /// Reads the binary's output until it ends. Returns the read error, if that is what ended it.
    fn read_stdout(&self, stdout: impl Read) -> Option<String> {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_line(&mut reader, MAX_LINE) {
                Ok(Line::Text(line)) => self.handle(parse_line(&line)),
                Ok(Line::TooLong) => self.emit(Event::Error(format!("dropped a line longer than {MAX_LINE} bytes"))),
                Ok(Line::End) => return None,
                Err(error) => return Some(error.to_string()),
            }
        }
    }

    fn read_stderr(&self, stderr: impl Read) {
        let mut reader = BufReader::new(stderr);
        loop {
            match read_line(&mut reader, MAX_LINE) {
                Ok(Line::Text(line)) => {
                    let line = line.trim_end();
                    if !line.is_empty() {
                        self.emit(Event::Stderr(line.to_owned()));
                    }
                }
                Ok(Line::TooLong) => {}
                Ok(Line::End) | Err(_) => return,
            }
        }
    }

    /// Runs on the output thread once the output ended: reaps the process and reports [`Event::Closed`].
    fn finish(&self, pipe_error: Option<String>, stderr_closed: &mpsc::Receiver<()>) {
        if pipe_error.is_some() {
            // Without its output the process is of no use, and it may never exit by itself.
            let _ = lock(&self.child).kill();
        }
        while !self.wait_exit(Duration::MAX) {}
        let exit_code = lock(&self.child).try_wait().ok().flatten().and_then(|status| status.code());
        // Let the last error lines through first; bounded, because a grandchild may keep that pipe open.
        let _ = stderr_closed.recv_timeout(Duration::from_secs(1));
        let error =
            pipe_error.map(|error| format!("reading from the Claude Code binary failed: {error}")).or_else(|| {
                match exit_code {
                    Some(0) => None,
                    Some(code) => Some(format!("the Claude Code binary exited with code {code}")),
                    None => Some("the Claude Code binary was terminated by a signal".into()),
                }
            });
        *lock(&self.stdin) = None;
        lock(&self.pending).clear();
        let callback = lock(&self.callback).take();
        if let Some(mut callback) = callback {
            callback(Event::Closed { exit_code, error });
        }
    }

    /// Polls until the process has exited (or can no longer be queried); `false` when `limit` passes first.
    fn wait_exit(&self, limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            if !matches!(lock(&self.child).try_wait(), Ok(None)) {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            thread::sleep(POLL);
        }
    }
}

/// The `control_response` that answers a `can_use_tool` request.
fn decision_response(request_id: &str, decision: Decision, original_input: Value) -> Value {
    let mut response = Map::new();
    match decision {
        Decision::Allow { updated_input, updated_permissions } => {
            response.insert("behavior".into(), "allow".into());
            response.insert("updatedInput".into(), updated_input.unwrap_or(original_input));
            if !updated_permissions.is_empty() {
                response.insert("updatedPermissions".into(), updated_permissions.into());
            }
        }
        Decision::Deny { message, interrupt } => {
            response.insert("behavior".into(), "deny".into());
            response.insert("message".into(), message.into());
            if interrupt {
                response.insert("interrupt".into(), true.into());
            }
        }
    }
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response},
    })
}

/// One read from a line-delimited stream.
#[derive(Debug, PartialEq)]
enum Line {
    /// A line, with its newline if it had one. Invalid UTF-8 is replaced, not rejected.
    Text(String),
    /// A line longer than the cap; it has been skipped.
    TooLong,
    /// End of input.
    End,
}

/// Reads one line of at most `cap` bytes (newline not counted), never buffering more than that.
fn read_line(reader: &mut impl BufRead, cap: usize) -> io::Result<Line> {
    let mut line = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader.by_ref().take(limit).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(Line::End);
    }
    if line.len() <= cap || line.ends_with(b"\n") {
        return Ok(Line::Text(String::from_utf8_lossy(&line).into_owned()));
    }
    // Skip the rest of the line without keeping it.
    loop {
        let chunk = reader.fill_buf()?;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let used = newline.map_or(chunk.len(), |at| at + 1);
        reader.consume(used);
        if newline.is_some() || used == 0 {
            return Ok(Line::TooLong);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{Block, SystemPrompt};

    #[test]
    fn session_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Session>();
    }

    #[test]
    fn lines_are_capped() {
        let input = format!("short\n{}\nexact\r\n{}\nlast", "x".repeat(11), "y".repeat(5000));
        // A tiny buffer, so that both the read and the skip span several refills.
        let mut reader = BufReader::with_capacity(4, Cursor::new(input));
        let mut read = || read_line(&mut reader, 10).unwrap();
        assert_eq!(read(), Line::Text("short\n".into()));
        assert_eq!(read(), Line::TooLong);
        assert_eq!(read(), Line::Text("exact\r\n".into()));
        assert_eq!(read(), Line::TooLong);
        assert_eq!(read(), Line::Text("last".into()));
        assert_eq!(read(), Line::End);
        assert_eq!(read(), Line::End);
    }

    #[test]
    fn a_line_of_exactly_the_cap_is_kept() {
        let mut reader = Cursor::new("0123456789\n0123456789");
        assert_eq!(read_line(&mut reader, 10).unwrap(), Line::Text("0123456789\n".into()));
        assert_eq!(read_line(&mut reader, 10).unwrap(), Line::Text("0123456789".into()));
        assert_eq!(read_line(&mut reader, 10).unwrap(), Line::End);
    }

    #[test]
    fn an_endless_line_is_dropped_without_buffering_it() {
        let mut reader = io::repeat(b'a').take(3 * MAX_LINE as u64);
        let mut reader = BufReader::new(&mut reader);
        assert_eq!(read_line(&mut reader, MAX_LINE).unwrap(), Line::TooLong);
        assert_eq!(read_line(&mut reader, MAX_LINE).unwrap(), Line::End);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let mut reader = Cursor::new(b"{\"a\":\"\xff\"}\n".to_vec());
        assert_eq!(read_line(&mut reader, 100).unwrap(), Line::Text("{\"a\":\"\u{fffd}\"}\n".into()));
    }

    #[test]
    fn allow_echoes_the_original_input() {
        let decision = Decision::Allow { updated_input: None, updated_permissions: Vec::new() };
        let expected = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "cli_1",
                "response": {"behavior": "allow", "updatedInput": {"command": "ls"}},
            },
        });
        assert_eq!(decision_response("cli_1", decision, json!({"command": "ls"})), expected);
    }

    #[test]
    fn allow_with_changes() {
        let rule = json!({"type": "addRules", "behavior": "allow", "destination": "session"});
        let decision = Decision::Allow {
            updated_input: Some(json!({"command": "ls -a"})),
            updated_permissions: vec![rule.clone()],
        };
        let response = decision_response("cli_2", decision, json!({"command": "ls"}));
        let expected = json!({"behavior": "allow", "updatedInput": {"command": "ls -a"}, "updatedPermissions": [rule]});
        assert_eq!(response["response"]["response"], expected);
    }

    #[test]
    fn deny_with_and_without_interrupt() {
        let deny = |interrupt| {
            let decision = Decision::Deny { message: "not now".into(), interrupt };
            decision_response("cli_3", decision, Value::Null)["response"]["response"].clone()
        };
        assert_eq!(deny(false), json!({"behavior": "deny", "message": "not now"}));
        assert_eq!(deny(true), json!({"behavior": "deny", "message": "not now", "interrupt": true}));
    }

    /// Uses the owner's Claude usage: run by hand, once, to confirm the wire format against the real binary.
    #[test]
    #[ignore = "talks to the real Claude Code binary"]
    fn live_ping_pong() {
        let (sender, events) = mpsc::channel();
        let options = Options {
            cwd: Some(std::env::temp_dir()),
            system_prompt: SystemPrompt::Custom("Reply with exactly: pong".into()),
            max_turns: Some(1),
            include_partial_messages: true,
            ..Options::default()
        };
        let path = find_cli().expect("no Claude Code binary installed");
        println!("cli: {} version {}", path.display(), crate::cli_version(&path).unwrap());
        println!("args: {:?}", build_args(&options).unwrap());
        let session = Session::connect(&options, move |event| drop(sender.send(event))).unwrap();
        session.send("ping").unwrap();

        let (mut init, mut streamed, mut complete) = (false, String::new(), String::new());
        let result = loop {
            match events.recv_timeout(Duration::from_secs(180)).expect("the turn takes too long") {
                Event::Init(seen) => {
                    println!("init keys: {:?}", seen.raw.as_object().unwrap().keys().collect::<Vec<_>>());
                    println!(
                        "init: model={} permissionMode={} apiKeySource={} version={} tools={} mcp={:?}",
                        seen.model,
                        seen.permission_mode,
                        seen.api_key_source,
                        seen.claude_code_version,
                        seen.tools.len(),
                        seen.mcp_servers
                    );
                    assert!(!seen.session_id.is_empty() && !seen.model.is_empty() && !seen.cwd.is_empty());
                    init = true;
                }
                Event::TextDelta { text, .. } => streamed.push_str(&text),
                Event::Assistant { blocks, usage, error, .. } => {
                    println!("assistant: {blocks:?} {usage:?} error={error:?}");
                    complete.extend(blocks.into_iter().filter_map(|block| match block {
                        Block::Text(text) => Some(text),
                        _ => None,
                    }));
                }
                Event::Result(result) => break result,
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                Event::Other { kind, subtype, .. } => println!("other: {kind}/{subtype}"),
                other => println!("{other:?}"),
            }
        };
        println!("result: {result:?}");
        assert!(init, "no system/init before the result");
        assert!(streamed.to_lowercase().contains("pong"), "streamed text: {streamed:?}");
        assert!(complete.to_lowercase().contains("pong"), "assistant text: {complete:?}");
        assert!(!result.is_error && result.subtype == "success" && !result.session_id.is_empty());

        session.close();
        assert!(matches!(session.send("again"), Err(Error::Closed)));
        loop {
            match events.recv_timeout(Duration::from_secs(10)).expect("no Closed after close()") {
                Event::Closed { exit_code, error } => {
                    assert_eq!((exit_code, error), (Some(0), None));
                    break;
                }
                other => println!("after close: {other:?}"),
            }
        }
        assert!(events.recv().is_err(), "the callback is dropped after Closed");
    }
}
