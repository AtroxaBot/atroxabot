//! Streamed replies: text messages, function tools and their results, in
//! one shape for every provider, sent over the wire that provider speaks:
//! Anthropic's Messages (`messages.rs`), OpenAI's Responses (`openai.rs`)
//! or Chat Completions for every other OpenAI-compatible one
//! (`completions.rs`).

use std::io::{BufRead, Read as _};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::sse;

/// Longest SSE line accepted, in bytes; a longer one fails the stream.
const MAX_LINE: u64 = 8 << 20;
/// Most data one SSE event may carry, in bytes.
const MAX_EVENT: usize = 16 << 20;

/// Author of a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The human.
    User,
    /// The model.
    Assistant,
}

/// Token accounting reported when a response completes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens, including those read from or written to the cache.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens read from the provider's prompt cache.
    #[serde(skip_serializing_if = "is_zero")]
    pub cached_tokens: u64,
    /// Prompt tokens written to the prompt cache.
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Plain input and output counts, without cache details.
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self { input_tokens, output_tokens, ..Self::default() }
    }
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// How a provider is spoken to. A reply kept as it arrived goes back only
/// over the wire it came by: each has its own shape for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Anthropic's Messages API (`/messages`); what replies saved before
    /// the wire was noted came by.
    #[default]
    Messages,
    /// Chat Completions (`/chat/completions`): OpenRouter, DeepSeek,
    /// local servers and every other OpenAI-compatible provider.
    Chat,
    /// OpenAI's own Responses API (`/responses`).
    Responses,
}

impl Wire {
    /// Whether it is [`Wire::Messages`] (which a saved reply leaves out).
    #[must_use]
    pub fn is_messages(&self) -> bool {
        *self == Self::Messages
    }
}

/// A picture a tool's result shows the model (a `read` of a PNG).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Picture {
    /// Its MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Its bytes in base64.
    pub data: String,
}

/// A function call requested by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Identifier echoed back with the result.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON object in text form.
    pub arguments: String,
}

/// One item of a request's `input`.
#[derive(Debug, Clone, PartialEq)]
pub enum InputItem {
    /// A chat turn.
    Message {
        /// Its author.
        role: Role,
        /// Its text.
        text: String,
    },
    /// A tool call the model made earlier in the conversation.
    ToolCall(ToolCall),
    /// The result of running a tool call.
    ToolOutput {
        /// The call this answers.
        call_id: String,
        /// What the tool returned (or why it did not run).
        output: String,
        /// Whether the tool failed or was refused.
        is_error: bool,
        /// A picture the result shows, when it has one.
        picture: Option<Picture>,
    },
    /// A picture the user sent along with their next words.
    Image {
        /// Its MIME type, e.g. `image/png`.
        media_type: &'static str,
        /// Its bytes in base64.
        data: String,
    },
    /// A reply exactly as its wire sent it, replayed unchanged so the model
    /// keeps its reasoning: Claude's content blocks (thinking signatures,
    /// compaction blocks), a Chat Completions assistant message (with its
    /// `reasoning_content` or `reasoning_details`), or the Responses API's
    /// output items (encrypted reasoning). Only sent over the wire it came
    /// by (see [`Wire`]).
    Replay(Vec<Value>),
}

impl InputItem {
    /// A plain-text message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::Message { role, text: text.into() }
    }
}

/// A function the model may call.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec<'a> {
    /// Name the model uses.
    pub name: &'a str,
    /// What it does, for the model.
    pub description: &'a str,
    /// JSON Schema of its arguments.
    pub parameters: &'a Value,
}

/// An incremental update from a streaming response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A chunk of the visible answer.
    Text(String),
    /// A chunk of the model's reasoning, for servers that stream it.
    Reasoning(String),
    /// A keep-alive from the server; it sends one every 15 seconds while the
    /// model works, so a silent connection means a dead one.
    Ping,
    /// The model started writing a tool call. `index` identifies it in the
    /// [`StreamEvent::ToolCallDelta`]s that follow.
    ToolCallStarted {
        /// Position of the call in the response's output.
        index: u64,
        /// Tool name.
        name: String,
    },
    /// More of a tool call's JSON arguments, for showing it as it is written.
    /// The complete call arrives in [`StreamEvent::Completed`].
    ToolCallDelta {
        /// The call, as in [`StreamEvent::ToolCallStarted`].
        index: u64,
        /// The next piece of the arguments.
        delta: String,
    },
    /// Claude ran one of Anthropic's server tools (a web search, a page
    /// fetch); nothing is asked of the caller.
    ServerTool {
        /// Tool name.
        name: String,
        /// Its arguments as JSON text.
        input: String,
    },
    /// What a failed response was billed for; sent just before the error.
    Charged(Usage),
    /// The response finished, possibly cut short (see [`Completion::incomplete`]).
    Completed(Completion),
}

/// Final details of a finished response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completion {
    /// Token accounting.
    pub usage: Usage,
    /// The model's full reasoning, for servers that only send it at the end;
    /// empty for non-thinking models.
    pub reasoning: String,
    /// Function calls the model wants run before it continues.
    pub tool_calls: Vec<ToolCall>,
    /// Why the response stopped early (`max_output_tokens`,
    /// `content_filter`); `None` when it finished. Tool calls of an
    /// incomplete response may have truncated arguments and must not run.
    pub incomplete: Option<String>,
    /// The reply as its wire sent it, to replay unchanged with
    /// [`InputItem::Replay`]: Claude's content blocks, a Chat Completions
    /// assistant message, or the Responses API's output items.
    pub content: Vec<Value>,
    /// Whether the server paused a long turn of its own tools: send the
    /// conversation again, with this reply, to let it go on.
    pub paused: bool,
}

