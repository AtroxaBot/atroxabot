//! Turns the lines `codex app-server` writes into requests, answers and [`Event`]s.
//!
//! Nothing here trusts the input: missing or mistyped fields fall back to empty values, unknown messages and
//! items pass through or are dropped, and `serde_json` bounds the nesting depth.

use serde_json::Value;

use crate::{
    Account, ApprovalKind, ApprovalRequest, ChangeKind, Event, FileChange, Item, ItemStatus, Model, PlanStatus,
    PlanStep, Question, QuestionRequest, RateLimits, RateWindow, TokenUsage, TurnStatus,
};

/// What a line of output is.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    /// The answer to a request of ours.
    Response {
        /// The request's id.
        id: u64,
        /// Its result, or why it was refused.
        outcome: Result<Value, Refusal>,
    },
    /// A request of the binary, which waits for an answer.
    Request {
        /// Its id, a string or a number: echoed back as it is.
        id: Value,
        /// What it asks for.
        method: String,
        /// Its parameters.
        params: Value,
    },
    /// News that takes no answer.
    Notification {
        /// What it is about.
        method: String,
        /// Its parameters.
        params: Value,
    },
    /// Nothing to act on: a blank or non-JSON line, or JSON of another shape.
    Nothing,
}

/// Why the binary refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// Its message.
    pub message: String,
    /// The running turn takes no input (it compacts or reviews); the input has to wait for its end.
    pub not_steerable: bool,
}

/// How a request of the binary is answered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    /// With a `decision` of the current protocol: `accept`, `decline`, ...
    Decision,
    /// With the permissions granted, out of these requested ones.
    Permissions(Value),
    /// With a `decision` of the older protocol: `approved`, `denied`, ...
    Review,
    /// With answers to questions.
    Answers,
}

/// What a request of the binary amounts to.
#[derive(Debug, PartialEq)]
pub(crate) enum Asked {
    /// The user decides.
    Approval(ApprovalRequest, Reply),
    /// The user answers.
    Question(QuestionRequest),
    /// An MCP server wants input from the user, which this client does not show: declined at once.
    Elicitation,
    /// Something this client does not do (its own tools, tokens, attestation): refused at once.
    Unsupported,
}

static NULL: Value = Value::Null;

pub(crate) fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&NULL)
}

pub(crate) fn opt_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub(crate) fn text(value: &Value, key: &str) -> String {
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

/// The strings of an array; anything else is skipped.
fn strings(value: &Value) -> Vec<String> {
    list(value).iter().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// The elements of an array; none for anything else.
fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

/// Parses one line of the binary's standard output.
pub(crate) fn parse_line(line: &str) -> Incoming {
    let line = line.trim();
    // Only JSON objects count; whatever else a build may print there is not for us.
    if !line.starts_with('{') {
        return Incoming::Nothing;
    }
    let Ok(mut data) = serde_json::from_str::<Value>(line) else { return Incoming::Nothing };
    let id = take(&mut data, "id");
    if let Some(method) = opt_text(&data, "method") {
        let params = take(&mut data, "params");
        return if id.is_string() || id.is_number() {
            Incoming::Request { id, method, params }
        } else {
            Incoming::Notification { method, params }
        };
    }
    // Our own ids are numbers; an answer to anything else is not ours.
    let Some(id) = id.as_u64() else { return Incoming::Nothing };
    let error = take(&mut data, "error");
    let outcome = if error.is_null() { Ok(take(&mut data, "result")) } else { Err(refusal(&error)) };
    Incoming::Response { id, outcome }
}

/// The `error` of an answer.
fn refusal(error: &Value) -> Refusal {
    let message = match (opt_text(error, "message"), error.as_str()) {
        (Some(message), _) => message,
        (None, Some(message)) => message.to_owned(),
        (None, None) => "unknown error".to_owned(),
    };
    // `turn/steer` says it in its error's data, `turn/start` only in its message.
    let not_steerable = field(field(error, "data"), "codexErrorInfo").get("activeTurnNotSteerable").is_some()
        || message.to_ascii_lowercase().contains("notsteerable");
    Refusal { message, not_steerable }
}

/// A request's id as the key callers name it by.
pub(crate) fn request_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
}

/// The version in the `userAgent` of the answer to `initialize`, e.g. `atroxabot/0.167.1 (Windows …)`: what
/// follows the first slash.
pub(crate) fn version_of(user_agent: &str) -> String {
    let rest = user_agent.split_once('/').map_or("", |(_, rest)| rest);
    rest.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or_default().to_owned()
}

/// What a notification says: the thread it is about (when it names one) and the event. `None` for the many
/// notifications this crate does not model.
pub(crate) fn notification(method: &str, mut params: Value) -> Option<(Option<String>, Event)> {
    let thread = opt_text(&params, "threadId");
    let item_id = text(&params, "itemId");
    let delta = || text(&params, "delta");
    let event = match method {
        // A turn without an id is none: an empty id is how a turn that never began is reported.
        "turn/started" | "turn/completed" if text(field(&params, "turn"), "id").is_empty() => return None,
        "turn/started" => Event::TurnStarted { turn_id: text(field(&params, "turn"), "id") },
        "turn/completed" => turn_completed(field(&params, "turn")),
        "item/started" => match item(take(&mut params, "item")) {
            Item::UserMessage { client_id, text, .. } => Event::UserMessage { client_id, text },
            item => Event::ItemStarted(item),
        },
        "item/completed" => match item(take(&mut params, "item")) {
            // Reported when it started.
            Item::UserMessage { .. } => return None,
            item => Event::ItemCompleted(item),
        },
        "item/agentMessage/delta" => Event::TextDelta { text: delta(), item_id },
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            Event::ReasoningDelta { text: delta(), item_id }
        }
        "item/reasoning/summaryPartAdded" if number(&params, "summaryIndex") > 0 => Event::ReasoningBreak { item_id },
        "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
            Event::OutputDelta { text: delta(), item_id }
        }
        "turn/plan/updated" => Event::Plan {
            explanation: opt_text(&params, "explanation"),
            steps: list(field(&params, "plan"))
                .iter()
                .map(|step| PlanStep {
                    step: text(step, "step"),
                    status: match field(step, "status").as_str() {
                        Some("inProgress") => PlanStatus::InProgress,
                        Some("completed") => PlanStatus::Completed,
                        _ => PlanStatus::Pending,
                    },
                })
                .collect(),
        },
        "thread/tokenUsage/updated" => {
            let usage = field(&params, "tokenUsage");
            Event::TokenUsage {
                last: token_usage(field(usage, "last")),
                total: token_usage(field(usage, "total")),
                context_window: field(usage, "modelContextWindow").as_u64(),
            }
        }
        "account/rateLimits/updated" => Event::RateLimits(rate_limits(field(&params, "rateLimits"))),
        "error" => {
            let error = field(&params, "error");
            let (kind, http_status) = error_info(field(error, "codexErrorInfo"));
            Event::Error {
                message: text(error, "message"),
                details: opt_text(error, "additionalDetails"),
                kind,
                http_status,
                will_retry: flag(&params, "willRetry"),
            }
        }
        "warning" | "guardianWarning" => Event::Warning(text(&params, "message")),
        "configWarning" => Event::Warning(match (text(&params, "summary"), opt_text(&params, "details")) {
            (summary, Some(details)) if !details.is_empty() => format!("{summary}: {details}"),
            (summary, _) => summary,
        }),
        "thread/compacted" => Event::Compacted,
        "model/rerouted" => Event::ModelRerouted { from: text(&params, "fromModel"), to: text(&params, "toModel") },
        "serverRequest/resolved" => Event::RequestResolved { request_id: request_key(field(&params, "requestId")) },
        _ => return None,
    };
    Some((thread, event))
}

