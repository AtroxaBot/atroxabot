//! The wire format: JSON-RPC messages, the protocol versions, and reading what a server sends.

use std::io::{self, BufRead};

use serde_json::{Map, Value, json};

use crate::{Tool, error::Error};

/// The stateless protocol revision: no handshake, every request carries its version in `_meta`.
pub(crate) const MODERN: &str = "2026-07-28";
/// The revision asked for in an `initialize` handshake.
pub(crate) const HANDSHAKE: &str = "2025-11-25";
/// The revisions with an `initialize` handshake this client accepts from a server.
pub(crate) const LEGACY: [&str; 4] = [HANDSHAKE, "2025-06-18", "2025-03-26", "2024-11-05"];
/// JSON-RPC errors only a modern server sends: header mismatch, missing client capability, unsupported version.
pub(crate) const MODERN_ERRORS: std::ops::RangeInclusive<i64> = -32022..=-32020;
/// `UnsupportedProtocolVersionError`; its `data.supported` lists what the server speaks.
pub(crate) const UNSUPPORTED_VERSION: i64 = -32022;

/// The longest line (stdio), body or event (HTTP) accepted from a server.
pub(crate) const MAX_LINE: usize = 8 << 20;
/// How many tools one server may list.
pub(crate) const MAX_TOOLS: usize = 512;
/// How many `tools/list` pages are followed.
pub(crate) const MAX_PAGES: usize = 32;
/// How many characters of a tool result reach the model.
pub(crate) const MAX_TEXT: usize = 200_000;
const MAX_NAME: usize = 256;
const MAX_DESCRIPTION: usize = 8_000;
const MAX_MESSAGE: usize = 2_000;

/// A JSON-RPC request.
pub(crate) fn request(id: u64, method: &str, params: Map<String, Value>) -> Value {
    let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if let Some(fields) = message.as_object_mut() {
        fields.insert("params".to_owned(), Value::Object(params));
    }
    message
}

/// A JSON-RPC notification.
pub(crate) fn notification(method: &str, params: Option<Value>) -> Value {
    let mut message = json!({ "jsonrpc": "2.0", "method": method });
    if let (Some(params), Some(fields)) = (params, message.as_object_mut()) {
        fields.insert("params".to_owned(), params);
    }
    message
}

/// This client's name and version, as servers are told.
pub(crate) fn client_info() -> Value {
    json!({ "name": "atroxabot", "version": env!("CARGO_PKG_VERSION") })
}

/// The `_meta` every request to a modern server carries. No client capabilities are declared.
pub(crate) fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientInfo": client_info(),
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A message as one line of the stdio transport. `serde_json` escapes every newline inside strings, so the
/// only raw one is the terminator.
pub(crate) fn frame(message: &Value) -> Vec<u8> {
    let mut line = message.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// Whether `versions` (a JSON array of strings, when the server is well behaved) names one of `wanted`.
pub(crate) fn offers(versions: Option<&Value>, wanted: &[&str]) -> bool {
    let mut listed = versions.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    listed.any(|version| wanted.contains(&version))
}

/// What a message from a server is.
pub(crate) enum Incoming {
    /// The answer to the request with this id: its `result`, or its `error`.
    Response {
        /// The id of the request answered.
        id: u64,
        /// The result, or why there is none.
        outcome: Result<Value, Error>,
    },
    /// A request from the server (only `initialize`-era servers send them). `reply` is this client's answer:
    /// an empty result to `ping`, "method not found" to everything else.
    Request {
        /// The JSON-RPC response to send back.
        reply: Value,
    },
    /// A notification, or something unknown. Ignored.
    Other,
}

/// Sorts a message from a server. Unknown fields and shapes are tolerated.
pub(crate) fn classify(mut message: Value) -> Incoming {
    let id = message.get("id").filter(|id| !id.is_null()).cloned();
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let Some(id) = id else { return Incoming::Other };
        let reply = if method == "ping" {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
        };
        return Incoming::Request { reply };
    }
    // This client only issues numeric ids.
    let Some(id) = id.as_ref().and_then(Value::as_u64) else { return Incoming::Other };
    if let Some(error) = message.get_mut("error").filter(|error| error.is_object()) {
        return Incoming::Response { id, outcome: Err(rpc_error(error)) };
    }
    match message.get_mut("result").map(Value::take) {
        Some(result) => Incoming::Response { id, outcome: completed(result) },
        None => Incoming::Other,
    }
}

