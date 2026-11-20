//! Reading what the binary writes: JSON-RPC messages, a line each, and the pieces of the Agent Client Protocol
//! in them. Everything here takes whatever arrives: what is missing or of another shape reads as empty.

use serde_json::Value;

use crate::{
    ApprovalRequest, Catalog, Diff, Effort, Model, PlanStatus, PlanStep, QuestionRequest, SlashCommand, TokenUsage,
    Tool, ToolKind, ToolStatus,
};

/// What the binary prints, outside the protocol, when it waits for a sign-in in the browser.
const SIGN_IN_LINK: &str = "Open the following link to authenticate the ACP server: ";
/// What the binary answers a sign-in with when it has none and no browser to ask in.
const NO_BROWSER: &str = "No browser is available here";
/// The protocol's code for "sign in first".
const AUTH_REQUIRED: i64 = -32000;
/// JSON-RPC's code for a failure inside the receiver.
const INTERNAL_ERROR: i64 = -32603;
/// The most of a tool's output that is kept, in bytes: its end.
const OUTPUT_KEPT: usize = 256 * 1024;
/// What stands where the beginning of a long output was.
const CUT: &str = "[earlier output cut]\n";
/// What the binary calls the tool calls that have other agents work. It reports them as ordinary tools.
const SUBAGENT_TITLES: [&str; 5] = [
    "Running start_subagent",
    "Run start_subagent?",
    "Running invoke_subagent",
    "Run invoke_subagent?",
    "Run invoke_subagent",
];
/// What begins a command's output where the binary puts when it ran and how it ended before what it printed.
const RAN_AT: &str = "Created At:";
/// What stands before what the command printed, there.
const PRINTED: &str = "\nCommand output:\n";
/// The ids of the requests that are a question of the agent, not a tool call to approve, begin with this.
const QUESTION_PREFIX: &str = "interaction_";

/// Why the binary refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// JSON-RPC's error code.
    pub code: i64,
    /// What it says.
    pub message: String,
}

impl Refusal {
    /// Whether it says that the binary is not signed in.
    pub(crate) fn needs_sign_in(&self) -> bool {
        self.code == AUTH_REQUIRED || (self.code == INTERNAL_ERROR && self.message.starts_with(NO_BROWSER))
    }
}

/// One line of the binary's output.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    /// Nothing of the protocol: an empty line, a log line, a message of no known shape.
    Nothing,
    /// The binary waits for the user to sign in in a browser.
    SignInLink,
    /// The answer to a request of ours.
    Response {
        /// The request's id.
        id: u64,
        /// Its result, or why it was refused.
        outcome: Result<Value, Refusal>,
    },
    /// Something the binary tells.
    Notification {
        /// What about.
        method: String,
        /// The details.
        params: Value,
    },
    /// Something the binary asks.
    Request {
        /// The id to answer under.
        id: Value,
        /// What it asks.
        method: String,
        /// The details.
        params: Value,
    },
}

/// Reads one line of the binary's output.
pub(crate) fn parse_line(line: &str) -> Incoming {
    let line = line.trim();
    if line.starts_with(SIGN_IN_LINK) {
        return Incoming::SignInLink;
    }
    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(line) else { return Incoming::Nothing };
    let id = message.remove("id").filter(|id| !id.is_null());
    let method = match message.remove("method") {
        Some(Value::String(method)) => Some(method),
        _ => None,
    };
    let params = message.remove("params").unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request { id, method, params },
        (Some(method), None) => Incoming::Notification { method, params },
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return Incoming::Nothing };
            let outcome = match message.remove("error") {
                Some(error) if !error.is_null() => Err(refusal(&error)),
                _ => Ok(message.remove("result").unwrap_or(Value::Null)),
            };
            Incoming::Response { id, outcome }
        }
        (None, None) => Incoming::Nothing,
    }
}

/// Whether a line of the binary's standard error says that it waits for a sign-in in the browser.
pub(crate) fn is_sign_in_link(line: &str) -> bool {
    line.trim_start().starts_with(SIGN_IN_LINK)
}

fn refusal(error: &Value) -> Refusal {
    let code = field(error, "code").as_i64().unwrap_or_default();
    let said = text(error, "message");
    // The binary puts what went wrong behind a general message.
    let details = match field(error, "data") {
        Value::String(details) => details.clone(),
        data => text(data, "details"),
    };
    let message = match (said.trim(), details.trim()) {
        ("", "") => format!("error {code}"),
        (said, "") => said.to_owned(),
        ("", details) => details.to_owned(),
        (said, details) if details.contains(said) => details.to_owned(),
        (said, details) => format!("{said}: {details}"),
    };
    Refusal { code, message }
}

/// The member `key` of `value`; `null` when there is none.
pub(crate) fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// The string `key` of `value`; empty when there is none.
pub(crate) fn text(value: &Value, key: &str) -> String {
    field(value, key).as_str().unwrap_or_default().to_owned()
}

/// The string `key` of `value`, when there is one and it says something.
pub(crate) fn opt_text(value: &Value, key: &str) -> Option<String> {
    field(value, key).as_str().filter(|text| !text.trim().is_empty()).map(str::to_owned)
}

/// The items of the array `key` of `value`; none when it is no array.
pub(crate) fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    field(value, key).as_array().map_or(&[], Vec::as_slice)
}

