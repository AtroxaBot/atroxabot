//! End to end over Streamable HTTP, against small servers on `127.0.0.1`.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use atroxabot_mcp::{Hub, ServerConfig, Transport};
use serde_json::{Value, json};

/// A request as the server saw it. Header names are in lower case.
#[derive(Clone)]
struct Seen {
    verb: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }

    fn method(&self) -> &str {
        self.body["method"].as_str().unwrap_or_default()
    }
}

struct Answer {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Answer {
    fn status(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: String::new() }
    }

    fn json(status: u16, body: &Value) -> Self {
        Self::status(status).header("Content-Type", "application/json").body(body.to_string())
    }

    /// A stream of server-sent events, closed after the last one.
    fn events(events: &[String]) -> Self {
        let stream = events.iter().fold(String::new(), |stream, event| stream + event + "\n\n");
        Self::status(200).header("Content-Type", "text/event-stream; charset=utf-8").body(stream)
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    fn body(mut self, body: String) -> Self {
        self.body = body;
        self
    }
}

/// Starts a server that answers every request with `handler` and closes the connection. Returns its
/// address and the requests it has seen.
fn serve(handler: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, handler) = (Arc::clone(&seen), Arc::new(handler));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (log, handler) = (Arc::clone(&log), Arc::clone(&handler));
            thread::spawn(move || {
                let Some(request) = read_request(&stream) else { return };
                log.lock().unwrap().push(request.clone());
                let answer = handler(&request);
                let headers: Vec<String> =
                    answer.headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
                let head = format!("HTTP/1.1 {} Fake\r\nConnection: close\r\n{}", answer.status, headers.concat());
                // No Content-Length: the body ends when the connection does, as a stream would.
                let _ = (&stream).write_all(format!("{head}\r\n{}", answer.body).as_bytes());
                let _ = stream.shutdown(Shutdown::Both);
            });
        }
    });
    (address, seen)
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut start = line.split_whitespace().map(str::to_owned);
    let (verb, path) = (start.next()?, start.next()?);
    let mut headers = HashMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let Some((name, value)) = line.trim_end().split_once(':') else { break };
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let mut body = vec![0; headers.get("content-length").and_then(|length| length.parse().ok()).unwrap_or(0)];
    reader.read_exact(&mut body).ok()?;
    Some(Seen { verb, path, headers, body: serde_json::from_slice(&body).unwrap_or_default() })
}

fn hub(url: String) -> Hub {
    let headers = vec![("X-Api-Key".to_owned(), "s3cret-key".to_owned())];
    Hub::new(vec![ServerConfig { name: "remote".to_owned(), transport: Transport::Http { url, headers } }])
}

