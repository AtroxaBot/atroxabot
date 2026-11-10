//! A small [Model Context Protocol](https://modelcontextprotocol.io) client: it starts local MCP servers
//! (stdio) or reaches remote ones (Streamable HTTP), lists their tools and calls them.
//!
//! Two eras of the protocol are spoken. Revision `2026-07-28` is stateless: every request carries its
//! protocol version in `_meta` and the server never asks the client anything. Revisions up to `2025-11-25`
//! open with an `initialize` handshake, may hand out an HTTP session, and let the server send requests of
//! its own (`ping` is answered, everything else refused: no client capability is declared). Which one a
//! server speaks is found out when it is first reached, the way the specification prescribes: a modern
//! `server/discover` first, `initialize` when that is not understood.
//!
//! Left out on purpose: OAuth (static headers only), resources, prompts, sampling, elicitation, roots,
//! subscriptions, and the HTTP+SSE transport of 2024.
//!
//! Everything is blocking and runs on plain threads. A [`Hub`] holds the configured servers; share it in
//! an `Arc`.

mod conn;
mod error;
mod http;
mod proto;
mod stdio;

pub use proto::base64;

use std::{
    fmt,
    sync::{
        Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use conn::{Conn, Stop};
use error::Error;

/// How long a tool call may take unless [`Hub::with_call_timeout`] says otherwise.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long [`Hub::close`] lets the servers exit by themselves before killing them.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// How to reach one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The name its tools are listed under and [`Hub::call`] finds it by (the first server of that name).
    pub name: String,
    /// Where the server is.
    pub transport: Transport,
}

/// The ways a server is reached. `Debug` names the environment variables and headers without their
/// values, and leaves out the query of the address.
#[derive(Clone, PartialEq, Eq)]
pub enum Transport {
    /// A local server started as a child process.
    Stdio {
        /// The program: a path, or a name found on `PATH`. On Windows a script shim such as `npx` works too.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Environment variables added to this process's own, as name and value.
        env: Vec<(String, String)>,
    },
    /// A remote server speaking Streamable HTTP.
    Http {
        /// The MCP endpoint: `https://…`, or `http://` on `localhost` or `127.0.0.1`.
        url: String,
        /// Headers sent with every request (an `Authorization`, an API key), as name and value.
        headers: Vec<(String, String)>,
    },
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// The names of name and value pairs.
        fn names(pairs: &[(String, String)]) -> Vec<&str> {
            pairs.iter().map(|(name, _)| name.as_str()).collect()
        }
        match self {
            Self::Stdio { command, args, env } => {
                f.debug_struct("Stdio").field("command", command).field("args", args).field("env", &names(env)).finish()
            }
            Self::Http { url, headers } => {
                // Keys are often passed as `?key=…`.
                let address = url.split_once('?').map_or_else(|| url.clone(), |(address, _)| format!("{address}?…"));
                f.debug_struct("Http").field("url", &address).field("headers", &names(headers)).finish()
            }
        }
    }
}

/// Splits a command line as the user typed it (`npx -y @scope/server "C:\My Docs"`) into program and
/// arguments: whitespace separates, double and single quotes group, a backslash is literal (Windows
/// paths). Returns `None` when the line is empty or a quote is unclosed.
#[must_use]
pub fn split_command(line: &str) -> Option<(String, Vec<String>)> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    for c in line.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                // `""` is a word, an empty one.
                word.get_or_insert_default();
            }
            None if c.is_whitespace() => words.extend(word.take()),
            _ => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    let mut words = words.into_iter();
    let program = words.next().filter(|program| quote.is_none() && !program.is_empty())?;
    Some((program, words.collect()))
}

/// A tool a server offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// The [`ServerConfig::name`] of the server that has it.
    pub server: String,
    /// The tool's name, unique within its server.
    pub name: String,
    /// What it does, in the server's words (untrusted text). Empty when the server gave none.
    pub description: String,
    /// The JSON Schema of its arguments; `{"type": "object"}` when the server gave none.
    pub input_schema: serde_json::Value,
}

/// One configured server: its connection once there is one, and its tools once they were listed.
struct Server {
    config: ServerConfig,
    /// Held for the length of a request, so calls to one server take turns.
    conn: Mutex<Option<Conn>>,
    tools: OnceLock<Vec<Tool>>,
}

impl Server {
    /// The connection, made now if there is none or the process behind it has died.
    fn connected<'a>(&self, slot: &'a mut Option<Conn>, stop: &Stop<'_>) -> Result<&'a mut Conn, Error> {
        let mut existing = slot.take();
        if existing.as_mut().is_some_and(|conn| !conn.alive()) {
            existing = None;
        }
        let conn = match existing {
            Some(conn) => conn,
            None if stop.requested() => return Err(Error::Stopped),
            None => Conn::connect(&self.config.transport, stop)?,
        };
        Ok(slot.insert(conn))
    }