/// The first of `keys` that `value` has as a string that says something.
fn first_text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| opt_text(value, key))
}

/// What the handshake says the binary can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Abilities {
    /// The binary's version.
    pub version: String,
    /// It continues a session without telling its history again (`session/resume`).
    pub resume: bool,
    /// It continues a session by telling its history again (`session/load`).
    pub load: bool,
    /// It takes pictures in a message.
    pub images: bool,
    /// It takes MCP servers reached over HTTP.
    pub http_mcp: bool,
    /// It takes instructions on top of its own system prompt.
    pub instructions: bool,
}

/// Reads the answer to `initialize`.
pub(crate) fn abilities(answer: &Value) -> Abilities {
    let can = field(answer, "agentCapabilities");
    let flag = |value: &Value, key: &str| field(value, key).as_bool() == Some(true);
    let prompt = field(field(field(can, "_meta"), "antigravity"), "systemPrompt");
    Abilities {
        version: crate::cli::parse_version(&text(field(answer, "agentInfo"), "version")).unwrap_or_default(),
        resume: !field(field(can, "sessionCapabilities"), "resume").is_null(),
        load: flag(can, "loadSession"),
        images: flag(field(can, "promptCapabilities"), "image"),
        http_mcp: flag(field(can, "mcpCapabilities"), "http"),
        instructions: flag(prompt, "append"),
    }
}

/// How the session's model and level of thinking are set: the protocol has had more than one way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The models and levels on offer and in use.
    pub catalog: Catalog,
    /// The id of the option that sets the model; `None` where the binary only takes `session/set_model`.
    pub model_option: Option<String>,
    /// The id of the option that sets the level of thinking; `None` when the model in use has no levels.
    pub effort_option: Option<String>,
    /// The id of the option that sets the mode; `None` where the binary only takes `session/set_mode`.
    pub mode_option: Option<String>,
    /// The mode the session is in; empty when the binary does not say.
    pub mode: String,
    /// The modes on offer.
    pub modes: Vec<String>,
}

impl Settings {
    /// Reads what a session was opened with: the answer to `session/new` or `session/resume`.
    pub(crate) fn of(setup: &Value) -> Self {
        let mut settings = Self::default();
        let legacy = field(setup, "models");
        settings.catalog.model = text(legacy, "currentModelId");
        for model in items(legacy, "availableModels") {
            push_model(&mut settings.catalog.models, &text(model, "modelId"), model);
        }
        let modes = field(setup, "modes");
        settings.mode = text(modes, "currentModeId");
        settings.modes = items(modes, "availableModes").iter().filter_map(|mode| opt_text(mode, "id")).collect();
        settings.update(field(setup, "configOptions"));
        settings
    }

    /// Takes the options the binary reports: when a session opens, when one of them is set, and whenever they
    /// change. `options` is the whole list; anything else changes nothing.
    pub(crate) fn update(&mut self, options: &Value) {
        let Some(options) = options.as_array() else { return };
        let select = |option: &&Value| text(option, "type") == "select";
        if let Some(modes) = options.iter().filter(select).find(|option| is(option, "mode")) {
            self.mode_option = Some(text(modes, "id"));
            // One that says no mode leaves the mode as it was.
            self.mode = opt_text(modes, "currentValue").unwrap_or_else(|| std::mem::take(&mut self.mode));
            self.modes = choices(modes).into_iter().filter_map(|choice| opt_text(choice, "value")).collect();
        }
        let models = options.iter().filter(select).find(|option| text(option, "id") == "model" || is(option, "model"));
        let Some(models) = models else { return };
        self.model_option = Some(text(models, "id"));
        self.catalog.model = text(models, "currentValue");
        self.catalog.models.clear();
        for choice in choices(models) {
            push_model(&mut self.catalog.models, &text(choice, "value"), choice);
        }
        // The levels are those of the model in use; a list without them says it has none.
        let levels = options.iter().filter(select).find(|option| is(option, "thought_level"));
        self.effort_option = levels.map(|levels| text(levels, "id"));
        self.catalog.effort = levels.map(|levels| text(levels, "currentValue")).unwrap_or_default();
        self.catalog.efforts =
            levels.map(|levels| choices(levels)).unwrap_or_default().into_iter().filter_map(effort).collect();
    }
}

/// Whether a session option is of `category`.
fn is(option: &Value, category: &str) -> bool {
    text(option, "category") == category
}

/// The choices of a select option, whether listed plainly or in groups.
fn choices(option: &Value) -> Vec<&Value> {
    let mut found = Vec::new();
    for entry in items(option, "options") {
        if field(entry, "value").is_string() {
            found.push(entry);
        } else {
            found.extend(items(entry, "options").iter().filter(|choice| field(choice, "value").is_string()));
        }
    }
    found
}

fn effort(choice: &Value) -> Option<Effort> {
    let id = opt_text(choice, "value")?;
    let name = opt_text(choice, "name").unwrap_or_else(|| id.clone());
    Some(Effort { id, name })
}

/// Adds the model `id` that `source` describes, unless it is there. The binary can list two models under one
/// name; the later one then says its id too, so that a picker tells them apart.
fn push_model(models: &mut Vec<Model>, id: &str, source: &Value) {
    if id.trim().is_empty() || models.iter().any(|known| known.id == id) {
        return;
    }
    let name = opt_text(source, "name").unwrap_or_else(|| id.to_owned());
    let taken = models.iter().any(|known| known.name == name);
    let name = if taken { format!("{name} ({id})") } else { name };
    models.push(Model { id: id.to_owned(), name, description: text(source, "description") });
}

