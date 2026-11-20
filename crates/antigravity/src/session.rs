//! A running `agy acp` process: its pipes, its reader threads and what the caller asks of it.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::{build_args, command, profile_env};
use crate::machine::{Command, Machine, Message, NOT_SIGNED_IN, Output, Part, initialize_params, request_line};
use crate::parse::{self, Incoming, Refusal, Settings, field, opt_text};
use crate::{Decision, Effort, Error, Event, Info, Input, Options, SlashCommand, find_cli};

/// Longest line accepted from the binary; a longer one is dropped and reported.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// How long [`Session::close`] waits at each step: the binary needs a moment after end of input to save its
/// conversation.
const GRACE: Duration = Duration::from_secs(5);
/// Interval for polling the process and a busy input lock.
const POLL: Duration = Duration::from_millis(20);
/// The largest picture sent along with a message, in bytes; the binary takes no larger.
const MAX_PICTURE: u64 = 10 * 1024 * 1024;
/// The models [`info`] asks for their levels of thinking, at most.
const MAX_MODELS: usize = 64;

type Callback = Box<dyn FnMut(Event) + Send>;

/// A connection to a running Antigravity process and the one session (conversation) it has open there.
///
/// All methods take `&self` and may be called from any thread, also from the callback; writes are serialized.
/// What is asked before the session is open ([`Event::Ready`]) waits and is sent, in order, once it is.
/// Dropping the session closes it, which can block for a few seconds (see [`Session::close`]).
pub struct Session {
    shared: Arc<Shared>,
}

/// State shared with the reader threads.
struct Shared {
    /// The binary's standard input; `None` once closed.
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    /// The protocol's state. Held while its lines are written and its events queued, so that both leave in
    /// the order it made them; never held while the callback runs.
    machine: Mutex<Machine>,
    /// The caller's callback; dropped after [`Event::Closed`].
    callback: Mutex<Option<Callback>>,
    /// What the callback is still to be given. Events come from the reader threads and from the caller's own
    /// (a message sent begins a turn), which may be inside the callback: whoever holds the callback empties
    /// the queue, so no two calls run at once and none waits for itself.
    queue: Mutex<VecDeque<Event>>,
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

/// Starts `agy acp` for `options`, its input and output piped, and says which folder the session works in.
fn launch(options: &Options, stderr: Stdio) -> Result<(Child, String), Error> {
    let path = match &options.cli_path {
        Some(path) => path.clone(),
        None => find_cli().ok_or(Error::NotFound)?,
    };
    let mut command = command(&path)?;
    command.args(build_args(options)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
    // The environment is inherited as it is: the binary signs in from what it finds there and in its own
    // folder, which is none of this crate's business.
    if let Some(profile) = &options.profile {
        command.envs(profile_env(profile));
    }
    command.envs(options.env.iter().map(|(key, value)| (key, value)));
    let cwd = match &options.cwd {
        Some(cwd) if cwd.is_dir() => {
            command.current_dir(cwd);
            cwd.clone()
        }
        Some(cwd) => {
            return Err(Error::InvalidOption(format!("working directory does not exist: {}", cwd.display())));
        }
        None => std::env::current_dir().map_err(Error::Io)?,
    };
    // The protocol wants the folder named in full.
    let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
    let child = command.spawn().map_err(Error::Spawn)?;
    Ok((child, cwd.to_string_lossy().into_owned()))
}

impl Session {
    /// Starts the Antigravity binary and returns at once; the handshake and the opening of the session happen
    /// on the reader thread, and end in an [`Event::Ready`]. The binary unpacks itself each time it starts,
    /// which can take a minute.
    ///
    /// `on_event` receives everything that happens, never two calls at once, possibly before this function
    /// returns: mostly on the reader threads, and on the caller's own for what a call of its brings about.
    /// The last event is [`Event::Closed`], after which the callback is dropped. It should return quickly and
    /// not panic, since nothing else is read meanwhile; sending from it is fine, [`Session::close`] would
    /// stall it for the grace period.
    ///
    /// # Errors
    /// [`Error::NotFound`] when no binary is found, [`Error::BatchScript`] for a `.cmd`/`.bat` path on Windows,
    /// [`Error::InvalidOption`] for a refused option, [`Error::Spawn`] when the binary cannot be started, and
    /// [`Error::Io`] when the first write or a reader thread fails.
    pub fn connect(options: &Options, on_event: impl FnMut(Event) + Send + 'static) -> Result<Self, Error> {
        let (mut child, cwd) = launch(options, Stdio::piped())?;
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let callback: Callback = Box::new(on_event);
        let mut machine = Machine::new(options, cwd);
        let hello = machine.hello();
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            machine: Mutex::new(machine),
            callback: Mutex::new(Some(callback)),
            queue: Mutex::new(VecDeque::new()),
            failure: Mutex::new(None),
            closed: AtomicBool::new(false),
            said: Mutex::new(VecDeque::new()),
        });
        // From here on an early return drops the session, which stops the process.
        let session = Self { shared };

        let (stderr_open, stderr_closed) = mpsc::channel::<()>();
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("agy-stderr".into()).spawn(move || {
            if let Some(stderr) = stderr {
                shared.read_stderr(stderr);
            }
            drop(stderr_open);
        });
        spawned.map_err(Error::Io)?;
        let shared = Arc::clone(&session.shared);
        let spawned = thread::Builder::new().name("agy-stdout".into()).spawn(move || {
            let pipe_error = stdout.and_then(|stdout| shared.read_stdout(stdout));
            shared.finish(pipe_error, &stderr_closed);
        });
        spawned.map_err(Error::Io)?;

        session.shared.write(&hello)?;
        Ok(session)
    }