/// The `turn` of a `turn/completed`.
fn turn_completed(turn: &Value) -> Event {
    let error = opt_text(field(turn, "error"), "message");
    let status = match field(turn, "status").as_str() {
        Some("completed") => TurnStatus::Completed,
        Some("interrupted") => TurnStatus::Interrupted,
        Some("failed") => TurnStatus::Failed,
        // A status this crate does not know: a turn with an error failed.
        _ if error.is_some() => TurnStatus::Failed,
        _ => TurnStatus::Completed,
    };
    Event::TurnCompleted { turn_id: text(turn, "id"), status, error }
}

/// A `codexErrorInfo`: its name and, where it carries one, the HTTP status. It is a plain name, or an object
/// with the name as its only key.
fn error_info(info: &Value) -> (String, Option<u16>) {
    match info {
        Value::String(name) => (name.clone(), None),
        Value::Object(object) => {
            let Some((name, detail)) = object.iter().next() else { return (String::new(), None) };
            let status = field(detail, "httpStatusCode").as_u64().and_then(|status| u16::try_from(status).ok());
            (name.clone(), status)
        }
        _ => (String::new(), None),
    }
}

fn token_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input: number(usage, "inputTokens"),
        cached_input: number(usage, "cachedInputTokens"),
        cache_write_input: number(usage, "cacheWriteInputTokens"),
        output: number(usage, "outputTokens"),
        reasoning_output: number(usage, "reasoningOutputTokens"),
        total: number(usage, "totalTokens"),
    }
}

/// A `RateLimitSnapshot`.
pub(crate) fn rate_limits(snapshot: &Value) -> RateLimits {
    let window = |key: &str| {
        let window = field(snapshot, key);
        window.is_object().then(|| RateWindow {
            used_percent: field(window, "usedPercent").as_f64().unwrap_or(0.0).clamp(0.0, 100.0),
            window_minutes: field(window, "windowDurationMins").as_u64(),
            resets_at: field(window, "resetsAt").as_u64(),
        })
    };
    RateLimits {
        primary: window("primary"),
        secondary: window("secondary"),
        plan: text(snapshot, "planType"),
        reached: !field(snapshot, "rateLimitReachedType").is_null(),
    }
}

/// The answer to `account/read`: the account, if signed in, and whether a sign-in is needed.
pub(crate) fn account(answer: &Value) -> (Option<Account>, bool) {
    let account = field(answer, "account");
    let account = account.is_object().then(|| Account {
        kind: text(account, "type"),
        email: text(account, "email"),
        plan: text(account, "planType"),
    });
    (account, flag(answer, "requiresOpenaiAuth"))
}

/// One page of the answer to `model/list`: its models, and the cursor of the next page if there is one.
pub(crate) fn models(answer: &Value) -> (Vec<Model>, Option<String>) {
    let models = list(field(answer, "data"))
        .iter()
        .map(|model| Model {
            // `model` is what a turn takes. `id` names the entry, and is the same in every build seen so far.
            id: opt_text(model, "model").filter(|id| !id.is_empty()).unwrap_or_else(|| text(model, "id")),
            name: text(model, "displayName"),
            description: text(model, "description"),
            efforts: list(field(model, "supportedReasoningEfforts"))
                .iter()
                .filter_map(|effort| opt_text(effort, "reasoningEffort"))
                .collect(),
            default_effort: text(model, "defaultReasoningEffort"),
            is_default: flag(model, "isDefault"),
            hidden: flag(model, "hidden"),
        })
        .filter(|model| !model.id.is_empty())
        .collect();
    (models, opt_text(answer, "nextCursor").filter(|cursor| !cursor.is_empty()))
}

fn status(item: &Value) -> ItemStatus {
    match field(item, "status").as_str() {
        Some("completed") => ItemStatus::Completed,
        Some("failed") => ItemStatus::Failed,
        Some("declined") => ItemStatus::Declined,
        Some("interrupted") => ItemStatus::Interrupted,
        _ => ItemStatus::InProgress,
    }
}

