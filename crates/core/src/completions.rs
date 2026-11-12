//! `POST /chat/completions` for OpenAI-compatible providers (OpenRouter,
//! DeepSeek, Moonshot, Inworld, Ollama, local servers): the same
//! [`ResponseRequest`] in and the same [`StreamEvent`]s out as the other
//! wires, translated both ways.
//!
//! A reply is kept as the assistant message it adds up to, with whatever
//! reasoning the provider sent along (`reasoning_content` from DeepSeek,
//! Moonshot and vLLM, OpenRouter's `reasoning_details`), and goes back
//! unchanged to the same model: DeepSeek refuses a conversation with tools
//! without it, Gemini's thought signatures ride in it, and a thinking model
//! loses its train of thought between tool calls otherwise. OpenRouter also
//! gets its own fields: `reasoning` for the effort (the only way it gives
//! Claude one), `cache_control` so Claude is cached at all, and `session_id`
//! so a conversation keeps going to the provider that holds its cache.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};

use crate::client::{Client, parse_error_body};
use crate::error::{Error, Result};
use crate::responses::{Completion, InputItem, ResponseRequest, StreamEvent, ToolCall, ToolChoice, Usage, read_events};

/// Most tool calls one reply may make; a hostile stream can't grow past it.
const MAX_CALLS: usize = 64;
/// Most reasoning details one reply may keep; a hostile stream can't grow past it.
const MAX_DETAILS: usize = 256;
/// Longest `session_id` OpenRouter takes.
const MAX_SESSION_ID: usize = 256;

/// Streams a reply from `client`'s provider; see [`Client::stream_response`].
pub(crate) fn stream(
    client: &Client,
    request: &ResponseRequest<'_>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<bool> {
    let dialect = Dialect { openrouter: client.openrouter };
    let response = client.post("/chat/completions", &body(request, dialect))?;
    let mut chunks = Chunks::default();
    let done = read_events(response, cancel, on_event, |event, on_event| chunks.data(&event.data, on_event))?;
    // Some servers close the stream after the last chunk without `[DONE]`.
    if done || (chunks.finish.is_some() && !cancel.load(Ordering::Relaxed)) {
        on_event(StreamEvent::Completed(chunks.completion()));
        return Ok(true);
    }
    Ok(false)
}

/// What a provider takes beyond plain Chat Completions.
#[derive(Debug, Clone, Copy, Default)]
struct Dialect {
    /// OpenRouter: `reasoning`, `cache_control` and `session_id`.
    openrouter: bool,
}

