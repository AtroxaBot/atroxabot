//! The stdio transport: the server is a child process, and each message is one line of JSON on its stdin
//! or stdout. Its stderr is logging; the end of it is kept to explain a server that dies.

use std::{
    io::{self, BufReader, Read, Write},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    conn::{Stop, wait},
    error::Error,
    lock,
    proto::{self, Line, MAX_LINE},
};

/// `CREATE_NO_WINDOW`: a console program started by a GUI app would otherwise flash a console window.
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;
/// What `cmd.exe` gives a meaning to, plus line breaks. No argument holding one is passed through it.
const SHELL_SPECIAL: [char; 10] = ['&', '|', '<', '>', '^', '%', '!', '"', '\r', '\n'];
/// How many bytes of stderr are kept.
const STDERR_KEPT: usize = 4096;
/// How many messages may wait unread before the reader (and so the server's stdout) is held back.
const BACKLOG: usize = 16;

/// A running server process.
pub(crate) struct Link {
    child: Child,
    /// Lines for the server's stdin, written by a thread so a server that stops reading cannot block a
    /// caller. Dropping it closes stdin.
    outgoing: Option<Sender<Vec<u8>>>,
    /// Messages from the server's stdout; `None` stands for a line over [`MAX_LINE`] that was dropped.
    incoming: Receiver<Option<Value>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The environment values given to the server, removed from any stderr text that is shown.
    secrets: Vec<String>,
}

impl Link {
    /// Starts `command` with `args`, adding `env` to this process's environment.
    pub(crate) fn spawn(command: &str, args: &[String], env: &[(String, String)]) -> Result<Self, Error> {
        let mut child = start(command, args, env)?;
        let (to_server, outgoing) = mpsc::channel();
        let (to_caller, incoming) = mpsc::sync_channel(BACKLOG);
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&stderr);
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let threads = match pipes {
            (Some(stdin), Some(stdout), Some(errors)) => worker("mcp-stdin", move || write_lines(stdin, &outgoing))
                .and_then(|()| worker("mcp-stdout", move || read_messages(stdout, &to_caller)))
                .and_then(|()| worker("mcp-stderr", move || keep_end(errors, &kept))),
            _ => Err(io::Error::other("its pipes are missing")),
        };
        let secrets = env.iter().map(|(_, value)| value.clone()).filter(|value| !value.is_empty()).collect();
        // From here on dropping the link stops the process.
        let link = Self { child, outgoing: Some(to_server), incoming, stderr, secrets };
        threads.map(|()| link).map_err(Error::Spawn)
    }

    /// Queues a message for the server.
    pub(crate) fn send(&mut self, message: &Value) -> Result<(), Error> {
        let sent = self.outgoing.as_ref().is_some_and(|outgoing| outgoing.send(proto::frame(message)).is_ok());
        if sent { Ok(()) } else { Err(self.exited()) }
    }

    /// Waits for the server's next message. `None` is a message that was too large to read.
    pub(crate) fn receive(&mut self, deadline: Instant, stop: &Stop<'_>) -> Result<Option<Value>, Error> {
        wait(&self.incoming, deadline, stop)?.ok_or_else(|| self.exited())
    }

    /// Whether the process is still running.
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Closes the server's stdin, which asks it to exit.
    pub(crate) fn hang_up(&mut self) {
        self.outgoing = None;
    }

    /// Closes stdin, gives the process until `deadline` to exit and then kills it.
    pub(crate) fn stop(&mut self, deadline: Instant) {
        self.hang_up();
        while self.alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            kill(&mut self.child);
        }
    }

    /// The error for a server that is gone, with the end of its stderr.
    fn exited(&self) -> Error {
        // A dying process usually explains itself on stderr; give the thread reading it a moment to finish.
        for _ in 0..20 {
            if Arc::strong_count(&self.stderr) == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let mut said = String::from_utf8_lossy(&lock(&self.stderr)).into_owned();
        for secret in &self.secrets {
            said = said.replace(secret.as_str(), "[hidden]");
        }
        Error::Exited(said.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop(Instant::now());
    }
}

/// Runs `work` on a named thread.
fn worker(name: &str, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new().name(name.to_owned()).spawn(work).map(drop)
}

/// Starts the server process with piped standard streams and no console window.
///
/// On Windows a program that cannot be found as an executable and is a bare name (`npx`, `uvx`: no folder,
/// no extension) is taken for a `.cmd` or `.bat` shim, which only `cmd.exe` can run, and started as
/// `cmd.exe /D /C <program> <arguments…>`. The arguments are quoted by the standard library as for any
/// program (double quotes around one with spaces, backslashes before a quote doubled), which `cmd.exe` then
/// reads a second time with rules of its own. So nothing it gives a meaning to is let through: the program
/// and every argument must be free of `& | < > ^ % ! "` and line breaks, and the program of spaces too
/// (a line that starts with a quote makes `cmd.exe` strip quotes). What is left is plain words and quoted
/// words with spaces, which both parsers read the same way. `/D` keeps registry `AutoRun` commands out.
fn start(command: &str, args: &[String], env: &[(String, String)]) -> Result<Child, Error> {
    let bare = !command.contains(['/', '\\', ':', '.']);
    match prepare(Command::new(command), args, env).spawn() {
        Err(error) if cfg!(windows) && bare && error.kind() == io::ErrorKind::NotFound => {
            let special = |text: &str| text.contains(SHELL_SPECIAL);
            if special(command) || command.contains(char::is_whitespace) || args.iter().any(|arg| special(arg)) {
                return Err(Error::Config(format!(
                    "{command:?} is not a program but a script that only cmd.exe can run, and its arguments \
                     must not contain any of & | < > ^ % ! \" or a line break"
                )));
            }
            let mut shell = Command::new("cmd.exe");
            shell.args(["/D", "/C", command]);
            prepare(shell, args, env).spawn().map_err(Error::Spawn)
        }
        started => started.map_err(Error::Spawn),
    }
}

fn prepare(mut command: Command, args: &[String], env: &[(String, String)]) -> Command {
    command.args(args).envs(env.iter().map(|(name, value)| (name, value)));
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, NO_WINDOW);
    command
}

