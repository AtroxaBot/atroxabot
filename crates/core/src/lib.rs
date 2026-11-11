//! AtroxaBot's agents, sessions and engines.
//!
//! * [`Agent`]: who the user talks to; [`Records`] keeps them in `~/.atroxabot/agents/`.
//! * [`Session`]: a conversation with an agent; [`SessionStore`] keeps them in `~/.atroxabot/sessions/`.
//! * [`Config`]: settings and the [`Connection`]s agents think through.
//! * Engines: [`ClaudeAgent`] drives the user's own Claude Code installation
//!   (see the `atroxabot-claude-agent` crate); [`ApiEngine`] runs the agent
//!   loop over Anthropic's Messages API or any OpenAI-compatible gateway
//!   with the tools in [`tools`]. Both report a turn as [`AgentEvent`]s.
//!
//! All network calls and tools are blocking; run them off the UI thread.

mod agent;
pub mod attach;
mod claude;
mod client;
mod completions;
mod config;
mod engine;
mod error;
pub mod git;
mod mcp;
mod messages;
mod responses;
mod session;
mod sse;
pub mod stats;
pub mod tools;

pub use agent::{Agent, AgentVoice, Autonomy, CALL_ENDED, CALL_STARTED, FOLLOW_UP, INTERRUPTING, Records};
pub use atroxabot_claude_agent::{
    Error as ClaudeError, Info as ClaudeInfo, ModelInfo as ClaudeModel, SlashCommand as ClaudeCommand,
};
pub use claude::{ClaudeAgent, TerminalCommand, claude_info, claude_login, find_claude};
pub use client::{Client, Model};
pub use config::{Config, Connection, ConnectionKind, McpServer, VoiceSettings, lock_instance};
pub use engine::{AgentEvent, ApiEngine, Ask, Control, FollowUp, Inbox, LimitWindow, Limits, Verdict};
pub use error::{Error, Result};
pub use mcp::{McpHub, claude_mcp_config};
pub use responses::{Completion, InputItem, ResponseRequest, Role, StreamEvent, ToolCall, ToolChoice, ToolSpec, Usage};
pub use session::{
    Block, Entry, Reply, SearchHit, Session, SessionStore, SessionSummary, Todo, TodoState, ToolUse, new_id, unix_now,
};
