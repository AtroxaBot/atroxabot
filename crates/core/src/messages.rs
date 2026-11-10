//! `POST /v1/messages`, Anthropic's own API: the same [`ResponseRequest`] in
//! and the same [`StreamEvent`]s out as Chat Completions, plus what Claude
//! only offers here: prompt caching and its summarized thinking.

use std::sync::atomic::AtomicBool;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{Client, Model};
use crate::error::{Error, Result};
use crate::responses::{
    Completion, InputItem, ResponseRequest, Role, StreamEvent, ToolCall, ToolChoice, Usage, read_events,
};

/// Output cap sent with every request: the smallest any served Claude model allows.
const MAX_TOKENS: u64 = 64_000;
/// Most tool calls one reply may make; a hostile stream can't grow past it.
const MAX_CALLS: usize = 64;
/// The efforts Claude accepts, weakest first.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Most web searches, and most page fetches, one reply may make.
const WEB_USES: u64 = 8;
/// Largest reply kept for replaying, in content blocks; a hostile stream can't grow past it.
const MAX_BLOCKS: usize = 512;
/// The betas requests to Anthropic itself opt into: server-side compaction
/// of long conversations, and fallbacks for declined requests.
pub(crate) const BETAS: &str = "compact-2026-01-12,server-side-fallback-2026-07-01";
/// Models whose declined requests Anthropic can answer with another model.
const FALLBACK_MODELS: [&str; 4] = ["claude-fable-5-", "claude-opus-5", "claude-sonnet-5-5", "claude-mythos-5-"];

/// Streams a reply from Anthropic; see [`Client::stream_response`].
pub(crate) fn stream(
    client: &Client,
    request: &ResponseRequest<'_>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<bool> {
    let response = client.post("/messages", &body(request, client.first_party)).map_err(overflow)?;
    let mut blocks = Blocks::default();
    let done = read_events(response, cancel, on_event, |event, on_event| blocks.event(&event.data, on_event))?;
    if done {
        on_event(StreamEvent::Completed(blocks.completion()));
    }
    Ok(done)
}

/// Claude's models, from `GET /v1/models`. Efforts are offered only for those
/// that think adaptively, the one way [`body`] asks for thinking.
pub(crate) fn models(response: ureq::http::Response<ureq::Body>) -> Result<Vec<Model>> {
    #[derive(Deserialize)]
    struct Body {
        data: Vec<Value>,
    }
    let body: Body = crate::client::read_json(response)?;
    let models = body.data.iter().filter_map(|entry| {
        let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let supported = |path: &str| entry.pointer(path).and_then(Value::as_bool) == Some(true);
        let reasoning_levels = if supported("/capabilities/thinking/types/adaptive/supported") {
            EFFORTS
                .iter()
                .filter(|effort| supported(&format!("/capabilities/effort/{effort}/supported")))
                .map(|effort| (*effort).to_owned())
                .collect()
        } else {
            Vec::new()
        };
        let id = text("id");
        let (input, output, cache_read) = price(&id).unwrap_or_default();
        let model = Model {
            input_cost_per_million: input,
            output_cost_per_million: output,
            cache_read_cost_per_million: (input > 0.0).then_some(cache_read),
            // A cache write costs a quarter more than plain input.
            cache_write_cost_per_million: (input > 0.0).then_some(input * 1.25),
            id,
            name: text("display_name"),
            context_window: entry.get("max_input_tokens").and_then(Value::as_u64).unwrap_or(0),
            reasoning_levels,
            ..Model::default()
        };
        (!model.id.is_empty()).then_some(model)
    });
    Ok(models.collect())
}

/// USD per million input, output and cache-read tokens of the Claude model
/// `id`, which the Models API does not list.
// ponytail: a table of list prices as of November 2026 that goes stale; an
// unknown model simply shows no cost. Read prices from the API once it has them.
fn price(id: &str) -> Option<(f64, f64, f64)> {
    const PRICES: [(&str, f64, f64, f64); 11] = [
        ("claude-fable-5-2", 8.0, 40.0, 0.2),
        ("claude-mythos-5-2", 8.0, 40.0, 0.2),
        ("claude-fable-5", 10.0, 50.0, 1.0),
        ("claude-opus-5-5", 4.0, 20.0, 0.2),
        ("claude-opus-5", 5.0, 25.0, 0.5),
        ("claude-opus-4", 5.0, 25.0, 0.5),
        ("claude-sonnet-5-5", 2.0, 10.0, 0.1),
        ("claude-sonnet-5", 2.0, 10.0, 0.2),
        ("claude-sonnet-4", 3.0, 15.0, 0.3),
        ("claude-haiku-5", 0.1, 0.5, 0.01),
        ("claude-haiku-4", 1.0, 5.0, 0.1),
    ];
    // The most specific name wins: they are listed that way.
    PRICES.iter().find(|(prefix, ..)| id.starts_with(prefix)).map(|(_, input, output, read)| (*input, *output, *read))
}