/// Ends the process now. On Windows its children go with it: a script shim is a chain of processes
/// (`cmd.exe`, the launcher, the server) of which `TerminateProcess` would only end the first.
fn kill(child: &mut Child) {
    #[cfg(windows)]
    {
        let mut tree = Command::new("taskkill.exe");
        tree.args(["/T", "/F", "/PID", &child.id().to_string()]);
        tree.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        std::os::windows::process::CommandExt::creation_flags(&mut tree, NO_WINDOW);
        let _ = tree.status();
    }
    if child.kill().is_ok() {
        let _ = child.wait();
    }
}

/// Writes queued lines to the server until the queue is dropped or the server stops reading.
fn write_lines(mut stdin: ChildStdin, outgoing: &Receiver<Vec<u8>>) {
    for line in outgoing {
        if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
            return;
        }
    }
}

/// Reads the server's stdout until it ends. A line that is not JSON is passed over: servers do print to
/// stdout by mistake. So is a notification (a message without an id): none is acted on, and a server that
/// chatters while nobody calls it must not fill the queue and block on its own output.
fn read_messages(stdout: ChildStdout, incoming: &SyncSender<Option<Value>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        let message = match proto::read_line(&mut reader, &mut line, MAX_LINE) {
            Ok(Line::Text) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) if message.get("id").is_some_and(|id| !id.is_null()) => Some(message),
                _ => continue,
            },
            Ok(Line::TooLong) => None,
            Ok(Line::End) | Err(_) => return,
        };
        if incoming.send(message).is_err() {
            return;
        }
    }
}

/// Keeps the last whole lines of the server's stderr, [`STDERR_KEPT`] bytes at most.
fn keep_end(mut stderr: ChildStderr, kept: &Mutex<Vec<u8>>) {
    let mut chunk = [0_u8; 1024];
    while let Ok(read @ 1..) = stderr.read(&mut chunk) {
        let mut kept = lock(kept);
        kept.extend_from_slice(chunk.get(..read).unwrap_or_default());
        if kept.len() > STDERR_KEPT {
            // Cut at a line break, so that a secret is never shown in part (and then not recognised).
            let excess = kept.len() - STDERR_KEPT;
            let line_start =
                kept.iter().skip(excess).position(|byte| *byte == b'\n').map_or(kept.len(), |at| excess + at + 1);
            kept.drain(..line_start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let Err(error) = Link::spawn("C:/no/such/folder/atroxabot-missing.exe", &[], &[]) else { panic!("it started") };
        assert!(matches!(error, Error::Spawn(_)), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn the_shell_fallback_refuses_what_cmd_would_interpret() {
        let key = ("KEY".to_owned(), "s3cret-value".to_owned());
        for bad in ["a&b", "50%", "x|y", "<in", ">out", "^", "!x!", "say \"hi\"", "line\nbreak"] {
            let error =
                Link::spawn("atroxabot-no-such-shim", &["ok".to_owned(), bad.to_owned()], std::slice::from_ref(&key));
            let Err(Error::Config(why)) = error else { panic!("{bad} was let through") };
            assert!(why.contains("cmd.exe") && !why.contains("s3cret-value"), "{why}");
        }
        assert!(matches!(Link::spawn("no such shim", &[], &[]), Err(Error::Config(_))));
        // A name with an extension or a folder is never handed to the shell.
        assert!(matches!(Link::spawn("atroxabot-no-such.exe", &["a&b".to_owned()], &[]), Err(Error::Spawn(_))));
        // Clean arguments reach cmd.exe, which starts, finds nothing by that name and says so on stderr.
        let mut link = Link::spawn("atroxabot-no-such-shim", &["two words".to_owned()], &[]).unwrap();
        let never = std::sync::atomic::AtomicBool::new(false);
        let gone = link.receive(Instant::now() + Duration::from_secs(20), &Stop { cancel: &never, closed: &never });
        assert!(matches!(gone, Err(Error::Exited(said)) if said.contains("atroxabot-no-such-shim")));
    }
}