    /// Lists the server's tools into `self.tools`, unless that was done before.
    fn list(&self, stop: &Stop<'_>) -> Result<(), Error> {
        let mut slot = lock(&self.conn);
        if self.tools.get().is_some() {
            return Ok(());
        }
        let listed = self.connected(&mut slot, stop).and_then(|conn| conn.list_tools(&self.config.name, stop));
        match listed {
            Ok(tools) => {
                let _ = self.tools.set(tools);
                Ok(())
            }
            Err(error) => {
                if error.ends_connection() {
                    *slot = None;
                }
                Err(error)
            }
        }
    }

    fn call(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
        timeout: Duration,
        stop: &Stop<'_>,
    ) -> Result<String, Error> {
        let mut slot = lock(&self.conn);
        // ponytail: a modern HTTP server's `x-mcp-header` parameters are only mirrored once `tools()` has
        // listed this server. Callers list before they call; if one ever does not, list here first.
        let schema = self.tools.get().and_then(|tools| tools.iter().find(|known| known.name == tool));
        let schema = schema.map(|tool| &tool.input_schema);
        let mut retried = false;
        loop {
            let conn = self.connected(&mut slot, stop)?;
            match conn.call_tool(tool, arguments, schema, Instant::now() + timeout, stop) {
                // The server forgot the session before it saw the request: start a new one and ask again.
                Err(Error::SessionExpired) if !retried => {
                    retried = true;
                    *slot = None;
                }
                Err(error) => {
                    if error.ends_connection() {
                        *slot = None;
                    }
                    return Err(error);
                }
                Ok(text) => return Ok(text),
            }
        }
    }
}

/// Every configured server, connected on demand and kept for reuse. `Send + Sync`; share it in an `Arc`.
///
/// Each server has its own lock: calls to different servers run side by side, calls to one server take
/// turns. Dropping the hub closes it.
pub struct Hub {
    servers: Vec<Server>,
    closed: AtomicBool,
    call_timeout: Duration,
}

impl Hub {
    /// A hub for `servers`. Nothing is started or contacted yet.
    #[must_use]
    pub fn new(servers: Vec<ServerConfig>) -> Self {
        let servers =
            servers.into_iter().map(|config| Server { config, conn: Mutex::new(None), tools: OnceLock::new() });
        Self { servers: servers.collect(), closed: AtomicBool::new(false), call_timeout: CALL_TIMEOUT }
    }

    /// The same hub with another limit on how long one tool call may take (120 s by default).
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// The tools of every server that can be reached, sorted by server name then tool name (a stable
    /// order), and for each server that could not be, its name and why.
    ///
    /// Servers not listed yet are connected, all at the same time, each bounded: about 20 s to start and
    /// agree on a protocol, and as long again to list. A server's list is kept after the first success, so
    /// later calls are instant and identical for it; a server that failed is tried again on every call.
    #[must_use]
    pub fn tools(&self) -> (Vec<Tool>, Vec<(String, String)>) {
        let never = AtomicBool::new(false);
        let stop = &Stop { cancel: &never, closed: &self.closed };
        let mut failures = Vec::new();
        thread::scope(|scope| {
            let unlisted = self.servers.iter().filter(|server| server.tools.get().is_none());
            // Collected, so that every server is being listed before the first one is waited for.
            let listing: Vec<_> = unlisted
                .map(|server| (server, thread::Builder::new().spawn_scoped(scope, move || server.list(stop))))
                .collect();
            for (server, thread) in listing {
                let listed = match thread {
                    Ok(thread) => {
                        thread.join().unwrap_or_else(|_| Err(Error::Protocol("could not be listed".to_owned())))
                    }
                    Err(error) => Err(Error::Spawn(error)),
                };
                if let Err(error) = listed {
                    failures.push((server.config.name.clone(), error.to_string()));
                }
            }
        });
        let mut tools: Vec<Tool> =
            self.servers.iter().filter_map(|server| server.tools.get()).flatten().cloned().collect();
        tools.sort_by(|a, b| (&a.server, &a.name).cmp(&(&b.server, &b.name)));
        (tools, failures)
    }