/// The text parts (objects of this `kind` with a `text`) of a content list, joined by newlines; `None` when
/// there are none.
fn text_parts(content: &Value, kind: &str) -> Option<String> {
    let parts: Vec<&str> = list(content)
        .iter()
        .filter(|part| field(part, "type").as_str() == Some(kind))
        .filter_map(|part| field(part, "text").as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// A `ThreadItem`.
pub(crate) fn item(mut value: Value) -> Item {
    let id = text(&value, "id");
    let kind = text(&value, "type");
    match kind.as_str() {
        "agentMessage" => Item::AgentMessage { id, text: text(&value, "text"), phase: opt_text(&value, "phase") },
        "reasoning" => Item::Reasoning {
            id,
            summary: strings(field(&value, "summary")),
            content: strings(field(&value, "content")),
        },
        "commandExecution" => Item::Command {
            id,
            command: text(&value, "command"),
            cwd: text(&value, "cwd"),
            status: status(&value),
            output: opt_text(&value, "aggregatedOutput"),
            exit_code: field(&value, "exitCode").as_i64(),
            duration_ms: field(&value, "durationMs").as_u64(),
        },
        "fileChange" => Item::FileChange {
            id,
            status: status(&value),
            changes: list(field(&value, "changes"))
                .iter()
                .map(|change| {
                    let kind = field(change, "kind");
                    FileChange {
                        path: text(change, "path"),
                        kind: match field(kind, "type").as_str() {
                            Some("add") => ChangeKind::Add,
                            Some("delete") => ChangeKind::Delete,
                            _ => ChangeKind::Update,
                        },
                        move_path: opt_text(kind, "move_path"),
                        diff: text(change, "diff"),
                    }
                })
                .collect(),
        },
        "mcpToolCall" => {
            let result = field(&value, "result");
            Item::McpToolCall {
                id,
                server: text(&value, "server"),
                tool: text(&value, "tool"),
                status: status(&value),
                result: text_parts(field(result, "content"), "text")
                    .or_else(|| (!result.is_null()).then(|| result.to_string())),
                error: opt_text(field(&value, "error"), "message"),
                arguments: take(&mut value, "arguments"),
            }
        }
        "dynamicToolCall" => Item::ToolCall {
            id,
            tool: text(&value, "tool"),
            status: status(&value),
            output: text_parts(field(&value, "contentItems"), "inputText"),
            arguments: take(&mut value, "arguments"),
        },
        "webSearch" => {
            let action = take(&mut value, "action");
            Item::WebSearch { id, query: text(&value, "query"), action: (!action.is_null()).then_some(action) }
        }
        "collabAgentToolCall" => Item::Collab {
            id,
            tool: text(&value, "tool"),
            status: status(&value),
            prompt: opt_text(&value, "prompt"),
            receivers: strings(field(&value, "receiverThreadIds")),
            states: field(&value, "agentsStates")
                .as_object()
                .into_iter()
                .flatten()
                .map(|(thread, state)| (thread.clone(), text(state, "status"), opt_text(state, "message")))
                .collect(),
        },
        "plan" => Item::Plan { id, text: text(&value, "text") },
        "imageView" => Item::ImageView { id, path: text(&value, "path") },
        "contextCompaction" => Item::Compaction { id },
        "userMessage" => Item::UserMessage {
            id,
            client_id: opt_text(&value, "clientId"),
            text: text_parts(field(&value, "content"), "text").unwrap_or_default(),
        },
        _ => Item::Other { id, kind, raw: value },
    }
}

/// An approval request. The older protocol (answered with a [`Reply::Review`]) gives the command as its words,
/// and other names to the thread and the item.
fn approval(request_id: String, kind: ApprovalKind, reply: Reply, params: Value) -> Asked {
    let legacy = reply == Reply::Review;
    let command = field(&params, "command");
    let words = || command.is_array().then(|| strings(command).join(" "));
    let request = ApprovalRequest {
        request_id,
        kind,
        item_id: text(&params, if legacy { "callId" } else { "itemId" }),
        thread_id: text(&params, if legacy { "conversationId" } else { "threadId" }),
        command: command.as_str().map(str::to_owned).or_else(words),
        cwd: opt_text(&params, "cwd"),
        reason: opt_text(&params, "reason"),
        grant_root: opt_text(&params, "grantRoot"),
        host: opt_text(field(&params, "networkApprovalContext"), "host"),
        raw: params,
    };
    Asked::Approval(request, reply)
}

/// What a request of the binary amounts to. `request_id` is its id as callers will name it.
pub(crate) fn request(request_id: String, method: &str, params: Value) -> Asked {
    match method {
        "item/commandExecution/requestApproval" => approval(request_id, ApprovalKind::Command, Reply::Decision, params),
        "item/fileChange/requestApproval" => approval(request_id, ApprovalKind::FileChange, Reply::Decision, params),
        "item/permissions/requestApproval" => {
            let wanted = field(&params, "permissions").clone();
            approval(request_id, ApprovalKind::Permissions, Reply::Permissions(wanted), params)
        }
        "execCommandApproval" => approval(request_id, ApprovalKind::Command, Reply::Review, params),
        "applyPatchApproval" => approval(request_id, ApprovalKind::FileChange, Reply::Review, params),
        "item/tool/requestUserInput" => Asked::Question(QuestionRequest {
            request_id,
            item_id: text(&params, "itemId"),
            thread_id: text(&params, "threadId"),
            questions: list(field(&params, "questions"))
                .iter()
                .map(|question| Question {
                    id: text(question, "id"),
                    header: text(question, "header"),
                    question: text(question, "question"),
                    is_other: flag(question, "isOther"),
                    is_secret: flag(question, "isSecret"),
                    options: list(field(question, "options"))
                        .iter()
                        .map(|option| (text(option, "label"), text(option, "description")))
                        .collect(),
                })
                .collect(),
        }),
        "mcpServer/elicitation/request" => Asked::Elicitation,
        _ => Asked::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The event of a notification line, with the thread it names.
    fn note(line: &str) -> (Option<String>, Event) {
        match parse_line(line) {
            Incoming::Notification { method, params } => {
                notification(&method, params).unwrap_or_else(|| panic!("{method} is not modelled"))
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    fn event(line: &str) -> Event {
        note(line).1
    }

    fn asked(line: &str) -> Asked {
        match parse_line(line) {
            Incoming::Request { id, method, params } => request(request_key(&id), &method, params),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn lines_are_told_apart() {
        assert_eq!(
            parse_line(r#"{"id":1,"result":{"userAgent":"x"}}"#),
            Incoming::Response { id: 1, outcome: Ok(json!({"userAgent": "x"})) }
        );
        assert_eq!(
            parse_line(r#"{"error":{"code":-32600,"message":"no active turn to steer"},"id":20}"#),
            Incoming::Response {
                id: 20,
                outcome: Err(Refusal { message: "no active turn to steer".into(), not_steerable: false })
            }
        );
        assert_eq!(
            parse_line(r#"{"method":"warning","params":{"threadId":null,"message":"m"},"emittedAtMs":1794416759497}"#),
            Incoming::Notification { method: "warning".into(), params: json!({"threadId": null, "message": "m"}) }
        );
        assert_eq!(
            parse_line(r#"{"method":"item/tool/call","id":"srv-7","params":{"tool":"x"}}"#),
            Incoming::Request { id: json!("srv-7"), method: "item/tool/call".into(), params: json!({"tool": "x"}) }
        );
        assert_eq!(
            parse_line(r#"{"method":"attestation/generate","id":0}"#),
            Incoming::Request { id: json!(0), method: "attestation/generate".into(), params: Value::Null }
        );
        // An answer without a result is an empty one.
        assert_eq!(parse_line(r#"{"id":4}"#), Incoming::Response { id: 4, outcome: Ok(Value::Null) });
    }

    #[test]
    fn hostile_lines_are_nothing() {
        // Deeper than `serde_json` follows.
        let deep = format!("{{\"a\":{}", "[".repeat(100_000));
        for line in [
            "",
            "   ",
            "WARNING: proceeding",
            "{",
            "{}",
            "[]",
            "null",
            "{\"id\":\"ours-are-numbers\",\"result\":{}}",
            "{\"id\":-1,\"result\":{}}",
            "{\"id\":1.5,\"result\":{}}",
            "{\"id\":{},\"result\":{}}",
            "{\"method\":7,\"id\":null}",
            "{\"method\":\"x\"} trailing",
            deep.as_str(),
        ] {
            assert_eq!(parse_line(line), Incoming::Nothing, "{}", preview(line));
        }
        // A method of the wrong shape with a usable id is at most an answer, never a request.
        assert!(matches!(parse_line(r#"{"method":7,"id":3}"#), Incoming::Response { id: 3, .. }));
        // Ids that are neither text nor a number make a notification, which takes no answer.
        assert!(matches!(parse_line(r#"{"method":"x","id":null}"#), Incoming::Notification { .. }));
        assert!(matches!(parse_line(r#"{"method":"x","id":[1]}"#), Incoming::Notification { .. }));
    }

    fn preview(line: &str) -> String {
        line.chars().take(60).collect()
    }

    #[test]
    fn notifications_of_the_wrong_shape_do_not_panic() {
        let methods = [
            "turn/started",
            "turn/completed",
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/summaryPartAdded",
            "item/reasoning/textDelta",
            "item/commandExecution/outputDelta",
            "item/fileChange/outputDelta",
            "turn/plan/updated",
            "thread/tokenUsage/updated",
            "account/rateLimits/updated",
            "error",
            "warning",
            "configWarning",
            "thread/compacted",
            "model/rerouted",
            "serverRequest/resolved",
            "no/such/method",
        ];
        let shapes = [
            json!(null),
            json!(7),
            json!("text"),
            json!([1, 2]),
            json!({}),
            json!({"threadId": 5, "turn": "x", "item": [], "plan": {}, "tokenUsage": 1, "rateLimits": "y", "error": 3}),
            json!({"item": {"type": "fileChange", "changes": [1, {"kind": 2}], "status": 9}, "delta": {}, "itemId": 1}),
            json!({"item": {"type": "collabAgentToolCall", "agentsStates": {"a": 1}, "receiverThreadIds": "x"}}),
            json!({"item": {"type": "mcpToolCall", "result": {"content": "x"}, "error": "y", "arguments": [[]]}}),
            json!({"error": {"codexErrorInfo": {"x": {"httpStatusCode": 99999}}}, "plan": [1, {"status": 2}]}),
            json!({"error": {"codexErrorInfo": {}}, "rateLimits": {"primary": {"usedPercent": "a"}}}),
        ];
        for method in methods {
            for shape in &shapes {
                let _ = notification(method, shape.clone());
            }
        }
        for shape in shapes {
            for method in ["item/commandExecution/requestApproval", "item/tool/requestUserInput", "execCommandApproval"]
            {
                let _ = request("1".into(), method, shape.clone());
            }
            let _ = (item(shape.clone()), account(&shape), models(&shape), rate_limits(&shape), refusal(&shape));
        }
    }

    #[test]
    fn unknown_notifications_are_not_modelled() {
        for method in ["thread/started", "thread/status/changed", "remoteControl/status/changed", "item/plan/delta", ""]
        {
            assert_eq!(notification(method, json!({"threadId": "t"})), None, "{method}");
        }
        // The first part of a summary starts no new paragraph.
        let first = json!({"threadId": "t", "turnId": "u", "itemId": "r", "summaryIndex": 0});
        assert_eq!(notification("item/reasoning/summaryPartAdded", first), None);
    }

    #[test]
    fn turns() {
        let started = r#"{"method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1","rootTurnId":"u1",
            "items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":1794416752,
            "completedAt":null,"durationMs":null}},"emittedAtMs":1794416752678}"#;
        assert_eq!(note(started), (Some("t1".into()), Event::TurnStarted { turn_id: "u1".into() }));

        let failed = r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","items":[],
            "status":"failed","error":{"message":"unexpected status 401 Unauthorized","codexErrorInfo":
            {"httpConnectionFailed":{"httpStatusCode":401}},"additionalDetails":null,"misalignment":null},
            "durationMs":14780}}}"#;
        let expected = Event::TurnCompleted {
            turn_id: "u1".into(),
            status: TurnStatus::Failed,
            error: Some("unexpected status 401 Unauthorized".into()),
        };
        assert_eq!(event(failed), expected);

        let turn = |status: &str| {
            let params = json!({"threadId": "t1", "turn": {"id": "u2", "status": status, "error": null}});
            match notification("turn/completed", params) {
                Some((_, Event::TurnCompleted { status, error: None, .. })) => status,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(turn("completed"), TurnStatus::Completed);
        assert_eq!(turn("interrupted"), TurnStatus::Interrupted);
        assert_eq!(turn("failed"), TurnStatus::Failed);
        assert_eq!(turn("somethingNew"), TurnStatus::Completed);
    }

    #[test]
    fn user_messages_are_reported_once() {
        let started = r#"{"method":"item/started","params":{"item":{"type":"userMessage","id":"i1",
            "clientId":"client-2","content":[{"type":"text","text":"more","text_elements":[]},
            {"type":"localImage","detail":null,"path":"C:/a.png"},{"type":"text","text":"and this"}],
            "startedAtMs":1794416877592,"completedAtMs":null},"threadId":"t1","turnId":"u1",
            "startedAtMs":1794416877592},"emittedAtMs":1794416877592}"#;
        let expected = Event::UserMessage { client_id: Some("client-2".into()), text: "more\nand this".into() };
        assert_eq!(note(started), (Some("t1".into()), expected));
        let completed = json!({"item": {"type": "userMessage", "id": "i1", "clientId": null, "content": []}});
        assert_eq!(notification("item/completed", completed), None);
    }

    #[test]
    fn deltas() {
        let delta = |method: &str| {
            let params = json!({"threadId": "t1", "turnId": "u1", "itemId": "i7", "delta": "abc", "summaryIndex": 2});
            notification(method, params).map(|(_, event)| event)
        };
        let item_id = || "i7".to_owned();
        let text = || "abc".to_owned();
        assert_eq!(delta("item/agentMessage/delta"), Some(Event::TextDelta { item_id: item_id(), text: text() }));
        for method in ["item/reasoning/summaryTextDelta", "item/reasoning/textDelta"] {
            assert_eq!(delta(method), Some(Event::ReasoningDelta { item_id: item_id(), text: text() }));
        }
        for method in ["item/commandExecution/outputDelta", "item/fileChange/outputDelta"] {
            assert_eq!(delta(method), Some(Event::OutputDelta { item_id: item_id(), text: text() }));
        }
        assert_eq!(delta("item/reasoning/summaryPartAdded"), Some(Event::ReasoningBreak { item_id: item_id() }));
    }

    #[test]
    fn items() {
        let parse = |value: Value| item(value);
        assert_eq!(
            parse(json!({"type": "agentMessage", "id": "a", "text": "pong", "phase": "final_answer",
                "memoryCitation": null, "delivery": null, "questions": null})),
            Item::AgentMessage { id: "a".into(), text: "pong".into(), phase: Some("final_answer".into()) }
        );
        assert_eq!(
            parse(json!({"type": "reasoning", "id": "r", "summary": ["one", "two", 3], "content": []})),
            Item::Reasoning { id: "r".into(), summary: vec!["one".into(), "two".into()], content: Vec::new() }
        );
        assert_eq!(
            parse(json!({"type": "commandExecution", "id": "c", "pluginId": null, "scriptPath": null,
                "command": "cargo test", "cwd": "C:\\work", "processId": "9", "source": "agent",
                "status": "failed", "commandActions": [], "aggregatedOutput": "boom", "exitCode": -1,
                "durationMs": 1234})),
            Item::Command {
                id: "c".into(),
                command: "cargo test".into(),
                cwd: "C:\\work".into(),
                status: ItemStatus::Failed,
                output: Some("boom".into()),
                exit_code: Some(-1),
                duration_ms: Some(1234),
            }
        );
        assert_eq!(
            parse(json!({"type": "commandExecution", "id": "c", "command": "ls", "cwd": "/w", "status": "inProgress",
                "aggregatedOutput": null, "exitCode": null, "durationMs": null})),
            Item::Command {
                id: "c".into(),
                command: "ls".into(),
                cwd: "/w".into(),
                status: ItemStatus::InProgress,
                output: None,
                exit_code: None,
                duration_ms: None,
            }
        );
        assert_eq!(
            parse(json!({"type": "fileChange", "id": "f", "status": "declined", "changes": [
                {"path": "a.rs", "kind": {"type": "add"}, "diff": "fn a() {}"},
                {"path": "b.rs", "kind": {"type": "delete"}, "diff": ""},
                {"path": "c.rs", "kind": {"type": "update", "move_path": "d.rs"}, "diff": "@@ -1 +1 @@"},
                {"path": "e.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@"},
            ]})),
            Item::FileChange {
                id: "f".into(),
                status: ItemStatus::Declined,
                changes: vec![
                    FileChange {
                        path: "a.rs".into(),
                        kind: ChangeKind::Add,
                        move_path: None,
                        diff: "fn a() {}".into()
                    },
                    FileChange { path: "b.rs".into(), kind: ChangeKind::Delete, move_path: None, diff: String::new() },
                    FileChange {
                        path: "c.rs".into(),
                        kind: ChangeKind::Update,
                        move_path: Some("d.rs".into()),
                        diff: "@@ -1 +1 @@".into()
                    },
                    FileChange { path: "e.rs".into(), kind: ChangeKind::Update, move_path: None, diff: "@@".into() },
                ],
            }
        );
        assert_eq!(
            parse(json!({"type": "webSearch", "id": "w", "query": "rust wgpu",
                "action": {"type": "search", "query": "rust wgpu"}, "results": null})),
            Item::WebSearch {
                id: "w".into(),
                query: "rust wgpu".into(),
                action: Some(json!({"type": "search", "query": "rust wgpu"}))
            }
        );
        assert_eq!(
            parse(json!({"type": "webSearch", "id": "w", "query": "q", "action": null})),
            Item::WebSearch { id: "w".into(), query: "q".into(), action: None }
        );
        assert_eq!(
            parse(json!({"type": "plan", "id": "p", "text": "1. do"})),
            Item::Plan { id: "p".into(), text: "1. do".into() }
        );
        assert_eq!(
            parse(json!({"type": "imageView", "id": "v", "path": "C:\\a.png"})),
            Item::ImageView { id: "v".into(), path: "C:\\a.png".into() }
        );
        assert_eq!(
            parse(json!({"type": "contextCompaction", "id": "k", "startedAtMs": 1, "completedAtMs": null})),
            Item::Compaction { id: "k".into() }
        );
        assert_eq!(
            parse(
                json!({"type": "userMessage", "id": "u", "clientId": null, "content": [{"type": "text", "text": "hi"}]})
            ),
            Item::UserMessage { id: "u".into(), client_id: None, text: "hi".into() }
        );
        let sleep = json!({"type": "sleep", "id": "s", "durationMs": 500});
        assert_eq!(parse(sleep.clone()), Item::Other { id: "s".into(), kind: "sleep".into(), raw: sleep });
        assert_eq!(parse(json!("junk")), Item::Other { id: String::new(), kind: String::new(), raw: json!("junk") });
    }

    #[test]
    fn tool_call_items() {
        let mcp = |result: Value, error: Value| {
            item(json!({"type": "mcpToolCall", "id": "m", "server": "docs", "tool": "search", "status": "completed",
                "arguments": {"q": "x"}, "appContext": null, "mcpAppUi": null, "pluginId": null,
                "readOnlyHint": true, "result": result, "error": error, "durationMs": 12}))
        };
        let expect = |result: Option<&str>, error: Option<&str>| Item::McpToolCall {
            id: "m".into(),
            server: "docs".into(),
            tool: "search".into(),
            arguments: json!({"q": "x"}),
            status: ItemStatus::Completed,
            result: result.map(str::to_owned),
            error: error.map(str::to_owned),
        };
        let content =
            json!([{"type": "text", "text": "one"}, {"type": "image", "data": "…"}, {"type": "text", "text": "two"}]);
        let texts = json!({"content": content, "structuredContent": null, "_meta": null});
        assert_eq!(mcp(texts, Value::Null), expect(Some("one\ntwo"), None));
        let structured = json!({"content": [], "structuredContent": {"n": 1}, "_meta": null});
        assert_eq!(mcp(structured.clone(), Value::Null), expect(Some(&structured.to_string()), None));
        assert_eq!(mcp(Value::Null, json!({"message": "timed out"})), expect(None, Some("timed out")));

        let dynamic = item(json!({"type": "dynamicToolCall", "id": "d", "namespace": null, "tool": "lookup",
            "arguments": [1], "status": "failed", "success": false, "durationMs": null, "contentItems": [
                {"type": "inputText", "text": "no"}, {"type": "inputImage", "imageUrl": "data:…"}]}));
        let expected = Item::ToolCall {
            id: "d".into(),
            tool: "lookup".into(),
            arguments: json!([1]),
            status: ItemStatus::Failed,
            output: Some("no".into()),
        };
        assert_eq!(dynamic, expected);

        let collab = item(json!({"type": "collabAgentToolCall", "id": "g", "tool": "spawnAgent",
            "status": "interrupted", "senderThreadId": "t1", "receiverThreadIds": ["t2"], "prompt": "look around",
            "model": null, "reasoningEffort": null,
            "agentsStates": {"t2": {"status": "running", "message": null},
                "t3": {"status": "errored", "message": "x"}}}));
        let expected = Item::Collab {
            id: "g".into(),
            tool: "spawnAgent".into(),
            status: ItemStatus::Interrupted,
            prompt: Some("look around".into()),
            receivers: vec!["t2".into()],
            states: vec![("t2".into(), "running".into(), None), ("t3".into(), "errored".into(), Some("x".into()))],
        };
        assert_eq!(collab, expected);
    }

    #[test]
    fn items_arrive_started_and_completed() {
        let line = |method: &str| {
            format!(
                r#"{{"method":"{method}","params":{{"item":{{"type":"contextCompaction","id":"k1","startedAtMs":1,
                "completedAtMs":null}},"threadId":"t1","turnId":"u1","startedAtMs":1}},"emittedAtMs":2}}"#
            )
        };
        let item = || Item::Compaction { id: "k1".into() };
        assert_eq!(note(&line("item/started")), (Some("t1".into()), Event::ItemStarted(item())));
        assert_eq!(note(&line("item/completed")), (Some("t1".into()), Event::ItemCompleted(item())));
        assert_eq!(item().id(), "k1");
    }

    #[test]
    fn plan_and_usage() {
        let plan = r#"{"method":"turn/plan/updated","params":{"threadId":"t","turnId":"u","explanation":null,
            "plan":[{"step":"read","status":"completed"},{"step":"write","status":"inProgress"},
            {"step":"test","status":"pending"},{"step":"odd","status":"??"}]}}"#;
        let step = |step: &str, status| PlanStep { step: step.into(), status };
        let expected = Event::Plan {
            explanation: None,
            steps: vec![
                step("read", PlanStatus::Completed),
                step("write", PlanStatus::InProgress),
                step("test", PlanStatus::Pending),
                step("odd", PlanStatus::Pending),
            ],
        };
        assert_eq!(event(plan), expected);

        let usage = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t","turnId":"u","tokenUsage":{
            "total":{"totalTokens":1200,"inputTokens":1000,"cachedInputTokens":800,"cacheWriteInputTokens":50,
            "outputTokens":200,"reasoningOutputTokens":120},
            "last":{"totalTokens":300,"inputTokens":250,"cachedInputTokens":200,"cacheWriteInputTokens":0,
            "outputTokens":50,"reasoningOutputTokens":30},"modelContextWindow":272000}}}"#;
        let expected = Event::TokenUsage {
            last: TokenUsage {
                input: 250,
                cached_input: 200,
                cache_write_input: 0,
                output: 50,
                reasoning_output: 30,
                total: 300,
            },
            total: TokenUsage {
                input: 1000,
                cached_input: 800,
                cache_write_input: 50,
                output: 200,
                reasoning_output: 120,
                total: 1200,
            },
            context_window: Some(272_000),
        };
        assert_eq!(event(usage), expected);
    }

    #[test]
    fn errors_and_warnings() {
        let retry = r#"{"method":"error","params":{"error":{"message":"Reconnecting... 2/5","codexErrorInfo":
            {"responseStreamDisconnected":{"httpStatusCode":401}},"additionalDetails":"unexpected status 401",
            "misalignment":null},"willRetry":true,"threadId":"t1","turnId":"u1"},"emittedAtMs":1794416753152}"#;
        let expected = Event::Error {
            message: "Reconnecting... 2/5".into(),
            details: Some("unexpected status 401".into()),
            kind: "responseStreamDisconnected".into(),
            http_status: Some(401),
            will_retry: true,
        };
        assert_eq!(note(retry), (Some("t1".into()), expected));

        let error = json!({"message": "limit", "codexErrorInfo": "usageLimitExceeded", "additionalDetails": null});
        let plain = json!({"error": error, "willRetry": false, "threadId": "t1", "turnId": "u1"});
        let expected = Event::Error {
            message: "limit".into(),
            details: None,
            kind: "usageLimitExceeded".into(),
            http_status: None,
            will_retry: false,
        };
        assert_eq!(notification("error", plain).unwrap().1, expected);

        assert_eq!(error_info(&json!(null)), (String::new(), None));
        assert_eq!(
            error_info(&json!({"httpConnectionFailed": {"httpStatusCode": null}})),
            ("httpConnectionFailed".into(), None)
        );
        assert_eq!(error_info(&json!({"activeTurnNotSteerable": {"turnKind": "compact"}})).0, "activeTurnNotSteerable");

        let warning = r#"{"method":"warning","params":{"threadId":"t1","message":"Falling back to HTTPS"}}"#;
        assert_eq!(note(warning), (Some("t1".into()), Event::Warning("Falling back to HTTPS".into())));
        let config = json!({"summary": "bad config", "details": "line 3", "path": "C:\\config.toml"});
        assert_eq!(notification("configWarning", config), Some((None, Event::Warning("bad config: line 3".into()))));
        let config = json!({"summary": "bad config", "details": null});
        assert_eq!(notification("configWarning", config), Some((None, Event::Warning("bad config".into()))));
    }

    #[test]
    fn the_rest_of_the_notifications() {
        let params = json!({"threadId": "t1", "turnId": "u1"});
        assert_eq!(notification("thread/compacted", params), Some((Some("t1".into()), Event::Compacted)));
        let params = json!({"threadId": "t1", "turnId": "u1", "fromModel": "a", "toModel": "b", "reason": "x"});
        let expected = Event::ModelRerouted { from: "a".into(), to: "b".into() };
        assert_eq!(notification("model/rerouted", params), Some((Some("t1".into()), expected)));
        for (id, key) in [(json!(12), "12"), (json!("srv-12"), "srv-12")] {
            let params = json!({"threadId": "t1", "requestId": id});
            let expected = Event::RequestResolved { request_id: key.into() };
            assert_eq!(notification("serverRequest/resolved", params), Some((Some("t1".into()), expected)));
        }
    }

    #[test]
    fn rate_limits_and_account() {
        let snapshot = json!({"limitId": "codex", "limitName": null, "primary": {"usedPercent": 42.5,
            "windowDurationMins": 300, "resetsAt": 1794420000}, "secondary": {"usedPercent": 150,
            "windowDurationMins": null, "resetsAt": null}, "credits": null, "planType": "plus",
            "rateLimitReachedType": null});
        let expected = RateLimits {
            primary: Some(RateWindow { used_percent: 42.5, window_minutes: Some(300), resets_at: Some(1_794_420_000) }),
            secondary: Some(RateWindow { used_percent: 100.0, window_minutes: None, resets_at: None }),
            plan: "plus".into(),
            reached: false,
        };
        assert_eq!(rate_limits(&snapshot), expected);
        let update = json!({"rateLimits": {"primary": null, "secondary": null, "planType": null,
            "rateLimitReachedType": "rate_limit_reached"}});
        let expected = RateLimits { primary: None, secondary: None, plan: String::new(), reached: true };
        assert_eq!(notification("account/rateLimits/updated", update), Some((None, Event::RateLimits(expected))));

        assert_eq!(
            account(&json!({"account": null, "requiresOpenaiAuth": true, "workspaceRouting": null})),
            (None, true)
        );
        let chatgpt = json!({"account": {"type": "chatgpt", "email": "a@b.c", "planType": "pro", "workspaceName": null},
            "requiresOpenaiAuth": true});
        let expected = Account { kind: "chatgpt".into(), email: "a@b.c".into(), plan: "pro".into() };
        assert_eq!(account(&chatgpt), (Some(expected), true));
        let key = json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": false});
        assert_eq!(account(&key), (Some(Account { kind: "apiKey".into(), ..Account::default() }), false));
    }

    #[test]
    fn model_pages() {
        let page = json!({"data": [
            {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1 Sol", "description": "Frontier",
             "hidden": false, "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "fast"},
             {"reasoningEffort": "high", "description": "deep"}, "junk"], "defaultReasoningEffort": "medium",
             "isDefault": true},
            {"id": "old", "displayName": "Old", "hidden": true},
            {"displayName": "nameless"},
            7,
        ], "nextCursor": "page-2"});
        let (models, next) = models(&page);
        let first = Model {
            id: "gpt-6.1-sol".into(),
            name: "GPT-6.1 Sol".into(),
            description: "Frontier".into(),
            efforts: vec!["low".into(), "high".into()],
            default_effort: "medium".into(),
            is_default: true,
            hidden: false,
        };
        let second = Model { id: "old".into(), name: "Old".into(), hidden: true, ..Model::default() };
        assert_eq!(models, [first, second]);
        assert_eq!(next.as_deref(), Some("page-2"));
        assert_eq!(super::models(&json!({"data": [], "nextCursor": null})), (Vec::new(), None));
        assert_eq!(super::models(&json!({"data": [], "nextCursor": ""})), (Vec::new(), None));
    }

    #[test]
    fn versions_come_from_the_user_agent() {
        let agent = "atroxabot/0.167.1 (Windows 10.0.26200; x86_64) xterm-256color (atroxabot; 0.1.0)";
        assert_eq!(version_of(agent), "0.167.1");
        assert_eq!(version_of("x/1.2.3"), "1.2.3");
        assert_eq!(version_of("x/1.2.3(y)"), "1.2.3");
        assert_eq!(version_of("no version"), "");
        assert_eq!(version_of(""), "");
    }

    #[test]
    fn refusals_say_when_a_turn_takes_no_input() {
        let steer = json!({"code": -32600, "data": {"message": "cannot steer a compact turn", "codexErrorInfo":
            {"activeTurnNotSteerable": {"turnKind": "compact"}}, "additionalDetails": null},
            "message": "cannot steer a compact turn"});
        assert_eq!(refusal(&steer), Refusal { message: "cannot steer a compact turn".into(), not_steerable: true });
        let start = json!({"code": -32603,
            "message": "failed to submit turn input: ActiveTurnNotSteerable { turn_kind: Compact }"});
        assert!(refusal(&start).not_steerable);
        assert!(!refusal(&json!({"code": -32600, "message": "no active turn to steer"})).not_steerable);
        assert_eq!(refusal(&json!("plain")).message, "plain");
        assert_eq!(refusal(&json!({"code": 1})).message, "unknown error");
    }

    #[test]
    fn command_approvals() {
        let line = r#"{"method":"item/commandExecution/requestApproval","id":7,"params":{"kind":"command",
            "threadId":"t1","turnId":"u1","itemId":"i1","startedAtMs":1,"approvalId":null,"environmentId":"local",
            "reason":"needs the network","networkApprovalContext":{"host":"crates.io","protocol":"https"},
            "command":"cargo fetch","cwd":"C:\\work","commandActions":[]}}"#;
        let Asked::Approval(request, reply) = asked(line) else { panic!("not an approval") };
        assert_eq!(reply, Reply::Decision);
        assert_eq!(request.request_id, "7");
        assert_eq!(request.kind, ApprovalKind::Command);
        assert_eq!((request.item_id.as_str(), request.thread_id.as_str()), ("i1", "t1"));
        assert_eq!(request.command.as_deref(), Some("cargo fetch"));
        assert_eq!(request.cwd.as_deref(), Some("C:\\work"));
        assert_eq!(request.reason.as_deref(), Some("needs the network"));
        assert_eq!(request.host.as_deref(), Some("crates.io"));
        assert_eq!(request.grant_root, None);
        assert_eq!(request.raw["kind"], "command");
    }

    #[test]
    fn file_and_permission_approvals() {
        let line = r#"{"method":"item/fileChange/requestApproval","id":"req-8","params":{"threadId":"t2",
            "turnId":"u1","itemId":"i2","startedAtMs":1,"reason":null,"grantRoot":"C:\\other"}}"#;
        let Asked::Approval(request, reply) = asked(line) else { panic!("not an approval") };
        assert_eq!((reply, request.kind), (Reply::Decision, ApprovalKind::FileChange));
        assert_eq!(request.request_id, "req-8");
        assert_eq!((request.item_id.as_str(), request.thread_id.as_str()), ("i2", "t2"));
        assert_eq!(request.grant_root.as_deref(), Some("C:\\other"));
        assert_eq!((request.command, request.cwd, request.reason, request.host), (None, None, None, None));

        let wanted = json!({"network": {"enabled": true}, "fileSystem": null});
        let line = json!({"method": "item/permissions/requestApproval", "id": 9, "params": {"threadId": "t1",
            "turnId": "u1", "itemId": "i3", "environmentId": null, "startedAtMs": 1, "cwd": "/w",
            "reason": "to download", "permissions": wanted}});
        let Asked::Approval(request, reply) = asked(&line.to_string()) else { panic!("not an approval") };
        assert_eq!(reply, Reply::Permissions(wanted.clone()));
        assert_eq!(request.kind, ApprovalKind::Permissions);
        assert_eq!(request.cwd.as_deref(), Some("/w"));
        assert_eq!(request.reason.as_deref(), Some("to download"));
        assert_eq!(request.raw["permissions"], wanted);
    }

    #[test]
    fn approvals_of_the_older_protocol() {
        let line = r#"{"method":"execCommandApproval","id":3,"params":{"conversationId":"t1","callId":"call-1",
            "approvalId":null,"command":["git","push"],"cwd":"/w","reason":null,"parsedCmd":[]}}"#;
        let Asked::Approval(request, reply) = asked(line) else { panic!("not an approval") };
        assert_eq!((reply, request.kind), (Reply::Review, ApprovalKind::Command));
        assert_eq!((request.item_id.as_str(), request.thread_id.as_str()), ("call-1", "t1"));
        assert_eq!(request.command.as_deref(), Some("git push"));

        let line = r#"{"method":"applyPatchApproval","id":4,"params":{"conversationId":"t1","callId":"call-2",
            "fileChanges":{"a.rs":{"type":"add","content":"x"}},"reason":"outside","grantRoot":"/other"}}"#;
        let Asked::Approval(request, reply) = asked(line) else { panic!("not an approval") };
        assert_eq!((reply, request.kind), (Reply::Review, ApprovalKind::FileChange));
        assert_eq!(request.item_id, "call-2");
        assert_eq!(request.grant_root.as_deref(), Some("/other"));
        assert_eq!(request.raw["fileChanges"]["a.rs"]["type"], "add");
    }

    #[test]
    fn questions() {
        let line = r#"{"method":"item/tool/requestUserInput","id":11,"params":{"threadId":"t1","turnId":"u1",
            "itemId":"i9","isBlocking":true,"autoResolutionMs":null,"questions":[
            {"id":"db","header":"Database","question":"Which one?","isOther":true,"isSecret":false,
             "options":[{"label":"SQLite","description":"a file"},{"label":"Postgres","description":"a server"}]},
            {"id":"key","header":"Key","question":"The key?","isOther":false,"isSecret":true,"options":null}]}}"#;
        let Asked::Question(request) = asked(line) else { panic!("not a question") };
        assert_eq!(
            (request.request_id.as_str(), request.item_id.as_str(), request.thread_id.as_str()),
            ("11", "i9", "t1")
        );
        let first = Question {
            id: "db".into(),
            header: "Database".into(),
            question: "Which one?".into(),
            is_other: true,
            is_secret: false,
            options: vec![("SQLite".into(), "a file".into()), ("Postgres".into(), "a server".into())],
        };
        let second = Question {
            id: "key".into(),
            header: "Key".into(),
            question: "The key?".into(),
            is_other: false,
            is_secret: true,
            options: Vec::new(),
        };
        assert_eq!(request.questions, [first, second]);
    }

    #[test]
    fn requests_this_client_does_not_serve() {
        let elicitation = r#"{"method":"mcpServer/elicitation/request","id":5,"params":{"threadId":"t1",
            "turnId":null,"serverName":"docs","mode":"form","_meta":null,"message":"Name?","requestedSchema":{}}}"#;
        assert_eq!(asked(elicitation), Asked::Elicitation);
        for method in
            ["item/tool/call", "account/chatgptAuthTokens/refresh", "attestation/generate", "some/new/request"]
        {
            let line = json!({"method": method, "id": 6, "params": {"reason": "unauthorized"}});
            assert_eq!(asked(&line.to_string()), Asked::Unsupported, "{method}");
        }
    }
}
