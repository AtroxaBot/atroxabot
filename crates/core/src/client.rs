//! Blocking HTTP client for an OpenAI-compatible provider, spoken to with
//! Chat Completions (see `completions.rs`), or for Anthropic's own API,
//! spoken to with Messages (see `messages.rs`).
//!
//! Every call blocks the calling thread; the desktop app runs them on worker
//! threads so the render loop never waits on the network.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use ureq::http::Response;

use crate::error::{Error, Result};

/// Longest a response body may take to arrive, streams included.
const BODY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Longest wait a `Retry-After` header is trusted with.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
/// Anthropic's API root, whichever way its address was entered.
const ANTHROPIC: &str = "https://api.anthropic.com/v1";
/// The `anthropic-version` requests are made against.
const ANTHROPIC_VERSION: &str = "2026-10-21";

/// A cheaply clonable handle to the API. Clones share one connection pool.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    /// The provider's API root: the part before `/chat/completions`.
    base: String,
    /// The provider's API key.
    token: Option<String>,
    /// Whether the provider is spoken to with Messages: Anthropic, or
    /// Cloudflare at its `/messages` address.
    pub(crate) anthropic: bool,
    /// Whether the provider is Cloudflare's AI REST API (AI Gateway).
    cloudflare: bool,
    /// Whether requests go to Anthropic itself, which takes its betas.
    pub(crate) first_party: bool,
}

