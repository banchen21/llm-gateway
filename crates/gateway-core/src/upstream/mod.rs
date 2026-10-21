//! Talks to upstream providers and normalizes what they return.

pub mod anthropic;
pub mod openai;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{names_account_problem, Error};
use crate::ir::{ChatRequest, EventStream};
use crate::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    /// An Anthropic-compatible Messages API endpoint.
    Anthropic,
    /// An OpenAI-compatible Chat Completions endpoint.
    OpenAi,
}

/// Which field carries the output limit in a Chat Completions request.
///
/// The ecosystem is genuinely split: OpenAI's reasoning models reject `max_tokens`,
/// while most third-party OpenAI-compatible endpoints reject `max_completion_tokens`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`. The default, because third-party compatible vendors outnumber
    /// OpenAI's own reasoning models as upstreams for a gateway.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, required by OpenAI's reasoning models.
    MaxCompletionTokens,
}

impl MaxTokensField {
    pub fn name(&self) -> &'static str {
        match self {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Body fields a model's `extra` params may not overwrite, because they carry the
/// request's actual meaning.
const RESERVED_BODY_FIELDS: &[&str] = &[
    "model",
    "stream",
    "stream_options",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "max_tokens",
    "max_completion_tokens",
    "thinking",
];

/// One configured provider endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub protocol: ProviderProtocol,
    /// The API root without a path, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    /// Every key accepted for this endpoint, in the order they should be tried.
    pub api_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_headers: BTreeMap<String, String>,
    /// Fallback for clients that omit `max_tokens`, which Anthropic requires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Which field carries the output limit for an OpenAI-compatible upstream.
    #[serde(default)]
    pub max_tokens_field: MaxTokensField,
    /// Provider-specific parameters merged into the request body.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl ProviderConfig {
    /// Joins the configured base URL with an API path, tolerating a trailing slash.
    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Merges the model's `extra` params into a request body.
    pub fn merge_extra(&self, body: &mut serde_json::Map<String, Value>) -> Result<()> {
        for (name, value) in &self.extra {
            if RESERVED_BODY_FIELDS.contains(&name.as_str()) {
                return Err(Error::config(format!(
                    "model extra params cannot replace `{name}`"
                )));
            }
            body.insert(name.clone(), value.clone());
        }
        Ok(())
    }
}

/// Notified with a key and the status that refused it.
pub type KeyRefusal = dyn Fn(&str, u16) + Send + Sync;

/// A single upstream call.
pub struct UpstreamCall {
    pub client: reqwest::Client,
    pub config: ProviderConfig,
    pub request: ChatRequest,
    /// The provider's own model id, which may differ from the client's alias.
    pub model: String,
    /// Which key to try first. Requests rotate through the list so a key that is
    /// rate limited is not the first one every caller touches.
    pub key_start: usize,
    /// How long the stream may go without an event before it is failed.
    pub idle: Option<Duration>,
    /// Called with a key and the status that refused it, so the caller can stop choosing
    /// that account for a while.
    pub on_key_refused: Option<Arc<KeyRefusal>>,
}

/// Starts an upstream call and returns its normalized event stream.
///
/// The request is sent before returning, so an upstream rejection (bad key, unknown
/// model) is still reportable to the client as a matching HTTP status. Failures that
/// occur after the response starts streaming can only be reported in-band, because by
/// then the gateway has already committed a 200.
///
/// The stream owns the upstream connection, so a client that disconnects mid-response
/// closes it by dropping the stream.
pub async fn stream(call: UpstreamCall) -> Result<EventStream> {
    let idle = call.idle;
    let events = match call.config.protocol {
        ProviderProtocol::Anthropic => anthropic::stream(call).await?,
        ProviderProtocol::OpenAi => openai::stream(call).await?,
    };
    Ok(match idle {
        Some(idle) if !idle.is_zero() => with_idle_timeout(events, idle),
        _ => events,
    })
}

/// Fails a stream that stops producing events instead of waiting on it forever.
///
/// A provider that sends its opening event and then goes quiet leaves the connection
/// healthy and the client hanging; only a clock between events can tell that apart from
/// a model that is simply thinking for a long time.
fn with_idle_timeout(mut events: EventStream, idle: Duration) -> EventStream {
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(idle, events.next()).await {
                Ok(Some(event)) => yield event,
                Ok(None) => break,
                Err(_) => {
                    yield Err(Error::Timeout(format!(
                        "the upstream produced no event for {}ms",
                        idle.as_millis()
                    )));
                    break;
                }
            }
        }
    })
}