fn call(hub: &Hub, tool: &str, arguments: &Value) -> Result<String, String> {
    hub.call("remote", tool, arguments, &AtomicBool::new(false))
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A server of the `initialize` era: sessions, a JSON answer to `tools/list`, an event stream (with a
/// notification and a `ping` of its own before the answer) to `tools/call`.
#[test]
fn an_initialize_era_server() {
    let sessions = AtomicUsize::new(0);
    let (address, seen) = serve(move |request| {
        let id = request.body["id"].clone();
        let result = |result: Value| Answer::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let said = |words: &str| json!({"content": [{"type": "text", "text": words}]});
        let current = format!("sess-{}", sessions.load(Ordering::SeqCst));
        let session = request.header("mcp-session-id");
        match (request.verb.as_str(), request.method()) {
            ("DELETE", _) => Answer::status(200),
            (_, "server/discover") => {
                Answer::json(400, &error(&Value::Null, -32000, "Bad Request: Server not initialized"))
            }
            (_, "initialize") => {
                let session = format!("sess-{}", sessions.fetch_add(1, Ordering::SeqCst) + 1);
                let server = json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}});
                result(server).header("Mcp-Session-Id", &session)
            }
            _ if session != current => Answer::json(404, &error(&Value::Null, -32001, "Session not found")),
            // Notifications, and the client's answers to this server's own requests.
            (_, "notifications/initialized" | "notifications/cancelled" | "") => Answer::status(202),
            (_, "tools/list") => {
                result(json!({"tools": [{"name": "add", "description": "Adds.", "inputSchema": {"type": "object"}}]}))
            }
            (_, "tools/call") => match request.body["params"]["name"].as_str().unwrap_or_default() {
                "add" => Answer::events(&[
                    ": a comment".to_owned(),
                    "id: 1\ndata:".to_owned(),
                    format!(
                        "event: message\ndata: {}",
                        json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}})
                    ),
                    format!("data: {}", json!({"jsonrpc": "2.0", "id": "s-1", "method": "ping"})),
                    format!("id: 2\ndata: {{\"jsonrpc\":\"2.0\",\ndata: \"id\":{id},\"result\":{}}}", said("3")),
                ]),
                // The first session is forgotten here; the client has to start another and ask again.
                "expire" if session == "sess-1" => Answer::status(404),
                "expire" => result(said(&format!("answered in {session}"))),
                "slow" => {
                    thread::sleep(Duration::from_secs(3));
                    result(said("too late"))
                }
                _ => Answer::json(200, &error(&id, -32602, "Unknown tool")),
            },
            _ => Answer::json(200, &error(&id, -32601, "Method not found")),
        }
    });
    let hub = hub(format!("{address}/mcp"));
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        tools.iter().map(|tool| (tool.server.as_str(), tool.name.as_str())).collect::<Vec<_>>(),
        [("remote", "add")]
    );
    assert_eq!(call(&hub, "add", &json!({"a": 1, "b": 2})), Ok("3".to_owned()));
    let unknown = call(&hub, "nope", &json!({})).unwrap_err();
    assert!(unknown.contains("Unknown tool"), "{unknown}");

    // Cancelled while the server works: the call returns at once and the server is told.
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::Relaxed);
        });
        hub.call("remote", "slow", &json!({}), &cancel)
    });
    assert_eq!(stopped, Err("stopped".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    assert_eq!(call(&hub, "expire", &json!({})), Ok("answered in sess-2".to_owned()));
    hub.close();

    let seen = seen.lock().unwrap().clone();
    let of = |method: &str| seen.iter().filter(|request| request.method() == method).cloned().collect::<Vec<_>>();
    assert!(seen.iter().all(|request| request.path == "/mcp" && request.header("x-api-key") == "s3cret-key"));
    let posts = seen.iter().filter(|request| request.verb == "POST");
    assert!(posts.clone().all(|request| request.header("accept").contains("application/json")
        && request.header("accept").contains("text/event-stream")));
    assert!(posts.clone().all(|request| request.header("content-type") == "application/json"));

    let probe = &of("server/discover")[0];
    assert_eq!((probe.header("mcp-protocol-version"), probe.header("mcp-method")), ("2026-07-28", "server/discover"));
    assert_eq!(probe.body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
    let initialize = of("initialize");
    assert_eq!(initialize.len(), 2, "once, and once more after the session expired");
    assert_eq!(
        initialize[0].body["params"],
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "atroxabot", "version": env!("CARGO_PKG_VERSION")}})
    );
    assert_eq!((initialize[0].header("mcp-protocol-version"), initialize[0].header("mcp-session-id")), ("", ""));
    let ready = &of("notifications/initialized")[0];
    assert_eq!((ready.header("mcp-session-id"), ready.body.get("id")), ("sess-1", None));
    let list = &of("tools/list")[0];
    assert_eq!((list.header("mcp-protocol-version"), list.header("mcp-session-id")), ("2025-11-25", "sess-1"));
    assert_eq!((list.header("mcp-method"), &list.body["params"]), ("", &json!({})), "no stateless trimmings");
    let add = &of("tools/call")[0];
    assert_eq!(add.body["params"], json!({"name": "add", "arguments": {"a": 1, "b": 2}}));
    // The server's ping, sent on the stream, was answered with a POST of its own.
    let pong = seen.iter().find(|request| request.body["id"] == "s-1").expect("the ping was answered");
    assert_eq!((pong.body["result"].clone(), pong.header("mcp-session-id")), (json!({}), "sess-1"));
    let cancelled = &of("notifications/cancelled")[0];
    assert!(cancelled.body["params"]["requestId"].is_u64());
    let ended = seen.last().unwrap();
    assert_eq!((ended.verb.as_str(), ended.header("mcp-session-id")), ("DELETE", "sess-2"));
}