impl std::fmt::Debug for Client {
    // Hand-written so the bearer token never ends up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("anthropic", &self.anthropic)
            .field("authenticated", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

/// A chat model offered by the API.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Model {
    /// Identifier sent in requests, e.g. `claude-sonnet-5.5`.
    pub id: String,
    /// Display name, e.g. `Claude Sonnet 5.5`.
    #[serde(default)]
    pub name: String,
    /// USD per million prompt tokens; `0` when the server omits it.
    #[serde(default)]
    pub input_cost_per_million: f64,
    /// USD per million generated tokens; `0` when the server omits it.
    #[serde(default)]
    pub output_cost_per_million: f64,
    /// USD per million prompt tokens read from the cache; the input price when omitted.
    #[serde(default)]
    pub cache_read_cost_per_million: Option<f64>,
    /// USD per million prompt tokens written to the cache; the input price when omitted.
    #[serde(default)]
    pub cache_write_cost_per_million: Option<f64>,
    /// Accepted input kinds, e.g. `text`, `image`, `audio`.
    #[serde(default)]
    pub input_types: Vec<String>,
    /// Prompt plus output tokens the model accepts; `0` when the server omits
    /// it. OpenRouter calls it `context_length`.
    #[serde(default, alias = "context_length")]
    pub context_window: u64,
    /// Reasoning efforts the model accepts (`none`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`); empty when it cannot reason.
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

impl Model {
    /// Cost in USD of a response with the given token counts, pricing cached
    /// prompt tokens the way the server bills them.
    #[must_use]
    pub fn cost(&self, usage: crate::Usage) -> f64 {
        let cached = usage.cached_tokens;
        let written = usage.cache_write_tokens;
        let uncached = usage.input_tokens.saturating_sub(cached + written);
        (uncached as f64 * self.input_cost_per_million
            + cached as f64 * self.cache_read_cost_per_million.unwrap_or(self.input_cost_per_million)
            + written as f64 * self.cache_write_cost_per_million.unwrap_or(self.input_cost_per_million)
            + usage.output_tokens as f64 * self.output_cost_per_million)
            / 1_000_000.0
    }
}

impl Client {
    /// Creates a client for an OpenAI-compatible provider whose API root is
    /// `base_url` (e.g. `https://openrouter.ai/api/v1`), authenticated with
    /// `api_key` when there is one (local servers often need none). An
    /// address on `api.anthropic.com` means Anthropic's own API. One on
    /// `api.cloudflare.com` is AI Gateway: `…/ai/v1/messages` speaks
    /// Messages, any other of its endpoints Chat Completions at `…/ai/v1`.
    #[must_use]
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            // Streams may legitimately idle while a model thinks, and the
            // caller gives up on silent ones by itself. This only bounds how
            // long a dead connection can keep its worker thread blocked.
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .user_agent(concat!("atroxabot/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let host = base_url.split_once("://").and_then(|(_, rest)| rest.split(['/', ':']).next());
        let on = |name: &str| host.is_some_and(|host| host.eq_ignore_ascii_case(name));
        let cloudflare = on("api.cloudflare.com");
        let base = base_url.trim_end_matches('/');
        // Whichever of Cloudflare's endpoints was pasted (`/ai/run`, `/ai/v1/chat/completions`,
        // `/ai/v1/responses`, `/ai/v1/messages`), its root is `…/ai/v1`; only `/messages` means Messages.
        let gateway = base.find("/ai/").filter(|_| cloudflare).map(|at| format!("{}/ai/v1", &base[..at]));
        let anthropic = on("api.anthropic.com") || (gateway.is_some() && base.ends_with("/messages"));
        let base = match gateway {
            _ if on("api.anthropic.com") => ANTHROPIC.to_owned(),
            Some(root) => root,
            None => base.to_owned(),
        };
        Self { agent, base, token: api_key, anthropic, cloudflare, first_party: on("api.anthropic.com") }
    }

    /// The same client, speaking Anthropic's Messages API whatever the
    /// address looks like: for a Claude API connection behind a proxy.
    #[must_use]
    pub fn speaking_messages(mut self) -> Self {
        self.anthropic = true;
        self
    }

    /// Lists the available chat models.
    ///
    /// # Errors
    /// Network failure or a non-success response.
    pub fn models(&self) -> Result<Vec<Model>> {
        #[derive(Deserialize)]
        struct Body {
            data: Vec<Model>,
        }
        // ponytail: Cloudflare lists no models (`GET /ai/v1/models` is a 405),
        // so their names are typed; read its list once cloudflare/ai#549 adds one.
        if self.cloudflare {
            return Ok(Vec::new());
        }
        if self.anthropic {
            let request = self.agent.get(format!("{}/models?limit=1000", self.base));
            let response = self.authorize(request.header("anthropic-version", ANTHROPIC_VERSION)).call()?;
            return crate::messages::models(response);
        }
        let response = self.authorize(self.agent.get(format!("{}/models", self.base))).call()?;
        let body: Body = read_json(response)?;
        Ok(body.data)
    }

    /// Whether replies from `model` use Messages: every model on Anthropic
    /// (or Cloudflare's `/messages`), and Claude (`anthropic/…`) anywhere on
    /// Cloudflare, whose Chat Completions answers Claude in Messages events.
    pub(crate) fn speaks_messages(&self, model: &str) -> bool {
        self.anthropic || (self.cloudflare && model.starts_with("anthropic/"))
    }

    /// POSTs `body` as JSON, authenticated, and returns the raw response
    /// after checking the status. Used by streaming calls that consume the
    /// body incrementally.
    pub(crate) fn post(&self, path: &str, body: &Value) -> Result<Response<ureq::Body>> {
        let mut request = self.agent.post(format!("{}{path}", self.base)).content_type("application/json");
        if path == "/messages" {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
            if self.first_party {
                request = request.header("anthropic-beta", crate::messages::BETAS);
            }
        }
        check_status(self.authorize(request).send(&serde_json::to_vec(body)?[..])?)
    }

    /// `request` with the provider's credentials: Anthropic's own header,
    /// or a bearer token (Cloudflare's, with its default gateway named,
    /// which its own Workers AI models need).
    fn authorize<B>(&self, mut request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if self.cloudflare {
            request = request.header("cf-aig-gateway-id", "default");
        }
        match &self.token {
            Some(token) if self.anthropic && !self.cloudflare => request.header("x-api-key", token),
            Some(token) => request.header("Authorization", format!("Bearer {token}")),
            None => request,
        }
    }
}

impl Default for Client {
    /// A client with no provider yet: every request fails.
    fn default() -> Self {
        Self::new("", None)
    }
}

/// Decodes a JSON body after checking the status code.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(response: Response<ureq::Body>) -> Result<T> {
    let mut response = check_status(response)?;
    let text = response.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// Turns non-2xx responses into [`Error::Api`], extracting the server's
/// message from the OpenAI-style `{"error": {...}}` envelope when present.
fn check_status(mut response: Response<ureq::Body>) -> Result<Response<ureq::Body>> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(parse_retry_after);
    let text = response.body_mut().read_to_string().unwrap_or_default();
    let (code, message) = parse_error_body(&text);
    Err(Error::Api {
        status: status.as_u16(),
        code,
        message: message.unwrap_or_else(|| status.canonical_reason().unwrap_or("request failed").to_owned()),
        retry_after,
    })
}

/// A `Retry-After` header in seconds, capped at [`MAX_RETRY_AFTER`]. HTTP
/// dates are not worth a date parser here: they mean "use your default".
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(|secs| Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// Extracts `(code, message)` from an error body. Accepts both
/// `{"error": {"code", "message"}}` and flat `{"error": "code", "message"}`.
pub(crate) fn parse_error_body(text: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (None, None);
    };
    let error = value.get("error").unwrap_or(&value);
    let field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let code = field(error, "code").or_else(|| error.as_str().map(str::to_owned));
    let message = field(error, "message").or_else(|| field(&value, "message"));
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::{Model, parse_error_body, parse_retry_after};
    use crate::Usage;

    #[test]
    fn model_pricing() {
        let json = r#"{"id":"m","name":"M","input_cost_per_million":2,"output_cost_per_million":10}"#;
        let model: Model = serde_json::from_str(json).unwrap();
        let cost = model.cost(Usage::new(1_000, 500));
        assert!((cost - 0.007).abs() < 1e-12);
        let bare: Model = serde_json::from_str(r#"{"id":"m"}"#).unwrap();
        assert!(bare.cost(Usage::new(5, 5)).abs() < f64::EPSILON);
        assert_eq!(bare.context_window, 0);

        // 1M input: 600k cached at 0.2, 100k written at 2.5, 300k plain at 2.
        let json = r#"{"id":"m","input_cost_per_million":2,"output_cost_per_million":10,
            "cache_read_cost_per_million":0.2,"cache_write_cost_per_million":2.5,"context_window":1000000}"#;
        let cached: Model = serde_json::from_str(json).unwrap();
        let usage =
            Usage { input_tokens: 1_000_000, output_tokens: 0, cached_tokens: 600_000, cache_write_tokens: 100_000 };
        assert!((cached.cost(usage) - (0.6 + 0.12 + 0.25)).abs() < 1e-9);
        assert_eq!(cached.context_window, 1_000_000);
    }

    #[test]
    fn anthropic_is_known_by_its_host() {
        let client = |url: &str| super::Client::new(url, None);
        for url in
            ["https://api.anthropic.com", "https://API.anthropic.com/v1/messages", "http://api.anthropic.com:443/"]
        {
            assert!(client(url).anthropic && client(url).base == super::ANTHROPIC, "{url}");
        }
        for url in ["https://openrouter.ai/api/v1", "https://api.anthropic.com.evil.example/v1", "api.anthropic.com"] {
            assert!(!client(url).anthropic, "{url}");
        }
        assert_eq!(client("http://localhost:11434/v1/").base, "http://localhost:11434/v1");
        assert!(!client("https://example.com/v1/messages").anthropic, "only Cloudflare's /messages");
    }

    #[test]
    fn cloudflare_speaks_what_its_address_ends_in() {
        let account = "https://api.cloudflare.com/client/v4/accounts/abc";
        let root = format!("{account}/ai/v1");
        let messages = super::Client::new(&format!("{root}/messages/"), None);
        assert!(messages.cloudflare && messages.anthropic && messages.base == root);
        for end in ["/ai/v1", "/ai/v1/chat/completions", "/ai/v1/responses", "/ai/run"] {
            let completions = super::Client::new(&format!("{account}{end}"), None);
            assert!(completions.cloudflare && !completions.anthropic && completions.base == root, "{end}");
            // Its Chat Completions answers Claude in Messages events: Claude speaks Messages.
            assert!(completions.speaks_messages("anthropic/claude-haiku-5.5"), "{end}");
            assert!(!completions.speaks_messages("@cf/google/gemma-4"), "{end}");
        }
        assert!(
            !super::Client::new("https://openrouter.ai/api/v1", None).speaks_messages("anthropic/claude-haiku-5.5")
        );
        assert!(super::Client::new(&root, None).models().unwrap().is_empty(), "no request: Cloudflare lists none");
    }

    #[test]
    fn error_bodies() {
        let nested = r#"{"error":{"message":"Wrong code","type":"x","code":"invalid_code"}}"#;
        assert_eq!(parse_error_body(nested), (Some("invalid_code".into()), Some("Wrong code".into())));
        let flat = r#"{"error":"authorization_pending","message":"Not yet"}"#;
        assert_eq!(parse_error_body(flat), (Some("authorization_pending".into()), Some("Not yet".into())));
        assert_eq!(parse_error_body("<html>"), (None, None));
    }

    #[test]
    fn retry_after_headers() {
        assert_eq!(parse_retry_after(" 7 "), Some(std::time::Duration::from_secs(7)));
        assert_eq!(parse_retry_after("999999"), Some(super::MAX_RETRY_AFTER), "capped");
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }
}