    /// Sends the user's input. It starts a turn when none runs and is put into the running one otherwise,
    /// where the agent reads it at its next step. `client_id` names the message: it comes back in the
    /// [`Event::UserMessage`] that says the message was given to the agent.
    ///
    /// While the running turn is being stopped the message waits and starts the turn after it.
    ///
    /// # Errors
    /// [`Error::InvalidOption`] for an empty input, [`Error::Closed`] after the session closed, [`Error::Io`]
    /// when the write fails.
    pub fn send(&self, input: &[Input], client_id: Option<&str>) -> Result<(), Error> {
        self.shared.command(Command::Send(message(input, client_id)?))
    }

    /// Gives the agent input in the turn that runs, and only there: an [`Event::UserMessage`] with
    /// `client_id` says so. When no turn runs, or the running one is being stopped, the input is dropped and
    /// no such event comes: it is the caller's to send again.
    ///
    /// # Errors
    /// [`Error::InvalidOption`] for an empty input, [`Error::Closed`] after the session closed, [`Error::Io`]
    /// when the write fails.
    pub fn steer(&self, input: &[Input], client_id: &str) -> Result<(), Error> {
        self.shared.command(Command::Steer(message(input, Some(client_id))?))
    }

    /// Asks the binary to stop the running turn, which ends with an [`Event::TurnCompleted`]; what the binary
    /// still asked is withdrawn ([`Event::RequestResolved`]). With no turn running it stops the commands the
    /// agent left running, if any.
    ///
    /// # Errors
    /// [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn interrupt(&self) -> Result<(), Error> {
        self.shared.command(Command::Interrupt)
    }

    /// Answers the [`Event::Approval`] with this `request_id`, with the nearest to `decision` that the binary
    /// offers.
    ///
    /// # Errors
    /// [`Error::UnknownRequest`] when no such approval is pending (already answered, or withdrawn as an
    /// [`Event::RequestResolved`] says), [`Error::Closed`] after the session closed, [`Error::Io`] when the
    /// write fails.
    pub fn respond(&self, request_id: &str, decision: Decision) -> Result<(), Error> {
        self.shared.reply(|machine| machine.respond(request_id, decision))
    }

    /// Answers the [`Event::Question`] with this `request_id` with one of its choices, named by its id or by
    /// its text; `None` leaves it unanswered, which the agent is told.
    ///
    /// # Errors
    /// [`Error::UnknownRequest`] when no such question is pending or `choice` is none of its choices (the
    /// question then stays), [`Error::Closed`] after the session closed, [`Error::Io`] when the write fails.
    pub fn answer(&self, request_id: &str, choice: Option<&str>) -> Result<(), Error> {
        self.shared.reply(|machine| machine.answer(request_id, choice))
    }

    /// Changes the model and the level of thinking the following turns run with; `None` leaves them as they
    /// are. They are set as the next turn starts: a message put into a running turn does not change them.
    pub fn retune(&self, model: Option<String>, effort: Option<String>) {
        lock(&self.shared.machine).retune(model, effort);
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

    /// Carries out what the machine made of a line or a command: writes its lines and queues its events.
    /// Called with the machine still locked, so that nothing overtakes it.
    fn carry(&self, output: Output) -> Result<(), Error> {
        let mut written = Ok(());
        for line in &output.lines {
            // The first failure is the one to tell; after it the process is going away.
            let result = self.write(line);
            written = written.and(result);
        }
        lock(&self.queue).extend(output.events);
        if let Some(why) = output.fatal {
            // Nothing was begun that the binary would have to save: it is ended at once.
            self.closed.store(true, Ordering::Relaxed);
            *lock(&self.failure) = Some(why);
            *lock(&self.stdin) = None;
            let _ = lock(&self.child).kill();
        }
        written
    }

    /// Gives the callback what is queued, unless another thread is at it: that one gives it this too.
    fn deliver(&self) {
        loop {
            let mut callback = match self.callback.try_lock() {
                Ok(callback) => callback,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            loop {
                let next = lock(&self.queue).pop_front();
                let Some(event) = next else { break };
                let last = matches!(event, Event::Closed { .. });
                if let Some(callback) = callback.as_mut() {
                    callback(event);
                }
                if last {
                    *callback = None;
                }
            }
            drop(callback);
            // What was queued between the last look and the letting go would otherwise wait for the next event.
            if lock(&self.queue).is_empty() {
                return;
            }
        }
    }

    /// Does what the caller asks, now or once it can be done.
    fn command(&self, command: Command) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let done = {
            let mut machine = lock(&self.machine);
            let output = machine.command(command);
            self.carry(output)
        };
        self.deliver();
        done
    }

    /// Writes the answer to a request of the binary, as `answer` makes it.
    fn reply(&self, answer: impl FnOnce(&mut Machine) -> Result<Value, Error>) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let mut machine = lock(&self.machine);
        self.write(&answer(&mut machine)?)
    }

    /// Takes one line of the binary's output: writes what it calls for, then reports what it says.
    fn handle(&self, line: &str) {
        {
            let mut machine = lock(&self.machine);
            let output = machine.receive(line);
            // A failed write means the process is going away; `Closed` will say so.
            let _ = self.carry(output);
        }
        self.deliver();
    }

    /// Reads the binary's output until it ends. Returns the read error, if that is what ended it.
    fn read_stdout(&self, stdout: impl Read) -> Option<String> {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_line(&mut reader, MAX_LINE) {
                Ok(Line::Text(line)) => self.handle(&line),
                Ok(Line::TooLong) => {
                    lock(&self.queue).push_back(Event::Warning(format!("dropped a line longer than {MAX_LINE} bytes")));
                    self.deliver();
                }
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
                    if line.is_empty() {
                        continue;
                    }
                    if parse::is_sign_in_link(&line) {
                        // It waits for a sign-in in a browser, which is not this crate's to see through. The
                        // link is not passed on: it is the start of a sign-in.
                        let mut machine = lock(&self.machine);
                        let mut output = Output::default();
                        machine.signed_out(&mut output);
                        let _ = self.carry(output);
                    } else {
                        keep_said(&mut lock(&self.said), &line);
                        lock(&self.queue).push_back(Event::Stderr(line));
                    }
                    self.deliver();
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
        let pipe_error = pipe_error.map(|error| format!("reading from the Antigravity binary failed: {error}"));
        let error = failure.or(pipe_error).or_else(|| match exit_code {
            Some(0) => None,
            Some(code) => Some(match explanation(&lock(&self.said)) {
                Some(said) => format!("the Antigravity binary exited with code {code} ({said})"),
                None => format!("the Antigravity binary exited with code {code}"),
            }),
            None => Some("the Antigravity binary was terminated by a signal".into()),
        });
        self.closed.store(true, Ordering::Relaxed);
        *lock(&self.stdin) = None;
        lock(&self.machine).close();
        lock(&self.queue).push_back(Event::Closed { exit_code, error });
        self.deliver();
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

/// The user's input, read and ready to send: pictures are read here, off the protocol's state.
fn message(input: &[Input], client_id: Option<&str>) -> Result<Message, Error> {
    if input.is_empty() {
        return Err(Error::InvalidOption("there is nothing to send".into()));
    }
    let part = |part: &Input| match part {
        Input::Text(text) => Part::Text(text.clone()),
        Input::LocalImage(path) => {
            let named = path.to_string_lossy().into_owned();
            match picture(path) {
                Some((mime, data)) => Part::Image { mime, data, path: named },
                // Not a picture the binary reads: the agent is told where the file is.
                None => Part::Text(format!("[picture: {named}]")),
            }
        }
    };
    Ok(Message { parts: input.iter().map(part).collect(), client_id: client_id.map(str::to_owned) })
}

/// The file at `path` as a picture to send along: its media type and its bytes in base64. `None` for a kind of
/// file the binary does not take as a picture, one too large, or one that cannot be read.
fn picture(path: &Path) -> Option<(&'static str, String)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    let size = std::fs::metadata(path).ok()?.len();
    if size == 0 || size > MAX_PICTURE {
        return None;
    }
    let mut bytes = Vec::new();
    // The file may have grown since it was measured.
    std::fs::File::open(path).ok()?.take(MAX_PICTURE + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_PICTURE).then(|| (mime, base64(&bytes)))
}

/// `bytes` in base64, padded.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let bits = (u32::from(group[0]) << 16) | (u32::from(group[1]) << 8) | u32::from(group[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            let letter = ALPHABET[(bits >> shift) as usize & 63];
            // A group short of three bytes is filled up with padding.
            encoded.push(if index <= chunk.len() { char::from(letter) } else { '=' });
        }
    }
    encoded
}

