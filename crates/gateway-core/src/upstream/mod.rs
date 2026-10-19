//! Talks to upstream providers and normalizes what they return.

pub mod anthropic;
pub mod openai;

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
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
    match call.config.protocol {
        ProviderProtocol::Anthropic => anthropic::stream(call).await,
        ProviderProtocol::OpenAi => openai::stream(call).await,
    }
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
/// exhausted (429), rejected credential (401), or a plan that excludes the request (403)
/// — moves on to the next key. Anything else is the upstream's answer and is returned.
pub async fn send_with_keys(
    keys: &[String],
    key_start: usize,
    build: impl Fn(&str) -> reqwest::RequestBuilder,
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
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let body = response.text().await.unwrap_or_default();
            let mut message = body;
            message.truncate(500);
            last_error = Some(Error::upstream(status, message));
            continue;
        }
        return Ok(response);
    }
    Err(last_error.unwrap_or_else(|| Error::config("the provider has no API key")))
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
