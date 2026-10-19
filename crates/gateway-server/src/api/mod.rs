//! The gateway's HTTP surface.
//!
//! Both protocol endpoints share one request path — mode gate, authorization, parsing,
//! routing, upstream call, logging, serialization — so the two inbound protocols cannot
//! drift apart in how they are served.
pub mod admin;
pub mod anthropic;
pub mod openai;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use gateway_core::config::{Config, ProtocolMode, Provider, ProviderModel};
use gateway_core::convert::{anthropic_in, anthropic_out, openai_in, openai_out};
use gateway_core::ir::ChatRequest;
use gateway_core::upstream::{self, ProviderConfig, UpstreamCall};
use gateway_core::Error;
use serde_json::{json, Value};

use crate::logging::{self, Outcome, PromptShape, Record};
use crate::state::AppState;

/// The protocols the gateway speaks on its inbound side.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    OpenAi,
}

impl Protocol {
    fn serves(self, mode: &ProtocolMode) -> bool {
        match self {
            Protocol::Anthropic => mode.serves_anthropic(),
            Protocol::OpenAi => mode.serves_openai(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Protocol::Anthropic => "Anthropic Messages",
            Protocol::OpenAi => "OpenAI Chat Completions",
        }
    }

    fn parse(self, value: &Value) -> gateway_core::Result<ChatRequest> {
        match self {
            Protocol::Anthropic => anthropic_in::parse_request(value),
            Protocol::OpenAi => openai_in::parse_request(value),
        }
    }

    /// Errors go back in the envelope the calling protocol's clients expect.
    fn error(self, error: Error) -> Response {
        match self {
            Protocol::Anthropic => anthropic_error(error),
            Protocol::OpenAi => openai_error(error),
        }
    }

    fn new_id(self) -> String {
        let id = uuid::Uuid::new_v4().simple();
        match self {
            Protocol::Anthropic => format!("msg_{id}"),
            Protocol::OpenAi => format!("chatcmpl-{id}"),
        }
    }
}

pub async fn serve(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: &[u8],
    protocol: Protocol,
) -> Response {
    match serve_inner(state, headers, body, protocol).await {
        Ok(response) => response,
        Err(error) => protocol.error(error),
    }
}

async fn serve_inner(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: &[u8],
    protocol: Protocol,
) -> gateway_core::Result<Response> {
    let started = std::time::Instant::now();
    let snapshot = state.snapshot();
    // The mode is checked per request rather than at routing time, so changing it takes
    // effect without restarting the gateway.
    if !protocol.serves(&snapshot.config.protocol_mode) {
        return Err(Error::not_found(format!(
            "this gateway is in `{}` mode and does not serve the {} API",
            mode_name(&snapshot.config.protocol_mode),
            protocol.name()
        )));
    }
    authorize(&snapshot.config, headers)?;

    let value: Value = serde_json::from_slice(body).map_err(|error| {
        Error::bad_request(format!("the request body is not valid JSON: {error}"))
    })?;
    let request = protocol.parse(&value)?;
    let route = snapshot.router.resolve(&snapshot.config, &request.model)?;

    let prompt = PromptShape::of(&request);
    let alias = request.model.clone();
    let mut provider_name = route.provider.name.clone();
    let mut upstream_model = route.model.upstream_model.clone();
    let streaming = request.stream;
    let mut current_provider_id = route.provider.id.clone();
    let mut provider = provider_config(route.provider, route.model);
    let mut route_key = format!("{}/{}", route.provider.id, route.model.id);
    let deadline = (snapshot.config.slow_after_ms > 0)
        .then(|| Duration::from_millis(snapshot.config.slow_after_ms));

    // A route that stalled a moment ago does not get to stall this request too.
    if deadline.is_some() && snapshot.config.failover && state.is_slow(&route_key) {
        if let Some(target) = alternate_target(&snapshot.config, &current_provider_id) {
            tracing::warn!(
                stalled = %route_key,
                answering = %target.name,
                "a route that stalled recently was skipped"
            );
            provider_name = target.name;
            upstream_model = target.model;
            provider = target.config;
            route_key = target.key;
            current_provider_id = target.provider_id;
        }
    }

    let usage = Arc::new(Mutex::new(None));
    let mut primary = call_upstream(
        state,
        &provider,
        &request,
        &upstream_model,
        state.http.clone(),
        deadline,
    )
    .await;

    // The first byte never arrived. Treat it as a stall rather than an answer: remember
    // it so the next request skips this route, and let another provider try now.
    if matches!(&primary, Err(Error::Provider(_))) && deadline.is_some() {
        state.mark_slow(&route_key);
        if snapshot.config.failover {
            if let Some(target) = alternate_target(&snapshot.config, &current_provider_id) {
                tracing::warn!(
                    stalled = %route_key,
                    answering = %target.name,
                    "the first byte did not arrive in time; answering elsewhere"
                );
                provider_name = target.name;
                upstream_model = target.model;
                provider = target.config;
                route_key = target.key;
                current_provider_id = target.provider_id;
                primary = call_upstream(
                    state,
                    &provider,
                    &request,
                    &upstream_model,
                    state.http.clone(),
                    deadline,
                )
                .await;
                // The alternate can stall too; record it so it is skipped next time.
                if matches!(&primary, Err(Error::Provider(_))) {
                    state.mark_slow(&route_key);
                }
            }
        }
    }

    // A connection that fails directly may still get through the proxy Windows is
    // configured to use. That proxy is deliberately ignored everywhere else: making it
    // the default would route domestic upstreams through whatever tunnel happens to run.
    if matches!(&primary, Err(Error::Http(_))) && !crate::proxy::from_env() {
        if let Some(client) = state.client_via_system_proxy() {
            primary = call_upstream(
                state,
                &provider,
                &request,
                &upstream_model,
                client,
                deadline,
            )
            .await;
        }
    }

    let attempted = match primary {
        Ok(events) => Ok(events),
        Err(error) if snapshot.config.failover && error.is_provider_refusal() => {
            match failover(state, &snapshot.config, &request, &current_provider_id).await {
                Some((events, provider, model)) => {
                    provider_name = provider;
                    upstream_model = model;
                    Ok(events)
                }
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    };

    let events = match attempted {
        Ok(events) => events,
        Err(error) => {
            // A 5xx means nothing answered usefully — a dead route, a proxy that is not
            // running, an upstream that fell over. That is the operator's problem to fix,
            // not the request's, so it goes where they will see it. Refusals (401/403/429)
            // stay quiet: another provider already had its chance, and the log has them.
            if error.http_status() >= 500 {
                crate::notify::network_problem(
                    "无法连接上游，请检查网络连接或代理设置。网关会在恢复后自动重试。",
                );
            }
            state
                .log
                .record(Record {
                    alias,
                    provider: provider_name,
                    upstream_model,
                    stream: streaming,
                    prompt,
                    duration_ms: started.elapsed().as_millis() as u64,
                    outcome: Outcome::Failed {
                        status: error.http_status(),
                        message: error.to_string(),
                    },
                })
                .await;
            return Err(error);
        }
    };
    let events = logging::capture_usage(events, usage.clone());

    let id = protocol.new_id();
    let model = alias.clone();
    if !streaming {
        let body = match protocol {
            Protocol::Anthropic => anthropic_out::complete(events, &id, &model).await?,
            Protocol::OpenAi => openai_out::complete(events, &id, &model).await?,
        };
        state
            .log
            .record(Record {
                alias,
                provider: provider_name,
                upstream_model,
                stream: false,
                prompt,
                duration_ms: started.elapsed().as_millis() as u64,
                outcome: Outcome::Served {
                    usage: logging::captured_usage(&usage),
                },
            })
            .await;
        return Ok(Json(body).into_response());
    }

    let frames: Pin<Box<dyn Stream<Item = gateway_core::Result<String>> + Send>> = match protocol {
        Protocol::Anthropic => Box::pin(anthropic_out::stream_response(events, id, model)),
        Protocol::OpenAi => Box::pin(openai_out::stream_response(events, id, model)),
    };
    // A streamed request is recorded once its last frame has been produced, which is the
    // only point at which the upstream's usage is known.
    let log = state.log.clone();
    let frames = async_stream::stream! {
        let mut frames = frames;
        while let Some(frame) = frames.next().await {
            yield frame;
        }
        log.record(Record {
            alias,
            provider: provider_name,
            upstream_model,
            stream: true,
            prompt,
            duration_ms: started.elapsed().as_millis() as u64,
            outcome: Outcome::Served { usage: logging::captured_usage(&usage) },
        })
        .await;
    };
    Ok(sse_response(frames))
}

fn sse_response(
    frames: impl Stream<Item = gateway_core::Result<String>> + Send + 'static,
) -> Response {
    let body = Body::from_stream(frames.map(|frame| frame.map(Bytes::from)));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .expect("a response head with static headers always builds")
}

/// Rejects a request that does not present the configured inbound API key.
///
/// With no key configured the gateway accepts anything, which is what lets Anthropic SDK
/// clients present an arbitrary placeholder key.
pub fn authorize(config: &Config, headers: &HeaderMap) -> Result<(), Error> {
    let expected = inbound_keys(config);
    if expected.is_empty() {
        return Ok(());
    }
    match presented_key(headers) {
        Some(presented) if expected.contains(&presented) => Ok(()),
        _ => Err(Error::unauthorized("a valid API key is required")),
    }
}

/// Every non-empty inbound key the gateway accepts, the legacy single key first.
///
/// Empty entries are dropped rather than compared: an entry left blank in the GUI must
/// never authenticate a caller that sends an empty `x-api-key`.
pub fn inbound_keys(config: &Config) -> Vec<String> {
    std::iter::once(config.server.api_key.as_deref())
        .flatten()
        .chain(config.server.api_keys.iter().map(|key| key.key.as_str()))
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
    {
        return Some(key.to_owned());
    }
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())?;
    let (scheme, token) = authorization.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.to_owned())
}

/// Projects a resolved route into the configuration an upstream adapter needs.
/// One upstream call, optionally abandoned when the first event takes too long.
///
/// The deadline is what makes "working but slow" visible: a provider like that never
/// returns an error, so only a clock can tell the difference between thinking and stalling.
async fn call_upstream(
    state: &Arc<AppState>,
    config: &ProviderConfig,
    request: &ChatRequest,
    model: &str,
    client: reqwest::Client,
    deadline: Option<Duration>,
) -> gateway_core::Result<gateway_core::ir::EventStream> {
    let call = UpstreamCall {
        client,
        config: config.clone(),
        request: request.clone(),
        model: model.to_owned(),
        key_start: state.next_key_start(),
    };
    let Some(limit) = deadline else {
        return upstream::stream(call).await;
    };
    // The clock covers the whole exchange up to the first event: connect, response
    // headers, and the upstream's own thinking time. Timing only the stream would miss a
    // provider that sits on the request before sending anything.
    let opening = async move {
        let mut events = upstream::stream(call).await?;
        match events.next().await {
            Some(first) => Ok(Box::pin(
                futures_util::stream::once(async move { first }).chain(events),
            ) as gateway_core::ir::EventStream),
            None => Ok(events),
        }
    };
    match tokio::time::timeout(limit, opening).await {
        Ok(result) => result,
        Err(_) => Err(Error::Provider(format!(
            "the upstream produced no output within {}ms",
            limit.as_millis()
        ))),
    }
}

/// Another provider that could answer instead, with the route key it will be recorded under.
struct Alternate {
    provider_id: String,
    name: String,
    model: String,
    key: String,
    config: ProviderConfig,
}

/// The first enabled provider other than `exclude`, with its first enabled model.
fn alternate_target(config: &Config, exclude: &str) -> Option<Alternate> {
    for candidate in &config.providers {
        if !candidate.enabled || candidate.id == exclude {
            continue;
        }
        let Some(model) = candidate.models.iter().find(|model| model.enabled) else {
            continue;
        };
        return Some(Alternate {
            provider_id: candidate.id.clone(),
            name: candidate.name.clone(),
            model: model.upstream_model.clone(),
            key: format!("{}/{}", candidate.id, model.id),
            config: provider_config(candidate, model),
        });
    }
    None
}

/// Asks the other enabled providers, in configuration order, when the one the alias
/// names refuses the request.
///
/// An alias means nothing to another provider, so each candidate answers with its own
/// first enabled model. That is the deliberate compromise of a switch that needs no
/// per-alias setup: the client keeps working, and the request log records which provider
/// and model actually answered.
async fn failover(
    state: &Arc<AppState>,
    config: &Config,
    request: &ChatRequest,
    primary_provider_id: &str,
) -> Option<(gateway_core::ir::EventStream, String, String)> {
    for candidate in &config.providers {
        if !candidate.enabled || candidate.id == primary_provider_id {
            continue;
        }
        let Some(model) = candidate.models.iter().find(|model| model.enabled) else {
            continue;
        };
        let call = UpstreamCall {
            client: state.http.clone(),
            config: provider_config(candidate, model),
            request: request.clone(),
            model: model.upstream_model.clone(),
            key_start: state.next_key_start(),
        };
        match upstream::stream(call).await {
            Ok(events) => {
                tracing::warn!(
                    provider = %candidate.name,
                    model = %model.upstream_model,
                    "the primary provider refused; a fallback answered"
                );
                return Some((events, candidate.name.clone(), model.upstream_model.clone()));
            }
            Err(error) => {
                tracing::warn!(provider = %candidate.name, %error, "a fallback provider failed too");
            }
        }
    }
    None
}

pub fn provider_config(provider: &Provider, model: &ProviderModel) -> ProviderConfig {
    ProviderConfig {
        protocol: provider.protocol,
        base_url: provider.base_url.clone(),
        api_keys: provider_keys(provider),
        custom_headers: provider.custom_headers.clone(),
        max_output_tokens: model.max_output_tokens,
        max_tokens_field: model.max_tokens_field,
        extra: model.extra.clone(),
    }
}

/// Every key the provider can authenticate with, the legacy single key first.
///
/// Blank entries are dropped: a row the GUI left empty must not be sent as a credential.
pub fn provider_keys(provider: &Provider) -> Vec<String> {
    std::iter::once(provider.api_key.as_str())
        .chain(provider.api_keys.iter().map(|key| key.key.as_str()))
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// Renders an error in the Anthropic error envelope.
pub fn anthropic_error(error: Error) -> Response {
    anthropic_error_body(
        error.anthropic_error_type(),
        error.http_status(),
        error.to_string(),
    )
}

/// Renders an error in the OpenAI error envelope.
pub fn openai_error(error: Error) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({
            "error": {
                "message": error.to_string(),
                "type": error.openai_error_type(),
                "code": Value::Null,
            }
        })),
    )
        .into_response()
}

/// Builds the Anthropic error envelope from parts.
pub fn anthropic_error_body(kind: &str, status: u16, message: String) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        })),
    )
        .into_response()
}

/// The mode's wire name, read from serde rather than a second copy of the mapping.
pub fn mode_name(mode: &ProtocolMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}