/// What Antigravity offers for an account: its models, how hard each can think and the commands it takes.
/// Starts the binary, opens a session in [`Options::cwd`] without sending it a message, so nothing of the
/// account's quota is used, and ends it. The binary keeps that empty session among the account's
/// conversations, and makes `.gemini` and `.agents` folders in the working directory: give it a folder of no
/// account. Waits at most `timeout` in all; the binary alone can take a minute to start.
///
/// # Errors
/// The binary could not be started ([`Error::NotFound`], [`Error::BatchScript`], [`Error::InvalidOption`],
/// [`Error::Spawn`], [`Error::Io`]), or ended first, refused or did not answer in time ([`Error::NoInfo`]). An
/// account that is not signed in is no error: [`Info::signed_in`] says so.
pub fn info(options: &Options, timeout: Duration) -> Result<Info, Error> {
    let (mut child, cwd) = launch(options, Stdio::null())?;
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    let (sender, heard) = mpsc::channel();
    // Ends the process when dropped, however this function returns.
    let mut probe =
        Probe { child, stdin, heard, next_id: 1, deadline: Instant::now().checked_add(timeout), commands: Vec::new() };
    let spawned = thread::Builder::new().name("agy-info".into()).spawn(move || {
        let Some(stdout) = stdout else { return };
        let mut reader = BufReader::new(stdout);
        while let Ok(line) = read_line(&mut reader, MAX_LINE) {
            let heard = match line {
                Line::Text(line) => match parse::parse_line(&line) {
                    Incoming::Response { id, outcome } => Heard::Answer(id, outcome),
                    Incoming::SignInLink => Heard::SignInLink,
                    Incoming::Notification { method, params } if method == "session/update" => {
                        let update = field(&params, "update");
                        if parse::text(update, "sessionUpdate") != "available_commands_update" {
                            continue;
                        }
                        Heard::Commands(parse::commands(update))
                    }
                    // Requests of the binary get no answer here: none is expected without a message.
                    _ => continue,
                },
                Line::TooLong => continue,
                Line::End => return,
            };
            if sender.send(heard).is_err() {
                return;
            }
        }
    });
    spawned.map_err(Error::Io)?;
    probe.ask_all(options, &cwd).map_err(Error::NoInfo)
}

