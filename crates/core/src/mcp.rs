//! The user's MCP servers, as each engine wants them: a hub of live
//! connections for the agent loop of API providers, and a `--mcp-config`
//! for Claude Code, which connects to them itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use atroxabot_mcp::{Hub, ServerConfig, Tool, Transport, split_command};
use serde_json::{Map, Value, json};

use crate::config::McpServer;

/// Longest name a tool may have with the providers this app speaks to.
const MAX_NAME: usize = 64;
/// Longest description of a tool passed on to the model, in characters.
const MAX_DESCRIPTION: usize = 2_000;

/// How to reach `server`, or `None` when it is switched off or its command
/// line cannot be read (an unclosed quote).
fn transport(server: &McpServer) -> Option<Transport> {
    if !server.enabled {
        return None;
    }
    if server.url.trim().is_empty() {
        let (command, args) = split_command(&server.command)?;
        Some(Transport::Stdio { command, args, env: server.pairs() })
    } else {
        Some(Transport::Http { url: server.url.trim().to_owned(), headers: server.pairs() })
    }
}

/// The servers that are switched on as Claude Code's `--mcp-config` JSON;
/// `None` when there are none. Claude Code starts and reaches them itself.
#[must_use]
pub fn claude_mcp_config(servers: &[McpServer]) -> Option<String> {
    let pairs = |list: Vec<(String, String)>| -> Value {
        Value::Object(list.into_iter().map(|(name, value)| (name, Value::String(value))).collect())
    };
    let mut entries = Map::new();
    for server in servers {
        let entry = match transport(server) {
            Some(Transport::Stdio { command, args, env }) => {
                json!({ "command": command, "args": args, "env": pairs(env) })
            }
            Some(Transport::Http { url, headers }) => json!({ "type": "http", "url": url, "headers": pairs(headers) }),
            None => continue,
        };
        entries.insert(server.name.clone(), entry);
    }
    (!entries.is_empty()).then(|| json!({ "mcpServers": entries }).to_string())
}

/// A server's tool as the model is offered it.
pub(crate) struct McpTool {
    /// The name the model calls it by: `mcp__server__tool`, in the
    /// characters and length every provider takes.
    pub name: String,
    /// The server it belongs to.
    server: String,
    /// What the server calls it.
    tool: String,
    /// What it does, for the model.
    pub description: String,
    /// JSON Schema of its arguments.
    pub schema: Value,
}

/// The live connections to the user's MCP servers, shared by every turn of
/// every API session. Cloning it shares them.
#[derive(Clone)]
pub struct McpHub {
    hub: Arc<Hub>,
    /// Whether the servers were asked for their tools before: after that, asking is instant.
    listed: Arc<AtomicBool>,
}

impl std::fmt::Debug for McpHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpHub")
    }
}

impl McpHub {
    /// Over the servers that are switched on; `None` when there are none.
    /// Nothing is started until a turn asks for the tools.
    #[must_use]
    pub fn new(servers: &[McpServer]) -> Option<Self> {
        let configs: Vec<ServerConfig> = servers
            .iter()
            .filter_map(|server| Some(ServerConfig { name: server.name.clone(), transport: transport(server)? }))
            .collect();
        (!configs.is_empty()).then(|| Self { hub: Arc::new(Hub::new(configs)), listed: Arc::default() })
    }

    /// Whether the servers have yet to be started and asked for their
    /// tools, which the first turn waits for.
    pub(crate) fn cold(&self) -> bool {
        !self.listed.load(Ordering::Relaxed)
    }

    /// Stops the servers it started. They also stop when the last clone goes.
    pub fn close(&self) {
        self.hub.close();
    }

    /// Calls `tool` with `input`. `Err` is the failure as the model reads it.
    pub(crate) fn call(&self, tool: &McpTool, input: &Value, cancel: &AtomicBool) -> Result<String, String> {
        self.hub.call(&tool.server, &tool.tool, input, cancel)
    }