/// How many times a call is replayed after a transport failure.
///
/// A transport failure means the upstream never produced a response, so replaying cannot
/// duplicate anything it did. An HTTP error response is deliberately not retried: it is
/// the upstream's answer, not a lost request.
const TRANSPORT_RETRIES: usize = 2;

/// The pause before a replay, multiplied by the attempt number.
const REPLAY_BACKOFF: Duration = Duration::from_millis(250);

/// Sends a request with the first key the upstream accepts.
///
/// Keys are tried in order starting at `key_start`, so a rate-limited account is not the
/// first door every request knocks on. A response that names a key problem — quota
/// exhausted (429), rejected credential (401), a plan that excludes the request (403), or
/// an exhausted balance (402, and some endpoints report it as 400) — moves on to the next
/// key. Anything else is the upstream's answer and is returned.
pub async fn send_with_keys(
    keys: &[String],
    key_start: usize,
    build: impl Fn(&str) -> reqwest::RequestBuilder,
    on_refusal: Option<&KeyRefusal>,
) -> Result<reqwest::Response> {
    if keys.is_empty() {
        return Err(Error::config(
            "the provider has no API key; add one in the provider settings",
        ));
    }
    let mut last_error = None;
    for offset in 0..keys.len() {
        let key = &keys[(key_start + offset) % keys.len()];
        let response = send_replaying(|| build(key)).await?;
        let status = response.status();
        if !is_key_problem(status) {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let message: String = body.chars().take(500).collect();
        // A 400 is normally about the request, which every account would reject the same
        // way. Only a body that names the account is worth retrying with another key.
        if status == reqwest::StatusCode::BAD_REQUEST && !names_account_problem(&message) {
            return Err(Error::upstream(status, message));
        }
        if let Some(notify) = on_refusal {
            notify(key, status.as_u16());
        }
        last_error = Some(Error::upstream(status, message));
    }
    Err(last_error.unwrap_or_else(|| Error::config("the provider has no API key")))
}

/// Statuses that can mean this account cannot serve the request right now.
fn is_key_problem(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::PAYMENT_REQUIRED
            | reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::TOO_MANY_REQUESTS
    )
}

/// Sends a request, replaying it after a transport failure.
///
/// `build` is called once per attempt because a `RequestBuilder` is consumed by `send` and
/// cannot be cloned; the body is rebuilt from the same value each time.
pub async fn send_replaying(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        match build().send().await {
            Ok(response) => return Ok(response),
            Err(error) if attempt < TRANSPORT_RETRIES => {
                attempt += 1;
                tracing::warn!(%error, attempt, "upstream transport error; replaying the call");
                tokio::time::sleep(REPLAY_BACKOFF * attempt as u32).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::names_account_problem;

    #[test]
    fn a_400_that_names_the_account_is_retryable() {
        // The exact shape CommandCode uses when a key is out of credit.
        assert!(names_account_problem(
            r#"{"error":{"message":"You have insufficient credits to make this request. Please purchase more credits to continue using the service.","type":"invalid_request_error"}}"#
        ));
        assert!(names_account_problem(
            r#"{"error":{"message":"You've reached your weekly usage limit for your plan."}}"#
        ));
    }

    #[test]
    fn a_400_about_the_request_is_not_retryable() {
        assert!(!names_account_problem(
            r#"{"error":{"message":"messages: field required","type":"invalid_request_error"}}"#
        ));
        assert!(!names_account_problem(
            r#"{"error":{"message":"max_tokens must be greater than 0"}}"#
        ));
    }
}
