//! The Streamable HTTP transport: every message is a POST to one address, and the answer is a JSON body or
//! a stream of server-sent events that ends with it.

use std::{
    io::{self, BufReader, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use ureq::http::{Response, Uri};

use crate::{
    conn::{Stop, wait},
    error::Error,
    proto::{self, Incoming, Line, MAX_LINE, Sse},
};

/// How many redirects (to the same origin) are followed.
const MAX_HOPS: usize = 3;
/// How long a message nobody waits for (a cancellation, the end of a session) may take.
const PARTING: Duration = Duration::from_secs(2);

/// What a POST carries besides its body.
pub(crate) struct Envelope {
    /// The protocol's own headers for this message.
    pub(crate) headers: Vec<(String, String)>,
    /// The server is of the `initialize` era: it may ask things on a stream, which are answered.
    pub(crate) legacy: bool,
    /// A session id is among the headers, so a 404 means that the session is over.
    pub(crate) session: bool,
}

/// The answer to a POST.
pub(crate) struct Reply {
    /// The JSON-RPC result; null for a notification.
    pub(crate) result: Value,
    /// The `Mcp-Session-Id` response header, if the server sent a usable one.
    pub(crate) session: Option<String>,
}

/// A remote server.
pub(crate) struct Link {
    agent: ureq::Agent,
    /// The address; replaced when the server redirects within its origin.
    url: String,
    /// The configured headers. Never logged or shown.
    headers: Vec<(String, String)>,
}

impl Link {
    /// Prepares requests to `url`, which has to pass [`check_url`]. Nothing is sent yet.
    pub(crate) fn new(url: &str, headers: &[(String, String)]) -> Result<Self, Error> {
        check_url(url)?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Redirects are followed here, by hand: only within the origin, so no header leaves it.
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(15)))
            .user_agent(concat!("atroxabot/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Self { agent, url: url.to_owned(), headers: headers.to_vec() })
    }

    /// POSTs `message` and waits for the answer to request `id` (for a notification, `None`: that the
    /// server took it). The exchange runs on a thread of its own so the wait can end at `deadline` or when
    /// `stop` asks; a thread left behind ends with its request, at `deadline` at the latest.
    pub(crate) fn request(
        &mut self,
        message: &Value,
        id: Option<u64>,
        envelope: Envelope,
        deadline: Instant,
        stop: &Stop<'_>,
    ) -> Result<Reply, Error> {
        let mut exchange = self.exchange(envelope, deadline.saturating_duration_since(Instant::now()));
        let body = message.to_string().into_bytes();
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&abandoned);
        let (done, outcome) = mpsc::sync_channel(1);
        let work = move || {
            let reply = exchange.run(&body, id, &flag);
            let _ = done.send(reply.map(|reply| (reply, exchange.url)));
        };
        thread::Builder::new().name("mcp-http".to_owned()).spawn(work).map_err(Error::Spawn)?;
        match wait(&outcome, deadline, stop) {
            Ok(Some(outcome)) => {
                let (reply, url) = outcome?;
                self.url = url;
                Ok(reply)
            }
            Ok(None) => Err(Error::Network("the request was lost".to_owned())),
            Err(error) => {
                abandoned.store(true, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    /// POSTs a notification without waiting for the outcome.
    pub(crate) fn notify_detached(&self, message: &Value, envelope: Envelope) {
        let mut exchange = self.exchange(envelope, PARTING);
        let body = message.to_string().into_bytes();
        let work = move || drop(exchange.run(&body, None, &AtomicBool::new(false)));
        let _ = thread::Builder::new().name("mcp-http".to_owned()).spawn(work);
    }

    /// Tells an `initialize`-era server that its session is over (an HTTP DELETE), briefly and best effort.
    pub(crate) fn end_session(&self, envelope: &Envelope) {
        let mut request = self.agent.delete(&self.url).config().timeout_global(Some(PARTING)).build();
        for (name, value) in self.headers.iter().chain(&envelope.headers) {
            request = request.header(name.as_str(), value.as_str());
        }
        let _ = request.call();
    }

    fn exchange(&self, envelope: Envelope, timeout: Duration) -> Exchange {
        let Envelope { mut headers, legacy, session } = envelope;
        headers.extend(self.headers.iter().cloned());
        Exchange { agent: self.agent.clone(), url: self.url.clone(), headers, legacy, session, timeout }
    }
}

/// Everything one POST needs, owned, so that it can run on its own thread.
struct Exchange {
    agent: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    legacy: bool,
    session: bool,
    timeout: Duration,
}

impl Exchange {
    /// Sends `body` and reads the answer to request `id`, or just the status when `id` is `None`.
    fn run(&mut self, body: &[u8], id: Option<u64>, abandoned: &AtomicBool) -> Result<Reply, Error> {
        let response = self.post(body)?;
        let status = response.status();
        let header =
            |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).unwrap_or_default();
        let session = Some(header("mcp-session-id").to_owned())
            .filter(|id| !id.is_empty() && id.len() <= 1024 && id.bytes().all(|byte| byte.is_ascii_graphic()));
        let events = header("content-type").to_ascii_lowercase().starts_with("text/event-stream");
        if status.as_u16() == 404 && self.session {
            return Err(Error::SessionExpired);
        }
        let Some(id) = id else {
            return if status.is_success() {
                Ok(Reply { result: Value::Null, session })
            } else {
                Err(Error::Status(status.as_u16()))
            };
        };
        if status.is_success() && events {
            return self.read_events(response, id, abandoned).map(|result| Reply { result, session });
        }
        let mut bytes = Vec::new();
        let limit = MAX_LINE as u64 + 1;
        response.into_body().into_reader().take(limit).read_to_end(&mut bytes).map_err(|error| read_error(&error))?;
        if bytes.len() > MAX_LINE {
            return Err(Error::Protocol("sent an answer that is too large".to_owned()));
        }
        if !status.is_success() {
            return Err(proto::error_body(&bytes).unwrap_or(Error::Status(status.as_u16())));
        }
        match serde_json::from_slice(&bytes).map(proto::classify) {
            Ok(Incoming::Response { id: answered, outcome }) if answered == id => {
                outcome.map(|result| Reply { result, session })
            }
            _ => Err(Error::Protocol("did not answer the request".to_owned())),
        }
    }

    /// One POST. A 307 or 308 to the same origin is followed (a missing trailing slash is the usual
    /// reason); any other redirect is the response, and so an error.
    fn post(&mut self, body: &[u8]) -> Result<Response<ureq::Body>, Error> {
        for _ in 0..=MAX_HOPS {
            let mut request = self.agent.post(&self.url).config().timeout_global(Some(self.timeout)).build();
            request = request.content_type("application/json").header("Accept", "application/json, text/event-stream");
            for (name, value) in &self.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request.send(body).map_err(|error| network_error(&error))?;
            if !matches!(response.status().as_u16(), 307 | 308) {
                return Ok(response);
            }
            let location = response.headers().get("location").and_then(|value| value.to_str().ok()).unwrap_or_default();
            self.url = same_origin(&self.url, location)
                .ok_or_else(|| Error::Network("it redirects to another address, which is not followed".to_owned()))?;
        }
        Err(Error::Network("it redirects too many times".to_owned()))
    }

    /// Reads a stream of events until the answer to request `id`. On the way an `initialize`-era server
    /// may send notifications (ignored) and requests (answered with a POST of their own).
    // ponytail: a stream that ends before the answer is an error; resuming one (`Last-Event-ID`, earlier
    // revisions only) is not done.
    fn read_events(&mut self, response: Response<ureq::Body>, id: u64, abandoned: &AtomicBool) -> Result<Value, Error> {
        let mut reader = BufReader::new(response.into_body().into_reader());
        let mut line = Vec::new();
        let mut sse = Sse::default();
        loop {
            if abandoned.load(Ordering::Relaxed) {
                return Err(Error::Stopped);
            }
            let data = match proto::read_line(&mut reader, &mut line, MAX_LINE).map_err(|error| read_error(&error))? {
                Line::Text => sse.feed(&String::from_utf8_lossy(&line))?,
                Line::TooLong => return Err(Error::Protocol("sent an event that is too large".to_owned())),
                // The last event may lack its closing empty line.
                Line::End => Some(sse.feed("")?.ok_or_else(|| Error::Protocol("ended its answer early".to_owned()))?),
            };
            let Some(Ok(message)) = data.map(|data| serde_json::from_str::<Value>(&data)) else { continue };
            match proto::classify(message) {
                Incoming::Response { id: answered, outcome } if answered == id => return outcome,
                Incoming::Request { reply } if self.legacy => drop(self.post(reply.to_string().as_bytes())),
                _ => {}
            }
        }
    }
}

/// Whether a remote server may be reached at `url`: over https anywhere, over plain http on this machine
/// only, and without credentials in the address (those belong in a header).
pub(crate) fn check_url(url: &str) -> Result<(), Error> {
    let refuse = |why: &str| Err(Error::Config(format!("the server address {why}")));
    let Ok(uri) = url.parse::<Uri>() else { return refuse("is not a valid URL") };
    let host = uri.host().unwrap_or_default();
    if uri.authority().is_some_and(|authority| authority.as_str().contains('@')) {
        return refuse("must not contain a user name or password");
    }
    let local = host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "[::1]" | "::1");
    match uri.scheme_str() {
        _ if host.is_empty() => refuse("is not a valid URL"),
        Some("https") => Ok(()),
        Some("http") if local => Ok(()),
        Some("http") => refuse("must use https (plain http is only allowed for localhost and 127.0.0.1)"),
        _ => refuse("must start with https://"),
    }
}

/// The address a redirect from `current` to `location` leads to, if that is the same scheme, host and
/// port: a full URL naming them, or an absolute path.
fn same_origin(current: &str, location: &str) -> Option<String> {
    let (current, next) = (current.parse::<Uri>().ok()?, location.parse::<Uri>().ok()?);
    match next.authority() {
        Some(authority) => {
            (next.scheme() == current.scheme() && Some(authority) == current.authority()).then(|| location.to_owned())
        }
        // `//host/path` would name another host.
        None if location.starts_with('/') && !location.starts_with("//") => {
            Some(format!("{}://{}{location}", current.scheme_str()?, current.authority()?))
        }
        None => None,
    }
}

/// A failed request as this crate's error. A malformed address is not echoed: it may hold a key.
fn network_error(error: &ureq::Error) -> Error {
    match error {
        ureq::Error::Timeout(_) => Error::Timeout,
        ureq::Error::BadUri(_) => Error::Config("the server address is not a valid URL".to_owned()),
        other => Error::Network(other.to_string()),
    }
}

/// A failure while reading a response body.
fn read_error(error: &io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut { Error::Timeout } else { Error::Network(error.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_this_machine() {
        for good in [
            "https://mcp.example.com/mcp",
            "https://example.com:8443/a/b?key=abc",
            "http://localhost:3000/mcp",
            "http://LOCALHOST/mcp",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/mcp",
        ] {
            assert!(check_url(good).is_ok(), "{good}");
        }
        for bad in [
            "http://example.com/mcp",
            "http://127.0.0.1.example.com/mcp",
            "http://localhost.example.com/",
            "http://localhost@example.com/",
            "https://user:password@example.com/",
            "http://192.168.1.10/mcp",
            "http://0.0.0.0/mcp",
            "ftp://example.com/",
            "file:///etc/passwd",
            "example.com/mcp",
            "/mcp",
            "https://",
            "",
            "not a url",
        ] {
            let Err(error) = check_url(bad) else { panic!("{bad} was accepted") };
            assert!(matches!(error, Error::Config(_)), "{bad}");
        }
        // The message never repeats the address, which may hold a key.
        let Err(error) = check_url("http://example.com/mcp?key=s3cret") else { panic!("accepted") };
        assert!(!error.to_string().contains("s3cret"));
    }

    #[test]
    fn redirects_stay_within_the_origin() {
        let here = "https://example.com/mcp?key=1";
        assert_eq!(same_origin(here, "/mcp/").as_deref(), Some("https://example.com/mcp/"));
        assert_eq!(same_origin(here, "https://EXAMPLE.com/v2/mcp").as_deref(), Some("https://EXAMPLE.com/v2/mcp"));
        assert_eq!(same_origin("http://127.0.0.1:9000/mcp", "/mcp/").as_deref(), Some("http://127.0.0.1:9000/mcp/"));
        for elsewhere in [
            "https://evil.example/mcp",
            "http://example.com/mcp",
            "https://example.com:8443/mcp",
            "https://example.com.evil.example/mcp",
            "//evil.example/mcp",
            "mcp/",
            "",
        ] {
            assert_eq!(same_origin(here, elsewhere), None, "{elsewhere}");
        }
    }
}
