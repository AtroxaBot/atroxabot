//! `POST /responses`, OpenAI's own API (`api.openai.com`): the same
//! [`ResponseRequest`] in and the same [`StreamEvent`]s out as the other
//! wires.
//!
//! Nothing is kept on OpenAI's side (`store: false`): every request carries
//! the conversation, and a reply goes back as the output items it arrived
//! as, its encrypted reasoning included, so a reasoning model keeps its
//! reasoning between tool calls, which Chat Completions cannot carry.
//! `prompt_cache_key` (the session) keeps a conversation on the machines
//! that hold its cache.

use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::responses::{
    Completion, InputItem, ResponseRequest, Role, StreamEvent, ToolCall, ToolChoice, Usage, read_events,
};

/// Most tool calls one reply may make; a hostile stream can't grow past it.
const MAX_CALLS: usize = 64;
/// Most output items one reply may keep for replaying.
const MAX_ITEMS: usize = 512;

/// Streams a reply from OpenAI; see [`Client::stream_response`].
pub(crate) fn stream(
    client: &Client,
    request: &ResponseRequest<'_>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<bool> {
    let response = client.post("/responses", &body(request))?;
    let mut items = Items::default();
    let done = read_events(response, cancel, on_event, |event, on_event| items.event(&event.data, on_event))?;
    if done {
        on_event(StreamEvent::Completed(items.completion()));
    }
    Ok(done)
}

/// Whether `model` is one of OpenAI's reasoning models (the o-series, GPT-5
/// and later, Codex), which take `reasoning` and return reasoning items; the
/// others refuse both.
fn reasons(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let model = model.rsplit('/').next().unwrap_or_default();
    if model.contains("codex") {
        return true;
    }
    if let Some(rest) = model.strip_prefix('o') {
        return rest.starts_with(|c: char| c.is_ascii_digit());
    }
    let Some(rest) = model.strip_prefix("gpt-") else { return false };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    // A `chat` model is GPT-5's chat flavour, which does not reason.
    major.parse::<u32>().is_ok_and(|major| major >= 5) && !rest.contains("chat")
}

/// The Responses body for `request`.
fn body(request: &ResponseRequest<'_>) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for item in request.input {
        match item {
            InputItem::Message { text, .. } if text.trim().is_empty() => {}
            InputItem::Message { role: Role::User, text } => input.push(json!({ "role": "user", "content": text })),
            InputItem::Message { role: Role::Assistant, text } => {
                input.push(json!({ "role": "assistant", "content": text }));
            }
            InputItem::Image { media_type, data } => {
                let part = json!({ "type": "input_image", "image_url": format!("data:{media_type};base64,{data}"), "detail": "auto" });
                input.push(json!({ "role": "user", "content": [part] }));
            }
            InputItem::ToolCall(call) => {
                let arguments = if call.arguments.trim().is_empty() { "{}" } else { call.arguments.as_str() };
                input.push(json!({ "type": "function_call", "call_id": call.call_id, "name": call.name, "arguments": arguments }));
            }
            InputItem::ToolOutput { call_id, output, picture, .. } => {
                let output = match picture {
                    Some(picture) => json!([
                        { "type": "input_text", "text": output },
                        { "type": "input_image", "image_url": format!("data:{};base64,{}", picture.media_type, picture.data), "detail": "auto" },
                    ]),
                    None => output.as_str().into(),
                };
                input.push(json!({ "type": "function_call_output", "call_id": call_id, "output": output }));
            }
            // The output items as they arrived, encrypted reasoning and all.
            InputItem::Replay(items) => input.extend(items.iter().filter(|item| item.is_object()).cloned()),
        }
    }
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
        "parallel_tool_calls": true,
    });
    if let Some(text) = request.instructions.filter(|text| !text.trim().is_empty()) {
        body["instructions"] = text.into();
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false }))
            .collect();
        body["tools"] = tools.into();
    }
    if let Some(choice) = request.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Function(name) => json!({ "type": "function", "name": name }),
        };
    }
    if reasons(request.model) {
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(effort) = request.reasoning.filter(|effort| !effort.is_empty()) {
            reasoning["effort"] = effort.into();
        }
        body["reasoning"] = reasoning;
        // Without storage, this is how a reasoning item can come back.
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(key) = request.cache_key.filter(|key| !key.is_empty()) {
        body["prompt_cache_key"] = key.into();
    }
    body
}

/// What the events of one streamed response added up to.
#[derive(Default)]
struct Items {
    /// Calls by their `output_index`, as they stream.
    calls: Vec<(u64, ToolCall)>,
    /// Finished output items, in order, for replaying.
    output: Vec<Value>,
    usage: Usage,
    /// Why the response stopped early, when it did.
    incomplete: Option<String>,
    /// The reasoning summary part last written (item and index), to set the next one apart.
    summary: Option<(String, u64)>,
}