/// Anthropic says a prompt is too long only in words; the app knows it by its code.
fn overflow(error: Error) -> Error {
    match error {
        Error::Api { status: 400, message, retry_after, .. } if message.starts_with("prompt is too long") => {
            Error::Api { status: 400, code: Some("context_length_exceeded".into()), message, retry_after }
        }
        other => other,
    }
}

/// The Messages body for `request`. The system prompt and the conversation
/// up to its latest block are cached, and nothing before that block is ever
/// rewritten: histories are append-only, which keeps both the cache and
/// Claude's thinking blocks (bound to the conversation before them) valid.
fn body(request: &ResponseRequest<'_>, first_party: bool) -> Value {
    let mut turns: Vec<(Role, Vec<Value>)> = Vec::new();
    for item in request.input {
        let (role, block) = match item {
            // Claude refuses blank text blocks.
            InputItem::Message { text, .. } if text.trim().is_empty() => continue,
            InputItem::Message { role, text } => (*role, json!({ "type": "text", "text": text })),
            InputItem::Image { media_type, data } => {
                let source = json!({ "type": "base64", "media_type": media_type, "data": data });
                (Role::User, json!({ "type": "image", "source": source }))
            }
            InputItem::ToolCall(call) => {
                // A call cut off mid-way never ran; Claude still wants an object.
                let input = serde_json::from_str::<Value>(&call.arguments).ok().filter(Value::is_object);
                let block = json!({ "type": "tool_use", "id": tool_id(&call.call_id), "name": call.name, "input": input.unwrap_or_else(|| json!({})) });
                (Role::Assistant, block)
            }
            InputItem::ToolOutput { call_id, output, is_error } => {
                let mut block = json!({ "type": "tool_result", "tool_use_id": tool_id(call_id), "content": output });
                if *is_error {
                    block["is_error"] = true.into();
                }
                (Role::User, block)
            }
            // A reply exactly as it arrived: its own turn, never merged or edited.
            InputItem::Blocks(blocks) => {
                if !blocks.is_empty() {
                    turns.push((Role::Assistant, blocks.clone()));
                }
                continue;
            }
        };
        let n = turns.len();
        // A reply's calls come each with its result; Claude wants them all in
        // one message, answered together in the next.
        if block["type"] == "tool_use"
            && n >= 2
            && turns[n - 1].1.iter().all(|b| b["type"] == "tool_result")
            && turns[n - 2].1.last().is_some_and(|b| b["type"] == "tool_use")
        {
            turns[n - 2].1.push(block);
        } else if let Some((_, blocks)) = turns.last_mut().filter(|(last, _)| *last == role) {
            blocks.push(block);
        } else {
            turns.push((role, vec![block]));
        }
    }
    // The newest user block ends the cached prefix; the next request extends it.
    let newest = turns.last_mut().filter(|(role, _)| *role == Role::User).and_then(|(_, blocks)| blocks.last_mut());
    if let Some(block) = newest {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
    let messages: Vec<Value> =
        turns.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect();
    let mut body = json!({ "model": request.model, "max_tokens": MAX_TOKENS, "messages": messages, "stream": true });
    if let Some(text) = request.instructions.filter(|text| !text.trim().is_empty()) {
        body["system"] = json!([{ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }]);
    }
    // Eager: a long tool input (a file's contents) shows as it is written,
    // not when its call is done. The caller checks the JSON it adds up to.
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters, "eager_input_streaming": true }))
        .collect();
    if request.web {
        tools.push(json!({ "type": "web_search_20260209", "name": "web_search", "max_uses": WEB_USES }));
        tools.push(json!({ "type": "web_fetch_20260209", "name": "web_fetch", "max_uses": WEB_USES }));
    }
    if !tools.is_empty() {
        body["tools"] = tools.into();
        // Current Claude models refuse to be made to call a tool, and so does
        // any model while it thinks: the prompt asks for the tools instead.
        if let Some(choice) = request.tool_choice {
            body["tool_choice"] = json!({ "type": if choice == ToolChoice::None { "none" } else { "auto" } });
        }
    }
    if let Some(effort) = request.reasoning.filter(|effort| EFFORTS.contains(effort)) {
        body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
        body["output_config"] = json!({ "effort": effort });
    }
    if first_party {
        // Long conversations are summarised by the server once they near the
        // limit; the compaction block it returns is replayed like any other.
        body["context_management"] = json!({ "edits": [{ "type": "compact_20260112" }] });
        // A request a safety classifier declines is answered by the model
        // Anthropic names for that case, inside the same call.
        if FALLBACK_MODELS.iter().any(|prefix| request.model.starts_with(prefix)) {
            body["fallbacks"] = "default".into();
        }
    }
    body
}

