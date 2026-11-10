//! `POST /chat/completions` for OpenAI-compatible providers (OpenRouter,
//! Inworld, local servers): the same [`ResponseRequest`] in and the same
//! [`StreamEvent`]s out as the Responses API, translated both ways.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::client::{Client, parse_error_body};
use crate::error::{Error, Result};
use crate::responses::{Completion, InputItem, ResponseRequest, StreamEvent, ToolCall, ToolChoice, Usage, read_events};

/// Most tool calls one reply may make; a hostile stream can't grow past it.
const MAX_CALLS: usize = 64;

/// Streams a reply from `client`'s provider; see [`Client::stream_response`].
pub(crate) fn stream(
    client: &Client,
    request: &ResponseRequest<'_>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<bool> {
    let response = client.post("/chat/completions", &body(request))?;
    let mut chunks = Chunks::default();
    let done = read_events(response, cancel, on_event, |event, on_event| chunks.data(&event.data, on_event))?;
    // Some servers close the stream after the last chunk without `[DONE]`.
    if done || (chunks.finish.is_some() && !cancel.load(Ordering::Relaxed)) {
        on_event(StreamEvent::Completed(chunks.completion()));
        return Ok(true);
    }
    Ok(false)
}

/// The Chat Completions body for `request`: instructions become the system
/// message, and a reply's calls join its assistant message.
fn body(request: &ResponseRequest<'_>) -> Value {
    let mut messages: Vec<Value> =
        request.instructions.map(|text| json!({ "role": "system", "content": text })).into_iter().collect();
    for item in request.input {
        match item {
            InputItem::Message { role, text } => messages.push(json!({ "role": role, "content": text })),
            InputItem::Image { media_type, data } => {
                let url = format!("data:{media_type};base64,{data}");
                let part = json!({ "type": "image_url", "image_url": { "url": url } });
                messages.push(json!({ "role": "user", "content": [part] }));
            }
            InputItem::ToolCall(call) => {
                let call = json!({ "id": call.call_id, "type": "function", "function": { "name": call.name, "arguments": call.arguments } });
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
            InputItem::ToolOutput { call_id, output, .. } => {
                messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": output }));
            }
            // Claude's own blocks mean nothing here; the engine sends such a
            // reply as text and calls instead.
            InputItem::Blocks(_) => {}
        }
    }
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
        body["reasoning_effort"] = effort.into();
    }
    body
}

/// What the chunks of one streamed reply added up to.
#[derive(Default)]
struct Chunks {
    /// Calls by their `index` in the chunks.
    calls: Vec<(u64, ToolCall)>,
    usage: Usage,
    /// The choice's `finish_reason`, once sent.
    finish: Option<String>,
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
            let count = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
            self.usage = Usage {
                input_tokens: count("/prompt_tokens"),
                output_tokens: count("/completion_tokens"),
                cached_tokens: count("/prompt_tokens_details/cached_tokens"),
                cache_write_tokens: count("/prompt_tokens_details/cache_write_tokens"),
            };
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(false);
        };
        let delta = &choice["delta"];
        let text = |key: &str| delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        // `reasoning` is OpenRouter's name, `reasoning_content` DeepSeek's and vLLM's.
        if let Some(reasoning) = text("reasoning").or_else(|| text("reasoning_content")) {
            on_event(StreamEvent::Reasoning(reasoning));
        }
        if let Some(content) = text("content") {
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

    /// The finished reply, in the Responses API's terms.
    fn completion(&mut self) -> Completion {
        let tool_calls = std::mem::take(&mut self.calls)
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
        Completion { usage: self.usage, reasoning: String::new(), tool_calls, incomplete, ..Completion::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::{Role, ToolSpec};

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
            InputItem::ToolOutput { call_id: "a".into(), output: "ok".into(), is_error: false },
            InputItem::ToolOutput { call_id: "b".into(), output: "ok".into(), is_error: true },
            call("c"),
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
        let body = body(&request);
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
        feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"remember","arguments":"{}"}}]}}]}"#);
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
    }
}