impl Items {
    /// Takes one event's data, passing on what it adds. `Ok(true)` once the
    /// response is complete (or was cut short).
    fn event(&mut self, data: &str, on_event: &mut dyn FnMut(StreamEvent)) -> Result<bool> {
        let value: Value = serde_json::from_str(data)?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let index = value.get("output_index").and_then(Value::as_u64).unwrap_or(0);
        match value["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(text) = text("delta") {
                    on_event(StreamEvent::Text(text));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let part = (
                    value.get("item_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    value.get("summary_index").and_then(Value::as_u64).unwrap_or(0),
                );
                if self.summary.as_ref().is_some_and(|last| *last != part) {
                    on_event(StreamEvent::Reasoning("\n\n".to_owned()));
                }
                self.summary = Some(part);
                if let Some(text) = text("delta") {
                    on_event(StreamEvent::Reasoning(text));
                }
            }
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"] == "function_call" && self.calls.len() < MAX_CALLS && self.call(index).is_none() {
                    let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                    on_event(StreamEvent::ToolCallStarted { index, name: field("name") });
                    let call = ToolCall { call_id: field("call_id"), name: field("name"), arguments: String::new() };
                    self.calls.push((index, call));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = text("delta")
                    && let Some(call) = self.call(index)
                {
                    call.arguments.push_str(&delta);
                    on_event(StreamEvent::ToolCallDelta { index, delta });
                }
            }
            "response.output_item.done" => {
                let item = &value["item"];
                if item.is_object() && self.output.len() < MAX_ITEMS {
                    self.output.push(item.clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                self.count(&response["usage"]);
                if let Some(reason) = response.pointer("/incomplete_details/reason").and_then(Value::as_str) {
                    self.incomplete = Some(reason.to_owned());
                } else if value["type"] == "response.incomplete" {
                    self.incomplete = Some("incomplete".to_owned());
                }
                // The response's own list is the whole of it; the items seen are the fallback.
                if let Some(output) =
                    response.get("output").and_then(Value::as_array).filter(|output| !output.is_empty())
                {
                    self.output = output.iter().filter(|item| item.is_object()).take(MAX_ITEMS).cloned().collect();
                }
                return Ok(true);
            }
            "response.failed" => return Err(failure(value.pointer("/response/error").unwrap_or(&Value::Null))),
            "error" => return Err(failure(&value)),
            _ => {}
        }
        Ok(false)
    }

    /// The call streaming at `index`.
    fn call(&mut self, index: u64) -> Option<&mut ToolCall> {
        self.calls.iter_mut().find(|(i, _)| *i == index).map(|(_, call)| call)
    }

    /// Takes the counts `usage` holds.
    fn count(&mut self, usage: &Value) {
        let count = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
        self.usage = Usage {
            input_tokens: count("/input_tokens"),
            output_tokens: count("/output_tokens"),
            cached_tokens: count("/input_tokens_details/cached_tokens"),
            cache_write_tokens: count("/input_tokens_details/cache_write_tokens"),
        };
    }

    /// The finished response: its calls as the final items name them (the
    /// streamed ones when there are none), and the items to replay.
    fn completion(&mut self) -> Completion {
        let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let finished: Vec<ToolCall> = self
            .output
            .iter()
            .filter(|item| item["type"] == "function_call")
            .take(MAX_CALLS)
            .map(|item| ToolCall {
                call_id: field(item, "call_id"),
                name: field(item, "name"),
                arguments: field(item, "arguments"),
            })
            .collect();
        let streamed = std::mem::take(&mut self.calls).into_iter().map(|(_, call)| call);
        let tool_calls = if finished.is_empty() { streamed.collect() } else { finished };
        let tool_calls = tool_calls
            .into_iter()
            .filter(|call: &ToolCall| !call.name.is_empty() && !call.call_id.is_empty())
            .map(|mut call| {
                if call.arguments.trim().is_empty() {
                    "{}".clone_into(&mut call.arguments);
                }
                call
            })
            .collect();
        Completion {
            usage: self.usage,
            reasoning: String::new(),
            tool_calls,
            incomplete: self.incomplete.take(),
            content: std::mem::take(&mut self.output),
            paused: false,
        }
    }
}

/// The error a failed response or an `error` event reports.
fn failure(error: &Value) -> Error {
    let field = |key: &str| error.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    Error::Response {
        code: field("code"),
        message: field("message").unwrap_or_else(|| "The provider reported an error.".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::{Picture, ToolSpec};

    #[test]
    fn reasoning_models_are_known_by_name() {
        for model in ["o3", "o4-mini", "gpt-5", "gpt-5.6-sol", "gpt-6", "gpt-5.1-codex", "openai/gpt-5"] {
            assert!(reasons(model), "{model}");
        }
        for model in ["gpt-4o", "gpt-4.1-mini", "gpt-5-chat-latest", "omni-moderation-latest", "chatgpt-4o"] {
            assert!(!reasons(model), "{model}");
        }
    }

    #[test]
    fn requests_translate() {
        let kept = vec![
            json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "e" }),
            json!({ "type": "function_call", "id": "fc_1", "call_id": "c1", "name": "read", "arguments": "{}", "status": "completed" }),
        ];
        let input = [
            InputItem::text(Role::User, "look"),
            InputItem::Image { media_type: "image/png", data: "AAAA".into() },
            InputItem::text(Role::Assistant, "  "),
            InputItem::Replay(kept.clone()),
            InputItem::ToolOutput {
                call_id: "c1".into(),
                output: "A picture.".into(),
                is_error: false,
                picture: Some(Picture { media_type: "image/png".into(), data: "BBBB".into() }),
            },
            InputItem::ToolCall(ToolCall { call_id: "c2".into(), name: "list".into(), arguments: String::new() }),
            InputItem::ToolOutput { call_id: "c2".into(), output: "a.rs".into(), is_error: false, picture: None },
        ];
        let schema = json!({ "type": "object" });
        let tools = [ToolSpec { name: "read", description: "d", parameters: &schema }];
        let request = ResponseRequest {
            model: "gpt-5.6-sol",
            instructions: Some("rules"),
            reasoning: Some("high"),
            input: &input,
            tools: &tools,
            tool_choice: None,
            web: false,
            cache_key: Some("s1"),
        };
        let body = body(&request);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 7, "blank text is dropped");
        assert_eq!(items[1]["content"][0]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(items[2..4], kept[..], "replayed unchanged");
        assert_eq!(items[4]["output"][1]["image_url"], "data:image/png;base64,BBBB");
        assert_eq!(items[5]["arguments"], "{}");
        assert_eq!(items[6]["output"], "a.rs");
        assert_eq!((&body["store"], &body["stream"]), (&json!(false), &json!(true)));
        assert_eq!(body["instructions"], "rules");
        assert_eq!(
            body["tools"][0],
            json!({ "type": "function", "name": "read", "description": "d", "parameters": schema, "strict": false })
        );
        assert_eq!(body["reasoning"], json!({ "summary": "auto", "effort": "high" }));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["prompt_cache_key"], "s1");

        let plain = super::body(&ResponseRequest { model: "gpt-4.1", reasoning: None, cache_key: None, ..request });
        assert!(
            plain.get("reasoning").is_none()
                && plain.get("include").is_none()
                && plain.get("prompt_cache_key").is_none()
        );
    }