/// What the reader thread of [`info`] passes on.
enum Heard {
    /// The answer to a request, by its id.
    Answer(u64, Result<Value, Refusal>),
    /// The slash commands the binary takes.
    Commands(Vec<SlashCommand>),
    /// The binary waits for a sign-in in a browser.
    SignInLink,
}

/// An Antigravity process that is only asked questions.
struct Probe {
    child: Child,
    stdin: Option<ChildStdin>,
    /// What the reader thread passes on.
    heard: mpsc::Receiver<Heard>,
    next_id: u64,
    /// When to give up; `None` never.
    deadline: Option<Instant>,
    /// The slash commands the binary said it takes.
    commands: Vec<SlashCommand>,
}

impl Probe {
    /// The questions of [`info`], in order.
    fn ask_all(&mut self, options: &Options, cwd: &str) -> Result<Info, String> {
        let said = |refusal: Refusal| refusal.message;
        let can = parse::abilities(&self.ask("initialize", initialize_params(options)).map_err(said)?);
        let open = json!({"cwd": cwd, "mcpServers": []});
        let mut opened = self.ask("session/new", open.clone());
        // It is asked to use the sign-in it has, where the caller allows that, as a session is.
        let method = options.authenticate.as_ref().filter(|method| !method.trim().is_empty());
        if let (Err(refusal), Some(method)) = (&opened, method)
            && refusal.needs_sign_in()
        {
            opened = self.ask("authenticate", json!({"methodId": method})).and_then(|_| self.ask("session/new", open));
        }
        let opened = match opened {
            Ok(opened) => opened,
            Err(refusal) if refusal.needs_sign_in() => {
                return Ok(Info { version: can.version, ..Info::default() });
            }
            Err(refusal) => return Err(said(refusal)),
        };
        let session = opt_text(&opened, "sessionId").ok_or("it opened a session without naming it")?;
        let mut settings = Settings::of(&opened);
        let catalog = settings.catalog.clone();
        let mut efforts: Vec<(String, Vec<Effort>)> = Vec::new();
        let mut note = |settings: &Settings| {
            let (model, levels) = (&settings.catalog.model, &settings.catalog.efforts);
            if !levels.is_empty() && efforts.iter().all(|(known, _)| known != model) {
                efforts.push((model.clone(), levels.clone()));
            }
        };
        note(&settings);
        // A model says how hard it can think only while it is the one in use: each is chosen in turn. The
        // session is thrown away, so the last one stays chosen.
        if let Some(option) = settings.model_option.clone() {
            for model in catalog.models.iter().filter(|model| model.id != catalog.model).take(MAX_MODELS) {
                let params = json!({"sessionId": session, "configId": option, "value": model.id});
                // One that cannot be chosen, or the time running out, leaves what is known so far.
                match self.ask("session/set_config_option", params) {
                    Ok(answer) => {
                        settings.catalog.model.clone_from(&model.id);
                        settings.update(field(&answer, "configOptions"));
                        note(&settings);
                    }
                    Err(refusal) if refusal.code == 0 => break,
                    Err(_) => {}
                }
            }
        }
        Ok(Info {
            version: can.version,
            signed_in: true,
            catalog,
            efforts,
            commands: std::mem::take(&mut self.commands),
        })
    }