/// The Chat Completions body for `request`: instructions become the system
/// message, a reply's calls join its assistant message, and a reply kept as
/// it arrived goes back as it was.
fn body(request: &ResponseRequest<'_>, dialect: Dialect) -> Value {
    let mut messages: Vec<Value> =
        request.instructions.map(|text| json!({ "role": "system", "content": text })).into_iter().collect();
    // A tool message holds text only: the pictures results show follow the
    // last of them, in a message of their own.
    let mut shown: Vec<Value> = Vec::new();
    for item in request.input {
        if !matches!(item, InputItem::ToolOutput { .. }) {
            show(&mut messages, &mut shown);
        }
        match item {
            InputItem::Message { role, text } => messages.push(json!({ "role": role, "content": text })),
            InputItem::Image { media_type, data } => {
                let url = format!("data:{media_type};base64,{data}");
                let part = json!({ "type": "image_url", "image_url": { "url": url } });
                messages.push(json!({ "role": "user", "content": [part] }));
            }
            InputItem::ToolCall(call) => {
                let call = json!({ "id": call.call_id, "type": "function", "function": { "name": call.name, "arguments": arguments(&call.arguments) } });
                match messages.last_mut() {
                    Some(last) if last["role"] == "assistant" => {
                        match last.get_mut("tool_calls").and_then(Value::as_array_mut) {
                            Some(calls) => calls.push(call),
                            None => last["tool_calls"] = json!([call]),
                        }
                    }
                    _ => messages.push(json!({ "role": "assistant", "content": null, "tool_calls": [call] })),
                }
            }
            InputItem::ToolOutput { call_id, output, picture, .. } => {
                messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": output }));
                if let Some(picture) = picture {
                    let label = format!("The picture the result of call {call_id} shows:");
                    let url = format!("data:{};base64,{}", picture.media_type, picture.data);
                    shown.push(json!({ "type": "text", "text": label }));
                    shown.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                }
            }
            // The assistant message as it arrived, reasoning and all; only
            // OpenRouter knows its own `reasoning_details` (a session moved to
            // another provider with a model of the same name sends none).
            InputItem::Replay(kept) => {
                if let Some(message) = kept.first().and_then(Value::as_object) {
                    let mut message = message.clone();
                    message.insert("role".to_owned(), "assistant".into());
                    if !dialect.openrouter {
                        message.remove("reasoning_details");
                    }
                    messages.push(Value::Object(message));
                }
            }
        }
    }
    show(&mut messages, &mut shown);
    let mut body = json!({ "model": request.model, "messages": messages, "stream": true, "stream_options": { "include_usage": true } });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } }))
            .collect();
        body["tools"] = tools.into();
    }
    if let Some(choice) = request.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Function(name) => json!({ "type": "function", "function": { "name": name } }),
        };
    }
    if let Some(effort) = request.reasoning {
        if dialect.openrouter {
            body["reasoning"] = json!({ "effort": effort });
        } else {
            body["reasoning_effort"] = effort.into();
        }
    }
    if dialect.openrouter {
        // Claude is only cached when asked; the breakpoint moves along with the conversation.
        if request.model.starts_with("anthropic/") {
            body["cache_control"] = json!({ "type": "ephemeral" });
        }
        if let Some(key) = request.cache_key.filter(|key| !key.is_empty()) {
            body["session_id"] = key.chars().take(MAX_SESSION_ID).collect::<String>().into();
        }
    }
    body
}

/// Adds the pictures tool results showed, as one user message, once their
/// tool messages are done.
fn show(messages: &mut Vec<Value>, shown: &mut Vec<Value>) {
    if !shown.is_empty() {
        messages.push(json!({ "role": "user", "content": std::mem::take(shown) }));
    }
}

/// A call's arguments as sent back: some servers refuse an empty string.
fn arguments(text: &str) -> &str {
    if text.trim().is_empty() { "{}" } else { text }
}

/// What the chunks of one streamed reply added up to.
#[derive(Default)]
struct Chunks {
    /// Calls by their `index` in the chunks.
    calls: Vec<(u64, ToolCall)>,
    usage: Usage,
    /// The choice's `finish_reason`, once sent.
    finish: Option<String>,
    /// The visible answer so far.
    text: String,
    /// `reasoning_content` so far (DeepSeek, Moonshot, vLLM), which goes back with the reply.
    reasoning_content: String,
    /// OpenRouter's `reasoning_details`, its streamed pieces joined.
    details: Vec<Value>,
}