    #[test]
    fn events_become_stream_events() {
        let mut items = Items::default();
        let mut events = Vec::new();
        let mut feed = |data: &str| items.event(data, &mut |e| events.push(e)).unwrap();
        feed(r#"{"type":"response.created","response":{}}"#);
        feed(r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"First."}"#);
        feed(r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":1,"delta":"Then."}"#);
        feed(
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"e"}}"#,
        );
        feed(r#"{"type":"response.output_text.delta","output_index":1,"delta":"Hi"}"#);
        feed(
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"c1","name":"read","arguments":""}}"#,
        );
        feed(r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\""}"#);
        feed(r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":":\"a\"}"}"#);
        let completed = r#"{"type":"response.completed","response":{"output":[
            {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"e"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi"}]},
            {"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"a\"}"}],
            "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":9}}}"#;
        assert!(feed(&completed.replace('\n', "")));
        assert_eq!(
            events[..4],
            [
                StreamEvent::Reasoning("First.".into()),
                StreamEvent::Reasoning("\n\n".into()),
                StreamEvent::Reasoning("Then.".into()),
                StreamEvent::Text("Hi".into()),
            ]
        );
        assert_eq!(events[4], StreamEvent::ToolCallStarted { index: 2, name: "read".into() });
        let done = items.completion();
        assert_eq!(done.usage, Usage { input_tokens: 100, output_tokens: 9, cached_tokens: 60, cache_write_tokens: 0 });
        assert_eq!(
            done.tool_calls,
            [ToolCall { call_id: "c1".into(), name: "read".into(), arguments: r#"{"path":"a"}"#.into() }]
        );
        assert_eq!((done.content.len(), &done.content[0]["encrypted_content"]), (3, &json!("e")));
        assert!(done.incomplete.is_none());
    }

    #[test]
    fn failures_and_hostile_events() {
        let error = |data: &str| Items::default().event(data, &mut |_| {}).unwrap_err();
        let failed =
            error(r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"Oops."}}}"#);
        assert!(failed.is_retryable() && failed.to_string() == "Oops.");
        let overflow = error(r#"{"type":"error","code":"context_length_exceeded","message":"Too long."}"#);
        assert!(overflow.is_context_overflow());
        assert!(!error(r#"{"type":"error","code":"invalid_prompt","message":"No."}"#).is_retryable());

        let mut items = Items::default();
        assert!(items.event("{", &mut |_| {}).is_err());
        for i in 0..100 {
            let data = format!(
                r#"{{"type":"response.output_item.added","output_index":{i},"item":{{"type":"function_call","call_id":"c{i}","name":"x"}}}}"#
            );
            items.event(&data, &mut |_| {}).unwrap();
        }
        assert_eq!(items.calls.len(), MAX_CALLS);
        assert!(items.event(r#"{"type":"response.output_item.done","item":"odd"}"#, &mut |_| {}).is_ok());
        assert!(items.output.is_empty(), "an item that is no object is not kept");
        let cut = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"output":[]}}"#;
        assert!(items.event(cut, &mut |_| {}).unwrap());
        let done = items.completion();
        assert_eq!(done.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(done.tool_calls.len(), MAX_CALLS, "the streamed calls when no item names any");
    }
}