/// A call id Claude accepts (`[A-Za-z0-9_-]`); another provider's may hold more.
fn tool_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// What the events of one streamed reply added up to.
#[derive(Default)]
struct Blocks {
    /// Calls by the index of their content block.
    calls: Vec<(u64, ToolCall)>,
    /// As Claude counts it: `input_tokens` without the cached ones.
    usage: Usage,
    /// The message's `stop_reason`, once sent.
    stop: Option<String>,
    /// The reply's content blocks as they arrive, by index, with the JSON
    /// a tool call's input has streamed so far.
    content: Vec<(u64, Value, String)>,
}

impl Blocks {
    /// Takes one event's data, passing on what it adds. `Ok(true)` at `message_stop`.
    fn event(&mut self, data: &str, on_event: &mut dyn FnMut(StreamEvent)) -> Result<bool> {
        let value: Value = serde_json::from_str(data)?;
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match value["type"].as_str().unwrap_or_default() {
            "message_start" => self.count(&value["message"]["usage"]),
            "message_delta" => {
                self.count(&value["usage"]);
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(reason.to_owned());
                }
            }
            "message_stop" => return Ok(true),
            "ping" => on_event(StreamEvent::Ping),
            "error" => {
                let code = match value.pointer("/error/type").and_then(Value::as_str).unwrap_or_default() {
                    "" => None,
                    "overloaded_error" | "api_error" => Some("server_error".to_owned()),
                    "rate_limit_error" => Some("rate_limit_exceeded".to_owned()),
                    other => Some(other.to_owned()),
                };
                let message = value.pointer("/error/message").and_then(Value::as_str);
                return Err(Error::Response {
                    code,
                    message: message.unwrap_or("The provider reported an error.").to_owned(),
                });
            }
            "content_block_start" => {
                let block = &value["content_block"];
                if self.content.len() < MAX_BLOCKS && self.block(index).is_none() {
                    self.content.push((index, block.clone(), String::new()));
                }
                if block["type"] == "tool_use" && self.calls.len() < MAX_CALLS && self.call(index).is_none() {
                    let field = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                    on_event(StreamEvent::ToolCallStarted { index, name: field("name") });
                    let call = ToolCall { call_id: field("id"), name: field("name"), arguments: String::new() };
                    self.calls.push((index, call));
                }
            }
            "content_block_delta" => {
                let delta = &value["delta"];
                let text =
                    |key: &str| delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
                let kind = delta["type"].as_str().unwrap_or_default();
                self.keep(index, kind, delta);
                match kind {
                    "text_delta" => text("text").into_iter().for_each(|t| on_event(StreamEvent::Text(t))),
                    "thinking_delta" => text("thinking").into_iter().for_each(|t| on_event(StreamEvent::Reasoning(t))),
                    "input_json_delta" => {
                        if let Some(json) = text("partial_json")
                            && let Some(call) = self.call(index)
                        {
                            call.arguments.push_str(&json);
                            on_event(StreamEvent::ToolCallDelta { index, delta: json });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some((_, block, json)) = self.block(index) {
                    if !json.is_empty() {
                        // A call cut off or malformed keeps an empty input; it never runs.
                        let input = serde_json::from_str::<Value>(json).ok().filter(Value::is_object);
                        block["input"] = input.unwrap_or_else(|| json!({}));
                    }
                    if block["type"] == "server_tool_use" {
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        on_event(StreamEvent::ServerTool { name, input: block["input"].to_string() });
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// The content block at `index`, with its input's JSON so far.
    fn block(&mut self, index: u64) -> Option<&mut (u64, Value, String)> {
        self.content.iter_mut().find(|(i, _, _)| *i == index)
    }

    /// Adds a delta to the block it belongs to, so the reply can be sent
    /// back exactly as Claude wrote it.
    fn keep(&mut self, index: u64, kind: &str, delta: &Value) {
        let Some((_, block, json)) = self.block(index) else { return };
        let piece = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default();
        let append = |block: &mut Value, key: &str, more: &str| {
            let mut text = block.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            text.push_str(more);
            block[key] = text.into();
        };
        match kind {
            "text_delta" => append(block, "text", piece("text")),
            "thinking_delta" => append(block, "thinking", piece("thinking")),
            "signature_delta" => block["signature"] = piece("signature").into(),
            "input_json_delta" => json.push_str(piece("partial_json")),
            "compaction_delta" => block["content"] = delta["content"].clone(),
            "citations_delta" => match block.get_mut("citations").and_then(Value::as_array_mut) {
                Some(citations) => citations.push(delta["citation"].clone()),
                None => block["citations"] = json!([delta["citation"]]),
            },
            _ => {}
        }
    }

    /// The call written in content block `index`.
    fn call(&mut self, index: u64) -> Option<&mut ToolCall> {
        self.calls.iter_mut().find(|(i, _)| *i == index).map(|(_, call)| call)
    }

    /// Takes the counts `usage` holds; `message_delta` repeats them, cumulative.
    fn count(&mut self, usage: &Value) {
        let Usage { input_tokens, output_tokens, cached_tokens, cache_write_tokens } = &mut self.usage;
        for (key, slot) in [
            ("input_tokens", input_tokens),
            ("output_tokens", output_tokens),
            ("cache_read_input_tokens", cached_tokens),
            ("cache_creation_input_tokens", cache_write_tokens),
        ] {
            if let Some(n) = usage.get(key).and_then(Value::as_u64) {
                *slot = n;
            }
        }
    }

    /// The finished reply, in the Responses API's terms.
    fn completion(&mut self) -> Completion {
        let mut usage = self.usage;
        // AtroxaBot's input counts the cached tokens too.
        usage.input_tokens =
            usage.input_tokens.saturating_add(usage.cached_tokens).saturating_add(usage.cache_write_tokens);
        let tool_calls = std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(_, mut call)| {
                // A call without arguments streams none.
                if call.arguments.is_empty() {
                    "{}".clone_into(&mut call.arguments);
                }
                call
            })
            .collect();
        let incomplete = match self.stop.as_deref() {
            Some("max_tokens" | "model_context_window_exceeded") => Some("max_output_tokens".to_owned()),
            Some("refusal") => Some("refusal".to_owned()),
            _ => None,
        };
        let content = std::mem::take(&mut self.content).into_iter().map(|(_, block, _)| block).collect();
        let paused = self.stop.as_deref() == Some("pause_turn");
        Completion { usage, reasoning: String::new(), tool_calls, incomplete, content, paused }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::ToolSpec;

    #[test]
    fn requests_translate() {
        let call = |id: &str| {
            InputItem::ToolCall(ToolCall { call_id: id.into(), name: "speak".into(), arguments: "{\"a\":1}".into() })
        };
        let output =
            |id: &str| InputItem::ToolOutput { call_id: id.into(), output: "ok".into(), is_error: id == "b.1" };
        let input = [
            InputItem::text(Role::User, "hi"),
            InputItem::text(Role::Assistant, "  "),
            call("a"),
            output("a"),
            call("b.1"),
            output("b.1"),
            InputItem::text(Role::User, "next\n\nstate"),
        ];
        let schema = json!({ "type": "object" });
        let tools = [ToolSpec { name: "speak", description: "d", parameters: &schema }];
        let request = ResponseRequest {
            model: "m",
            instructions: Some("rules"),
            reasoning: Some("high"),
            input: &input,
            tools: &tools,
            tool_choice: Some(ToolChoice::Function("speak")),
            web: false,
        };
        let body = body(&request, false);
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"], "blank text dropped, a reply's calls together");
        let calls = messages[1]["content"].as_array().unwrap();
        assert_eq!((calls.len(), &calls[0]["input"], &calls[1]["id"]), (2, &json!({ "a": 1 }), &json!("b_1")));
        let answers = messages[2]["content"].as_array().unwrap();
        let kinds: Vec<&str> = answers.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["tool_result", "tool_result", "text"], "results first");
        assert_eq!(answers[1]["tool_use_id"], "b_1");
        assert!(answers[1].get("cache_control").is_none() && answers[2].get("cache_control").is_some());
        assert_eq!((answers[0].get("is_error"), &answers[1]["is_error"]), (None, &json!(true)));
        assert!(body.get("context_management").is_none(), "betas only go to Anthropic itself");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tool_choice"], json!({ "type": "auto" }), "never forced");
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["thinking"]["display"], "summarized");

        let plain =
            ResponseRequest { reasoning: Some("none"), tools: &[], tool_choice: Some(ToolChoice::None), ..request };
        let plain = super::body(&plain, false);
        assert!(plain.get("thinking").is_none() && plain.get("tool_choice").is_none(), "not Claude's");

        // A reply replayed as it arrived is its own turn, and Anthropic gets the betas.
        let thinking = json!({ "type": "thinking", "thinking": "Hm.", "signature": "sig" });
        let call = json!({ "type": "tool_use", "id": "t1", "name": "read", "input": {} });
        let input = [
            InputItem::text(Role::User, "hi"),
            InputItem::Blocks(vec![thinking.clone(), call]),
            InputItem::ToolOutput { call_id: "t1".into(), output: "ok".into(), is_error: false },
        ];
        let replay = ResponseRequest { model: "claude-opus-5-5", input: &input, web: true, ..request };
        let replay = super::body(&replay, true);
        assert_eq!(replay["messages"][1]["content"][0], thinking, "thinking goes back unchanged");
        assert!(replay["messages"][1]["content"][1].get("cache_control").is_none());
        assert_eq!(replay["messages"][2]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(replay["context_management"]["edits"][0]["type"], "compact_20260112");
        assert_eq!(replay["fallbacks"], "default");
        assert_eq!(replay["tools"][1]["type"], "web_search_20260209");
        assert!(replay["tools"][1].get("eager_input_streaming").is_none());
    }

    #[test]
    fn events_become_stream_events() {
        let mut blocks = Blocks::default();
        let mut events = Vec::new();
        let mut feed = |data: &str| blocks.event(data, &mut |e| events.push(e)).unwrap();
        feed(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":1}}}"#,
        );
        feed(r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#);
        feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Hm."}}"#);
        feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#);
        feed(r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#);
        feed(
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"speak","input":{}}}"#,
        );
        feed(r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#);
        feed(r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":":1}"}}"#);
        feed(
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"remember"}}"#,
        );
        feed(r#"{"type":"ping"}"#);
        feed(r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4}}"#);
        assert!(feed(r#"{"type":"message_stop"}"#));
        assert_eq!(events[..2], [StreamEvent::Reasoning("Hm.".into()), StreamEvent::Text("Hi".into())]);
        assert_eq!(events[2], StreamEvent::ToolCallStarted { index: 2, name: "speak".into() });
        assert_eq!(events[4], StreamEvent::ToolCallDelta { index: 2, delta: ":1}".into() });
        assert_eq!(events[6], StreamEvent::Ping);

        let done = blocks.completion();
        assert_eq!(done.usage, Usage { input_tokens: 10, output_tokens: 4, cached_tokens: 5, cache_write_tokens: 2 });
        assert_eq!(done.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(
            done.tool_calls[0],
            ToolCall { call_id: "t1".into(), name: "speak".into(), arguments: r#"{"a":1}"#.into() }
        );
        assert_eq!(done.tool_calls[1].arguments, "{}");
        assert_eq!(done.content[0], json!({ "type": "thinking", "thinking": "Hm.", "signature": "x" }));
        assert_eq!(done.content[1]["input"], json!({}), "an input never closed stays empty");
        assert!(!done.paused);
    }

    #[test]
    fn failures_and_hostile_events() {
        let mut blocks = Blocks::default();
        let error = |data: &str| Blocks::default().event(data, &mut |_| {}).unwrap_err();
        let overloaded = error(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        assert!(overloaded.is_retryable() && overloaded.to_string() == "Overloaded");
        assert!(!error(r#"{"type":"error","error":{"type":"invalid_request_error","message":"No."}}"#).is_retryable());
        assert!(blocks.event("{", &mut |_| {}).is_err());
        for i in 0..100 {
            let data = format!(r#"{{"type":"content_block_start","index":{i},"content_block":{{"type":"tool_use"}}}}"#);
            blocks.event(&data, &mut |_| {}).unwrap();
        }
        assert_eq!(blocks.calls.len(), MAX_CALLS);
        let long = Error::Api {
            status: 400,
            code: None,
            message: "prompt is too long: 1000001 tokens > 1000000 maximum".into(),
            retry_after: None,
        };
        assert!(overflow(long).is_context_overflow());
        assert_eq!(price("claude-opus-5-5"), Some((4.0, 20.0, 0.2)));
        assert_eq!(price("claude-opus-5"), Some((5.0, 25.0, 0.5)));
        assert_eq!(price("gpt-5"), None);
    }
}