/// The JSON-RPC error in an HTTP error body, if that is what the body is.
pub(crate) fn error_body(body: &[u8]) -> Option<Error> {
    let mut message: Value = serde_json::from_slice(body).ok()?;
    message.get_mut("error").filter(|error| error.is_object()).map(rpc_error)
}

fn rpc_error(error: &mut Value) -> Error {
    let mut message = error.get("message").and_then(Value::as_str).unwrap_or("no message").to_owned();
    clip(&mut message, MAX_MESSAGE);
    Error::Rpc {
        code: error.get("code").and_then(Value::as_i64).unwrap_or_default(),
        message,
        data: error.get_mut("data").map(Value::take).unwrap_or_default(),
    }
}

/// A result is final when its `resultType` is `complete`, or absent as in earlier revisions.
fn completed(result: Value) -> Result<Value, Error> {
    match result.get("resultType").and_then(Value::as_str) {
        None | Some("complete") => Ok(result),
        // No client capability is declared, so a conforming server never sends this.
        Some("input_required") => Err(Error::Protocol("asked for input this app does not give".to_owned())),
        Some(_) => Err(Error::Protocol("sent a result of an unknown kind".to_owned())),
    }
}

/// Appends the usable tools of one `tools/list` page to `tools`, up to [`MAX_TOOLS`], and returns the
/// cursor of the next page. Over HTTP a tool with a broken `x-mcp-header` annotation is left out, as the
/// specification requires.
pub(crate) fn read_tools(server: &str, page: &Value, http: bool, tools: &mut Vec<Tool>) -> Option<String> {
    for entry in page.get("tools").and_then(Value::as_array).into_iter().flatten() {
        if tools.len() >= MAX_TOOLS {
            break;
        }
        let name = entry.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() || name.len() > MAX_NAME {
            continue;
        }
        let schema = entry.get("inputSchema").filter(|schema| schema.is_object());
        let input_schema = schema.cloned().unwrap_or_else(|| json!({ "type": "object" }));
        if http && header_params(&input_schema).is_none() {
            continue;
        }
        let mut description = entry.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
        clip(&mut description, MAX_DESCRIPTION);
        tools.push(Tool { server: server.to_owned(), name: name.to_owned(), description, input_schema });
    }
    // Any string is a cursor, the empty one too.
    page.get("nextCursor").and_then(Value::as_str).map(str::to_owned)
}

/// A `tools/call` result as text for the model: text blocks (and the text of embedded resources) joined by
/// newlines, other blocks named (`[image]`), `structuredContent` appended as JSON when no block had text,
/// all cut at [`MAX_TEXT`] characters with a note. A result flagged `isError` is the tool's own failure.
pub(crate) fn flatten(result: &Value) -> Result<String, Error> {
    let mut text = String::new();
    let mut worded = false;
    for block in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let field = |pointer: &str| block.pointer(pointer).and_then(Value::as_str);
        let part = match (field("/type").unwrap_or_default(), field("/text"), field("/resource/text")) {
            ("text", Some(words), _) | ("resource", _, Some(words)) => {
                worded = true;
                words.to_owned()
            }
            ("resource_link", ..) => format!("[resource_link: {:.500}]", field("/uri").unwrap_or_default()),
            ("", ..) => continue,
            (kind, ..) => format!("[{kind:.40}]"),
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part);
    }
    if let Some(structured) = result.get("structuredContent").filter(|value| !worded && !value.is_null()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&structured.to_string());
    }
    if clip(&mut text, MAX_TEXT) {
        text.push_str("\n[cut: the result was longer than ");
        text.push_str(&MAX_TEXT.to_string());
        text.push_str(" characters]");
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(Error::Tool(if text.is_empty() { "the tool failed without saying why".to_owned() } else { text }));
    }
    Ok(text)
}

/// Cuts `text` to its first `max` characters. True when something was cut.
fn clip(text: &mut String, max: usize) -> bool {
    match text.char_indices().nth(max) {
        Some((at, _)) => {
            text.truncate(at);
            true
        }
        None => false,
    }
}

/// The parameters a tool wants mirrored into `Mcp-Param-{name}` headers: each `x-mcp-header` name with the
/// path of its property. `None` when an annotation breaks the specification's rules (a bad header name, a
/// duplicate, a type other than string, integer or boolean, or a place not reached through `properties`
/// alone), which makes the tool unusable over HTTP.
pub(crate) fn header_params(schema: &Value) -> Option<Vec<(String, Vec<String>)>> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    collect_params(schema, true, &mut Vec::new(), &mut found).then_some(found)
}