impl Chunks {
    /// Takes one event's data, passing on what it adds. `Ok(true)` at `[DONE]`.
    fn data(&mut self, data: &str, on_event: &mut dyn FnMut(StreamEvent)) -> Result<bool> {
        if data == "[DONE]" {
            return Ok(true);
        }
        let value: Value = serde_json::from_str(data)?;
        // OpenRouter reports failures mid-stream as a chunk holding `error`.
        if value.get("error").is_some() {
            let (code, message) = parse_error_body(data);
            return Err(Error::Response {
                code,
                message: message.unwrap_or_else(|| "The provider reported an error.".into()),
            });
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            let count = |path: &str| usage.pointer(path).and_then(Value::as_u64);
            self.usage = Usage {
                input_tokens: count("/prompt_tokens").unwrap_or(0),
                output_tokens: count("/completion_tokens").unwrap_or(0),
                // DeepSeek and Moonshot say it their own way.
                cached_tokens: count("/prompt_tokens_details/cached_tokens")
                    .or_else(|| count("/prompt_cache_hit_tokens"))
                    .or_else(|| count("/cached_tokens"))
                    .unwrap_or(0),
                cache_write_tokens: count("/prompt_tokens_details/cache_write_tokens").unwrap_or(0),
            };
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(false);
        };
        let delta = &choice["delta"];
        let text = |key: &str| delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let details: &[Value] = delta.get("reasoning_details").and_then(Value::as_array).map_or(&[], Vec::as_slice);
        for detail in details {
            self.detail(detail);
        }
        // `reasoning` is OpenRouter's name, `reasoning_content` DeepSeek's and
        // vLLM's; a provider that only sends details shows their text.
        let content = text("reasoning_content");
        if let Some(content) = &content {
            self.reasoning_content.push_str(content);
        }
        let shown = text("reasoning").or(content).or_else(|| {
            let said: String = details
                .iter()
                .filter_map(|detail| detail.get("text").or_else(|| detail.get("summary")).and_then(Value::as_str))
                .collect();
            (!said.is_empty()).then_some(said)
        });
        if let Some(reasoning) = shown {
            on_event(StreamEvent::Reasoning(reasoning));
        }
        if let Some(content) = text("content") {
            self.text.push_str(&content);
            on_event(StreamEvent::Text(content));
        }
        for (position, call) in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(position as u64);
            let field = |path: &str| call.pointer(path).and_then(Value::as_str).unwrap_or_default();
            let slot = match self.calls.iter().position(|(i, _)| *i == index) {
                Some(slot) => slot,
                None if self.calls.len() < MAX_CALLS => {
                    self.calls.push((index, ToolCall::default()));
                    on_event(StreamEvent::ToolCallStarted { index, name: field("/function/name").to_owned() });
                    self.calls.len() - 1
                }
                None => continue,
            };
            let made = &mut self.calls[slot].1;
            // The id and name come once, usually in the first chunk; some servers repeat them.
            if made.call_id.is_empty() {
                field("/id").clone_into(&mut made.call_id);
            }
            if made.name.is_empty() {
                field("/function/name").clone_into(&mut made.name);
            }
            let arguments = field("/function/arguments");
            if !arguments.is_empty() {
                made.arguments.push_str(arguments);
                on_event(StreamEvent::ToolCallDelta { index, delta: arguments.to_owned() });
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_owned());
        }
        Ok(false)
    }

    /// Joins a streamed piece of OpenRouter's reasoning to the detail it
    /// continues (the same `index` and type), so the reply goes back with
    /// whole details, as a non-streamed one has them: text, summaries and
    /// encrypted data add up, a signature or id is taken as it comes.
    fn detail(&mut self, piece: &Value) {
        let Some(piece) = piece.as_object() else { return };
        let index = piece.get("index").and_then(Value::as_u64);
        let kind = piece.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        let same = |detail: &Value| {
            detail.get("index").and_then(Value::as_u64) == index
                && detail.get("type").and_then(Value::as_str).unwrap_or_default() == kind
        };
        // Pieces without an index continue the latest detail, when it is of their kind.
        let found = if index.is_some() {
            self.details.iter().rposition(same)
        } else {
            self.details.len().checked_sub(1).filter(|last| same(&self.details[*last]))
        };
        if let Some(Value::Object(detail)) = found.and_then(|at| self.details.get_mut(at)) {
            join(detail, piece);
        } else if self.details.len() < MAX_DETAILS {
            self.details.push(Value::Object(piece.clone()));
        }
    }

    /// The finished reply, in the Responses API's terms, with the assistant
    /// message it adds up to for replaying.
    fn completion(&mut self) -> Completion {
        let tool_calls: Vec<ToolCall> = std::mem::take(&mut self.calls)
            .into_iter()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, mut call)| {
                // A result must name its call; servers that send no id get one.
                if call.call_id.is_empty() {
                    call.call_id = format!("call_{index}");
                }
                call
            })
            .collect();
        let incomplete = match self.finish.as_deref() {
            Some("length") => Some("max_output_tokens".to_owned()),
            Some("content_filter") => Some("content_filter".to_owned()),
            _ => None,
        };
        let text = std::mem::take(&mut self.text);
        let content = if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { text.into() };
        let mut message = json!({ "role": "assistant", "content": content });
        if !tool_calls.is_empty() {
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|call| json!({ "id": call.call_id, "type": "function", "function": { "name": call.name, "arguments": arguments(&call.arguments) } }))
                .collect();
            message["tool_calls"] = calls.into();
        }
        if !self.reasoning_content.is_empty() {
            message["reasoning_content"] = std::mem::take(&mut self.reasoning_content).into();
        }
        if !self.details.is_empty() {
            message["reasoning_details"] = std::mem::take(&mut self.details).into();
        }
        Completion {
            usage: self.usage,
            reasoning: String::new(),
            tool_calls,
            incomplete,
            content: vec![message],
            ..Completion::default()
        }
    }
}