/// The slash commands of an `available_commands_update`.
pub(crate) fn commands(update: &Value) -> Vec<SlashCommand> {
    let mut found: Vec<SlashCommand> = Vec::new();
    for command in items(update, "availableCommands") {
        let name = text(command, "name").trim().trim_start_matches('/').to_owned();
        if name.is_empty() || found.iter().any(|known| known.name == name) {
            continue;
        }
        let description = text(command, "description").trim().to_owned();
        let hint = text(field(command, "input"), "hint").trim().to_owned();
        found.push(SlashCommand { name, description, hint });
    }
    found
}

/// The steps of a `plan` update.
pub(crate) fn plan(update: &Value) -> Vec<PlanStep> {
    let step = |(index, entry): (usize, &Value)| PlanStep {
        step: opt_text(entry, "content").map_or_else(|| format!("Step {}", index + 1), |step| step.trim().to_owned()),
        status: match text(entry, "status").as_str() {
            "in_progress" => PlanStatus::InProgress,
            "completed" => PlanStatus::Completed,
            _ => PlanStatus::Pending,
        },
    };
    items(update, "entries").iter().enumerate().map(step).collect()
}

/// The token counts a turn reports. The binary counts as Gemini does: cached input apart from the input, and
/// thinking apart from the output; here both are part of the whole, as for every other provider.
pub(crate) fn usage(usage: &Value) -> Option<TokenUsage> {
    if !usage.is_object() {
        return None;
    }
    let count = |key: &str| field(usage, key).as_u64().unwrap_or_default();
    let (cached, thought) = (count("cachedReadTokens"), count("thoughtTokens"));
    Some(TokenUsage {
        input: count("inputTokens").saturating_add(cached),
        cached_input: cached,
        cache_write_input: count("cachedWriteTokens"),
        output: count("outputTokens").saturating_add(thought),
        reasoning_output: thought,
    })
}

/// The text of a content block: what a message or a tool's result is made of.
pub(crate) fn block_text(block: &Value) -> Option<String> {
    match text(block, "type").as_str() {
        "text" => Some(text(block, "text")),
        "resource_link" => first_text(block, &["uri", "name"]),
        "resource" => {
            let resource = field(block, "resource");
            first_text(resource, &["text", "uri"])
        }
        // A picture or a sound says nothing in text.
        _ => None,
    }
}

/// `text`, or its end when it is longer than is kept.
fn bounded(text: &str) -> String {
    if text.len() <= OUTPUT_KEPT {
        return text.to_owned();
    }
    let mut start = text.len() - OUTPUT_KEPT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{CUT}{}", text.get(start..).unwrap_or_default())
}

/// The tool's own name in a title the binary makes of it: `run_command` of `Running run_command`,
/// `mcp_files_list` of `Run mcp_files_list?`.
fn titled(title: &str) -> Option<&str> {
    let rest = title.strip_prefix("Running ").or_else(|| title.strip_prefix("Run "))?;
    let name = rest.strip_suffix('?').unwrap_or(rest);
    (!name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '?')).then_some(name)
}

/// Whether a search called `title` (the tool `name`) looks on the web, not in the files: the binary sorts
/// both as searches.
fn searches_web(title: &str, name: &str) -> bool {
    let title = title.trim().to_lowercase();
    title.starts_with("web search")
        || title.starts_with("search the web")
        || matches!(name, "web_search" | "search_web")
}

/// What a command printed, of what the binary says it returned: without when it ran and how it ended,
/// which the binary puts before it and says apart too.
fn printed(output: &str) -> &str {
    match output.split_once(PRINTED) {
        Some((before, printed)) if before.starts_with(RAN_AT) => printed,
        _ => output,
    }
}