/// Walks a schema; `reachable` while only `properties` keys led here. False when an annotation is invalid.
/// Parsed JSON is at most 128 levels deep, which bounds the recursion.
fn collect_params(
    schema: &Value,
    reachable: bool,
    path: &mut Vec<String>,
    found: &mut Vec<(String, Vec<String>)>,
) -> bool {
    let Some(fields) = schema.as_object() else {
        return schema.as_array().is_none_or(|items| items.iter().all(|item| collect_params(item, false, path, found)));
    };
    for (key, value) in fields {
        let valid = match (key.as_str(), value) {
            ("x-mcp-header", _) => {
                let name = value.as_str().unwrap_or_default();
                let token = name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
                let primitive =
                    matches!(fields.get("type").and_then(Value::as_str), Some("string" | "integer" | "boolean"));
                let fresh = !found.iter().any(|(taken, _)| taken.eq_ignore_ascii_case(name));
                found.push((name.to_owned(), path.clone()));
                reachable && !path.is_empty() && !name.is_empty() && token && primitive && fresh
            }
            ("properties", Value::Object(properties)) => properties.iter().all(|(name, property)| {
                path.push(name.clone());
                let valid = collect_params(property, reachable, path, found);
                path.pop();
                valid
            }),
            _ => collect_params(value, false, path, found),
        };
        if !valid {
            return false;
        }
    }
    true
}

/// The `Mcp-Param-*` headers of a `tools/call`: the annotated arguments that are present and not null.
pub(crate) fn param_headers(schema: &Value, arguments: &Value) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    for (name, path) in header_params(schema).unwrap_or_default() {
        let text = match path.iter().try_fold(arguments, |at, key| at.get(key)) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Bool(flag)) => flag.to_string(),
            // Integers a double holds exactly, as the specification limits them.
            Some(Value::Number(number)) if number.as_i64().is_some_and(|n| n.unsigned_abs() < 1 << 53) => {
                number.to_string()
            }
            _ => continue,
        };
        headers.push((format!("Mcp-Param-{name}"), header_value(&text)));
    }
    headers
}

/// A value as the specification wants it in a header: as it is when it is plain visible ASCII, otherwise
/// (other characters, outer whitespace, or looking like the marker itself) as `=?base64?…?=` of its UTF-8.
pub(crate) fn header_value(text: &str) -> String {
    let plain = text.bytes().all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && text.trim_matches([' ', '\t']).len() == text.len()
        && !(text.starts_with("=?base64?") && text.ends_with("?="));
    if plain { text.to_owned() } else { format!("=?base64?{}?=", base64(text.as_bytes())) }
}

/// Standard base64 with padding.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().fold(0_u32, |group, byte| group << 8 | u32::from(*byte)) << (8 * (3 - chunk.len()));
        for digit in 0..4 {
            let six = (group >> (18 - 6 * digit)) & 63;
            let letter = ALPHABET.get(six as usize).filter(|_| digit <= chunk.len());
            out.push(letter.map_or('=', |letter| char::from(*letter)));
        }
    }
    out
}

/// What [`read_line`] found.
pub(crate) enum Line {
    /// A line is in the buffer.
    Text,
    /// A line longer than the limit was skipped to its end.
    TooLong,
    /// The stream ended.
    End,
}

/// Reads one `\n`-terminated line into `line` (emptied first; the terminator and a `\r` before it are
/// dropped) without ever holding more than `max` bytes: a longer line is read past and reported.
pub(crate) fn read_line(reader: &mut impl BufRead, line: &mut Vec<u8>, max: usize) -> io::Result<Line> {
    line.clear();
    let mut too_long = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if chunk.is_empty() {
            return Ok(match (too_long, line.is_empty()) {
                (true, _) => Line::TooLong,
                (false, true) => Line::End,
                (false, false) => Line::Text,
            });
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let part = end.and_then(|end| chunk.get(..end)).unwrap_or(chunk);
        if too_long || line.len() + part.len() > max {
            too_long = true;
            line.clear();
        } else {
            line.extend_from_slice(part);
        }
        let used = end.map_or(chunk.len(), |end| end + 1);
        reader.consume(used);
        if end.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(if too_long { Line::TooLong } else { Line::Text });
        }
    }
}