    /// The tools of every reachable server, in a stable order (the hub
    /// sorts them, and keeps a server's list once it has it, so the
    /// provider's prompt cache stays valid), and for each server that could
    /// not be reached, a sentence saying so. The first call may take as
    /// long as the servers take to start.
    pub(crate) fn tools(&self) -> (Vec<McpTool>, Vec<String>) {
        let (listed, failed) = self.hub.tools();
        self.listed.store(true, Ordering::Relaxed);
        let mut tools: Vec<McpTool> = Vec::with_capacity(listed.len());
        for Tool { server, name, description, input_schema } in listed {
            let offered = tool_name(&server, &name);
            // Two tools that shorten to one name: the first keeps it.
            if tools.iter().any(|tool| tool.name == offered) {
                continue;
            }
            // Providers want an object schema, whatever the server sent.
            let schema = if input_schema.get("type").and_then(Value::as_str) == Some("object") {
                input_schema
            } else {
                json!({ "type": "object" })
            };
            tools.push(McpTool {
                name: offered,
                description: description.chars().take(MAX_DESCRIPTION).collect(),
                server,
                tool: name,
                schema,
            });
        }
        let problems =
            failed.into_iter().map(|(server, why)| format!("The MCP server {server} is not available: {why}"));
        (tools, problems.collect())
    }
}

/// `mcp__server__tool`, with anything but letters, digits, `_` and `-`
/// replaced and the whole cut to what providers accept.
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
    };
    let mut name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.truncate(MAX_NAME);
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, command: &str, url: &str) -> McpServer {
        McpServer { name: name.into(), command: command.into(), url: url.into(), enabled: true, ..McpServer::default() }
    }

    #[test]
    fn claude_gets_the_enabled_servers_as_json() {
        let mut files = server("files", "npx -y @scope/server \"C:\\My Notes\"", "");
        files.vars = "TOKEN=abc".into();
        let mut remote = server("tracker", "", "https://mcp.example.com/mcp");
        remote.vars = "Authorization=Bearer x".into();
        let off = McpServer { enabled: false, ..server("off", "run", "") };
        let broken = server("broken", "run \"unclosed", "");
        let json: Value = serde_json::from_str(&claude_mcp_config(&[files, remote, off, broken]).unwrap()).unwrap();
        let servers = json["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["files", "tracker"]);
        assert_eq!(servers["files"]["command"], "npx");
        assert_eq!(servers["files"]["args"], json!(["-y", "@scope/server", "C:\\My Notes"]));
        assert_eq!(servers["files"]["env"]["TOKEN"], "abc");
        assert_eq!(servers["tracker"]["type"], "http");
        assert_eq!(servers["tracker"]["headers"]["Authorization"], "Bearer x");
        assert!(claude_mcp_config(&[]).is_none() && McpHub::new(&[]).is_none());
    }

    #[test]
    fn tool_names_fit_every_provider() {
        assert_eq!(tool_name("files", "read file"), "mcp__files__read_file");
        let long = tool_name("a server with a very long name indeed", &"tool".repeat(20));
        assert!(long.len() == MAX_NAME && long.is_ascii());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Starts a real MCP server (the reference filesystem one, through npx)
    /// and calls a tool of it: `cargo test -p atroxabot-core mcp::live -- --ignored`.
    #[test]
    #[ignore = "downloads and runs an MCP server with npx"]
    fn a_real_server_lists_and_runs_tools() {
        let dir = std::env::temp_dir().join(format!("atroxabot-mcp-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hi").unwrap();
        let command = format!("npx -y @modelcontextprotocol/server-filesystem \"{}\"", dir.display());
        let hub =
            McpHub::new(&[McpServer { name: "files".into(), command, enabled: true, ..McpServer::default() }]).unwrap();
        assert!(hub.cold());
        let (tools, problems) = hub.tools();
        assert!(problems.is_empty(), "{problems:?}");
        let list =
            tools.iter().find(|tool| tool.name == "mcp__files__list_directory").expect("the server lists its tools");
        let listed = hub.call(list, &json!({ "path": dir.display().to_string() }), &AtomicBool::new(false)).unwrap();
        assert!(listed.contains("hello.txt"), "{listed}");
        assert!(!hub.cold());
        hub.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