    /// Calls `tool` on the server named `server` and returns the result's text: text blocks joined by
    /// newlines, other kinds of block named (`[image]`), `structuredContent` appended as JSON when there
    /// is no text; cut at 200 000 characters with a note.
    ///
    /// The server is connected first if it is not. A stdio server that has died is started again, once
    /// per call; a call that was running when it died fails and is not repeated.
    ///
    /// # Errors
    ///
    /// The failure as a sentence for the model: the tool's own text when its result is flagged `isError`,
    /// a JSON-RPC error, a server that cannot be reached or died, a timeout (120 s unless
    /// [`Hub::with_call_timeout`] changed it), or exactly `stopped` when `cancel` was set (looked at every
    /// 25 ms while waiting) or the hub is closed. A cancelled or timed-out request is cancelled at the
    /// server as well.
    pub fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let stop = Stop { cancel, closed: &self.closed };
        let Some(found) = self.servers.iter().find(|known| known.config.name == server) else {
            return Err(format!("there is no MCP server named {server:?}"));
        };
        found.call(tool, arguments, self.call_timeout, &stop).map_err(|error| error.to_string())
    }

    /// Stops every started server: stdin is closed, the processes get a second to exit and are killed
    /// after it (on Windows with their child processes); sessions of remote servers are ended. Calls in
    /// flight return `stopped`, and so does every later one. Idempotent, also run on drop, and bounded:
    /// about a second for local servers plus up to two for each remote session that cannot be reached.
    pub fn close(&self) {
        // Waiting calls see the flag within a tick and let go of their server's lock.
        self.closed.store(true, Ordering::Relaxed);
        let mut conns: Vec<Conn> = self.servers.iter().filter_map(|server| lock(&server.conn).take()).collect();
        for conn in &mut conns {
            conn.hang_up();
        }
        let deadline = Instant::now() + EXIT_GRACE;
        for conn in &mut conns {
            conn.stop(deadline);
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.close();
    }
}

/// Locks a mutex whether or not a thread panicked while holding it: the data here stays usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_like_a_shell_without_escapes() {
        let splits = |line: &str, words: &[&str]| {
            let (program, args) = split_command(line).unwrap_or_else(|| panic!("{line:?} was not split"));
            let mut found = vec![program.as_str()];
            found.extend(args.iter().map(String::as_str));
            assert_eq!(found, words, "{line:?}");
        };
        splits("npx", &["npx"]);
        splits("  npx   -y\t@scope/server  ", &["npx", "-y", "@scope/server"]);
        splits(r#"npx -y @scope/server "C:\My Docs""#, &["npx", "-y", "@scope/server", r"C:\My Docs"]);
        splits(r#""C:\Program Files\nodejs\node.exe" server.js"#, &[r"C:\Program Files\nodejs\node.exe", "server.js"]);
        splits(r"python C:\tools\server.py --root=C:\data\", &["python", r"C:\tools\server.py", r"--root=C:\data\"]);
        splits(
            r#"run 'single "inner" quotes' "double 'inner'""#,
            &["run", r#"single "inner" quotes"#, "double 'inner'"],
        );
        splits(r#"run --name="two words" a""b '' """#, &["run", "--name=two words", "ab", "", ""]);
        splits("uvx mcp-server-git\n", &["uvx", "mcp-server-git"]);
        for bad in ["", "   ", "\t\n", r#"npx "unclosed"#, "npx 'unclosed", r#""" arg"#, r#"npx "a" 'b"#] {
            assert_eq!(split_command(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn debug_shows_no_secret() {
        let stdio = ServerConfig {
            name: "files".to_owned(),
            transport: Transport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned()],
                env: vec![("API_TOKEN".to_owned(), "s3cret-env".to_owned())],
            },
        };
        let http = Transport::Http {
            url: "https://example.com/mcp?key=s3cret-query".to_owned(),
            headers: vec![("Authorization".to_owned(), "Bearer s3cret-header".to_owned())],
        };
        let shown = format!("{stdio:?} {stdio:#?} {http:?} {http:#?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        for kept in ["files", "npx", "-y", "API_TOKEN", "Authorization", "https://example.com/mcp?…"] {
            assert!(shown.contains(kept), "{kept} is missing from {shown}");
        }
    }

    #[test]
    fn the_hub_is_shareable_and_lazy() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Hub>();
        let config = |name: &str, transport| ServerConfig { name: name.to_owned(), transport };
        let hub = Hub::new(vec![
            config(
                "remote",
                Transport::Http {
                    url: "http://example.com/mcp".to_owned(),
                    headers: vec![("X-Key".to_owned(), "s3cret".to_owned())],
                },
            ),
            config(
                "local",
                Transport::Stdio {
                    command: "C:/no/such/atroxabot-missing.exe".to_owned(),
                    args: Vec::new(),
                    env: vec![("KEY".to_owned(), "s3cret".to_owned())],
                },
            ),
        ]);
        let never = AtomicBool::new(false);
        let (tools, failures) = hub.tools();
        assert!(tools.is_empty());
        assert_eq!(failures.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["remote", "local"]);
        assert!(failures[0].1.contains("https") && failures[1].1.contains("could not be started"), "{failures:?}");
        assert!(!format!("{failures:?}").contains("s3cret"));
        assert_eq!(
            hub.call("nowhere", "tool", &serde_json::Value::Null, &never),
            Err("there is no MCP server named \"nowhere\"".to_owned())
        );
        assert!(hub.call("remote", "tool", &serde_json::Value::Null, &never).unwrap_err().contains("https"));
        hub.close();
        hub.close();
        assert_eq!(hub.call("local", "tool", &serde_json::Value::Null, &never), Err("stopped".to_owned()));
    }
}