/// Takes a `tool_call` or `tool_call_update` into what is known of the call: an update says only what changed.
/// `None` when it names no call.
pub(crate) fn tool(known: Option<Tool>, update: &Value) -> Option<Tool> {
    let id = opt_text(update, "toolCallId")?;
    let mut tool = known.unwrap_or_default();
    tool.id = id;
    if let Some(title) = opt_text(update, "title") {
        tool.title = bounded(title.trim());
        if let Some(name) = titled(&tool.title) {
            name.clone_into(&mut tool.name);
        }
    }
    if let Some(status) = field(update, "status").as_str() {
        tool.status = match status {
            "in_progress" => ToolStatus::InProgress,
            "completed" => ToolStatus::Completed,
            "failed" => ToolStatus::Failed,
            _ => ToolStatus::Pending,
        };
    }
    if field(field(update, "_meta"), "is_mcp_tool_call").as_bool() == Some(true) {
        tool.mcp = true;
    }
    let input = field(update, "rawInput");
    if !input.is_null() {
        tool.input = input.clone();
        tool.command = first_text(input, &["CommandLine", "command_line", "commandLine", "command"]).or(tool.command);
        tool.cwd = first_text(input, &["Cwd", "WorkingDirectory", "working_dir", "workingDir", "cwd"]).or(tool.cwd);
        tool.daemon |= field(input, "IsDaemon").as_bool() == Some(true);
        let pair = |server: &str, name: &str| opt_text(input, server).zip(opt_text(input, name));
        if let Some((server, name)) = pair("ServerName", "ToolName") {
            // A tool of an MCP server that the binary reaches through a tool of its own, as if it ran a
            // command: the server, the tool and what the tool is given are all in what that one is given.
            (tool.mcp, tool.server, tool.name) = (true, Some(server.trim().to_owned()), name.trim().to_owned());
            tool.input = field(input, "Arguments").clone();
        } else if let Some((server, name)) = pair("server", "tool").filter(|_| tool.mcp) {
            (tool.server, tool.name) = (Some(server.trim().to_owned()), name.trim().to_owned());
        }
    }
    if let Some(locations) = field(update, "locations").as_array() {
        let place = |location: &Value| Some((opt_text(location, "path")?, field(location, "line").as_u64()));
        tool.locations = locations.iter().filter_map(place).collect();
    }
    if tool.locations.is_empty() && !tool.mcp {
        // A call that says no place may still be given the file it is about.
        let given = first_text(&tool.input, &["file_path", "filePath", "TargetFile", "AbsolutePath"]);
        tool.locations.extend(given.map(|path| (path, None)));
    }
    let mut said = None;
    if let Some(content) = field(update, "content").as_array() {
        let diff = |entry: &Value| {
            (text(entry, "type") == "diff").then(|| Diff {
                path: text(entry, "path"),
                old_text: field(entry, "oldText").as_str().map(str::to_owned),
                new_text: text(entry, "newText"),
            })
        };
        tool.diffs = content.iter().filter_map(diff).filter(|diff| !diff.path.is_empty()).collect();
        let texts = content.iter().filter(|entry| text(entry, "type") == "content");
        let texts: Vec<String> = texts.filter_map(|entry| block_text(field(entry, "content"))).collect();
        said = Some(texts.join("\n")).filter(|text| !text.trim().is_empty());
    }
    // What it returned: said outright, else among the fields of its result, else what its content says.
    let output = field(update, "rawOutput");
    let returned = match output {
        Value::String(output) => Some(output.clone()).filter(|output| !output.trim().is_empty()),
        output => first_text(output, &["combinedOutput", "combined_output", "output"]),
    };
    if let Some(output) = returned.or(said) {
        tool.output = Some(bounded(printed(&output)));
    }
    tool.command = tool.command.or_else(|| first_text(output, &["commandLine", "command_line"]));
    tool.cwd = tool.cwd.or_else(|| first_text(output, &["workingDir", "working_dir"]));
    tool.exit_code = ["exitCode", "exit_code"].iter().find_map(|key| field(output, key).as_i64()).or(tool.exit_code);
    let reported = match text(update, "kind").as_str() {
        "read" => Some(ToolKind::Read),
        "edit" => Some(ToolKind::Edit),
        "delete" => Some(ToolKind::Delete),
        "move" => Some(ToolKind::Move),
        "search" => Some(ToolKind::Search),
        "execute" => Some(ToolKind::Execute),
        "think" => Some(ToolKind::Think),
        "fetch" => Some(ToolKind::Fetch),
        _ => None,
    };
    if let Some(kind) = reported {
        tool.kind = kind;
    }
    if tool.kind == ToolKind::Other {
        // What the binary does not sort is told by what it carries.
        if !tool.mcp && SUBAGENT_TITLES.contains(&tool.title.as_str()) {
            tool.kind = ToolKind::Subagent;
        } else if tool.command.is_some() {
            tool.kind = ToolKind::Execute;
        }
    }
    if matches!(tool.kind, ToolKind::Search | ToolKind::Other) && !tool.mcp && searches_web(&tool.title, &tool.name) {
        tool.kind = ToolKind::WebSearch;
    }
    Some(tool)
}

/// One answer the binary offers to a request for permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    /// What to answer with.
    pub id: String,
    /// What it says.
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub kind: String,
}

/// What a `session/request_permission` asks.
#[derive(Debug, PartialEq)]
pub(crate) enum Asked {
    /// Whether a tool call may happen.
    Approval(Box<ApprovalRequest>),
    /// The user to choose an answer.
    Question(QuestionRequest),
}

