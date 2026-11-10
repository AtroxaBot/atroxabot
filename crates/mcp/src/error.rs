//! The crate's error type.

use std::{fmt, io};

use serde_json::Value;

/// Why a server could not be reached or a request failed. Its `Display` is a sentence for the model and the
/// user, and never holds a header or environment value.
#[derive(Debug)]
pub(crate) enum Error {
    /// The configuration cannot be used: a command that cannot be run safely, a refused address.
    Config(String),
    /// The server process (or a thread for it) could not be started.
    Spawn(io::Error),
    /// The server process ended or closed its pipes. Holds the end of what it wrote to stderr.
    Exited(String),
    /// The HTTP request failed before an answer arrived.
    Network(String),
    /// A failing HTTP status without a JSON-RPC error in its body.
    Status(u16),
    /// The session of an `initialize`-era HTTP server is gone (404): it has to be initialized again.
    SessionExpired,
    /// A JSON-RPC error response.
    Rpc {
        /// The error's `code`.
        code: i64,
        /// The error's `message`, shortened.
        message: String,
        /// The error's `data`, or null.
        data: Value,
    },
    /// The server answered with something this client cannot use. Reads on after "the server".
    Protocol(String),
    /// The tool ran and reported a failure (`isError`); the text is the tool's own.
    Tool(String),
    /// No answer arrived in time.
    Timeout,
    /// The caller cancelled, or the hub was closed.
    Stopped,
}

impl Error {
    /// Whether the connection is of no use after this error and has to be made again.
    pub(crate) fn ends_connection(&self) -> bool {
        matches!(self, Self::Exited(_) | Self::SessionExpired)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(why) => f.write_str(why),
            Self::Spawn(error) => write!(f, "the server could not be started: {error}"),
            Self::Exited(said) if said.is_empty() => f.write_str("the server process ended"),
            Self::Exited(said) => write!(f, "the server process ended: {said}"),
            Self::Network(why) => write!(f, "the server could not be reached: {why}"),
            Self::Status(status) => write!(f, "the server answered HTTP {status}"),
            Self::SessionExpired => f.write_str("the server ended the session"),
            Self::Rpc { code, message, .. } => write!(f, "the server answered with an error: {message} (code {code})"),
            Self::Protocol(what) => write!(f, "the server {what}"),
            Self::Tool(text) => f.write_str(text),
            Self::Timeout => f.write_str("the server did not answer in time"),
            Self::Stopped => f.write_str("stopped"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}