/// Adds `piece` to `detail`: its text, summary and encrypted data after
/// what came before, anything else (a signature, an id) as it comes.
fn join(detail: &mut Map<String, Value>, piece: &Map<String, Value>) {
    for (key, value) in piece {
        match (key.as_str(), value) {
            ("text" | "summary" | "data", Value::String(more)) => {
                let mut joined = detail.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                joined.push_str(more);
                detail.insert(key.clone(), joined.into());
            }
            (_, Value::Null) => {}
            (_, Value::String(empty)) if empty.is_empty() => {}
            _ => {
                detail.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::{Picture, Role, ToolSpec};

    fn request<'a>(input: &'a [InputItem], tools: &'a [ToolSpec<'a>], model: &'a str) -> ResponseRequest<'a> {
        ResponseRequest {
            model,
            instructions: Some("rules"),
            reasoning: Some("high"),
            input,
            tools,
            tool_choice: None,
            web: false,
            cache_key: Some("session-1"),
        }
    }

    #[test]
    fn requests_translate() {
        let call = |id: &str| {
            InputItem::ToolCall(ToolCall { call_id: id.into(), name: "speak".into(), arguments: "{}".into() })
        };
        let input = [
            InputItem::text(Role::User, "hi"),
            InputItem::text(Role::Assistant, "Hello."),
            call("a"),
            call("b"),
            InputItem::ToolOutput { call_id: "a".into(), output: "ok".into(), is_error: false, picture: None },
            InputItem::ToolOutput { call_id: "b".into(), output: "ok".into(), is_error: true, picture: None },
            call("c"),
        ];
        let schema = json!({ "type": "object" });
        let tools = [ToolSpec { name: "speak", description: "d", parameters: &schema }];
        let mut request = request(&input, &tools, "m");
        request.tool_choice = Some(ToolChoice::Function("speak"));
        let body = body(&request, Dialect::default());
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "assistant"]);
        assert_eq!(messages[2]["content"], "Hello.", "calls join the reply's text");
        assert_eq!(messages[2]["tool_calls"][1]["id"], "b");
        assert_eq!(messages[3]["tool_call_id"], "a");
        assert!(messages[5]["content"].is_null());
        assert_eq!(body["tools"][0]["function"]["name"], "speak");
        assert_eq!(body["tool_choice"], json!({ "type": "function", "function": { "name": "speak" } }));
        assert_eq!(body["reasoning_effort"], "high");
        assert!(
            body.get("reasoning").is_none() && body.get("cache_control").is_none() && body.get("session_id").is_none(),
            "OpenRouter's own fields go to OpenRouter only"
        );
    }

    #[test]
    fn openrouter_gets_its_own_fields() {
        let input = [InputItem::text(Role::User, "hi")];
        let openrouter = Dialect { openrouter: true };
        let claude = body(&request(&input, &[], "anthropic/claude-opus-5.5"), openrouter);
        assert_eq!(claude["reasoning"], json!({ "effort": "high" }));
        assert!(claude.get("reasoning_effort").is_none());
        assert_eq!(claude["cache_control"], json!({ "type": "ephemeral" }), "Claude is cached only when asked");
        assert_eq!(claude["session_id"], "session-1");
        let gemini = body(&request(&input, &[], "google/gemini-3-pro"), openrouter);
        assert!(gemini.get("cache_control").is_none(), "the others cache by themselves");
        let long = "x".repeat(400);
        let mut keyed = request(&input, &[], "m");
        keyed.cache_key = Some(&long);
        assert_eq!(body(&keyed, openrouter)["session_id"].as_str().unwrap().len(), MAX_SESSION_ID);
    }

    #[test]
    fn a_kept_reply_and_pictures_go_back_as_they_should() {
        let kept = json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "Let me look.",
            "reasoning_details": [{ "type": "reasoning.text", "text": "Hm.", "signature": "sig", "index": 0 }],
            "tool_calls": [{ "id": "a", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"x.png\"}" } }],
        });
        let picture = Picture { media_type: "image/png".into(), data: "AAAA".into() };
        let input = [
            InputItem::text(Role::User, "look"),
            InputItem::Replay(vec![kept.clone()]),
            InputItem::ToolOutput {
                call_id: "a".into(),
                output: "A picture.".into(),
                is_error: false,
                picture: Some(picture),
            },
            InputItem::text(Role::User, "next"),
        ];
        let openrouter = body(&request(&input, &[], "deepseek-reasoner"), Dialect { openrouter: true });
        assert_eq!(openrouter["messages"][2], kept, "unchanged, reasoning and all");
        let body = body(&request(&input, &[], "deepseek-reasoner"), Dialect::default());
        let messages = body["messages"].as_array().unwrap();
        let mut elsewhere = kept.clone();
        elsewhere.as_object_mut().unwrap().remove("reasoning_details");
        assert_eq!(messages[2], elsewhere, "OpenRouter's own details go to OpenRouter only");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[4]["role"], "user", "the picture follows the tool messages");
        assert_eq!(messages[4]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(messages[5]["content"], "next");
    }

    #[test]
    fn chunks_become_events() {
        let mut chunks = Chunks::default();
        let mut events = Vec::new();
        let mut feed = |data: &str| chunks.data(data, &mut |e| events.push(e)).unwrap();
        feed(r#"{"choices":[{"delta":{"role":"assistant","reasoning":"Hm."}}]}"#);
        feed(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#);
        feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"speak","arguments":"{\"a\""}}]}}]}"#,
        );
        feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}"#);
        feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"remember","arguments":""}}]}}]}"#);
        feed(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#);
        feed(
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":5}}}"#,
        );
        assert!(feed("[DONE]"));
        assert_eq!(events[..2], [StreamEvent::Reasoning("Hm.".into()), StreamEvent::Text("Hi".into())]);
        assert_eq!(events[2], StreamEvent::ToolCallStarted { index: 0, name: "speak".into() });
        assert_eq!(events[4], StreamEvent::ToolCallDelta { index: 0, delta: ":1}".into() });

        let done = chunks.completion();
        assert_eq!(done.usage, Usage { input_tokens: 9, output_tokens: 4, cached_tokens: 5, cache_write_tokens: 0 });
        assert_eq!(done.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(
            done.tool_calls[0],
            ToolCall { call_id: "c1".into(), name: "speak".into(), arguments: r#"{"a":1}"#.into() }
        );
        assert_eq!(done.tool_calls[1].call_id, "call_1", "a call without an id gets one");
        let message = &done.content[0];
        assert_eq!(message["content"], "Hi");
        assert_eq!(message["tool_calls"][1]["id"], "call_1", "the kept message names calls as their results do");
        assert_eq!(message["tool_calls"][1]["function"]["arguments"], "{}");
        assert!(message.get("reasoning_content").is_none(), "OpenRouter's plain reasoning is only shown");
    }

    #[test]
    fn reasoning_is_kept_to_go_back() {
        // DeepSeek: `reasoning_content`, and its own cache counts.
        let mut chunks = Chunks::default();
        let mut feed = |data: &str| chunks.data(data, &mut |_| {}).unwrap();
        feed(r#"{"choices":[{"delta":{"reasoning_content":"First "}}]}"#);
        feed(r#"{"choices":[{"delta":{"reasoning_content":"this."}}]}"#);
        feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        feed(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}}"#,
        );
        let done = chunks.completion();
        assert_eq!(done.content[0]["reasoning_content"], "First this.");
        assert!(done.content[0]["content"].is_null());
        assert_eq!(done.usage.cached_tokens, 80);

        // OpenRouter: pieces of one detail join; another index is another detail.
        let mut chunks = Chunks::default();
        let mut events = Vec::new();
        let mut feed = |data: &str| chunks.data(data, &mut |e| events.push(e)).unwrap();
        feed(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"Hm, ","index":0,"format":"anthropic-claude-v1"}]}}]}"#,
        );
        feed(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"yes.","index":0,"signature":null}]}}]}"#,
        );
        feed(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"","index":0,"signature":"sig"}]}}]}"#,
        );
        feed(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"abc","index":1}]}}]}"#,
        );
        feed(r#"{"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]}"#);
        let done = chunks.completion();
        assert_eq!(
            done.content[0]["reasoning_details"],
            json!([
                { "type": "reasoning.text", "text": "Hm, yes.", "index": 0, "format": "anthropic-claude-v1", "signature": "sig" },
                { "type": "reasoning.encrypted", "data": "abc", "index": 1 },
            ])
        );
        assert_eq!(events[..2], [StreamEvent::Reasoning("Hm, ".into()), StreamEvent::Reasoning("yes.".into())]);
    }

    #[test]
    fn hostile_chunks_are_bounded() {
        let mut chunks = Chunks::default();
        let error =
            chunks.data(r#"{"error":{"code":"rate_limit_exceeded","message":"Slow down."}}"#, &mut |_| {}).unwrap_err();
        assert!(error.is_retryable() && error.to_string() == "Slow down.");
        assert!(chunks.data("{", &mut |_| {}).is_err());
        let many: Vec<String> = (0..100).map(|i| format!(r#"{{"index":{i},"function":{{"name":"x"}}}}"#)).collect();
        let data = format!(r#"{{"choices":[{{"delta":{{"tool_calls":[{}]}}}}]}}"#, many.join(","));
        chunks.data(&data, &mut |_| {}).unwrap();
        assert_eq!(chunks.calls.len(), MAX_CALLS);
        let huge = r#"{"choices":[{"delta":{"tool_calls":[{"index":18446744073709551615,"function":{"name":"x"}}]}}]}"#;
        assert!(chunks.data(huge, &mut |_| {}).is_ok(), "an absurd index allocates nothing");
        let details: Vec<String> =
            (0..400).map(|i| format!(r#"{{"type":"reasoning.text","text":"x","index":{i}}}"#)).collect();
        let data = format!(r#"{{"choices":[{{"delta":{{"reasoning_details":[{}]}}}}]}}"#, details.join(","));
        chunks.data(&data, &mut |_| {}).unwrap();
        assert_eq!(chunks.details.len(), MAX_DETAILS);
        let odd = r#"{"choices":[{"delta":{"reasoning_details":[1,"x",null,{"type":7}]}}]}"#;
        assert!(chunks.data(odd, &mut |_| {}).is_ok(), "details of the wrong shape are passed over");
        let overflow =
            r#"{"error":{"code":400,"message":"Too long","metadata":{"error_type":"context_length_exceeded"}}}"#;
        assert!(chunks.data(overflow, &mut |_| {}).unwrap_err().is_context_overflow());
    }
}