    fn write(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("it stopped before answering")?;
        let mut line = message.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|error| error.to_string())
    }

    /// Sends a request and waits for its answer. A refusal of code zero is none of the binary's: it did not
    /// answer.
    fn ask(&mut self, method: &str, params: Value) -> Result<Value, Refusal> {
        let silent = |why: &str| Refusal { code: 0, message: why.to_owned() };
        let id = self.next_id;
        self.next_id += 1;
        self.write(&request_line(method, id, params)).map_err(|why| silent(&why))?;
        loop {
            let left =
                self.deadline.map_or(Duration::MAX, |deadline| deadline.saturating_duration_since(Instant::now()));
            match self.heard.recv_timeout(left) {
                Ok(Heard::Answer(answered, outcome)) if answered == id => {
                    return outcome.map_err(|refusal| Refusal {
                        code: refusal.code,
                        message: format!("{method}: {}", refusal.message),
                    });
                }
                Ok(Heard::Answer(..)) => {}
                Ok(Heard::Commands(commands)) => self.commands = commands,
                Ok(Heard::SignInLink) => return Err(Refusal { code: -32000, message: NOT_SIGNED_IN.to_owned() }),
                Err(RecvTimeoutError::Timeout) => return Err(silent("it did not answer in time")),
                Err(RecvTimeoutError::Disconnected) => return Err(silent("it stopped before answering")),
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
/// line. Lines of a stack trace explain nothing.
fn explanation(said: &VecDeque<String>) -> Option<&str> {
    let telling = |line: &&String| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with("at ") && !line.starts_with("File \"") && !line.starts_with("goroutine ")
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
    const ESCAPE: char = '\x1b';
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != ESCAPE {
            plain.push(c);
        } else if chars.next() == Some('[') {
            // A control sequence runs up to its final byte, a letter or the like.
            chars.by_ref().find(|c| ('\x40'..='\x7e').contains(c));
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
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn session_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Session>();
    }

    #[test]
    fn base64_pads_what_is_short_of_three_bytes() {
        for (bytes, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xff, 0xfe, 0xfd][..], "//79"),
        ] {
            assert_eq!(base64(bytes), encoded);
        }
    }

    #[test]
    fn only_pictures_the_binary_reads_are_sent_along() {
        let dir = std::env::temp_dir().join(format!("atroxabot-agy-picture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.PNG");
        std::fs::write(&png, b"abc").unwrap();
        assert_eq!(picture(&png), Some(("image/png", "YWJj".to_owned())));
        let gif = dir.join("a.gif");
        std::fs::write(&gif, b"abc").unwrap();
        assert_eq!(picture(&gif), None);
        let empty = dir.join("empty.jpg");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(picture(&empty), None);
        assert_eq!(picture(&dir.join("missing.png")), None);

        let input = [Input::Text("look".into()), Input::LocalImage(png.clone()), Input::LocalImage(gif.clone())];
        let message = message(&input, Some("m1")).unwrap();
        assert_eq!(message.client_id.as_deref(), Some("m1"));
        assert_eq!(message.parts.len(), 3);
        assert!(matches!(&message.parts[1], Part::Image { mime: "image/png", data, .. } if data == "YWJj"));
        assert_eq!(message.parts[2], Part::Text(format!("[picture: {}]", gif.display())));
        assert!(matches!(super::message(&[], None), Err(Error::InvalidOption(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session's shared state around a process that does nothing, to drive the delivery of events.
    fn shared(callback: Callback) -> Option<Arc<Shared>> {
        // Any program that starts and ends will do; the test is skipped where there is none.
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let mut command = std::process::Command::new(program);
        if cfg!(windows) {
            command.args(["/C", "exit"]);
        }
        let child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
        Some(Arc::new(Shared {
            stdin: Mutex::new(None),
            child: Mutex::new(child),
            machine: Mutex::new(Machine::new(&Options::default(), "/work".to_owned())),
            callback: Mutex::new(Some(callback)),
            queue: Mutex::new(VecDeque::new()),
            failure: Mutex::new(None),
            closed: AtomicBool::new(false),
            said: Mutex::new(VecDeque::new()),
        }))
    }

    #[test]
    fn events_queued_from_inside_the_callback_follow_without_waiting_for_themselves() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let slot: Arc<Mutex<Option<Arc<Shared>>>> = Arc::new(Mutex::new(None));
        let (log, inner) = (Arc::clone(&seen), Arc::clone(&slot));
        let callback: Callback = Box::new(move |event| {
            let first = matches!(&event, Event::Warning(text) if text == "first");
            lock(&log).push(event);
            if first {
                // What a call from inside the callback brings about: queued, and delivered after this one.
                let shared = lock(&inner).clone().unwrap();
                lock(&shared.queue).push_back(Event::TurnStarted);
                shared.deliver();
                assert_eq!(lock(&log).len(), 1, "not delivered inside itself");
            }
        });
        let Some(shared) = shared(callback) else { return };
        *lock(&slot) = Some(Arc::clone(&shared));
        lock(&shared.queue).push_back(Event::Warning("first".into()));
        shared.deliver();
        assert_eq!(*lock(&seen), [Event::Warning("first".into()), Event::TurnStarted]);
        // After the last event the callback is gone, and what comes later is not delivered.
        lock(&shared.queue).push_back(Event::Closed { exit_code: Some(0), error: None });
        lock(&shared.queue).push_back(Event::TurnStarted);
        shared.deliver();
        assert_eq!(lock(&seen).len(), 3);
        assert!(lock(&shared.callback).is_none());
        *lock(&slot) = None;
    }

    #[test]
    fn events_from_many_threads_all_arrive_one_at_a_time() {
        let (count, inside) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (counted, busy) = (Arc::clone(&count), Arc::clone(&inside));
        let callback: Callback = Box::new(move |_| {
            assert!(!busy.swap(true, Ordering::SeqCst), "two calls at once");
            counted.fetch_add(1, Ordering::SeqCst);
            busy.store(false, Ordering::SeqCst);
        });
        let Some(shared) = shared(callback) else { return };
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    for _ in 0..500 {
                        lock(&shared.queue).push_back(Event::TurnStarted);
                        shared.deliver();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 4000);
        assert!(lock(&shared.queue).is_empty());
    }

    #[test]
    fn an_exit_is_explained_by_the_error_line_not_the_stack() {
        let mut said = VecDeque::new();
        for line in ["starting", "RuntimeError: no route to host", "  File \"main.py\", line 3", "    at run (x:1)", ""]
        {
            keep_said(&mut said, line);
        }
        assert_eq!(explanation(&said), Some("RuntimeError: no route to host"));
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
    fn invalid_utf8_is_replaced() {
        let mut reader = Cursor::new(b"{\"a\":\"\xff\"}\n".to_vec());
        let Line::Text(line) = read_line(&mut reader, 100).unwrap() else { panic!("a line") };
        assert!(line.starts_with("{\"a\":\"") && line.ends_with("\"}\n") && line.chars().count() == 10);
    }

    #[test]
    fn colours_are_stripped_from_log_lines() {
        let line = "\x1b[2m2026-11-11T17:05:14Z\x1b[0m \x1b[31mERROR\x1b[0m failed to connect";
        assert_eq!(strip_ansi(line), "2026-11-11T17:05:14Z ERROR failed to connect");
        assert_eq!(strip_ansi("plain [text]"), "plain [text]");
        assert_eq!(strip_ansi("cut \x1b[3"), "cut ");
        assert_eq!(strip_ansi("lone \x1b"), "lone ");
    }

    #[test]
    fn options_are_checked_before_anything_is_started() {
        let missing = std::env::temp_dir().join("atroxabot-no-such-dir");
        let binary = missing.join("agy.exe");
        let options = Options { cli_path: Some(binary.clone()), ..Options::default() };
        assert!(matches!(Session::connect(&options, |_| {}), Err(Error::Spawn(_))));
        assert!(matches!(info(&options, Duration::from_secs(1)), Err(Error::Spawn(_))));
        let options = Options { cli_path: Some(binary), cwd: Some(missing), ..Options::default() };
        assert!(matches!(Session::connect(&options, |_| {}), Err(Error::InvalidOption(_))));
        if cfg!(windows) {
            let options = Options { cli_path: Some(PathBuf::from("C:\\bin\\agy.cmd")), ..Options::default() };
            assert!(matches!(Session::connect(&options, |_| {}), Err(Error::BatchScript(_))));
        }
    }
}
