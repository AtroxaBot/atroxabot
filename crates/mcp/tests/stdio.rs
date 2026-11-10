//! End to end over stdio, against a fake MCP server: this very program, started again with `FAKE_MCP` set.
//! Hence no test harness: `main` is either that server or the list of scenarios.

use std::{
    io::{self, BufRead, Write},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use atroxabot_mcp::{Hub, ServerConfig, Tool, Transport};
use serde_json::{Value, json};

const TOOLS: [&str; 10] = ["args", "ask", "big", "cancelled", "die", "echo", "fail", "hang", "pid", "shape"];

fn main() {
    if let Ok(mode) = std::env::var("FAKE_MCP") {
        return serve(&mode);
    }
    let scenarios: [(&str, fn()); 9] = [
        ("an initialize-era server", legacy),
        ("a call that times out", timeout),
        ("a stateless (2026-07-28) server", modern),
        ("a stateless server that is slow to start", slow_modern),
        ("an initialize-era server that ignores the probe", silent),
        ("an initialize-era server that dies of the probe", strict),
        ("two servers side by side", side_by_side),
        ("a server that exits at once", exits),
        ("a script shim run through cmd.exe", shim),
    ];
    for (name, scenario) in scenarios {
        let started = Instant::now();
        scenario();
        println!("test {name} ... ok ({:.1?})", started.elapsed());
    }
}

// ---- The scenarios ----

fn fake(name: &str, mode: &str) -> ServerConfig {
    let program = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let env = vec![("FAKE_MCP".to_owned(), mode.to_owned()), ("FAKE_KEY".to_owned(), "s3cret-key".to_owned())];
    ServerConfig { name: name.to_owned(), transport: Transport::Stdio { command: program, args: Vec::new(), env } }
}

fn call(hub: &Hub, tool: &str, arguments: &Value) -> Result<String, String> {
    hub.call("fake", tool, arguments, &AtomicBool::new(false))
}

fn names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn legacy() {
    let hub = Hub::new(vec![fake("fake", "legacy")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS, "both pages, sorted, the nameless entry dropped");
    let echo = Tool {
        server: "fake".to_owned(),
        name: "echo".to_owned(),
        description: "The echo tool.".to_owned(),
        input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    };
    assert_eq!(tools[5], echo);
    let again = Instant::now();
    assert_eq!(hub.tools(), (tools, Vec::new()));
    assert!(again.elapsed() < Duration::from_millis(200), "the list is kept");

    assert_eq!(call(&hub, "echo", &json!({"text": "hello\nthere"})), Ok("hello\nthere".to_owned()));
    assert_eq!(call(&hub, "echo", &Value::Null), Ok("nothing".to_owned()));
    assert_eq!(call(&hub, "fail", &json!({})), Err("it broke".to_owned()));
    assert_eq!(call(&hub, "shape", &json!({})), Ok("[image]\n[resource_link: file:///x]\n{\"a\":1}".to_owned()));
    let unknown = call(&hub, "nope", &json!({})).unwrap_err();
    assert!(unknown.contains("Unknown tool: nope") && unknown.contains("-32602"), "{unknown}");
    assert_eq!(
        hub.call("elsewhere", "echo", &json!({}), &AtomicBool::new(false)).unwrap_err(),
        "there is no MCP server named \"elsewhere\""
    );

    // The server pings and asks for a completion in the middle of the call.
    assert_eq!(call(&hub, "ask", &json!({})), Ok("ping answered: true, sampling refused: true".to_owned()));

    // An answer over the size limit fails that call only.
    let big = call(&hub, "big", &json!({})).unwrap_err();
    assert!(big.contains("too large"), "{big}");
    assert_eq!(call(&hub, "echo", &json!({"text": "still here"})), Ok("still here".to_owned()));

    // Cancelled while waiting, and the server is told.
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::Relaxed);
        });
        hub.call("fake", "hang", &json!({}), &cancel)
    });
    assert_eq!(stopped, Err("stopped".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(call(&hub, "cancelled", &json!({})), Ok("1".to_owned()));

    // The server dies in the middle of a call; the next call starts it again.
    let before = call(&hub, "pid", &json!({})).unwrap();
    let died = call(&hub, "die", &json!({})).unwrap_err();
    assert!(died.contains("the server process ended"), "{died}");
    let after = call(&hub, "pid", &json!({})).unwrap();
    assert!(before != after && after.parse::<u32>().is_ok(), "{before} then {after}");
    assert_eq!(call(&hub, "cancelled", &json!({})), Ok("0".to_owned()), "a fresh process");

    let started = Instant::now();
    hub.close();
    hub.close();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!(call(&hub, "echo", &json!({})), Err("stopped".to_owned()));
}

fn timeout() {
    let hub = Hub::new(vec![fake("fake", "legacy")]).with_call_timeout(Duration::from_millis(400));
    let started = Instant::now();
    let late = call(&hub, "hang", &json!({})).unwrap_err();
    assert!(late.contains("did not answer in time"), "{late}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(call(&hub, "cancelled", &json!({})), Ok("1".to_owned()), "the server was told to stop");
    // Closing while a call waits ends the call.
    let hub = Hub::new(vec![fake("fake", "legacy")]);
    let started = Instant::now();
    let stopped = thread::scope(|scope| {
        let waiting = scope.spawn(|| call(&hub, "hang", &json!({})));
        thread::sleep(Duration::from_millis(500));
        hub.close();
        waiting.join().unwrap()
    });
    assert_eq!(stopped, Err("stopped".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

fn modern() {
    let hub = Hub::new(vec![fake("fake", "modern")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS);
    // The fake refuses every request without the `_meta` of the stateless revision.
    assert_eq!(call(&hub, "echo", &json!({"text": "stateless"})), Ok("stateless".to_owned()));
    assert_eq!(call(&hub, "fail", &json!({})), Err("it broke".to_owned()));
    // A modern client never writes a response, so the server's (forbidden) requests go unanswered.
    assert_eq!(call(&hub, "ask", &json!({})), Ok("ping answered: false, sampling refused: false".to_owned()));
}

fn slow_modern() {
    let hub = Hub::new(vec![fake("fake", "slow-modern")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS);
    assert_eq!(call(&hub, "echo", &json!({"text": "late"})), Ok("late".to_owned()));
}

fn silent() {
    let hub = Hub::new(vec![fake("fake", "silent")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS);
    assert_eq!(call(&hub, "echo", &json!({"text": "after the probe"})), Ok("after the probe".to_owned()));
}

fn strict() {
    let hub = Hub::new(vec![fake("fake", "strict")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS);
    assert_eq!(call(&hub, "echo", &json!({"text": "second start"})), Ok("second start".to_owned()));
}

fn side_by_side() {
    let hub = Hub::new(vec![fake("b", "legacy"), fake("a", "modern")]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    let listed: Vec<(&str, &str)> = tools.iter().map(|tool| (tool.server.as_str(), tool.name.as_str())).collect();
    let expected: Vec<(&str, &str)> = ["a", "b"].iter().flat_map(|server| TOOLS.map(|tool| (*server, tool))).collect();
    assert_eq!(listed, expected, "sorted by server, then by tool");

    let cancel = AtomicBool::new(false);
    thread::scope(|scope| {
        let hanging = scope.spawn(|| hub.call("a", "hang", &json!({}), &cancel));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(hub.call("b", "echo", &json!({"text": "meanwhile"}), &cancel), Ok("meanwhile".to_owned()));
        assert!(!hanging.is_finished(), "the other server's call is still waiting");
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(hanging.join().unwrap(), Err("stopped".to_owned()));
    });
}

fn exits() {
    let hub = Hub::new(vec![fake("fake", "exit")]);
    let (tools, failures) = hub.tools();
    assert!(tools.is_empty());
    let [(server, why)] = failures.as_slice() else { panic!("{failures:?}") };
    assert_eq!(server, "fake");
    assert!(why.contains("the server process ended") && why.contains("boom: bad key [hidden]"), "{why}");
    assert!(!why.contains("s3cret-key"), "{why}");
}

/// On Windows `npx` and its like are `.cmd` files. A stand-in for one is put on the server's `PATH`.
fn shim() {
    if !cfg!(windows) {
        return;
    }
    let folder = std::env::temp_dir().join(format!("atroxabot-mcp-shim-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("fakemcp.cmd"), "@echo off\r\n\"%FAKE_EXE%\" %*\r\n").unwrap();
    let mut path = vec![folder.clone()];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let path = std::env::join_paths(path).unwrap().to_string_lossy().into_owned();

    let mut config = fake("fake", "legacy");
    let Transport::Stdio { command, args, env } = &mut config.transport else { unreachable!() };
    env.push(("FAKE_EXE".to_owned(), std::mem::replace(command, "fakemcp".to_owned())));
    env.push(("PATH".to_owned(), path));
    *args = vec!["two words".to_owned(), "plain".to_owned(), r"C:\a folder\".to_owned()];
    let hub = Hub::new(vec![config.clone()]);
    let (tools, failures) = hub.tools();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(names(&tools), TOOLS);
    assert_eq!(
        call(&hub, "args", &json!({})),
        Ok(r"two words|plain|C:\a folder\".to_owned()),
        "arguments arrive whole"
    );
    hub.close();

    let Transport::Stdio { args, .. } = &mut config.transport else { unreachable!() };
    args.push("a & b".to_owned());
    let (_, failures) = Hub::new(vec![config]).tools();
    assert!(failures.iter().any(|(_, why)| why.contains("cmd.exe")), "{failures:?}");
    let _ = std::fs::remove_dir_all(folder);
}

// ---- The fake server ----

fn say(message: &Value) {
    let mut out = io::stdout().lock();
    writeln!(out, "{message}").unwrap();
    out.flush().unwrap();
}

fn tool(name: &str) -> Value {
    json!({
        "name": name,
        "title": name.to_uppercase(),
        "description": format!("The {name} tool."),
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
        "annotations": {"readOnlyHint": true}
    })
}

fn text(text: &str) -> Value {
    json!({"resultType": "complete", "content": [{"type": "text", "text": text}]})
}

/// Speaks the stateless revision in the `modern` modes and the `initialize` handshake otherwise, strictly:
/// a request in the other era's shape is refused.
fn serve(mode: &str) {
    if mode == "exit" {
        eprintln!("starting up\nboom: bad key {}", std::env::var("FAKE_KEY").unwrap_or_default());
        std::process::exit(2);
    }
    if mode == "slow-modern" {
        thread::sleep(Duration::from_millis(2600));
    }
    let modern = mode.ends_with("modern");
    let mut lines = io::stdin().lock().lines();
    let mut initialized = false;
    let mut cancelled = 0;
    while let Some(Ok(line)) = lines.next() {
        let message: Value = serde_json::from_str(&line).expect("the client writes one JSON message per line");
        let id = message["id"].clone();
        let params = &message["params"];
        let meta = &params["_meta"];
        let stateless = meta["io.modelcontextprotocol/protocolVersion"] == "2026-07-28"
            && meta["io.modelcontextprotocol/clientCapabilities"] == json!({})
            && meta["io.modelcontextprotocol/clientInfo"]["name"] == "atroxabot";
        let reply = |result: Value| say(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let refuse =
            |code: i64, why: &str| say(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": why}}));
        let method = message["method"].as_str().unwrap_or_default();
        if mode == "strict" && !initialized && !method.ends_with("initialize") && !method.ends_with("initialized") {
            eprintln!("expected initialize, got {method}");
            std::process::exit(1);
        }
        match method {
            "notifications/initialized" => initialized = true,
            "notifications/cancelled" => cancelled += 1,
            _ if id.is_null() => {}
            "server/discover" if mode == "silent" => {}
            _ if modern && !stateless => refuse(-32602, "the request lacks its _meta"),
            "server/discover" if modern => reply(json!({
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}},
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake", "version": "1"}}
            })),
            "initialize"
                if !modern && params["capabilities"] == json!({}) && params["protocolVersion"] == "2025-11-25" =>
            {
                // An older revision than the one asked for: the client has to go along.
                reply(json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "fake", "version": "1"}
                }));
            }
            _ if !modern && (!initialized || !meta.is_null()) => refuse(-32600, "not initialized"),
            "tools/list" => match params["cursor"].as_str() {
                None => reply(json!({
                    "resultType": "complete",
                    "tools": [tool("echo"), {"description": "nameless"}, tool("fail"), tool("args")],
                    "nextCursor": "page-2",
                    "ttlMs": 1000,
                    "cacheScope": "public"
                })),
                Some("page-2") => {
                    let rest = ["hang", "die", "ask", "pid", "cancelled", "shape", "big"].map(tool);
                    reply(json!({"resultType": "complete", "tools": rest}));
                }
                Some(_) => refuse(-32602, "an unknown cursor"),
            },
            "tools/call" => match params["name"].as_str().unwrap_or_default() {
                "echo" => {
                    say(
                        &json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "echoing"}}),
                    );
                    reply(text(params["arguments"]["text"].as_str().unwrap_or("nothing")));
                }
                "fail" => reply(json!({"content": [{"type": "text", "text": "it broke"}], "isError": true})),
                "args" => reply(text(&std::env::args().skip(1).collect::<Vec<_>>().join("|"))),
                "hang" => {}
                "die" => std::process::exit(3),
                "pid" => {
                    println!("a careless line that is not JSON");
                    reply(text(&std::process::id().to_string()));
                }
                "cancelled" => reply(text(&cancelled.to_string())),
                "big" => reply(text(&"x".repeat(9 << 20))),
                "shape" => reply(json!({
                    "content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}, {"type": "resource_link", "uri": "file:///x"}],
                    "structuredContent": {"a": 1}
                })),
                "ask" => {
                    say(&json!({"jsonrpc": "2.0", "id": "s-1", "method": "ping"}));
                    say(
                        &json!({"jsonrpc": "2.0", "id": "s-2", "method": "sampling/createMessage", "params": {"messages": [], "maxTokens": 1}}),
                    );
                    // An initialize-era client answers both; a modern one must not, so do not wait for it.
                    let answers: Vec<Value> = if modern {
                        Vec::new()
                    } else {
                        (0..2).map(|_| serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()).collect()
                    };
                    let pong = answers.iter().any(|answer| answer["id"] == "s-1" && answer["result"] == json!({}));
                    let refused =
                        answers.iter().any(|answer| answer["id"] == "s-2" && answer["error"]["code"] == -32601);
                    reply(text(&format!("ping answered: {pong}, sampling refused: {refused}")));
                }
                other => refuse(-32602, &format!("Unknown tool: {other}")),
            },
            _ => refuse(-32601, "Method not found"),
        }
    }
}
