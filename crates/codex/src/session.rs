//! A running `codex app-server` process: its pipes, its reader threads and what the caller asks of it.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::{build_args, command};
use crate::machine::{Command, Machine, Message, initialize_params, request_line};
use crate::parse::{self, Incoming, Refusal, field, text};
use crate::{Decision, Error, Event, Info, Input, Model, Options, find_cli};

/// Longest line accepted from the binary; a longer one is dropped and reported.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// How long [`Session::close`] waits at each step: the binary needs a moment after end of input to write its
/// conversation file.
const GRACE: Duration = Duration::from_secs(5);
/// Interval for polling the process and a busy input lock.
const POLL: Duration = Duration::from_millis(20);
/// Pages of models [`info`] follows at most; one is all there has ever been.
const MAX_PAGES: usize = 32;

type Callback = Box<dyn FnMut(Event) + Send>;

/// A connection to a running Codex process and the one thread (conversation) it has open there.
///
/// All methods take `&self` and may be called from any thread; writes are serialized. What is asked before the
/// thread is open ([`Event::Ready`]) waits and is sent, in order, once it is. Dropping the session closes it,
/// which can block for a few seconds (see [`Session::close`]).
pub struct Session {
    shared: Arc<Shared>,
}

/// State shared with the reader threads.
struct Shared {
    /// The binary's standard input; `None` once closed.
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    /// The protocol's state. Held while its lines are written, so that they leave in the order it made them;
    /// never held while the callback runs.
    machine: Mutex<Machine>,
    /// The caller's callback; dropped after [`Event::Closed`].
    callback: Mutex<Option<Callback>>,
    /// Why the session could not go on, when it is this crate that ended the process.
    failure: Mutex<Option<String>>,
    /// The session was closed or the process ended: nothing is accepted any more.
    closed: AtomicBool,
    /// The end of what the binary wrote to stderr, to say why it stopped.
    said: Mutex<VecDeque<String>>,
}

/// Locks `mutex`; a panic in the caller's callback must not take the session down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts `codex app-server` for `options`, its input and output piped.
fn launch(options: &Options, stderr: Stdio) -> Result<Child, Error> {
    let path = match &options.cli_path {
        Some(path) => path.clone(),
        None => find_cli().ok_or(Error::NotFound)?,
    };
    let mut command = command(&path)?;
    command.args(build_args(options)?).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
    // The environment is inherited as it is: the binary signs in from what it finds there and in its own
    // folder, which is none of this crate's business.
    command.envs(options.env.iter().map(|(key, value)| (key, value)));
    if let Some(cwd) = &options.cwd {
        if !cwd.is_dir() {
            return Err(Error::InvalidOption(format!("working directory does not exist: {}", cwd.display())));
        }
        command.current_dir(cwd);
    }
    command.spawn().map_err(Error::Spawn)
}

