//! One connection to one server: finding out which protocol revision it speaks, then requests and answers.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

use crate::{
    Tool, Transport,
    error::Error,
    http::{self, Envelope},
    proto::{self, HANDSHAKE, Incoming, LEGACY, MAX_PAGES, MAX_TOOLS, MODERN, MODERN_ERRORS, UNSUPPORTED_VERSION},
    stdio,
};

/// How long starting a server and agreeing on a protocol may take; listing its tools gets as long again.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a stdio server gets to answer the `server/discover` probe before `initialize` is tried as well.
const PROBE_PATIENCE: Duration = Duration::from_secs(2);
/// How often a waiting call looks at its cancel flag.
const TICK: Duration = Duration::from_millis(25);

/// The two flags that end a wait early.
pub(crate) struct Stop<'a> {
    /// Set by the caller of [`crate::Hub::call`].
    pub(crate) cancel: &'a AtomicBool,
    /// Set when the hub is closed.
    pub(crate) closed: &'a AtomicBool,
}

impl Stop<'_> {
    /// Whether to give up now.
    pub(crate) fn requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.closed.load(Ordering::Relaxed)
    }
}

/// Waits for the next item on a channel until `deadline`, looking at `stop` every [`TICK`]. `Ok(None)` means
/// the sending side is gone.
pub(crate) fn wait<T>(incoming: &Receiver<T>, deadline: Instant, stop: &Stop<'_>) -> Result<Option<T>, Error> {
    loop {
        if stop.requested() {
            return Err(Error::Stopped);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::Timeout);
        }
        match incoming.recv_timeout(left.min(TICK)) {
            Ok(item) => return Ok(Some(item)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

enum Link {
    Stdio(stdio::Link),
    Http(http::Link),
}

/// Which protocol the server speaks.
enum Era {
    /// Revision `2026-07-28`: stateless, every request carries `_meta`, the server never asks anything.
    Modern,
    /// The `initialize` handshake of an earlier revision is under way.
    Initializing,
    /// An earlier revision, negotiated by `initialize`.
    Legacy(String),
}

/// A server that has been reached and whose protocol revision is known.
pub(crate) struct Conn {
    link: Link,
    era: Era,
    /// The `Mcp-Session-Id` an `initialize`-era HTTP server handed out.
    session: Option<String>,
    next_id: u64,
}

impl Conn {
    /// Starts or reaches the server and settles the protocol revision, within [`CONNECT_TIMEOUT`].
    pub(crate) fn connect(transport: &Transport, stop: &Stop<'_>) -> Result<Self, Error> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut conn = Self::open(transport)?;
        match conn.negotiate(deadline, stop) {
            // Some servers of the handshake era end when their first message is not `initialize`. Such a
            // process is started again and greeted the old way. (One that dies for another reason dies twice.)
            Err(Error::Exited(_)) if matches!(conn.era, Era::Modern) && !stop.requested() => {
                conn = Self::open(transport)?;
                conn.initialize(None, deadline, stop)?;
            }
            negotiated => negotiated?,
        }
        Ok(conn)
    }

    /// Starts the process or prepares the HTTP client. No message is exchanged yet.
    fn open(transport: &Transport) -> Result<Self, Error> {
        let link = match transport {
            Transport::Stdio { command, args, env } => Link::Stdio(stdio::Link::spawn(command, args, env)?),
            Transport::Http { url, headers } => Link::Http(http::Link::new(url, headers)?),
        };
        Ok(Self { link, era: Era::Modern, session: None, next_id: 1 })
    }

    /// Finds the server's era as the specification lays out: a modern `server/discover` first; a
    /// `DiscoverResult` (or one of the errors only a modern server has) means modern, anything else means
    /// the server wants `initialize`.
    fn negotiate(&mut self, deadline: Instant, stop: &Stop<'_>) -> Result<(), Error> {
        let stdio = matches!(self.link, Link::Stdio(_));
        let (probe, message) = self.message("server/discover", Map::new());
        let envelope = self.envelope("server/discover", None);
        // An HTTP server always answers. A stdio server of an earlier revision may stay silent.
        let patience = if stdio { deadline.min(Instant::now() + PROBE_PATIENCE) } else { deadline };
        let unanswered = match self.round_trip(probe, &message, envelope, None, patience, stop) {
            Ok((_, result)) if proto::offers(result.get("supportedVersions"), &[MODERN]) => return Ok(()),
            Err(Error::Rpc { code: UNSUPPORTED_VERSION, ref data, .. })
                if proto::offers(data.get("supported"), &LEGACY) =>
            {
                None
            }
            Err(Error::Rpc { code, message, data }) if MODERN_ERRORS.contains(&code) => {
                return Err(Error::Rpc { code, message, data });
            }
            // "Anything else identifies a legacy server."
            Ok(_) | Err(Error::Rpc { .. } | Error::Status(_) | Error::Protocol(_)) => None,
            // Not cancelled: a server that was only slow to start may still answer it.
            Err(Error::Timeout) if stdio => Some(probe),
            Err(error) => return Err(error),
        };
        self.initialize(unanswered, deadline, stop)
    }

    /// The handshake of revisions up to `2025-11-25`. `probe` is a `server/discover` still unanswered.
    fn initialize(&mut self, probe: Option<u64>, deadline: Instant, stop: &Stop<'_>) -> Result<(), Error> {
        self.era = Era::Initializing;
        let params = Map::from_iter([
            ("protocolVersion".to_owned(), Value::from(HANDSHAKE)),
            ("capabilities".to_owned(), json!({})),
            ("clientInfo".to_owned(), proto::client_info()),
        ]);
        let (id, message) = self.message("initialize", params);
        let envelope = self.envelope("initialize", None);
        let (answered, result) = self.round_trip(id, &message, envelope, probe, deadline, stop)?;
        if answered != id {
            // The probe was answered after all: a modern server that took its time to start.
            self.era = Era::Modern;
            return Ok(());
        }
        let version = result.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !LEGACY.contains(&version) {
            let version: String = version.chars().take(40).collect();
            return Err(Error::Protocol(format!("speaks MCP version {version:?}, which this app does not know")));
        }
        self.era = Era::Legacy(version.to_owned());
        let ready = proto::notification("notifications/initialized", None);
        let envelope = self.envelope("notifications/initialized", None);
        match &mut self.link {
            Link::Stdio(link) => link.send(&ready),
            Link::Http(link) => link.request(&ready, None, envelope, deadline, stop).map(drop),
        }
    }

    /// Whether the connection can still be used.
    pub(crate) fn alive(&mut self) -> bool {
        match &mut self.link {
            Link::Stdio(link) => link.alive(),
            Link::Http(_) => true,
        }
    }

    /// Every tool of the server, pages followed up to [`MAX_PAGES`] and [`MAX_TOOLS`].
    pub(crate) fn list_tools(&mut self, server: &str, stop: &Stop<'_>) -> Result<Vec<Tool>, Error> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let http = matches!(self.link, Link::Http(_));
        let mut tools = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.take().map(|cursor: String| ("cursor".to_owned(), Value::from(cursor)));
            let page = match self.request("tools/list", params.into_iter().collect(), None, Vec::new(), deadline, stop)
            {
                Ok(page) => page,
                // A server without tools is a server with none.
                Err(Error::Rpc { code: -32601, .. }) => break,
                Err(error) => return Err(error),
            };
            cursor = proto::read_tools(server, &page, http, &mut tools);
            if cursor.is_none() || tools.len() >= MAX_TOOLS {
                break;
            }
        }
        Ok(tools)
    }

    /// Calls a tool and returns its result as text. `schema` is the tool's input schema when it is known:
    /// a modern HTTP server may want some arguments repeated as headers.
    pub(crate) fn call_tool(
        &mut self,
        tool: &str,
        arguments: &Value,
        schema: Option<&Value>,
        deadline: Instant,
        stop: &Stop<'_>,
    ) -> Result<String, Error> {
        let arguments = if arguments.is_null() { json!({}) } else { arguments.clone() };
        let mirrored = match (&self.link, &self.era, schema) {
            (Link::Http(_), Era::Modern, Some(schema)) => proto::param_headers(schema, &arguments),
            _ => Vec::new(),
        };
        let params = Map::from_iter([("name".to_owned(), Value::from(tool)), ("arguments".to_owned(), arguments)]);
        proto::flatten(&self.request("tools/call", params, Some(tool), mirrored, deadline, stop)?)
    }

    /// Closes a stdio server's stdin, which asks it to exit.
    pub(crate) fn hang_up(&mut self) {
        if let Link::Stdio(link) = &mut self.link {
            link.hang_up();
        }
    }

    /// Ends the connection: a stdio server gets until `deadline` to exit before it is killed, an
    /// `initialize`-era HTTP server is told that its session is over.
    pub(crate) fn stop(&mut self, deadline: Instant) {
        let envelope = self.envelope("", None);
        match &mut self.link {
            Link::Stdio(link) => link.stop(deadline),
            Link::Http(link) if envelope.session => link.end_session(&envelope),
            Link::Http(_) => {}
        }
    }

    /// One request and its result. A wait that is cancelled or times out is also cancelled at the server.
    fn request(
        &mut self,
        method: &str,
        params: Map<String, Value>,
        name: Option<&str>,
        mirrored: Vec<(String, String)>,
        deadline: Instant,
        stop: &Stop<'_>,
    ) -> Result<Value, Error> {
        let (id, message) = self.message(method, params);
        let mut envelope = self.envelope(method, name);
        envelope.headers.extend(mirrored);
        let outcome = self.round_trip(id, &message, envelope, None, deadline, stop);
        if matches!(outcome, Err(Error::Stopped | Error::Timeout)) {
            self.cancel(id);
        }
        outcome.map(|(_, result)| result)
    }

    /// Tells the server that nobody waits for request `id` any more.
    fn cancel(&mut self, id: u64) {
        let params = json!({ "requestId": id, "reason": "no longer wanted" });
        let cancelled = proto::notification("notifications/cancelled", Some(params));
        let envelope = self.envelope("notifications/cancelled", None);
        match &mut self.link {
            Link::Stdio(link) => {
                let _ = link.send(&cancelled);
            }
            // To a modern HTTP server the dropped response stream is the cancellation.
            Link::Http(link) if envelope.legacy => link.notify_detached(&cancelled, envelope),
            Link::Http(_) => {}
        }
    }

    /// The next request id and the request itself, with `_meta` for a modern server.
    fn message(&mut self, method: &str, mut params: Map<String, Value>) -> (u64, Value) {
        let id = self.next_id;
        self.next_id += 1;
        if matches!(self.era, Era::Modern) {
            params.insert("_meta".to_owned(), proto::meta());
        }
        (id, proto::request(id, method, params))
    }

    /// What an HTTP POST carries besides its body, by era. `name` is the tool of a `tools/call`.
    fn envelope(&self, method: &str, name: Option<&str>) -> Envelope {
        let mut headers = Vec::new();
        let mut add = |header: &str, value: &str| headers.push((header.to_owned(), value.to_owned()));
        match &self.era {
            Era::Modern => {
                add("MCP-Protocol-Version", MODERN);
                add("Mcp-Method", method);
                if let Some(name) = name {
                    add("Mcp-Name", &proto::header_value(name));
                }
            }
            // The version is not agreed yet; earlier revisions define the header for later requests only.
            Era::Initializing => {}
            Era::Legacy(version) => {
                add("MCP-Protocol-Version", version);
                if let Some(session) = &self.session {
                    add("Mcp-Session-Id", session);
                }
            }
        }
        Envelope { headers, legacy: !matches!(self.era, Era::Modern), session: self.session.is_some() }
    }

    /// Sends a request and waits for its answer, or (on stdio) for a `DiscoverResult` answering `probe`, an
    /// earlier request. Returns the id that was answered and the result.
    fn round_trip(
        &mut self,
        id: u64,
        message: &Value,
        envelope: Envelope,
        probe: Option<u64>,
        deadline: Instant,
        stop: &Stop<'_>,
    ) -> Result<(u64, Value), Error> {
        match &mut self.link {
            Link::Stdio(link) => {
                link.send(message)?;
                loop {
                    let Some(incoming) = link.receive(deadline, stop)? else {
                        return Err(Error::Protocol("sent an answer that is too large".to_owned()));
                    };
                    match proto::classify(incoming) {
                        Incoming::Response { id: answered, outcome } if answered == id => {
                            return outcome.map(|result| (id, result));
                        }
                        Incoming::Response { id: answered, outcome: Ok(result) }
                            if Some(answered) == probe && proto::offers(result.get("supportedVersions"), &[MODERN]) =>
                        {
                            return Ok((answered, result));
                        }
                        // Earlier revisions let the server ask; a modern client must never write a response.
                        Incoming::Request { reply } if envelope.legacy => link.send(&reply)?,
                        // Notifications, and answers to requests given up on.
                        _ => {}
                    }
                }
            }
            Link::Http(link) => {
                let reply = link.request(message, Some(id), envelope, deadline, stop)?;
                if matches!(self.era, Era::Initializing) {
                    self.session = reply.session;
                }
                Ok((id, reply.result))
            }
        }
    }
}