/// A server of the stateless revision, behind a redirect within its origin: no handshake, `_meta` and the
/// mirrored headers on every request, one argument mirrored into a header of its own.
#[test]
fn a_stateless_server() {
    let (address, seen) = serve(|request| {
        let id = request.body["id"].clone();
        let result = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let meta = &request.body["params"]["_meta"];
        let name = request.body["params"]["name"].as_str().unwrap_or_default();
        let stateless = request.header("mcp-protocol-version") == "2026-07-28"
            && request.header("mcp-method") == request.method()
            && request.header("mcp-session-id").is_empty()
            && meta["io.modelcontextprotocol/protocolVersion"] == "2026-07-28"
            && meta["io.modelcontextprotocol/clientCapabilities"] == json!({});
        match request.method() {
            _ if request.path == "/mcp" => Answer::status(307).header("Location", "/mcp/"),
            _ if !stateless => Answer::json(400, &error(&id, -32020, "Header mismatch")),
            "server/discover" => Answer::json(
                200,
                &result(
                    json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}),
                ),
            ),
            "tools/list" => {
                let region = |kind: &str| json!({"type": "object", "properties": {"region": {"type": kind, "x-mcp-header": "Region"}, "sql": {"type": "string"}}});
                let tools = json!([{"name": "query", "inputSchema": region("string")}, {"name": "broken", "inputSchema": region("number")}]);
                Answer::json(
                    200,
                    &result(json!({"resultType": "complete", "tools": tools, "ttlMs": 0, "cacheScope": "private"})),
                )
            }
            "tools/call" if request.header("mcp-name") != name => {
                Answer::json(400, &error(&id, -32020, "Header mismatch"))
            }
            "tools/call" if name == "query" => {
                let rows = json!({"resultType": "complete", "content": [{"type": "text", "text": format!("rows from {}", request.header("mcp-param-region"))}]});
                Answer::events(&[format!("data: {}", result(rows))])
            }
            "tools/call" if name == "slow" => {
                thread::sleep(Duration::from_secs(3));
                Answer::json(200, &result(json!({"resultType": "complete", "content": []})))
            }
            "tools/call" => Answer::json(200, &result(json!({"resultType": "input_required", "inputRequests": {}}))),
            _ => Answer::json(404, &error(&id, -32601, "Method not found")),
        }
    });
    let hub = hub(format!("{address}/mcp"));
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        ["query"],
        "the broken annotation is left out"
    );
    assert_eq!(
        call(&hub, "query", &json!({"region": "München", "sql": "select 1"})),
        Ok("rows from =?base64?TcO8bmNoZW4=?=".to_owned())
    );
    assert_eq!(call(&hub, "query", &json!({"region": "eu-west1"})), Ok("rows from eu-west1".to_owned()));
    let asked = call(&hub, "wants-input", &json!({})).unwrap_err();
    assert!(asked.contains("asked for input"), "{asked}");

    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::Relaxed);
        });
        hub.call("remote", "slow", &json!({}), &cancel)
    });
    assert_eq!(stopped, Err("stopped".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    hub.close();

    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|request| request.verb == "POST" && request.method() != "initialize"),
        "no handshake, no DELETE"
    );
    assert!(
        seen.iter().all(|request| !request.method().starts_with("notifications/")),
        "dropping the stream is the cancellation"
    );
    assert_eq!(seen.iter().filter(|request| request.path == "/mcp").count(), 1, "the redirect is remembered");
}

#[test]
fn servers_that_cannot_be_used() {
    let why = |handler: fn(&Seen) -> Answer| {
        let (address, _) = serve(handler);
        let (tools, mut failures) = hub(format!("{address}/mcp?key=s3cret-query")).tools();
        assert!(tools.is_empty());
        let (server, why) = failures.pop().expect("a failure");
        assert_eq!(server, "remote");
        assert!(!why.contains("s3cret"), "{why}");
        why
    };
    let elsewhere = why(|_| Answer::status(307).header("Location", "http://127.0.0.1:9/mcp"));
    assert!(elsewhere.contains("redirects to another address"), "{elsewhere}");
    let looping = why(|request| Answer::status(308).header("Location", &format!("{}/", request.path)));
    assert!(looping.contains("redirects too many times"), "{looping}");
    assert!(why(|_| Answer::status(302).header("Location", "/login")).contains("HTTP 302"));
    assert!(why(|_| Answer::status(401).body("<html>Unauthorized</html>".to_owned())).contains("HTTP 401"));
    let hostile = why(|_| {
        Answer::status(200)
            .header("Content-Type", "text/event-stream")
            .body(format!("data: {}\n\n", "x".repeat(9 << 20)))
    });
    assert!(hostile.contains("too large"), "{hostile}");
    let early =
        why(|_| Answer::events(&["data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}".to_owned()]));
    assert!(early.contains("ended its answer early"), "{early}");
    // A modern server that speaks no revision this client knows is not asked to `initialize`.
    let newer = why(|request| {
        assert_eq!(request.method(), "server/discover");
        let data = json!({"supported": ["2031-01-01"], "requested": "2026-07-28"});
        Answer::json(
            400,
            &json!({"jsonrpc": "2.0", "id": request.body["id"], "error": {"code": -32022, "message": "Unsupported protocol version", "data": data}}),
        )
    });
    assert!(newer.contains("Unsupported protocol version"), "{newer}");
    let (_, failures) = hub("http://example.com/mcp".to_owned()).tools();
    assert!(failures[0].1.contains("must use https"), "{failures:?}");
}