impl Session {
    /// Starts the Codex binary and returns at once; the handshake and the opening of the thread happen on the
    /// reader thread, and end in an [`Event::Ready`].
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
        let mut child = launch(options, Stdio::piped())?;
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let callback: Callback = Box::new(on_event);
        let mut machine = Machine::new(options);
        let hello = machine.hello();
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            machine: Mutex::new(machine),
            callback: Mutex::new(Some(callback)),
            failure: Mutex::new(None),
            closed: AtomicBool::new(false),
            said: Mutex::new(VecDeque::new()),
        });
        // From here on an early return drops the session, which stops the process.
        let session = Self { shared };

        let (stderr_open, stderr_closed) = mpsc::channel::<()>();
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("codex-stderr".into()).spawn(move || {
            if let Some(stderr) = stderr {
                shared.read_stderr(stderr);
            }
            drop(stderr_open);
        });
        spawned.map_err(Error::Io)?;
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("codex-stdout".into()).spawn(move || {
            let pipe_error = stdout.and_then(|stdout| shared.read_stdout(stdout));
            shared.finish(pipe_error, &stderr_closed);
        });
        spawned.map_err(Error::Io)?;

        session.shared.write(&hello)?;
        Ok(session)
    }

    /// Sends the user's input. It starts a turn when none runs and is added to the running one otherwise, where
    /// the agent reads it at its next step. `client_id` names the message: it comes back in the
    /// [`Event::UserMessage`] that says the message entered the conversation.
    ///
    /// While the running turn is being stopped, or takes no input (the conversation is being summarised), the
    /// message waits and starts the turn after it. A turn that cannot be started ends in an
    /// [`Event::TurnCompleted`] with an empty id; a message the running turn would not take is reported as an
    /// [`Event::Warning`].
    ///
    /// # Errors
    /// [`Error::InvalidOption`] for an empty input, [`Error::Closed`] after the session closed, [`Error::Io`]
    /// when the write fails.
    pub fn send(&self, input: &[Input], client_id: Option<&str>) -> Result<(), Error> {
        if input.is_empty() {
            return Err(Error::InvalidOption("there is nothing to send".into()));
        }
        self.shared.command(Command::Send(Message::new(input, client_id)))
    }

    /// Gives the agent input in the turn that runs, and only there: it reads it at its next step, and an
    /// [`Event::UserMessage`] with `client_id` says so. When no turn runs, or the running one is being stopped
    /// or takes no input, the input is dropped and no such event comes: it is the caller's to send again.
    ///
    /// # Errors
    /// [`Error::InvalidOption`] for an empty input, [`Error::Closed`] after the session closed, [`Error::Io`]
    /// when the write fails.
    pub fn steer(&self, input: &[Input], client_id: &str) -> Result<(), Error> {
        if input.is_empty() {
            return Err(Error::InvalidOption("there is nothing to send".into()));
        }
        self.shared.command(Command::Steer(Message::new(input, Some(client_id))))
    }

    /// Asks the binary to stop the running turn, which still ends with an [`Event::TurnCompleted`]. A turn
    /// that is only just being started is stopped as soon as Codex announces it. Does nothing when no turn
    /// runs.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn interrupt(&self) -> Result<(), Error> {
        self.shared.command(Command::Interrupt)
    }

    /// Answers the [`Event::Approval`] with this `request_id`.
    ///
    /// # Errors
    /// [`Error::UnknownRequest`] when no such approval is pending (already answered, or settled as an
    /// [`Event::RequestResolved`] says), [`Error::Closed`] after the session closed, [`Error::Io`] when the
    /// write fails.
    pub fn respond(&self, request_id: &str, decision: Decision) -> Result<(), Error> {
        self.shared.reply(|machine| machine.respond(request_id, decision))
    }

    /// Answers the [`Event::Question`] with this `request_id`: for each question's id, the answers given (the
    /// labels of the options chosen, or the user's own words). No answers at all dismiss the question.
    ///
    /// # Errors
    /// [`Error::UnknownRequest`] when no such question is pending, [`Error::Closed`] after the session closed,
    /// [`Error::Io`] when the write fails.
    pub fn answer(&self, request_id: &str, answers: &[(String, Vec<String>)]) -> Result<(), Error> {
        self.shared.reply(|machine| machine.answer(request_id, answers))
    }

    /// Changes the model and the effort the following turns are started with; `None` leaves the choice to
    /// Codex. A message that already waits for its turn goes with them too.
    pub fn retune(&self, model: Option<String>, effort: Option<String>) {
        lock(&self.shared.machine).retune(model, effort);
    }

    /// Has the conversation summarised to make room. It happens in a turn of its own, during which messages
    /// wait.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn compact(&self) -> Result<(), Error> {
        self.shared.command(Command::Compact)
    }

    /// Sends any other request of the app-server protocol; `params` of `null` sends none. Its answer is dropped,
    /// a refusal arrives as an [`Event::Warning`].
    ///
    /// # Errors
    /// [`Error::InvalidOption`] for `account/login/start` (signing in is `codex login`, in a terminal the user
    /// sees, never this crate's), [`Error::Closed`] after the session closed, [`Error::Io`] when the write
    /// fails.
    pub fn call(&self, method: &str, params: Value) -> Result<(), Error> {
        if method.is_empty() || method == "account/login/start" {
            return Err(Error::InvalidOption(format!("{method:?} is not a request this client sends")));
        }
        self.shared.command(Command::Call { method: method.to_owned(), params })
    }

    /// Ends the session: closes the binary's input, waits up to five seconds for it to exit by itself, then kills
    /// it. Idempotent, never panics, and returns within about fifteen seconds at worst (normally well under one).
    ///
    /// [`Event::Closed`] is delivered by the reader thread, usually moments after this returns; events already on
    /// their way may still arrive before it.
    pub fn close(&self) {
        let shared = &self.shared;
        shared.closed.store(true, Ordering::Relaxed);
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
            // std has no SIGTERM, so Unix goes from end-of-input straight to SIGKILL (on Windows terminate and
            // kill are the same call).
            let _ = lock(&shared.child).kill();
            shared.wait_exit(GRACE);
        }
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

    /// Does what the caller asks, now or once it can be done.
    fn command(&self, command: Command) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let mut machine = lock(&self.machine);
        machine.command(command).iter().try_for_each(|line| self.write(line))
    }

    /// Writes the answer to a request of the binary, as `answer` makes it.
    fn reply(&self, answer: impl FnOnce(&mut Machine) -> Result<Value, Error>) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let mut machine = lock(&self.machine);
        self.write(&answer(&mut machine)?)
    }

    fn emit(&self, event: Event) {
        if let Some(callback) = lock(&self.callback).as_mut() {
            callback(event);
        }
    }

    /// Takes one line of the binary's output: writes what it calls for, then reports what it says.
    fn handle(&self, line: &str) {
        let output = {
            let mut machine = lock(&self.machine);
            let output = machine.receive(line);
            for line in &output.lines {
                // A failed write means the process is going away; `Closed` will say so.
                let _ = self.write(line);
            }
            output
        };
        for event in output.events {
            self.emit(event);
        }
        if let Some(why) = output.fatal {
            // Nothing was begun that the binary would have to save: it is ended at once.
            self.closed.store(true, Ordering::Relaxed);
            *lock(&self.failure) = Some(why);
            *lock(&self.stdin) = None;
            let _ = lock(&self.child).kill();
        }
    }

    /// Reads the binary's output until it ends. Returns the read error, if that is what ended it.
    fn read_stdout(&self, stdout: impl Read) -> Option<String> {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_line(&mut reader, MAX_LINE) {
                Ok(Line::Text(line)) => self.handle(&line),
                Ok(Line::TooLong) => self.emit(Event::Warning(format!("dropped a line longer than {MAX_LINE} bytes"))),
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
                    let line = strip_ansi(line.trim_end());
                    if !line.is_empty() {
                        keep_said(&mut lock(&self.said), &line);
                        self.emit(Event::Stderr(line));
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
        let failure = lock(&self.failure).take();
        let pipe_error = pipe_error.map(|error| format!("reading from the Codex binary failed: {error}"));
        let error = failure.or(pipe_error).or_else(|| match exit_code {
            Some(0) => None,
            Some(code) => Some(match explanation(&lock(&self.said)) {
                Some(said) => format!("the Codex binary exited with code {code} ({said})"),
                None => format!("the Codex binary exited with code {code}"),
            }),
            None => Some("the Codex binary was terminated by a signal".into()),
        });
        self.closed.store(true, Ordering::Relaxed);
        *lock(&self.stdin) = None;
        lock(&self.machine).close();
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

/// What Codex offers for an account (its models, the account it is signed in to and where its plan's limits
/// stand), asked without opening a thread or starting a turn, so nothing is used: starts the binary, asks, and
/// ends it. Waits at most `timeout` in all.
///
/// The limits are left out when the binary cannot say them, as when it is signed out.
///
/// # Errors
/// The binary could not be started ([`Error::NotFound`], [`Error::BatchScript`], [`Error::InvalidOption`],
/// [`Error::Spawn`]), or ended first, refused or did not answer in time ([`Error::NoInfo`]).
pub fn info(options: &Options, timeout: Duration) -> Result<Info, Error> {
    let mut child = launch(options, Stdio::null())?;
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    let (sender, answers) = mpsc::channel();
    // Ends the process when dropped, however this function returns.
    let mut probe = Probe { child, stdin, answers, next_id: 1, deadline: Instant::now().checked_add(timeout) };
    let spawned = thread::Builder::new().name("codex-info".into()).spawn(move || {
        let Some(stdout) = stdout else { return };
        let mut reader = BufReader::new(stdout);
        while let Ok(line) = read_line(&mut reader, MAX_LINE) {
            match line {
                Line::Text(line) => {
                    // Requests of the binary get no answer here: none is expected without a thread.
                    if let Incoming::Response { id, outcome } = parse::parse_line(&line)
                        && sender.send((id, outcome)).is_err()
                    {
                        return;
                    }
                }
                Line::TooLong => {}
                Line::End => return,
            }
        }
    });
    spawned.map_err(Error::Io)?;
    probe.ask_all(options).map_err(Error::NoInfo)
}

/// A Codex process that is only asked questions.
struct Probe {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The answers the reader thread passes on, by request id.
    answers: mpsc::Receiver<(u64, Result<Value, Refusal>)>,
    next_id: u64,
    /// When to give up; `None` never.
    deadline: Option<Instant>,
}

impl Probe {
    /// The questions of [`info`], in order.
    fn ask_all(&mut self, options: &Options) -> Result<Info, String> {
        let hello = self.ask("initialize", initialize_params(options))?;
        self.write(&json!({"method": "initialized"}))?;
        // Without a refresh: the answer is what the binary knows by itself.
        let (account, requires_auth) = parse::account(&self.ask("account/read", json!({"refreshToken": false}))?);
        let mut models: Vec<Model> = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.take().map_or_else(|| json!({}), |cursor: String| json!({"cursor": cursor}));
            let (page, next) = parse::models(&self.ask("model/list", params)?);
            for model in page {
                if !models.iter().any(|known| known.id == model.id) {
                    models.push(model);
                }
            }
            let Some(next) = next else { break };
            cursor = Some(next);
        }
        // Refused when signed out, which is no failure of the whole.
        let limits = self.ask("account/rateLimits/read", Value::Null).ok();
        Ok(Info {
            version: parse::version_of(&text(&hello, "userAgent")),
            codex_home: text(&hello, "codexHome"),
            account,
            requires_auth,
            models,
            limits: limits.map(|answer| parse::rate_limits(field(&answer, "rateLimits"))),
        })
    }

    fn write(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("it stopped before answering")?;
        let mut line = message.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|error| error.to_string())
    }

    /// Sends a request and waits for its answer.
    fn ask(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = request_line(method, id, params);
        self.write(&request)?;
        loop {
            let left =
                self.deadline.map_or(Duration::MAX, |deadline| deadline.saturating_duration_since(Instant::now()));
            match self.answers.recv_timeout(left) {
                Ok((answered, outcome)) if answered == id => {
                    return outcome.map_err(|refusal| format!("{method}: {}", refusal.message));
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Err("it did not answer in time".to_owned()),
                Err(RecvTimeoutError::Disconnected) => return Err("it stopped before answering".to_owned()),
            }
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // End of input is its cue to leave; it has nothing to save, so it is not given long.
        self.stdin = None;
        let start = Instant::now();
        while matches!(self.child.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(1) {
            thread::sleep(POLL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How many of the binary's last stderr lines are kept to explain an exit.
const SAID_LINES: usize = 40;
/// Longest stderr line kept, in bytes; the rest of a longer one is dropped.
const SAID_LINE: usize = 400;

/// Keeps `line` among the last [`SAID_LINES`] of `said`, cut to [`SAID_LINE`] bytes.
fn keep_said(said: &mut VecDeque<String>, line: &str) {
    let mut end = line.len().min(SAID_LINE);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.get(..end).unwrap_or_default().to_owned());
}

/// What the binary said about why it stopped: the last of its stderr lines that names an error, else its last
/// line. Stack frames (`at …`) and the runtime's own banner (`Bun v1.2 (…)`, `Node.js v22`) explain nothing.
fn explanation(said: &VecDeque<String>) -> Option<&str> {
    let telling = |line: &&String| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with("at ") && !line.starts_with("Bun v") && !line.starts_with("Node.js v")
    };
    let names_error = |line: &&String| {
        let lower = line.to_ascii_lowercase();
        ["error", "exception", "panic", "fatal"].iter().any(|word| lower.contains(word))
    };
    let mut lines = said.iter().rev().filter(telling);
    let last = lines.clone().next();
    lines.find(names_error).or(last).map(|line| line.trim())
}

/// `line` without the escape sequences that colour a terminal, which the binary's log lines carry.
fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
        } else if chars.next() == Some('[') {
            // A control sequence runs up to its final byte, a letter or the like.
            chars.by_ref().find(|c| ('\u{40}'..='\u{7e}').contains(c));
        }
    }
    plain
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
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn an_exit_is_explained_by_the_error_line_not_the_stack_or_banner() {
        let mut said = VecDeque::new();
        for line in [
            "starting",
            "TypeError: undefined is not an object",
            "    at run (main.js:1:2)",
            "",
            "Bun v1.3.1 (Windows x64)",
        ] {
            keep_said(&mut said, line);
        }
        assert_eq!(explanation(&said), Some("TypeError: undefined is not an object"));
        let mut quiet = VecDeque::new();
        keep_said(&mut quiet, "shutting down");
        assert_eq!(explanation(&quiet), Some("shutting down"));
        assert_eq!(explanation(&VecDeque::new()), None);
    }

    #[test]
    fn only_the_end_of_stderr_is_kept_and_long_lines_are_cut_whole_characters() {
        let mut said = VecDeque::new();
        for n in 0..SAID_LINES + 5 {
            keep_said(&mut said, &format!("line {n}"));
        }
        assert_eq!(said.len(), SAID_LINES);
        assert_eq!(said.front().map(String::as_str), Some("line 5"));
        keep_said(&mut said, &"é".repeat(SAID_LINE));
        assert_eq!(said.back().map(String::len), Some(SAID_LINE));
    }
    use crate::{Approval, Sandbox, TurnStatus};

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
    fn colours_are_stripped_from_log_lines() {
        let line = "\u{1b}[2m2026-11-11T17:05:14Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m failed to connect";
        assert_eq!(strip_ansi(line), "2026-11-11T17:05:14Z ERROR failed to connect");
        assert_eq!(strip_ansi("plain [text]"), "plain [text]");
        assert_eq!(strip_ansi("cut \u{1b}[3"), "cut ");
        assert_eq!(strip_ansi("lone \u{1b}"), "lone ");
        assert_eq!(strip_ansi("\u{1b}Mnext"), "next");
    }

    #[test]
    fn options_are_checked_before_anything_is_started() {
        let missing = std::env::temp_dir().join("atroxabot-no-such-dir");
        let binary = missing.join("codex.exe");
        let options = Options { cli_path: Some(binary.clone()), ..Options::default() };
        assert!(matches!(Session::connect(&options, |_| {}), Err(Error::Spawn(_))));
        assert!(matches!(info(&options, Duration::from_secs(1)), Err(Error::Spawn(_))));
        let options = Options { cli_path: Some(binary.clone()), cwd: Some(missing), ..Options::default() };
        assert!(matches!(Session::connect(&options, |_| {}), Err(Error::InvalidOption(_))));
        let config = vec![("--listen".to_owned(), "ws://0.0.0.0:1".to_owned())];
        let options = Options { cli_path: Some(binary), config, ..Options::default() };
        assert!(matches!(Session::connect(&options, |_| {}), Err(Error::InvalidOption(_))));
        if cfg!(windows) {
            let options = Options { cli_path: Some(PathBuf::from("C:\\npm\\codex.cmd")), ..Options::default() };
            assert!(matches!(Session::connect(&options, |_| {}), Err(Error::BatchScript(_))));
        }
    }

    /// The binary to test against, and a `CODEX_HOME` for it when the environment names one.
    fn live_options() -> Options {
        let path = std::env::var_os("ATROXABOT_CODEX_BIN").map(PathBuf::from).or_else(find_cli);
        let path = path.expect("no Codex binary installed; set ATROXABOT_CODEX_BIN");
        println!("cli: {} version {}", path.display(), crate::cli_version(&path).unwrap());
        let home = std::env::var("CODEX_HOME").ok();
        Options {
            cli_path: Some(path),
            env: home.into_iter().map(|home| ("CODEX_HOME".to_owned(), home)).collect(),
            cwd: Some(std::env::temp_dir()),
            approval: Approval::Never,
            sandbox: Sandbox::ReadOnly,
            instructions: Some("Reply with exactly: pong".into()),
            client_version: "0.1.0".into(),
            ..Options::default()
        }
    }

    /// Starts no turn, so it uses nothing: run by hand to confirm the wire format against the real binary.
    #[test]
    #[ignore = "talks to the real Codex binary"]
    fn live_info_and_ready() {
        let options = live_options();
        let info = info(&options, Duration::from_secs(60)).unwrap();
        println!(
            "info: version={} home={} account={:?} requires_auth={}",
            info.version, info.codex_home, info.account, info.requires_auth
        );
        println!("limits: {:?}", info.limits);
        for model in &info.models {
            println!("model: {model:?}");
        }
        assert!(!info.version.is_empty() && !info.codex_home.is_empty());
        assert!(!info.models.is_empty());
        assert!(info.models.iter().all(|model| !model.id.is_empty() && !model.name.is_empty()));

        let (sender, events) = mpsc::channel();
        let session = Session::connect(&options, move |event| drop(sender.send(event))).unwrap();
        let thread_id = loop {
            match events.recv_timeout(Duration::from_secs(60)).expect("no Ready in time") {
                Event::Ready { thread_id, model, effort, sandbox, resumed, version } => {
                    println!(
                        "ready: thread={thread_id} model={model} effort={effort:?} sandbox={sandbox} version={version}"
                    );
                    assert!(!thread_id.is_empty() && !model.is_empty() && !resumed);
                    assert_eq!(version, info.version);
                    break thread_id;
                }
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                other => println!("{other:?}"),
            }
        };
        // Refused by the binary (signed out) or answered: either way the session goes on.
        session.call("account/rateLimits/read", Value::Null).unwrap();
        assert!(matches!(session.call("account/login/start", json!({})), Err(Error::InvalidOption(_))));
        session.interrupt().unwrap();
        session.close();
        assert!(matches!(session.send(&[Input::Text("again".into())], None), Err(Error::Closed)));
        loop {
            match events.recv_timeout(Duration::from_secs(15)).expect("no Closed after close()") {
                Event::Closed { exit_code, error } => {
                    assert_eq!((exit_code, error), (Some(0), None));
                    break;
                }
                other => println!("after close: {other:?}"),
            }
        }
        assert!(events.recv().is_err(), "the callback is dropped after Closed");

        // A thread that does not exist cannot be resumed: a new one is started, with a warning.
        let (sender, events) = mpsc::channel();
        let options = Options { resume: Some("00000000-0000-0000-0000-000000000000".into()), ..options };
        let session = Session::connect(&options, move |event| drop(sender.send(event))).unwrap();
        let mut warned = false;
        loop {
            match events.recv_timeout(Duration::from_secs(60)).expect("no Ready in time") {
                Event::Warning(warning) => {
                    println!("warning: {warning}");
                    warned |= warning.contains("could not be resumed");
                }
                Event::Ready { thread_id: new, resumed, .. } => {
                    assert!(!resumed && new != thread_id && warned);
                    break;
                }
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                other => println!("{other:?}"),
            }
        }
        drop(session);
    }

    /// Starts a turn, which uses the account when the binary is signed in (and fails after some retries when
    /// it is not): run by hand, once, to see a turn's events from the real binary.
    #[test]
    #[ignore = "talks to the real Codex binary and starts a turn"]
    fn live_turn() {
        let options = live_options();
        let (sender, events) = mpsc::channel();
        let session = Session::connect(&options, move |event| drop(sender.send(event))).unwrap();
        // Before the thread is open: both wait, the second for the first one's turn.
        session.send(&[Input::Text("ping".into())], Some("client-1")).unwrap();
        session.send(&[Input::Text("and again".into())], Some("client-2")).unwrap();
        let (mut thread, mut seen, mut started, mut reply) = (String::new(), Vec::new(), 0, String::new());
        let (turn_id, status, error) = loop {
            match events.recv_timeout(Duration::from_secs(180)).expect("the turn takes too long") {
                Event::Ready { thread_id, .. } => thread = thread_id,
                Event::UserMessage { client_id, text } => {
                    println!("user message: {client_id:?} {text:?}");
                    seen.extend(client_id);
                }
                Event::TurnStarted { turn_id } => {
                    println!("turn started: {turn_id}");
                    started += 1;
                }
                Event::TextDelta { text, .. } => reply.push_str(&text),
                Event::TurnCompleted { turn_id, status, error } => break (turn_id, status, error),
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                other => println!("{other:?}"),
            }
        };
        println!("turn {turn_id} ended {status:?}: {error:?}; reply {reply:?}");
        assert!(!thread.is_empty() && !turn_id.is_empty());
        assert_eq!(started, 1, "the second message joined the first one's turn");
        assert_eq!(seen, ["client-1", "client-2"]);
        match status {
            TurnStatus::Completed => assert!(reply.to_lowercase().contains("pong"), "reply: {reply:?}"),
            TurnStatus::Failed => assert!(error.is_some()),
            TurnStatus::Interrupted => panic!("nobody interrupted"),
        }
        drop(session);

        // The thread can be continued by its id.
        let (sender, events) = mpsc::channel();
        let options = Options { resume: Some(thread.clone()), ..options };
        let session = Session::connect(&options, move |event| drop(sender.send(event))).unwrap();
        loop {
            match events.recv_timeout(Duration::from_secs(60)).expect("no Ready in time") {
                Event::Ready { thread_id, resumed, .. } => {
                    assert!(resumed, "the thread was not resumed");
                    assert_eq!(thread_id, thread);
                    break;
                }
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                other => println!("{other:?}"),
            }
        }
        // A turn that is stopped ends as interrupted, also when it is stopped before Codex announced it.
        session.send(&[Input::Text("ping".into())], None).unwrap();
        session.interrupt().unwrap();
        let ended = |events: &mpsc::Receiver<Event>, until_message: Option<&str>| loop {
            match events.recv_timeout(Duration::from_secs(120)).expect("nothing came of it") {
                Event::TurnCompleted { status, .. } if until_message.is_none() => break Some(status),
                Event::UserMessage { text, .. } if until_message == Some(text.as_str()) => break None,
                Event::Closed { exit_code, error } => panic!("closed early: {exit_code:?} {error:?}"),
                other => println!("{other:?}"),
            }
        };
        assert_eq!(ended(&events, None), Some(TurnStatus::Interrupted));

        // A message sent while the conversation is summarised waits for that to end, then starts a turn.
        session.compact().unwrap();
        session.send(&[Input::Text("after".into())], None).unwrap();
        let compacted = ended(&events, None);
        println!("compaction ended {compacted:?}");
        assert_ne!(compacted, Some(TurnStatus::Interrupted));
        ended(&events, Some("after"));
        session.interrupt().unwrap();
        assert_eq!(ended(&events, None), Some(TurnStatus::Interrupted));
    }
}
