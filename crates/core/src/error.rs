//! The single error type used across the crate.

use std::fmt;
use std::time::Duration;

/// Convenience alias for results produced by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while talking to the provider or touching
/// the local configuration.
#[derive(Debug)]
pub enum Error {
    /// The request never produced an HTTP response: DNS, TCP, TLS or a
    /// protocol-level failure.
    Transport(Box<ureq::Error>),
    /// The server answered with a non-success status code.
    Api {
        /// HTTP status code.
        status: u16,
        /// Machine-readable error code (`error.code`), when the server sent one.
        code: Option<String>,
        /// Human-readable description suitable for display.
        message: String,
        /// How long the server asked us to wait before trying again
        /// (`Retry-After`), when it said.
        retry_after: Option<Duration>,
    },
    /// A streamed response failed after it started (`response.failed` or an
    /// `error` event). The server's codes: `server_error` and
    /// `rate_limit_exceeded` are worth retrying, `context_length_exceeded`
    /// means the input must shrink, `invalid_request_error` is final.
    Response {
        /// Machine-readable error code, when the server sent one.
        code: Option<String>,
        /// Human-readable description suitable for display.
        message: String,
    },
    /// A response body could not be decoded into the expected shape.
    Decode(serde_json::Error),
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// The configuration file is malformed.
    Config {
        /// One-based line number of the offending line.
        line: usize,
        /// What is wrong with it.
        message: &'static str,
    },
    /// The user's home directory could not be determined.
    NoHomeDir,
    /// A file was saved by a newer version of AtroxaBot, in a format this one
    /// does not know; it is left untouched rather than rewritten.
    Newer {
        /// What the file holds, e.g. `story`.
        what: &'static str,
    },
}

impl Error {
    /// Returns `true` when the server rejected the API key (a `401`), so the
    /// user has to enter it again.
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    /// The machine-readable API error code, if any.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } | Self::Response { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Whether trying the same request again later may succeed: network
    /// failures, dropped connections, rate limits and server errors. TLS,
    /// certificate, proxy and malformed-URL failures are final: retrying
    /// cannot fix them.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        use std::io::ErrorKind;
        match self {
            Self::Transport(e) => matches!(
                **e,
                ureq::Error::Timeout(_)
                    | ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::Protocol(_)
                    | ureq::Error::BodyStalled
                    | ureq::Error::Io(_)
            ),
            Self::Io(e) => matches!(
                e.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::NetworkDown
                    | ErrorKind::NetworkUnreachable
                    | ErrorKind::HostUnreachable
            ),
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            Self::Response { code, .. } => {
                matches!(code.as_deref(), Some("server_error" | "rate_limit_exceeded") | None)
            }
            Self::Decode(_) | Self::Config { .. } | Self::NoHomeDir | Self::Newer { .. } => false,
        }
    }

    /// How long the server asked us to wait before retrying, if it said.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Whether the request was too long for the model's context window.
    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        self.code() == Some("context_length_exceeded")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "network error: {e}"),
            Self::Api { status, message, .. } => write!(f, "{message} (HTTP {status})"),
            Self::Response { message, .. } => f.write_str(message),
            Self::Decode(e) => write!(f, "malformed JSON: {e}"),
            // Data that is not what it should be says why in a sentence.
            Self::Io(e) if e.kind() == std::io::ErrorKind::InvalidData && e.get_ref().is_some() => write!(f, "{e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Config { line, message } => write!(f, "config line {line}: {message}"),
            Self::NoHomeDir => f.write_str("could not determine the home directory"),
            Self::Newer { what } => {
                write!(f, "this {what} was saved by a newer version of AtroxaBot; update AtroxaBot to open it")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e.as_ref()),
            Self::Decode(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Api { .. } | Self::Response { .. } | Self::Config { .. } | Self::NoHomeDir | Self::Newer { .. } => {
                None
            }
        }
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        // Body read failures surface as `ureq::Error::Io`; keep them as I/O.
        match e {
            ureq::Error::Io(io) => Self::Io(io),
            other => Self::Transport(Box::new(other)),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Decode(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn classifies_failures() {
        let response = |code: &str| Error::Response { code: Some(code.into()), message: String::new() };
        let api = |status| Error::Api { status, code: None, message: String::new(), retry_after: None };
        assert!(response("server_error").is_retryable() && response("rate_limit_exceeded").is_retryable());
        assert!(!response("invalid_request_error").is_retryable());
        assert!(
            response("context_length_exceeded").is_context_overflow()
                && !response("context_length_exceeded").is_retryable()
        );
        assert!(
            api(503).is_retryable() && api(429).is_retryable() && !api(402).is_retryable() && !api(401).is_retryable()
        );
        let coded = |status, code: &str| Error::Api {
            status,
            code: Some(code.into()),
            message: String::new(),
            retry_after: None,
        };
        assert!(
            api(401).is_unauthorized()
                && !api(403).is_unauthorized()
                && !coded(402, "insufficient_balance").is_unauthorized()
        );
        assert!(Error::Io(std::io::ErrorKind::ConnectionReset.into()).is_retryable());
        assert!(!Error::Io(std::io::Error::other("attachment missing")).is_retryable());
        assert!(Error::Transport(Box::new(ureq::Error::HostNotFound)).is_retryable(), "offline");
        assert!(!Error::Transport(Box::new(ureq::Error::Tls("bad certificate"))).is_retryable());
        assert!(!Error::Transport(Box::new(ureq::Error::BadUri("x".into()))).is_retryable());
    }
}
