//! Turns one line of the binary's stream-json output into an [`Event`].
//!
//! Nothing here trusts the input: missing or mistyped fields fall back to empty values, unknown messages pass
//! through as [`Event::Other`], and `serde_json` bounds the nesting depth.

use serde_json::Value;

use crate::cli::preview;
use crate::{
    Account, Block, ContextCategory, ContextUsage, Event, Info, Init, McpServerStatus, ModelInfo, PermissionRequest,
    RateLimit, RateWindow, SlashCommand, Task, TaskState, TurnResult, Usage,
};

/// What a line of output amounts to.
#[derive(Debug, PartialEq)]
pub(crate) enum Parsed {
    /// Something for the caller (a permission request also needs the session's bookkeeping).
    Event(Event),
    /// A control request this crate does not serve (hooks, in-process MCP, ...); it must be refused so the
    /// binary does not wait for an answer.
    Unsupported {
        /// The request to refuse.
        request_id: String,
        /// What it asked for.
        subtype: String,
    },
    /// Nothing to report: a blank or non-JSON line, a successful control response, a host-only frame.
    Nothing,
}

static NULL: Value = Value::Null;

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&NULL)
}

fn opt_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn text(value: &Value, key: &str) -> String {
    opt_text(value, key).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> u64 {
    field(value, key).as_u64().unwrap_or(0)
}

fn flag(value: &Value, key: &str) -> bool {
    field(value, key).as_bool().unwrap_or(false)
}

/// Moves `key` out of `value`, leaving `null`.
fn take(value: &mut Value, key: &str) -> Value {
    value.get_mut(key).map(Value::take).unwrap_or_default()
}

/// Parses one line of the binary's standard output.
pub(crate) fn parse_line(line: &str) -> Parsed {
    let line = line.trim();
    // Some builds print debug text on stdout; like the SDK, only lines that look like JSON objects count.
    if !line.starts_with('{') {
        return Parsed::Nothing;
    }
    let mut data: Value = match serde_json::from_str(line) {
        Ok(data) => data,
        Err(error) => return Parsed::Event(Event::Error(format!("unparseable line ({error}): {}", preview(line)))),
    };
    let kind = text(&data, "type");
    let parent_tool_use_id = opt_text(&data, "parent_tool_use_id");
    Parsed::Event(match kind.as_str() {
        "system" => return system(data),
        "stream_event" => stream_event(data, parent_tool_use_id),
        "assistant" => {
            let error = opt_text(&data, "error");
            let mut message = take(&mut data, "message");
            Event::Assistant {
                model: text(&message, "model"),
                usage: usage(field(&message, "usage")),
                blocks: blocks(take(&mut message, "content")),
                parent_tool_use_id,
                error,
            }
        }
        "user" => Event::User {
            uuid: text(&data, "uuid"),
            replay: flag(&data, "isReplay"),
            blocks: blocks(take(&mut take(&mut data, "message"), "content")),
            parent_tool_use_id,
        },
        "result" => Event::Result(TurnResult {
            subtype: text(&data, "subtype"),
            is_error: flag(&data, "is_error"),
            result: opt_text(&data, "result"),
            session_id: text(&data, "session_id"),
            duration_ms: number(&data, "duration_ms"),
            num_turns: number(&data, "num_turns"),
            total_cost_usd: field(&data, "total_cost_usd").as_f64(),
            usage: usage(field(&data, "usage")),
            permission_denials: field(&data, "permission_denials").as_array().map_or(0, Vec::len),
            context_window: field(&data, "modelUsage")
                .as_object()
                .and_then(|models| models.values().map(|model| number(model, "contextWindow")).max())
                .unwrap_or(0),
            queued_turn_count: number(&data, "queued_turn_count"),
            raw: data,
        }),
        "control_request" => return control_request(&data),
        "control_response" => {
            let response = field(&data, "response");
            if field(response, "subtype").as_str() != Some("error") {
                // Only the answer to `initialize` lists models, only that to `mcp_status` servers, only
                // that to `get_context_usage` categories.
                let answer = field(response, "response");
                return if answer.get("models").is_some() {
                    Parsed::Event(Event::Ready(info(answer)))
                } else if let Some(servers) = answer.get("mcpServers").and_then(Value::as_array) {
                    Parsed::Event(Event::McpStatus(servers.iter().filter_map(mcp_server).collect()))
                } else if let Some(categories) = answer.get("categories").and_then(Value::as_array) {
                    Parsed::Event(Event::ContextUsage(ContextUsage {
                        categories: categories.iter().take(64).filter_map(context_category).collect(),
                        total: number(answer, "totalTokens"),
                        max: number(answer, "maxTokens"),
                    }))
                } else {
                    Parsed::Nothing
                };
            }
            Event::Error(format!(
                "control request {} failed: {}",
                text(response, "request_id"),
                opt_text(response, "error").unwrap_or_else(|| "unknown error".into())
            ))
        }
        "control_cancel_request" => Event::PermissionCancelled { request_id: text(&data, "request_id") },
        "rate_limit_event" => Event::RateLimit(rate_limit(field(&data, "rate_limit_info"))),
        // An empty guess is no guess.
        "prompt_suggestion" => match opt_text(&data, "suggestion").filter(|text| !text.trim().is_empty()) {
            Some(text) => Event::PromptSuggestion(text),
            None => return Parsed::Nothing,
        },
        _ => Event::Other { subtype: text(&data, "subtype"), kind, raw: data },
    })
}

/// One kind of thing the context holds, of the answer to `get_context_usage`; `None` without a name.
fn context_category(category: &Value) -> Option<ContextCategory> {
    let name = text(category, "name");
    if name.is_empty() {
        return None;
    }
    Some(ContextCategory {
        id: text(category, "id"),
        name,
        tokens: number(category, "tokens"),
        kind: text(category, "kind"),
    })
}

/// One server of the answer to `mcp_status`; `None` without a name.
fn mcp_server(server: &Value) -> Option<McpServerStatus> {
    let name = text(server, "name");
    if name.is_empty() {
        return None;
    }
    Some(McpServerStatus {
        name,
        status: text(server, "status"),
        source: opt_text(server, "source").unwrap_or_else(|| text(server, "scope")),
        error: text(server, "error"),
        tools: field(server, "tools").as_array().map_or(0, Vec::len),
    })
}

/// What the answer to `initialize` says, as far as this crate models it.
fn info(answer: &Value) -> Info {
    let models = answer.get("models").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let models = models
        .iter()
        .take(256)
        .map(|model| ModelInfo {
            value: text(model, "value"),
            resolved: text(model, "resolvedModel"),
            display_name: text(model, "displayName"),
            description: text(model, "description"),
            efforts: strings(field(model, "supportedEffortLevels")),
            auto_mode: field(model, "supportsAutoMode").as_bool() == Some(true),
        })
        .filter(|model| !model.value.is_empty())
        .collect();
    let account = field(answer, "account");
    Info {
        models,
        account: Account {
            email: text(account, "email"),
            organization: text(account, "organization"),
            subscription: text(account, "subscriptionType"),
        },
        version: text(answer, "claude_code_version"),
        commands: field(answer, "commands")
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .take(1024)
            .map(|command| SlashCommand {
                name: text(command, "name"),
                description: text(command, "description"),
                argument_hint: text(command, "argumentHint"),
            })
            .filter(|command| !command.name.is_empty())
            .collect(),
        raw: answer.clone(),
    }
}

fn system(data: Value) -> Parsed {
    let subtype = text(&data, "subtype");
    Parsed::Event(match subtype.as_str() {
        "init" => Event::Init(Init {
            session_id: text(&data, "session_id"),
            model: text(&data, "model"),
            cwd: text(&data, "cwd"),
            tools: strings(field(&data, "tools")),
            mcp_servers: field(&data, "mcp_servers")
                .as_array()
                .map(|servers| servers.iter().map(|server| (text(server, "name"), text(server, "status"))).collect())
                .unwrap_or_default(),
            permission_mode: text(&data, "permissionMode"),
            api_key_source: text(&data, "apiKeySource"),
            claude_code_version: text(&data, "claude_code_version"),
            raw: data,
        }),
        "api_retry" => {
            let error = text(&data, "error");
            Event::Retry {
                attempt: number(&data, "attempt"),
                max_retries: number(&data, "max_retries"),
                delay_ms: number(&data, "retry_delay_ms"),
                error: match field(&data, "error_status").as_u64() {
                    Some(status) if error.is_empty() => format!("HTTP {status}"),
                    Some(status) => format!("{error} (HTTP {status})"),
                    None => error,
                },
            }
        }
        "status" => Event::Status(opt_text(&data, "status")),
        "compact_boundary" => {
            let metadata = field(&data, "compact_metadata");
            Event::Compacted {
                manual: field(metadata, "trigger").as_str() == Some("manual"),
                pre_tokens: number(metadata, "pre_tokens"),
                post_tokens: number(metadata, "post_tokens"),
            }
        }
        "task_started" | "task_progress" | "task_notification" => {
            let usage = field(&data, "usage");
            let state = match (subtype.as_str(), field(&data, "status").as_str()) {
                ("task_started", _) => TaskState::Started,
                ("task_progress", _) => TaskState::Running,
                (_, Some("failed")) => TaskState::Failed,
                (_, Some("stopped")) => TaskState::Stopped,
                _ => TaskState::Completed,
            };
            Event::Task(Task {
                tool_use_id: text(&data, "tool_use_id"),
                state,
                description: text(&data, "description"),
                subagent_type: text(&data, "subagent_type"),
                task_type: text(&data, "task_type"),
                last_tool: text(&data, "last_tool_name"),
                tool_uses: number(usage, "tool_uses"),
                total_tokens: number(usage, "total_tokens"),
                duration_ms: number(usage, "duration_ms"),
                summary: text(&data, "summary"),
            })
        }
        // Sent to a host that set CLAUDE_CODE_SDK_READS_SESSION_STATE, as this crate does.
        "session_state_changed" => Event::SessionState(text(&data, "state")),
        _ => Event::Other { kind: "system".into(), subtype, raw: data },
    })
}

fn stream_event(data: Value, parent_tool_use_id: Option<String>) -> Event {
    let event = field(&data, "event");
    match field(event, "type").as_str() {
        Some("message_start") => return Event::MessageStart { parent_tool_use_id },
        Some("content_block_start") if field(field(event, "content_block"), "type").as_str() == Some("thinking") => {
            return Event::ThinkingStarted { parent_tool_use_id };
        }
        _ => {}
    }
    let delta = field(event, "delta");
    match field(delta, "type").as_str() {
        Some("text_delta") => Event::TextDelta { text: text(delta, "text"), parent_tool_use_id },
        Some("thinking_delta") => Event::ThinkingDelta { text: text(delta, "thinking"), parent_tool_use_id },
        _ => Event::Other { kind: "stream_event".into(), subtype: text(event, "type"), raw: data },
    }
}

fn control_request(data: &Value) -> Parsed {
    let request_id = text(data, "request_id");
    let request = field(data, "request");
    let subtype = text(request, "subtype");
    if subtype != "can_use_tool" {
        return Parsed::Unsupported { request_id, subtype };
    }
    Parsed::Event(Event::Permission(PermissionRequest {
        request_id,
        tool_name: text(request, "tool_name"),
        input: field(request, "input").clone(),
        tool_use_id: opt_text(request, "tool_use_id"),
        title: opt_text(request, "title"),
        display_name: opt_text(request, "display_name"),
        description: opt_text(request, "description"),
        decision_reason: opt_text(request, "decision_reason"),
        blocked_path: opt_text(request, "blocked_path"),
        suggestions: field(request, "permission_suggestions").as_array().cloned().unwrap_or_default(),
    }))
}

/// A `rate_limit_info` object.
fn rate_limit(info: &Value) -> RateLimit {
    let windows = field(info, "unifiedWindows").as_object();
    RateLimit {
        status: text(info, "status"),
        kind: text(info, "rateLimitType"),
        resets_at: number(info, "resetsAt"),
        windows: windows
            .into_iter()
            .flatten()
            .take(16)
            .filter_map(|(name, window)| {
                let status = text(window, "status");
                // A window that says no number is full when it is the one that refused, and unknown otherwise:
                // then it is left out, so that what was known of it stands.
                let used = field(window, "utilization").as_f64().or_else(|| (status == "rejected").then_some(1.0))?;
                Some(RateWindow {
                    name: name.clone(),
                    utilization: used.clamp(0.0, 1.0),
                    resets_at: number(window, "resetsAt"),
                    status,
                })
            })
            .collect(),
    }
}

fn usage(value: &Value) -> Usage {
    Usage {
        input_tokens: number(value, "input_tokens"),
        output_tokens: number(value, "output_tokens"),
        cache_read_input_tokens: number(value, "cache_read_input_tokens"),
        cache_creation_input_tokens: number(value, "cache_creation_input_tokens"),
    }
}

/// The strings of an array; anything else is skipped.
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// A message's `content`: plain text or a list of blocks.
fn blocks(content: Value) -> Vec<Block> {
    match content {
        Value::String(text) => vec![Block::Text(text)],
        Value::Array(items) => items.into_iter().map(block).collect(),
        _ => Vec::new(),
    }
}

fn block(mut item: Value) -> Block {
    let kind = text(&item, "type");
    match kind.as_str() {
        "text" => Block::Text(text(&item, "text")),
        "thinking" => Block::Thinking(text(&item, "thinking")),
        "tool_use" => {
            Block::ToolUse { id: text(&item, "id"), name: text(&item, "name"), input: take(&mut item, "input") }
        }
        "tool_result" => Block::ToolResult {
            tool_use_id: text(&item, "tool_use_id"),
            content: flatten(field(&item, "content")),
            is_error: flag(&item, "is_error"),
        },
        _ => Block::Other(item),
    }
}

/// A tool result's content as text: a string as is, a block list as its text parts joined by newlines, with
/// `[type]` standing in for parts that are not text (images, ...).
fn flatten(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match (part.as_str(), opt_text(part, "text")) {
                (Some(text), _) => text.to_owned(),
                (None, Some(text)) => text,
                (None, None) => format!("[{}]", opt_text(part, "type").unwrap_or_else(|| "content".into())),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(line: &str) -> Event {
        match parse_line(line) {
            Parsed::Event(event) => event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn init() {
        let line = r#"{"type":"system","subtype":"init","cwd":"C:\\work","session_id":"s1","tools":["Bash","Read",7],
            "mcp_servers":[{"name":"docs","status":"connected"},{"name":"x"},"junk"],"model":"claude-opus-5-5",
            "permissionMode":"default","apiKeySource":"none","claude_code_version":"2.1.322","slash_commands":[]}"#
            .replace('\n', " ");
        let Event::Init(init) = event(&line) else { panic!("not init") };
        assert_eq!(init.session_id, "s1");
        assert_eq!(init.model, "claude-opus-5-5");
        assert_eq!(init.cwd, "C:\\work");
        assert_eq!(init.tools, ["Bash", "Read"]);
        let servers =
            [("docs".to_owned(), "connected".to_owned()), ("x".to_owned(), String::new()), Default::default()];
        assert_eq!(init.mcp_servers, servers);
        assert_eq!(init.permission_mode, "default");
        assert_eq!(init.api_key_source, "none");
        assert_eq!(init.claude_code_version, "2.1.322");
        assert_eq!(init.raw["slash_commands"], json!([]));
    }

    #[test]
    fn limits_tasks_and_compaction() {
        let limits = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1794333000,
            "rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.1,"resetsAt":1794333000,
            "status":"allowed"},"seven_day":{"utilization":0.58,"resetsAt":1794326400,"status":"allowed"},"odd":7}}}"#;
        let Event::RateLimit(limits) = event(&limits.replace('\n', " ")) else { panic!("not limits") };
        assert_eq!(
            (limits.status.as_str(), limits.kind.as_str(), limits.resets_at),
            ("allowed", "five_hour", 1_794_333_000)
        );
        let windows: Vec<(&str, f64)> = limits.windows.iter().map(|w| (w.name.as_str(), w.utilization)).collect();
        assert_eq!(windows, [("five_hour", 0.1), ("seven_day", 0.58)], "what is no window is left out");
        assert_eq!(event(r#"{"type":"rate_limit_event"}"#), Event::RateLimit(RateLimit::default()));
        // As Claude Code 2.1.322 sends it, a moment after the turn's result.
        let guess =
            r#"{"type":"prompt_suggestion","suggestion":"Show me a full example program","uuid":"u","session_id":"s"}"#;
        assert_eq!(event(guess), Event::PromptSuggestion("Show me a full example program".into()));
        for nothing in
            [r#"{"type":"prompt_suggestion","suggestion":"  "}"#, r#"{"type":"prompt_suggestion","suggestion":7}"#]
        {
            assert!(matches!(parse_line(nothing), Parsed::Nothing), "{nothing}");
        }
        // A window without a number: full when it refused, left out (what was known stands) otherwise.
        let refused = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":9,
            "rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"status":"rejected","resetsAt":9},
            "seven_day":{"status":"allowed"}}}}"#;
        let Event::RateLimit(limits) = event(&refused.replace('\n', " ")) else { panic!("not limits") };
        let windows: Vec<(&str, f64)> = limits.windows.iter().map(|w| (w.name.as_str(), w.utilization)).collect();
        assert_eq!(windows, [("five_hour", 1.0)]);

        let progress = r#"{"type":"system","subtype":"task_progress","tool_use_id":"toolu_1","description":"Finding *",
            "subagent_type":"general-purpose","usage":{"total_tokens":26674,"tool_uses":1,"duration_ms":2350},
            "last_tool_name":"Glob"}"#;
        let Event::Task(task) = event(&progress.replace('\n', " ")) else { panic!("not a task") };
        assert_eq!(
            (task.tool_use_id.as_str(), task.state, task.description.as_str()),
            ("toolu_1", TaskState::Running, "Finding *")
        );
        assert_eq!(
            (task.last_tool.as_str(), task.tool_uses, task.total_tokens, task.duration_ms),
            ("Glob", 1, 26_674, 2_350)
        );
        let ended = |status: &str| {
            let line = format!(
                r#"{{"type":"system","subtype":"task_notification","tool_use_id":"t","status":"{status}","summary":"done"}}"#
            );
            match event(&line) {
                Event::Task(task) => (task.state, task.summary),
                other => panic!("not a task: {other:?}"),
            }
        };
        assert_eq!(ended("completed"), (TaskState::Completed, "done".into()));
        assert_eq!((ended("failed").0, ended("stopped").0), (TaskState::Failed, TaskState::Stopped));
        assert!(matches!(
            event(r#"{"type":"system","subtype":"task_started","tool_use_id":"t"}"#),
            Event::Task(Task { state: TaskState::Started, .. })
        ));

        assert_eq!(
            event(r#"{"type":"system","subtype":"status","status":"compacting"}"#),
            Event::Status(Some("compacting".into()))
        );
        assert_eq!(event(r#"{"type":"system","subtype":"status","status":null}"#), Event::Status(None));
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":39577,"post_tokens":11092}}"#;
        assert_eq!(event(boundary), Event::Compacted { manual: true, pre_tokens: 39_577, post_tokens: 11_092 });

        let result = r#"{"type":"result","subtype":"success","modelUsage":{"a":{"contextWindow":200000},"b":{"contextWindow":1000000},"c":{}}}"#;
        assert!(matches!(event(result), Event::Result(TurnResult { context_window: 1_000_000, .. })));
        let answer = serde_json::json!({"models": [], "commands": [
            {"name": "compact", "description": "Summarise", "argumentHint": "<focus>"}, {"description": "nameless"}, 7]});
        let info = info(&answer);
        assert_eq!(info.commands.len(), 1);
        assert_eq!((info.commands[0].name.as_str(), info.commands[0].argument_hint.as_str()), ("compact", "<focus>"));
    }

    #[test]
    fn init_with_nothing_in_it() {
        let Event::Init(init) = event(r#"{"type":"system","subtype":"init","tools":"all","mcp_servers":{}}"#) else {
            panic!("not init")
        };
        assert_eq!(init.session_id, "");
        assert!(init.tools.is_empty());
        assert!(init.mcp_servers.is_empty());
    }

    #[test]
    fn deltas() {
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"po"}},"parent_tool_use_id":null,"session_id":"s","uuid":"u"}"#;
        assert_eq!(event(&text.replace('\n', " ")), Event::TextDelta { text: "po".into(), parent_tool_use_id: None });
        let thinking = r#"{"type":"stream_event","event":{"type":"content_block_delta",
            "delta":{"type":"thinking_delta","thinking":"hm"}},"parent_tool_use_id":"toolu_1"}"#;
        assert_eq!(
            event(&thinking.replace('\n', " ")),
            Event::ThinkingDelta { text: "hm".into(), parent_tool_use_id: Some("toolu_1".into()) }
        );
    }

    #[test]
    fn starts_of_messages_and_thinking() {
        let start =
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}},"parent_tool_use_id":null}"#;
        assert_eq!(event(start), Event::MessageStart { parent_tool_use_id: None });
        let thinking = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,
            "content_block":{"type":"thinking","thinking":""}},"parent_tool_use_id":"toolu_1"}"#;
        assert_eq!(
            event(&thinking.replace('\n', " ")),
            Event::ThinkingStarted { parent_tool_use_id: Some("toolu_1".into()) }
        );
    }

    #[test]
    fn other_stream_events_pass_through() {
        for line in [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
        ] {
            let Event::Other { kind, subtype, raw } = event(line) else { panic!("not other") };
            assert_eq!(kind, "stream_event");
            assert!(subtype == "content_block_start" || subtype == "content_block_delta");
            assert!(raw.get("event").is_some());
        }
        let Event::Other { kind, subtype, .. } = event(r#"{"type":"stream_event","event":7}"#) else {
            panic!("not other")
        };
        assert_eq!((kind.as_str(), subtype.as_str()), ("stream_event", ""));
    }

    #[test]
    fn assistant() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_1","role":"assistant",
            "content":[{"type":"thinking","thinking":"let me see","signature":"sig"},{"type":"text","text":"pong"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.txt"}},
            {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}, 42],
            "usage":{"input_tokens":3,"output_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":11,
            "service_tier":"standard"}},"parent_tool_use_id":"toolu_0","session_id":"s","uuid":"u"}"#;
        let Event::Assistant { blocks, model, parent_tool_use_id, usage, error } = event(&line.replace('\n', " "))
        else {
            panic!("not assistant")
        };
        assert_eq!(model, "claude-opus-5-5");
        assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_0"));
        assert_eq!(error, None);
        let expected =
            Usage { input_tokens: 3, output_tokens: 5, cache_read_input_tokens: 7, cache_creation_input_tokens: 11 };
        assert_eq!(usage, expected);
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0], Block::Thinking("let me see".into()));
        assert_eq!(blocks[1], Block::Text("pong".into()));
        let tool_use =
            Block::ToolUse { id: "toolu_1".into(), name: "Read".into(), input: json!({"file_path": "a.txt"}) };
        assert_eq!(blocks[2], tool_use);
        assert!(matches!(&blocks[3], Block::Other(raw) if raw["type"] == "server_tool_use"));
        assert_eq!(blocks[4], Block::Other(json!(42)));
    }

    #[test]
    fn assistant_error_and_missing_message() {
        let line = r#"{"type":"assistant","error":"rate_limit","message":{"content":[{"type":"text"}],"usage":"?"}}"#;
        let Event::Assistant { blocks, model, usage, error, .. } = event(line) else { panic!("not assistant") };
        assert_eq!(blocks, [Block::Text(String::new())]);
        assert_eq!((model.as_str(), usage, error.as_deref()), ("", Usage::default(), Some("rate_limit")));

        let Event::Assistant { blocks, error, .. } = event(r#"{"type":"assistant","message":[1,2],"error":{"a":1}}"#)
        else {
            panic!("not assistant")
        };
        assert!(blocks.is_empty());
        assert_eq!(error, None);
    }

    #[test]
    fn user_tool_results() {
        let line = r#"{"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"file body"},
            {"type":"tool_result","tool_use_id":"toolu_2","is_error":true,
             "content":[{"type":"text","text":"line 1"},{"type":"image","source":{}},{"type":"text","text":"line 2"},"bare",{}]},
            {"type":"tool_result","tool_use_id":"toolu_3"},
            {"type":"tool_result","content":{"odd":true}}]},"parent_tool_use_id":null}"#;
        let Event::User { blocks, parent_tool_use_id, replay, .. } = event(&line.replace('\n', " ")) else {
            panic!("not user")
        };
        assert_eq!((parent_tool_use_id, replay), (None, false));
        let result = |id: &str, content: &str, is_error| Block::ToolResult {
            tool_use_id: id.into(),
            content: content.into(),
            is_error,
        };
        assert_eq!(
            blocks,
            [
                result("toolu_1", "file body", false),
                result("toolu_2", "line 1\n[image]\nline 2\nbare\n[content]", true),
                result("toolu_3", "", false),
                result("", r#"{"odd":true}"#, false),
            ]
        );
    }

    #[test]
    fn user_text_prompt() {
        let line =
            r#"{"type":"user","message":{"role":"user","content":"do the thing"},"parent_tool_use_id":"toolu_9"}"#;
        assert_eq!(
            event(line),
            Event::User {
                blocks: vec![Block::Text("do the thing".into())],
                parent_tool_use_id: Some("toolu_9".into()),
                uuid: String::new(),
                replay: false,
            }
        );
        let empty = Event::User { blocks: Vec::new(), parent_tool_use_id: None, uuid: String::new(), replay: false };
        assert_eq!(event(r#"{"type":"user"}"#), empty);
        // The caller's own message, replayed when a turn takes it in, under the id it was sent with.
        let replayed = r#"{"type":"user","message":{"role":"user","content":"Also: check the tests."},
            "parent_tool_use_id":null,"isReplay":true,"uuid":"22222222-2222-4222-8222-222222222222"}"#;
        let Event::User { blocks, uuid, replay, .. } = event(&replayed.replace('\n', " ")) else { panic!("not user") };
        assert_eq!(blocks, [Block::Text("Also: check the tests.".into())]);
        assert_eq!((uuid.as_str(), replay), ("22222222-2222-4222-8222-222222222222", true));
        let odd = event(r#"{"type":"user","isReplay":"yes","uuid":7}"#);
        assert!(matches!(odd, Event::User { replay: false, ref uuid, .. } if uuid.is_empty()));
    }

    #[test]
    fn permission_request() {
        let line = r#"{"type":"control_request","request_id":"cli_7","request":{"subtype":"can_use_tool",
            "tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"toolu_1","title":"Claude wants to run ls",
            "display_name":"Run command","description":"ls","decision_reason":"no rule","blocked_path":"C:\\x",
            "permission_suggestions":[{"type":"addRules","behavior":"allow"}]}}"#;
        let expected = PermissionRequest {
            request_id: "cli_7".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "ls"}),
            tool_use_id: Some("toolu_1".into()),
            title: Some("Claude wants to run ls".into()),
            display_name: Some("Run command".into()),
            description: Some("ls".into()),
            decision_reason: Some("no rule".into()),
            blocked_path: Some("C:\\x".into()),
            suggestions: vec![json!({"type": "addRules", "behavior": "allow"})],
        };
        assert_eq!(event(&line.replace('\n', " ")), Event::Permission(expected));
    }

    #[test]
    fn permission_request_with_only_a_name() {
        let line = r#"{"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Read",
            "permission_suggestions":null,"blocked_path":null}}"#;
        let Event::Permission(request) = event(&line.replace('\n', " ")) else { panic!("not a permission request") };
        assert_eq!(request.input, Value::Null);
        assert_eq!((request.tool_use_id, request.blocked_path, request.suggestions), (None, None, Vec::new()));
    }

    #[test]
    fn other_control_requests_are_unsupported() {
        let hook =
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"hook_callback","callback_id":"h"}}"#;
        assert_eq!(parse_line(hook), Parsed::Unsupported { request_id: "r1".into(), subtype: "hook_callback".into() });
        let mcp = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"mcp_message"}}"#;
        assert_eq!(parse_line(mcp), Parsed::Unsupported { request_id: "r2".into(), subtype: "mcp_message".into() });
        let empty = Parsed::Unsupported { request_id: String::new(), subtype: String::new() };
        assert_eq!(parse_line(r#"{"type":"control_request"}"#), empty);
    }

    #[test]
    fn control_responses() {
        let ok = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{}}}"#;
        assert_eq!(parse_line(ok), Parsed::Nothing);
        let failed =
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"req_2","error":"nope"}}"#;
        assert_eq!(event(failed), Event::Error("control request req_2 failed: nope".into()));
        let vague = r#"{"type":"control_response","response":{"subtype":"error"}}"#;
        assert_eq!(event(vague), Event::Error("control request  failed: unknown error".into()));
        assert_eq!(parse_line(r#"{"type":"control_response"}"#), Parsed::Nothing);
        let ready = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{
            "models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)",
            "description":"Opus 5.5 · Best","supportedEffortLevels":["low","high"],"supportsAutoMode":true},{"value":""},7],
            "account":{"email":"a@b.c","organization":"Org","subscriptionType":"Claude Max"},"claude_code_version":"2.1.0"}}}"#;
        let Event::Ready(info) = event(&ready.replace('\n', " ")) else { panic!("not ready") };
        assert_eq!(info.models.len(), 1, "nameless and mistyped entries are dropped");
        assert_eq!(
            (info.models[0].resolved.as_str(), info.models[0].efforts.len(), info.models[0].auto_mode),
            ("claude-opus-5-5", 2, true)
        );
        assert_eq!(
            (info.account.email.as_str(), info.account.subscription.as_str(), info.version.as_str()),
            ("a@b.c", "Claude Max", "2.1.0")
        );
    }

    #[test]
    fn mcp_status_answers() {
        // As Claude Code 2.1.322 answers: a connector of the account, one to sign in to, one that failed.
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_2","response":{
            "mcpServers":[{"name":"claude.ai Fish.Audio","status":"connected","transport":"claudeai-proxy",
            "scope":"claudeai","source":"claudeai","tools":[{"name":"a"},{"name":"b"}]},
            {"name":"claude.ai Gmail","status":"needs-auth","scope":"claudeai","tools":[]},
            {"name":"files","status":"failed","scope":"user","error":"spawn ENOENT"},{"status":"connected"},7]}}}"#;
        let Event::McpStatus(servers) = event(&line.replace('\n', " ")) else { panic!("not an MCP status") };
        let seen: Vec<(&str, &str, &str, usize)> =
            servers.iter().map(|s| (s.name.as_str(), s.status.as_str(), s.source.as_str(), s.tools)).collect();
        assert_eq!(
            seen,
            [
                ("claude.ai Fish.Audio", "connected", "claudeai", 2),
                ("claude.ai Gmail", "needs-auth", "claudeai", 0),
                ("files", "failed", "user", 0)
            ]
        );
        assert_eq!(servers[2].error, "spawn ENOENT");
        let none = r#"{"type":"control_response","response":{"subtype":"success","response":{"mcpServers":[]}}}"#;
        assert_eq!(event(none), Event::McpStatus(Vec::new()));
    }

    #[test]
    fn context_usage_answers() {
        // As Claude Code 2.1.322 answers `get_context_usage` (shortened: it also sends the grid it draws and
        // what each memory file, skill and MCP tool takes).
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_3","response":{
            "categories":[{"id":"system_prompt","name":"System prompt","tokens":3213,"color":"promptBorder",
            "kind":"used"},{"id":"mcp_tools_deferred","name":"MCP tools (deferred)","tokens":32959,
            "isDeferred":true,"kind":"deferred"},{"tokens":5},"junk",{"id":"messages","name":"Messages",
            "tokens":-4,"kind":"used"}],"totalTokens":24058,"maxTokens":1000000,"gridRows":[[]]}}}"#;
        let Event::ContextUsage(usage) = event(&line.replace('\n', " ")) else { panic!("not a context usage") };
        assert_eq!((usage.total, usage.max), (24_058, 1_000_000));
        let seen: Vec<_> = usage.categories.iter().map(|c| (c.id.as_str(), c.tokens, c.kind.as_str())).collect();
        assert_eq!(
            seen,
            [("system_prompt", 3213, "used"), ("mcp_tools_deferred", 32_959, "deferred"), ("messages", 0, "used")]
        );
        assert_eq!(usage.categories[0].name, "System prompt");
    }

    #[test]
    fn cancel_request() {
        let line = r#"{"type":"control_cancel_request","request_id":"cli_7"}"#;
        assert_eq!(event(line), Event::PermissionCancelled { request_id: "cli_7".into() });
    }

    #[test]
    fn result() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"duration_api_ms":1000,
            "num_turns":1,"result":"pong","session_id":"s1","total_cost_usd":0.0125,
            "usage":{"input_tokens":10,"output_tokens":4},"permission_denials":[{"tool_name":"Bash"},{}],
            "queued_turn_count":1}"#;
        let Event::Result(result) = event(&line.replace('\n', " ")) else { panic!("not a result") };
        assert_eq!(
            (result.subtype.as_str(), result.is_error, result.result.as_deref()),
            ("success", false, Some("pong"))
        );
        assert_eq!((result.session_id.as_str(), result.duration_ms, result.num_turns), ("s1", 1234, 1));
        assert_eq!(result.total_cost_usd, Some(0.0125));
        assert_eq!(result.usage, Usage { input_tokens: 10, output_tokens: 4, ..Usage::default() });
        assert_eq!((result.permission_denials, result.queued_turn_count), (2, 1));
        assert_eq!(result.raw["duration_api_ms"], 1000);
    }

    #[test]
    fn result_with_wrong_types() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"duration_ms":-5,"num_turns":"3",
            "total_cost_usd":"free","usage":[],"permission_denials":9}"#;
        let Event::Result(result) = event(&line.replace('\n', " ")) else { panic!("not a result") };
        assert!(result.is_error);
        assert_eq!((result.duration_ms, result.num_turns, result.permission_denials), (0, 0, 0));
        assert_eq!((result.result, result.total_cost_usd, result.usage), (None, None, Usage::default()));
    }

    #[test]
    fn retry() {
        let line = r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"retry_delay_ms":4000,
            "error_status":529,"error":"overloaded"}"#;
        let expected =
            Event::Retry { attempt: 2, max_retries: 10, delay_ms: 4000, error: "overloaded (HTTP 529)".into() };
        assert_eq!(event(&line.replace('\n', " ")), expected);
        let bare = r#"{"type":"system","subtype":"api_retry","error_status":null,"error":"unknown"}"#;
        assert_eq!(event(bare), Event::Retry { attempt: 0, max_retries: 0, delay_ms: 0, error: "unknown".into() });
        let status_only = r#"{"type":"system","subtype":"api_retry","error_status":500}"#;
        assert!(matches!(event(status_only), Event::Retry { error, .. } if error == "HTTP 500"));
    }

    #[test]
    fn other_system_and_unknown_types() {
        let Event::Other { kind, subtype, raw } = event(r#"{"type":"system","subtype":"hook_started","x":1}"#) else {
            panic!("not other")
        };
        assert_eq!((kind.as_str(), subtype.as_str()), ("system", "hook_started"));
        assert_eq!(raw["x"], 1);

        let Event::Other { kind, subtype, .. } = event(r#"{"type":"auth_status","isAuthenticating":false}"#) else {
            panic!("not other")
        };
        assert_eq!((kind.as_str(), subtype.as_str()), ("auth_status", ""));

        assert!(matches!(event("{}"), Event::Other { kind, .. } if kind.is_empty()));
        assert!(matches!(event(r#"{"type":["system"],"subtype":5}"#), Event::Other { kind, subtype, .. }
            if kind.is_empty() && subtype.is_empty()));
    }

    #[test]
    fn session_state_frames() {
        // As Claude Code 2.1.322 sends them to a host that asked.
        let running = r#"{"type":"system","subtype":"session_state_changed","state":"running","sdk_host_only":true}"#;
        assert_eq!(event(running), Event::SessionState("running".into()));
        let idle = r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#;
        assert_eq!(event(idle), Event::SessionState("idle".into()));
    }

    #[test]
    fn lines_that_are_not_messages() {
        for line in ["", "   \r\n", "[SandboxDebug] starting", "null", "42", "\"text\"", "[1,2]", "\u{feff}{}"] {
            assert_eq!(parse_line(line), Parsed::Nothing, "{line:?}");
        }
    }

    #[test]
    fn hostile_lines_become_errors() {
        let truncated = r#"{"type":"assistant","message":{"content":[{"type":"te"#;
        let deep = format!("{{\"type\":{}", "[".repeat(100_000));
        let long_garbage = format!("{{{}", "\u{1f980}".repeat(10_000));
        for line in
            [truncated, "{\"type\":\"user\"} trailing", "{'type':'user'}", "{\0}", deep.as_str(), long_garbage.as_str()]
        {
            let Event::Error(message) = event(line) else { panic!("not an error: {line:.40}") };
            assert!(message.starts_with("unparseable line"));
            assert!(message.chars().count() < 400, "the echoed line is cut short");
        }
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert!(matches!(event("  {\"type\":\"user\"}\r\n"), Event::User { .. }));
    }
}