/// Parameters for a single response.
#[derive(Debug, Clone, Copy)]
pub struct ResponseRequest<'a> {
    /// Model identifier.
    pub model: &'a str,
    /// Optional system/developer instructions.
    pub instructions: Option<&'a str>,
    /// Reasoning effort (`none`, `low`, `medium`, `high`); `None` leaves it
    /// to the model.
    pub reasoning: Option<&'a str>,
    /// Conversation so far, oldest first.
    pub input: &'a [InputItem],
    /// Functions the model may call; empty for plain chat.
    pub tools: &'a [ToolSpec<'a>],
    /// Whether and which tools the model must call; `None` lets it decide.
    pub tool_choice: Option<ToolChoice<'a>>,
    /// Let Claude search and read the web with Anthropic's server tools.
    /// Ignored by Chat Completions.
    pub web: bool,
    /// What names the conversation to the provider's prompt cache (the
    /// session's id): OpenRouter's `session_id`, OpenAI's
    /// `prompt_cache_key`. Anthropic needs none.
    pub cache_key: Option<&'a str>,
}

/// What a response must do with its tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice<'a> {
    /// Call none of them.
    None,
    /// Call at least one of them.
    Required,
    /// Call this one.
    Function(&'a str),
}

impl Client {
    /// Streams a response, invoking `on_event` for every update.
    ///
    /// Returns `Ok(true)` when the response completed (or was cut short, see
    /// [`Completion::incomplete`]) and `Ok(false)` when `cancel` was raised or
    /// the stream ended early. `cancel` is checked between SSE lines, which
    /// arrive at least every 15 seconds thanks to the server's keep-alives.
    ///
    /// # Errors
    /// Network failures, non-success statuses, and failures reported inside
    /// the stream ([`Error::Response`]).
    pub fn stream_response(
        &self,
        request: &ResponseRequest<'_>,
        cancel: &AtomicBool,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<bool> {
        match self.wire(request.model) {
            Wire::Messages => crate::messages::stream(self, request, cancel, &mut on_event),
            Wire::Responses => crate::openai::stream(self, request, cancel, &mut on_event),
            Wire::Chat => crate::completions::stream(self, request, cancel, &mut on_event),
        }
    }
}

/// Reads `response` as server-sent events, handing each to `handle` until it
/// returns `Ok(true)`: the response is complete. Comments are the server's
/// keep-alives. Returns `Ok(true)` when complete and `Ok(false)` when
/// `cancel` was raised or the stream ended first; `cancel` is checked
/// between lines.
pub(crate) fn read_events(
    response: ureq::http::Response<ureq::Body>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
    mut handle: impl FnMut(sse::Event, &mut dyn FnMut(StreamEvent)) -> Result<bool>,
) -> Result<bool> {
    let mut reader = std::io::BufReader::new(response.into_body().into_reader());
    let mut decoder = sse::Decoder::default();
    let mut buffer = Vec::new();
    let mut ended = false;
    while !ended {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        // The trailing empty line flushes an event the server did not terminate.
        let line = read_line(&mut reader, &mut buffer)?.unwrap_or_else(|| {
            ended = true;
            String::new()
        });
        if line.starts_with(':') {
            on_event(StreamEvent::Ping);
            continue;
        }
        let Some(event) = decoder.line(&line) else {
            if decoder.pending() > MAX_EVENT {
                return Err(too_large("event"));
            }
            continue;
        };
        if handle(event, on_event)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads one line (without its terminator) of at most [`MAX_LINE`] bytes
/// into `buffer`, which is reused between calls; `None` at the end of the
/// stream. Invalid UTF-8 is replaced: a line is JSON or a comment.
fn read_line(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> Result<Option<String>> {
    buffer.clear();
    let read = reader.by_ref().take(MAX_LINE + 1).read_until(b'\n', buffer)?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    } else if buffer.len() as u64 > MAX_LINE {
        return Err(too_large("line"));
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    Ok(Some(String::from_utf8_lossy(buffer).into_owned()))
}

/// The error for a stream sending more than we accept in one `what`.
fn too_large(what: &str) -> Error {
    Error::Response {
        code: Some("stream_too_large".into()),
        message: format!("The response stream sent an oversized {what}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_bounded() {
        let mut buffer = Vec::new();
        let mut reader = std::io::Cursor::new(b"data: a\r\n\nlast".to_vec());
        assert_eq!(read_line(&mut reader, &mut buffer).unwrap().as_deref(), Some("data: a"));
        assert_eq!(read_line(&mut reader, &mut buffer).unwrap().as_deref(), Some(""));
        assert_eq!(read_line(&mut reader, &mut buffer).unwrap().as_deref(), Some("last"));
        assert_eq!(read_line(&mut reader, &mut buffer).unwrap(), None);

        let huge = vec![b'x'; usize::try_from(MAX_LINE).unwrap() + 2];
        let err = read_line(&mut std::io::Cursor::new(huge), &mut buffer).unwrap_err();
        assert!(!err.is_retryable() && err.code() == Some("stream_too_large"));
    }
}