/// Reads a `session/request_permission`: what it asks, the answers it offers and the call it is about, which
/// `known` finds what is known of. `None` when it names no call or offers no answer.
pub(crate) fn asked(
    request_id: &str,
    params: &Value,
    known: impl FnOnce(&str) -> Option<Tool>,
) -> Option<(Asked, Vec<Choice>)> {
    let call = field(params, "toolCall");
    let id = opt_text(call, "toolCallId")?;
    let mut offered: Vec<Choice> = Vec::new();
    for option in items(params, "options") {
        let Some(id) = opt_text(option, "optionId") else { continue };
        if offered.iter().all(|known| known.id != id) {
            let name = opt_text(option, "name").map_or_else(|| id.clone(), |name| name.trim().to_owned());
            offered.push(Choice { id, name, kind: text(option, "kind") });
        }
    }
    if offered.is_empty() {
        return None;
    }
    let request_id = request_id.to_owned();
    if id.starts_with(QUESTION_PREFIX) {
        let question =
            opt_text(call, "title").map_or_else(|| "Choose an option.".to_owned(), |title| title.trim().to_owned());
        let options = offered.iter().map(|choice| (choice.id.clone(), choice.name.clone())).collect();
        return Some((Asked::Question(QuestionRequest { request_id, question, options }), offered));
    }
    let tool = tool(known(&id), call)?;
    let always = items(params, "options").iter().find(|option| text(option, "kind") == "allow_always");
    let warning = always.map(|option| field(field(option, "_meta"), "agy.security.warning"));
    let warning = warning.and_then(|warning| first_text(warning, &["message", "title"]));
    let request = ApprovalRequest { request_id, tool, can_remember: always.is_some(), warning };
    Some((Asked::Approval(Box::new(request)), offered))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn lines_are_told_apart() {
        assert_eq!(parse_line(""), Incoming::Nothing);
        assert_eq!(parse_line("starting up"), Incoming::Nothing);
        assert_eq!(parse_line("[1, 2]"), Incoming::Nothing);
        assert_eq!(parse_line("{\"jsonrpc\":\"2.0\"}"), Incoming::Nothing);
        assert_eq!(
            parse_line("{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"a\":1}}\r\n"),
            Incoming::Response { id: 3, outcome: Ok(json!({"a": 1})) }
        );
        assert_eq!(
            parse_line("{\"id\":4,\"error\":{\"code\":-32000,\"message\":\"Authentication required\"}}"),
            Incoming::Response {
                id: 4,
                outcome: Err(Refusal { code: -32000, message: "Authentication required".into() })
            }
        );
        // An answer under an id that is none of ours.
        assert_eq!(parse_line("{\"id\":\"x\",\"result\":{}}"), Incoming::Nothing);
        assert_eq!(
            parse_line("{\"method\":\"session/update\",\"params\":{\"sessionId\":\"s\"}}"),
            Incoming::Notification { method: "session/update".into(), params: json!({"sessionId": "s"}) }
        );
        assert_eq!(
            parse_line("{\"id\":\"r1\",\"method\":\"fs/read_text_file\"}"),
            Incoming::Request { id: json!("r1"), method: "fs/read_text_file".into(), params: Value::Null }
        );
        let link = "Open the following link to authenticate the ACP server: https://accounts.google.com/o/oauth2";
        assert_eq!(parse_line(link), Incoming::SignInLink);
        assert!(is_sign_in_link(link));
        assert!(!is_sign_in_link("INFO serving"));
    }

    #[test]
    fn deep_nesting_is_refused_not_followed() {
        let hostile = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(parse_line(&hostile), Incoming::Nothing);
    }

    #[test]
    fn a_refusal_says_what_went_wrong() {
        let said = |error: Value| refusal(&error).message;
        assert_eq!(
            said(json!({"code": -32603, "message": "Internal error", "data": {"details": "quota"}})),
            "Internal error: quota"
        );
        assert_eq!(said(json!({"code": -32603, "message": "Internal error", "data": "boom"})), "Internal error: boom");
        assert_eq!(said(json!({"code": -32602, "message": "bad model"})), "bad model");
        assert_eq!(said(json!({"code": 7})), "error 7");
        assert_eq!(said(json!("no")), "error 0");
        assert!(refusal(&json!({"code": -32000, "message": "x"})).needs_sign_in());
        let no_browser = json!({"code": -32603, "message": "No browser is available here to sign in."});
        assert!(refusal(&no_browser).needs_sign_in());
        assert!(!refusal(&json!({"code": -32603, "message": "Internal error"})).needs_sign_in());
    }

    #[test]
    fn the_handshake_says_what_the_binary_can_do() {
        let answer = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true, "audio": true, "embeddedContext": true},
                "mcpCapabilities": {"http": true},
                "sessionCapabilities": {"list": {}, "resume": {}},
                "_meta": {"antigravity": {"systemPrompt": {"append": true}}}
            },
            "authMethods": [{"id": "oauth-personal", "name": "Log in with Google"}],
            "agentInfo": {"name": "antigravity-acp", "title": "Google Antigravity", "version": "1.5.12"}
        });
        let all = Abilities {
            version: "1.5.12".into(),
            resume: true,
            load: true,
            images: true,
            http_mcp: true,
            instructions: true,
        };
        assert_eq!(abilities(&answer), all);
        assert_eq!(abilities(&json!({})), Abilities::default());
        assert_eq!(abilities(&json!("hostile")), Abilities::default());
    }

    fn options() -> Value {
        json!([
            {"id": "mode", "category": "mode", "type": "select", "currentValue": "default", "options": []},
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "gemini-3.8-flash",
             "options": [
                {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash", "description": "Fast"},
                {"group": "Others", "options": [
                    {"value": "claude-opus-5-5", "name": "Claude Opus"},
                    {"value": "claude-opus-5-5-b", "name": "Claude Opus"},
                    {"value": "claude-opus-5-5", "name": "Twice"},
                    {"value": " ", "name": "Empty"}
                ]}
             ]},
            {"id": "thinking", "category": "thought_level", "type": "select", "currentValue": "medium",
             "options": [{"value": "low", "name": "Low"}, {"value": "medium", "name": "Medium"}, {"value": "high"}]}
        ])
    }

    #[test]
    fn a_session_says_its_models_and_levels() {
        let settings = Settings::of(&json!({"sessionId": "s", "configOptions": options()}));
        assert_eq!(settings.model_option.as_deref(), Some("model"));
        assert_eq!(settings.effort_option.as_deref(), Some("thinking"));
        let names: Vec<&str> = settings.catalog.models.iter().map(|model| model.name.as_str()).collect();
        assert_eq!(names, ["Gemini 3.8 Flash", "Claude Opus", "Claude Opus (claude-opus-5-5-b)"]);
        assert_eq!(settings.catalog.models[0].description, "Fast");
        assert_eq!(settings.catalog.model, "gemini-3.8-flash");
        assert_eq!(settings.catalog.effort, "medium");
        let levels: Vec<(&str, &str)> =
            settings.catalog.efforts.iter().map(|effort| (effort.id.as_str(), effort.name.as_str())).collect();
        assert_eq!(levels, [("low", "Low"), ("medium", "Medium"), ("high", "high")]);
    }

    #[test]
    fn a_model_without_levels_clears_them_and_noise_changes_nothing() {
        let mut settings = Settings::of(&json!({"configOptions": options()}));
        let before = settings.clone();
        settings.update(&Value::Null);
        settings.update(&json!("hostile"));
        settings.update(&json!([{"id": "mode", "category": "mode", "type": "select"}]));
        assert_eq!(settings, before);
        settings.update(&json!([{"id": "model", "type": "select", "currentValue": "claude-opus-5-5",
            "options": [{"value": "claude-opus-5-5", "name": "Claude Opus"}]}]));
        assert_eq!(settings.catalog.model, "claude-opus-5-5");
        assert_eq!(settings.catalog.models.len(), 1);
        assert!(settings.catalog.efforts.is_empty() && settings.catalog.effort.is_empty());
        assert_eq!(settings.effort_option, None);
    }

    #[test]
    fn an_older_binary_lists_its_models_the_older_way() {
        let setup = json!({"models": {"currentModelId": "b", "availableModels": [
            {"modelId": "a", "name": "A"}, {"modelId": "b", "name": "B", "description": "second"}
        ]}});
        let settings = Settings::of(&setup);
        assert_eq!(settings.model_option, None);
        assert_eq!(settings.catalog.model, "b");
        assert_eq!(settings.catalog.models.len(), 2);
        assert_eq!(settings.catalog.models[1].description, "second");
        assert_eq!(Settings::of(&json!({})), Settings::default());
    }

    #[test]
    fn commands_plans_and_counts_are_read() {
        let update = json!({"availableCommands": [
            {"name": "compact", "description": " Summarise ", "input": {"hint": "what to keep"}},
            {"name": "/goal", "description": "Work until done"},
            {"name": "compact"}, {"name": " "}
        ]});
        let found = commands(&update);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0],
            SlashCommand { name: "compact".into(), description: "Summarise".into(), hint: "what to keep".into() }
        );
        assert_eq!(found[1].name, "goal");

        let steps = plan(&json!({"entries": [
            {"content": " Read ", "status": "completed"}, {"content": "", "status": "in_progress"}, {"content": "Write"}
        ]}));
        let read: Vec<(&str, PlanStatus)> = steps.iter().map(|step| (step.step.as_str(), step.status)).collect();
        assert_eq!(
            read,
            [("Read", PlanStatus::Completed), ("Step 2", PlanStatus::InProgress), ("Write", PlanStatus::Pending)]
        );

        let counted =
            usage(&json!({"inputTokens": 100, "cachedReadTokens": 900, "outputTokens": 20, "thoughtTokens": 30}));
        let expected =
            TokenUsage { input: 1000, cached_input: 900, cache_write_input: 0, output: 50, reasoning_output: 30 };
        assert_eq!(counted, Some(expected));
        assert_eq!(usage(&Value::Null), None);
        assert_eq!(usage(&json!({"inputTokens": "many"})), Some(TokenUsage::default()));
    }

    #[test]
    fn a_command_is_known_by_what_it_carries() {
        let begun = json!({"toolCallId": "t1", "title": "Running run_command", "status": "pending",
            "rawInput": {"CommandLine": "cargo test", "Cwd": "/work"}});
        let call = tool(None, &begun).unwrap();
        assert_eq!(call.kind, ToolKind::Execute);
        assert_eq!((call.command.as_deref(), call.cwd.as_deref()), (Some("cargo test"), Some("/work")));
        assert_eq!(call.status, ToolStatus::Pending);
        // An update says only what changed.
        let done = json!({"toolCallId": "t1", "status": "completed",
            "rawOutput": {"output": "ok\n", "exitCode": 0},
            "content": [{"type": "content", "content": {"type": "text", "text": "ok\n"}}]});
        let call = tool(Some(call), &done).unwrap();
        assert_eq!(call.title, "Running run_command");
        assert_eq!((call.output.as_deref(), call.exit_code), (Some("ok\n"), Some(0)));
        assert!(call.status.is_final());
        assert_eq!(call.command.as_deref(), Some("cargo test"));
        assert_eq!(tool(None, &json!({"title": "nameless"})), None);
    }

    #[test]
    fn a_tool_is_known_by_its_own_name_and_a_servers_tool_by_its_server() {
        let call = |update: Value| tool(None, &update).unwrap();
        assert_eq!(call(json!({"toolCallId": "t", "title": "Running run_command"})).name, "run_command");
        assert_eq!(call(json!({"toolCallId": "t", "title": "Run mcp_files_list?"})).name, "mcp_files_list");
        assert_eq!(call(json!({"toolCallId": "t", "title": "Run invoke_subagent"})).name, "invoke_subagent");
        for title in ["Editing main.rs", "Run", "Run a test?", "Running", "Run ??"] {
            assert_eq!(call(json!({"toolCallId": "t", "title": title})).name, "", "{title}");
        }
        // A server's tool reached through the binary's own: reported as a command, and none.
        let proxied = call(json!({"toolCallId": "t", "kind": "execute", "title": "MCP Tool list_pages",
            "rawInput": {"ServerName": "chrome-devtools", "ToolName": "list_pages", "Arguments": {"page": 2}}}));
        assert!(proxied.mcp && proxied.command.is_none());
        assert_eq!((proxied.server.as_deref(), proxied.name.as_str()), (Some("chrome-devtools"), "list_pages"));
        assert_eq!(proxied.input, json!({"page": 2}));
        // One the binary marks as a server's says both in what it is given.
        let marked = call(json!({"toolCallId": "t", "title": "Running list", "_meta": {"is_mcp_tool_call": true},
            "rawInput": {"server": "files", "tool": "list", "path": "."}}));
        assert_eq!((marked.server.as_deref(), marked.name.as_str(), marked.mcp), (Some("files"), "list", true));
        // What is not marked is not taken for one because of how its input is named.
        let plain = call(json!({"toolCallId": "t", "title": "Running x", "rawInput": {"server": "a", "tool": "b"}}));
        assert_eq!((plain.server, plain.mcp), (None, false));
    }

    #[test]
    fn a_search_of_the_web_a_lasting_command_and_a_file_given_are_told() {
        let kind = |update: Value| tool(None, &update).unwrap().kind;
        let search = |title: &str| kind(json!({"toolCallId": "t", "kind": "search", "title": title}));
        assert_eq!(search("Running search_web"), ToolKind::WebSearch);
        assert_eq!(search("Web search"), ToolKind::WebSearch);
        assert_eq!(search("Searching for main"), ToolKind::Search);
        assert_eq!(kind(json!({"toolCallId": "t", "title": "Running web_search"})), ToolKind::WebSearch);
        let served = json!({"toolCallId": "t", "title": "Running web_search", "_meta": {"is_mcp_tool_call": true}});
        assert_eq!(kind(served), ToolKind::Other);

        let stays =
            json!({"toolCallId": "t", "kind": "execute", "rawInput": {"CommandLine": "serve", "IsDaemon": true}});
        assert!(tool(None, &stays).unwrap().daemon);
        let ends =
            json!({"toolCallId": "t", "kind": "execute", "rawInput": {"CommandLine": "make", "IsDaemon": false}});
        assert!(!tool(None, &ends).unwrap().daemon);

        // The place a call says counts; without one, the file it is given does.
        let read = json!({"toolCallId": "t", "kind": "read", "rawInput": {"file_path": "/work/a.rs"}});
        assert_eq!(tool(None, &read).unwrap().locations, [("/work/a.rs".to_owned(), None)]);
        let edit = json!({"toolCallId": "t", "kind": "edit", "rawInput": {"TargetFile": "/work/b.rs"},
            "locations": [{"path": "/work/c.rs"}]});
        assert_eq!(tool(None, &edit).unwrap().locations, [("/work/c.rs".to_owned(), None)]);

        // What a command printed is given without the binary's lines on when it ran.
        let wrapped = "Created At: 10:00\nCompleted At: 10:01\nCommand exited with code 0.\n\nCommand output:\nok\n";
        let done = json!({"toolCallId": "t", "rawOutput": {"output": wrapped, "exit_code": 0}});
        let done = tool(None, &done).unwrap();
        assert_eq!((done.output.as_deref(), done.exit_code), (Some("ok\n"), Some(0)));
        let own = "a line\nCommand output:\nof the command's own";
        assert_eq!(tool(None, &json!({"toolCallId": "t", "rawOutput": own})).unwrap().output.as_deref(), Some(own));
    }

    #[test]
    fn a_session_says_its_mode_in_either_way() {
        let mut listed = options();
        listed[0]["options"] = json!([{"value": "default", "name": "Default"}, {"value": "auto", "name": "Auto"}]);
        let settings = Settings::of(&json!({"sessionId": "s", "configOptions": listed}));
        assert_eq!(settings.mode_option.as_deref(), Some("mode"));
        assert_eq!(
            (settings.mode.as_str(), settings.modes.as_slice()),
            ("default", ["default".to_owned(), "auto".to_owned()].as_slice())
        );
        let older = json!({"sessionId": "s", "modes": {"currentModeId": "auto",
            "availableModes": [{"id": "default", "name": "Default"}, {"id": "auto"}, {"name": "nameless"}]}});
        let settings = Settings::of(&older);
        assert_eq!((settings.mode_option, settings.mode.as_str(), settings.modes.len()), (None, "auto", 2));
    }

    #[test]
    fn an_edit_carries_its_diff_and_place() {
        let edit = json!({"toolCallId": "t2", "title": "Editing main.rs", "kind": "edit", "status": "in_progress",
            "locations": [{"path": "/work/main.rs", "line": 12}, {"line": 3}],
            "content": [{"type": "diff", "path": "/work/main.rs", "oldText": "a", "newText": "b"},
                        {"type": "diff", "path": "/work/new.rs", "newText": "c"},
                        {"type": "diff", "newText": "nowhere"}]});
        let call = tool(None, &edit).unwrap();
        assert_eq!(call.kind, ToolKind::Edit);
        assert_eq!(call.locations, [("/work/main.rs".to_owned(), Some(12))]);
        assert_eq!(call.diffs.len(), 2);
        assert_eq!(call.diffs[0].old_text.as_deref(), Some("a"));
        assert_eq!(call.diffs[1], Diff { path: "/work/new.rs".into(), old_text: None, new_text: "c".into() });
        assert_eq!(call.output, None);
    }

    #[test]
    fn other_agents_are_told_from_other_tools() {
        let call = |update: Value| tool(None, &update).unwrap().kind;
        assert_eq!(call(json!({"toolCallId": "a", "title": "Running start_subagent"})), ToolKind::Subagent);
        assert_eq!(
            call(json!({"toolCallId": "a", "title": "Run invoke_subagent?", "kind": "other"})),
            ToolKind::Subagent
        );
        // An MCP server may name a tool the same.
        let mcp = json!({"toolCallId": "a", "title": "Running start_subagent", "_meta": {"is_mcp_tool_call": true}});
        assert_eq!(call(mcp), ToolKind::Other);
        assert_eq!(call(json!({"toolCallId": "a", "title": "Running start_subagent", "kind": "read"})), ToolKind::Read);
        let returned =
            tool(None, &json!({"toolCallId": "a", "title": "Running start_subagent", "rawOutput": " done "}));
        assert_eq!(returned.unwrap().output.as_deref(), Some(" done "));
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        let long = format!("{}é{}", "a".repeat(10), "b".repeat(OUTPUT_KEPT - 1));
        let call = tool(None, &json!({"toolCallId": "t", "rawOutput": long})).unwrap();
        let output = call.output.unwrap();
        assert!(output.starts_with(CUT) && output.ends_with('b'));
        assert_eq!(output.len(), CUT.len() + OUTPUT_KEPT - 1);
    }

    fn permission(id: &str) -> Value {
        json!({"sessionId": "s",
        "toolCall": {"toolCallId": id, "title": "Run run_command?", "rawInput": {"CommandLine": "rm -r x"}},
        "options": [
            {"optionId": "once", "name": "Allow", "kind": "allow_once"},
            {"optionId": "always", "name": "Always allow", "kind": "allow_always",
             "_meta": {"agy.security.warning": {"title": "Careful", "message": "This allows every command."}}},
            {"optionId": "no", "name": "Deny", "kind": "reject_once"},
            {"optionId": "once", "name": "Twice", "kind": "allow_once"},
            {"name": "nameless"}
        ]})
    }

    #[test]
    fn an_approval_names_its_call_and_what_may_be_answered() {
        let (asked, offered) = asked("7", &permission("t1"), |_| None).unwrap();
        let Asked::Approval(request) = asked else { panic!("an approval") };
        assert_eq!(request.request_id, "7");
        assert_eq!(request.tool.command.as_deref(), Some("rm -r x"));
        assert_eq!(request.tool.kind, ToolKind::Execute);
        assert!(request.can_remember);
        assert_eq!(request.warning.as_deref(), Some("This allows every command."));
        let ids: Vec<&str> = offered.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(ids, ["once", "always", "no"]);
        // What is known of the call stays known.
        let known = Tool { id: "t1".into(), cwd: Some("/work".into()), ..Tool::default() };
        let (asked, _) = super::asked("7", &permission("t1"), |_| Some(known)).unwrap();
        let Asked::Approval(request) = asked else { panic!("an approval") };
        assert_eq!(request.tool.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn a_question_is_its_title_and_its_choices() {
        let params = json!({"toolCall": {"toolCallId": "interaction_9", "title": " Which one? "},
            "options": [{"optionId": "a", "name": "Apples"}, {"optionId": "b"}]});
        let (asked, offered) = asked("q", &params, |_| None).unwrap();
        let question = QuestionRequest {
            request_id: "q".into(),
            question: "Which one?".into(),
            options: vec![("a".into(), "Apples".into()), ("b".into(), "b".into())],
        };
        assert_eq!(asked, Asked::Question(question));
        assert_eq!(offered.len(), 2);
        // Nothing to answer with, or nothing asked about.
        assert_eq!(super::asked("q", &json!({"toolCall": {"toolCallId": "interaction_9"}}), |_| None), None);
        assert_eq!(super::asked("q", &json!({"options": [{"optionId": "a"}]}), |_| None), None);
    }

    #[test]
    fn content_blocks_say_what_they_can() {
        assert_eq!(block_text(&json!({"type": "text", "text": "hi"})).as_deref(), Some("hi"));
        assert_eq!(
            block_text(&json!({"type": "resource_link", "uri": "file:///a", "name": "a"})).as_deref(),
            Some("file:///a")
        );
        assert_eq!(
            block_text(&json!({"type": "resource", "resource": {"uri": "u", "text": "body"}})).as_deref(),
            Some("body")
        );
        assert_eq!(block_text(&json!({"type": "image", "data": "AAAA"})), None);
        assert_eq!(block_text(&Value::Null), None);
    }
}