/// Collects server-sent events line by line. Only `data` matters to MCP; event names, ids and comments are
/// passed over.
// ponytail: lines ending in a bare `\r` (allowed by the SSE standard, used by nobody) are not split.
#[derive(Default)]
pub(crate) struct Sse {
    data: String,
}

impl Sse {
    /// Takes one line of the stream. Returns an event's data when the line (an empty one) completes an
    /// event that has any, and an error once an event outgrows [`MAX_LINE`].
    pub(crate) fn feed(&mut self, line: &str) -> Result<Option<String>, Error> {
        if line.is_empty() {
            return Ok(Some(std::mem::take(&mut self.data)).filter(|data| !data.is_empty()));
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if self.data.len() + value.len() > MAX_LINE {
                return Err(Error::Protocol("sent an event that is too large".to_owned()));
            }
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn response(text: &str) -> (u64, Result<Value, Error>) {
        match classify(serde_json::from_str(text).unwrap()) {
            Incoming::Response { id, outcome } => (id, outcome),
            _ => panic!("not a response: {text}"),
        }
    }

    #[test]
    fn frames_are_single_lines() {
        let message = request(7, "tools/call", Map::from_iter([("text".to_owned(), json!("a\nb\r\nc"))]));
        let line = frame(&message);
        assert_eq!(line.iter().position(|byte| *byte == b'\n'), Some(line.len() - 1), "one newline, at the end");
        let back: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(back, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"text": "a\nb\r\nc"}}));
        assert_eq!(
            notification("notifications/initialized", None),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        );
        assert_eq!(meta()["io.modelcontextprotocol/protocolVersion"], MODERN);
        assert_eq!(meta()["io.modelcontextprotocol/clientCapabilities"], json!({}));
    }

    #[test]
    fn responses_are_sorted_from_requests_and_noise() {
        let (id, outcome) =
            response(r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","x":1},"extra":true}"#);
        assert_eq!((id, outcome.unwrap()["x"].clone()), (3, json!(1)));
        // A result without `resultType` comes from an earlier revision and is complete.
        assert!(response(r#"{"id":3,"result":{}}"#).1.is_ok());
        assert!(matches!(response(r#"{"id":3,"result":{"resultType":"input_required"}}"#).1, Err(Error::Protocol(_))));
        assert!(matches!(response(r#"{"id":3,"result":{"resultType":"later"}}"#).1, Err(Error::Protocol(_))));
        let (_, outcome) =
            response(r#"{"id":4,"error":{"code":-32022,"message":"no","data":{"supported":["2025-06-18"]}}}"#);
        let Err(Error::Rpc { code, message, data }) = outcome else { panic!("not an error") };
        assert_eq!((code, message.as_str()), (UNSUPPORTED_VERSION, "no"));
        assert!(offers(data.get("supported"), &LEGACY) && !offers(data.get("supported"), &[MODERN]));
        assert!(!offers(Some(&json!("2026-07-28")), &[MODERN]) && !offers(None, &[MODERN]));

        let ping = classify(json!({"jsonrpc": "2.0", "id": "s-1", "method": "ping"}));
        let Incoming::Request { reply } = ping else { panic!("not a request") };
        assert_eq!(reply, json!({"jsonrpc": "2.0", "id": "s-1", "result": {}}));
        let Incoming::Request { reply } = classify(json!({"id": 9, "method": "sampling/createMessage", "params": {}}))
        else {
            panic!("not a request")
        };
        assert_eq!((reply["id"].clone(), reply["error"]["code"].clone()), (json!(9), json!(-32601)));

        for noise in [
            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
            json!({"id": null, "error": {"code": -32700, "message": "parse error"}}),
            json!({"id": "text", "result": {}}),
            json!({"id": 5}),
            json!([1, 2, 3]),
            json!("hello"),
            Value::Null,
        ] {
            assert!(matches!(classify(noise), Incoming::Other));
        }
        assert!(matches!(
            error_body(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"x"}}"#),
            Some(Error::Rpc { code: -32000, .. })
        ));
        assert!(error_body(b"<html>Bad Request</html>").is_none() && error_body(b"").is_none());
        let long = format!(r#"{{"id":1,"error":{{"code":1,"message":"{}"}}}}"#, "é".repeat(50_000));
        let Err(Error::Rpc { message, .. }) = response(&long).1 else { panic!("not an error") };
        assert_eq!(message.chars().count(), MAX_MESSAGE);
    }

    #[test]
    fn tool_pages_survive_hostile_entries() {
        let page = json!({
            "tools": [
                {"name": "read", "title": "Read", "description": "Reads.", "inputSchema": {"type": "object", "properties": {}}, "annotations": {}},
                {"name": "bare"},
                {"description": "nameless"},
                {"name": 7},
                {"name": ""},
                {"name": "n".repeat(MAX_NAME + 1)},
                "junk",
                {"name": "wordy", "description": "d".repeat(MAX_DESCRIPTION * 2), "inputSchema": "junk"}
            ],
            "nextCursor": "",
            "ttlMs": 1000
        });
        let mut tools = Vec::new();
        assert_eq!(read_tools("files", &page, false, &mut tools), Some(String::new()));
        assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["read", "bare", "wordy"]);
        assert_eq!(
            tools[0],
            Tool {
                server: "files".into(),
                name: "read".into(),
                description: "Reads.".into(),
                input_schema: json!({"type": "object", "properties": {}})
            }
        );
        assert_eq!((tools[1].description.as_str(), &tools[1].input_schema), ("", &json!({"type": "object"})));
        assert_eq!((tools[2].description.len(), &tools[2].input_schema), (MAX_DESCRIPTION, &json!({"type": "object"})));

        let many: Vec<Value> = (0..MAX_TOOLS + 50).map(|n| json!({"name": format!("t{n}")})).collect();
        let mut tools = Vec::new();
        assert_eq!(read_tools("s", &json!({"tools": many, "nextCursor": null}), false, &mut tools), None);
        assert_eq!(tools.len(), MAX_TOOLS);
        assert_eq!(read_tools("s", &json!({"tools": "junk", "nextCursor": 5}), false, &mut Vec::new()), None);
        assert_eq!(read_tools("s", &json!([]), false, &mut Vec::new()), None);
    }

    #[test]
    fn results_flatten_to_text() {
        let text = |result: Value| flatten(&result).unwrap();
        assert_eq!(
            text(json!({"content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]})),
            "one\ntwo"
        );
        let mixed = json!({"content": [
            {"type": "text", "text": "see"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "audio", "data": "AAAA"},
            {"type": "resource_link", "uri": "file:///a.rs", "name": "a.rs"},
            {"type": "resource", "resource": {"uri": "file:///b.rs", "text": "fn main() {}"}},
            {"type": "resource", "resource": {"uri": "file:///c.bin", "blob": "AAAA"}},
            {"type": "hologram"},
            {"text": "typeless"},
            42
        ], "structuredContent": {"ignored": "because there is text"}});
        assert_eq!(
            text(mixed),
            "see\n[image]\n[audio]\n[resource_link: file:///a.rs]\nfn main() {}\n[resource]\n[hologram]"
        );
        assert_eq!(text(json!({"content": [], "structuredContent": {"a": [1, 2]}})), r#"{"a":[1,2]}"#);
        assert_eq!(text(json!({"content": [{"type": "image"}], "structuredContent": 5})), "[image]\n5");
        assert_eq!(text(json!({"structuredContent": null})), "");
        assert_eq!(text(json!({"content": "junk", "isError": "junk"})), "");
        assert_eq!(text(json!({"content": [{"type": "x".repeat(500)}]})).len(), 42);

        let failed = flatten(&json!({"content": [{"type": "text", "text": "no such file"}], "isError": true}));
        assert!(matches!(failed, Err(Error::Tool(why)) if why == "no such file"));
        assert!(
            matches!(flatten(&json!({"isError": true})), Err(Error::Tool(why)) if why.contains("without saying why"))
        );
    }

    #[test]
    fn long_results_are_cut_with_a_note() {
        let long = text_of(&"é".repeat(MAX_TEXT + 10));
        let (kept, note) = long.split_once('\n').unwrap();
        assert_eq!(kept.chars().count(), MAX_TEXT);
        assert_eq!(note, "[cut: the result was longer than 200000 characters]");
        assert_eq!(text_of(&"é".repeat(MAX_TEXT)).chars().count(), MAX_TEXT);
    }

    fn text_of(text: &str) -> String {
        flatten(&json!({"content": [{"type": "text", "text": text}]})).unwrap()
    }

    #[test]
    fn header_values_follow_the_specification_examples() {
        assert_eq!(header_value("us-west1"), "us-west1");
        assert_eq!(header_value("two words"), "two words");
        assert_eq!(header_value(""), "");
        assert_eq!(header_value("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(header_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(header_value("=?base64?literal?="), "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=");
        assert_eq!([b"".as_slice(), b"f", b"fo", b"foo"].map(base64), ["", "Zg==", "Zm8=", "Zm9v"]);
    }

    #[test]
    fn header_parameters_are_found_and_checked() {
        let schema = json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "query": {"type": "string"},
            "options": {"type": "object", "properties": {
                "limit": {"type": "integer", "x-mcp-header": "Limit"},
                "dry": {"type": "boolean", "x-mcp-header": "Dry-Run"},
                "tenant": {"type": "string", "x-mcp-header": "Tenant"}
            }}
        }});
        let arguments =
            json!({"region": "eu west", "query": "select 1", "options": {"limit": 42, "dry": false, "tenant": null}});
        let mut headers = param_headers(&schema, &arguments);
        headers.sort();
        let expected = [("Mcp-Param-Dry-Run", "false"), ("Mcp-Param-Limit", "42"), ("Mcp-Param-Region", "eu west")];
        assert_eq!(headers, expected.map(|(name, value)| (name.to_owned(), value.to_owned())));
        assert!(param_headers(&schema, &json!({})).is_empty() && param_headers(&schema, &Value::Null).is_empty());
        assert!(param_headers(&schema, &json!({"region": 1.5, "options": {"limit": 1_u64 << 60}})).is_empty());
        assert_eq!(header_params(&json!({"type": "object"})), Some(Vec::new()));
        // A property that happens to be called `x-mcp-header` is not an annotation.
        assert_eq!(header_params(&json!({"properties": {"x-mcp-header": {"type": "string"}}})), Some(Vec::new()));

        let invalid = [
            json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": ["string", "null"], "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "Bad Name"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "A\r\nX: y"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": 5}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "Same"}, "b": {"type": "string", "x-mcp-header": "same"}}}),
            json!({"properties": {"list": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}),
            json!({"anyOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
            json!({"type": "string", "x-mcp-header": "Root"}),
        ];
        for schema in invalid {
            assert_eq!(header_params(&schema), None, "{schema}");
            let mut tools = Vec::new();
            read_tools("s", &json!({"tools": [{"name": "t", "inputSchema": schema}]}), true, &mut tools);
            assert!(tools.is_empty(), "left out over HTTP");
        }
    }

    #[test]
    fn lines_are_bounded() {
        let mut input = io::Cursor::new(b"one\r\n\ntwo-is-too-long\nthree\nlast".to_vec());
        let mut reader = io::BufReader::with_capacity(4, &mut input);
        let mut line = Vec::new();
        let mut seen = Vec::new();
        loop {
            match read_line(&mut reader, &mut line, 8).unwrap() {
                Line::Text => seen.push(String::from_utf8(line.clone()).unwrap()),
                Line::TooLong => seen.push("<too long>".to_owned()),
                Line::End => break,
            }
        }
        assert_eq!(seen, ["one", "", "<too long>", "three", "last"]);
        let mut endless = io::repeat(b'x').take(3 * 1024 * 1024);
        let mut reader = io::BufReader::new(&mut endless);
        assert!(matches!(read_line(&mut reader, &mut line, 1024).unwrap(), Line::TooLong));
        assert!(line.is_empty(), "nothing of an oversized line is kept");
        assert!(matches!(read_line(&mut reader, &mut line, 1024).unwrap(), Line::End));
    }

    fn events(stream: &str) -> Vec<String> {
        let mut sse = Sse::default();
        let mut events: Vec<String> = stream.lines().filter_map(|line| sse.feed(line).unwrap()).collect();
        events.extend(sse.feed("").unwrap());
        events
    }

    #[test]
    fn server_sent_events_are_parsed() {
        let stream = ": keep-alive\n\nid: 1\ndata:\n\nevent: message\nid: 2\ndata: {\"a\":\ndata: 1}\nretry: 50\n\ndata:{\"b\":2}\n\nunknown\ndata: tail";
        assert_eq!(events(stream), ["{\"a\":\n1}", "{\"b\":2}", "tail"]);
        assert!(events("").is_empty() && events("\n\n\n: nothing\n\n").is_empty());

        let mut sse = Sse::default();
        let chunk = "x".repeat(MAX_LINE / 2);
        assert!(sse.feed(&format!("data: {chunk}")).unwrap().is_none());
        assert!(sse.feed(&format!("data: {chunk}")).unwrap().is_none());
        assert!(matches!(sse.feed("data: one more"), Err(Error::Protocol(_))));
    }
}
