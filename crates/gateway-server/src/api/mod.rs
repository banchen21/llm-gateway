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
use axum::http::{header::HeaderValue, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use gateway_core::config::{Config, ProtocolMode, Provider, ProviderModel};
use gateway_core::convert::{anthropic_in, anthropic_out, openai_in, openai_out};
use gateway_core::ir::{ChatRequest, ContentPart, ModelEvent};
use gateway_core::upstream::{self, ProviderConfig, UpstreamCall};
use gateway_core::Error;
use serde_json::{json, Value};

use crate::logging::{self, Outcome, PromptShape, Record};
use crate::state::{AppState, Path};

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
    // The opening deadline stops applying once the stream starts; this one covers the
    // whole response, so a provider that falls silent mid-stream is still noticed.
    let idle = (snapshot.config.stream_idle_ms > 0)
        .then(|| Duration::from_millis(snapshot.config.stream_idle_ms));

    // A route that stalled a moment ago does not get to stall this request too. A route
    // can be marked by a mid-stream timeout even when only the idle watchdog is enabled,
    // so this must not depend on the opening deadline.
    let mut answering_elsewhere = false;
    if snapshot.config.failover && state.is_slow(&route_key) {
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
            answering_elsewhere = true;
        }
    }

    let mut usage = Arc::new(Mutex::new(None));
    // Recent behaviour decides whether this request starts down the direct path or the
    // proxy; a lone measurement is not enough to switch, and the loser is still probed.
    let chosen = state.choose_path();
    let client = match chosen {
        Path::Proxy => state
            .client_via_system_proxy()
            .unwrap_or_else(|| state.http.clone()),
        Path::Direct => state.http.clone(),
    };
    let opening = std::time::Instant::now();
    // A provider picked because the first one stalled never issued the reasoning blocks
    // in this conversation, so it gets a request it can actually validate.
    let alternate_request;
    let primary_request = if answering_elsewhere {
        alternate_request = for_alternate_provider(&request);
        &alternate_request
    } else {
        &request
    };
    let mut primary = call_upstream(
        state,
        &current_provider_id,
        &provider,
        primary_request,
        &upstream_model,
        client,
        CallLimits {
            opening: deadline,
            idle,
        },
    )
    .await;
    state.record_path(chosen, opening.elapsed(), primary.is_ok());

    // The first byte never arrived. Treat it as a stall rather than an answer: remember
    // it so the next request skips this route, and let another provider try now.
    if matches!(&primary, Err(Error::Timeout(_))) && deadline.is_some() {
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
                let alternate_request = for_alternate_provider(&request);
                primary = call_upstream(
                    state,
                    &current_provider_id,
                    &provider,
                    &alternate_request,
                    &upstream_model,
                    state.http.clone(),
                    CallLimits {
                        opening: deadline,
                        idle,
                    },
                )
                .await;
                // The alternate can stall too; record it so it is skipped next time.
                if matches!(&primary, Err(Error::Timeout(_))) {
                    state.mark_slow(&route_key);
                }
            }
        }
    }

    // Whichever path was not tried first gets one chance: a tunnel can be down while
    // direct still works, and the reverse.
    if matches!(&primary, Err(Error::Http(_))) {
        let other = match chosen {
            Path::Direct => Path::Proxy,
            Path::Proxy => Path::Direct,
        };
        let other_client = match other {
            Path::Proxy => state.client_via_system_proxy(),
            Path::Direct => Some(state.http.clone()),
        };
        if let Some(client) = other_client {
            let opening = std::time::Instant::now();
            primary = call_upstream(
                state,
                &current_provider_id,
                &provider,
                primary_request,
                &upstream_model,
                client,
                CallLimits {
                    opening: deadline,
                    idle,
                },
            )
            .await;
            state.record_path(other, opening.elapsed(), primary.is_ok());
        }
    }

    let attempted = match primary {
        Ok(events) => Ok(events),
        Err(error) if snapshot.config.failover && error.is_provider_refusal() => {
            match failover(
                state,
                &snapshot.config,
                &request,
                &current_provider_id,
                idle,
            )
            .await
            {
                Some(answer) => {
                    provider_name = answer.provider_name;
                    upstream_model = answer.upstream_model;
                    route_key = answer.route_key;
                    current_provider_id = answer.provider_id;
                    Ok(answer.events)
                }
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    };

    let events = match attempted {
        Ok(events) => events,
        Err(error) => {
            record_failure(
                state,
                FailedRequest {
                    alias: alias.clone(),
                    provider_name: provider_name.clone(),
                    upstream_model: upstream_model.clone(),
                    streaming,
                    prompt,
                    started,
                },
                &error,
            )
            .await;
            return Err(error);
        }
    };
    // How the stream ends decides whether this route is avoided or trusted next time.
    let events = watch_route(state.clone(), route_key.clone(), events);
    let events = logging::capture_usage(events, usage.clone());

    let id = protocol.new_id();
    let model = alias.clone();
    if !streaming {
        let mut body = complete(protocol, events, &id, &model).await;
        // Nothing has reached the client yet, so a failure partway through the upstream
        // stream can still be answered by another provider. A streaming response has
        // already committed its opening frames by then and cannot be replayed.
        if body.is_err() && snapshot.config.failover {
            if let Some(answer) = failover(
                state,
                &snapshot.config,
                &request,
                &current_provider_id,
                idle,
            )
            .await
            {
                provider_name = answer.provider_name;
                upstream_model = answer.upstream_model;
                route_key = answer.route_key;
                // A fresh sink: tokens the failed attempt produced must not be reported
                // as the answer that was actually returned.
                let retry_usage = Arc::new(Mutex::new(None));
                let retry_events = watch_route(state.clone(), route_key.clone(), answer.events);
                let retry_events = logging::capture_usage(retry_events, retry_usage.clone());
                usage = retry_usage;
                body = complete(protocol, retry_events, &id, &model).await;
            }
        }
        let body = match body {
            Ok(body) => body,
            Err(error) => {
                record_failure(
                    state,
                    FailedRequest {
                        alias: alias.clone(),
                        provider_name: provider_name.clone(),
                        upstream_model: upstream_model.clone(),
                        streaming: false,
                        prompt,
                        started,
                    },
                    &error,
                )
                .await;
                return Err(error);
            }
        };
        state
            .log
            .record(Record {
                alias,
                provider: provider_name.clone(),
                upstream_model: upstream_model.clone(),
                stream: false,
                prompt,
                duration_ms: started.elapsed().as_millis() as u64,
                outcome: Outcome::Served {
                    usage: logging::captured_usage(&usage),
                },
            })
            .await;
        return Ok(with_route(
            Json(body).into_response(),
            &provider_name,
            &upstream_model,
        ));
    }

    let frames: Pin<Box<dyn Stream<Item = gateway_core::Result<String>> + Send>> = match protocol {
        Protocol::Anthropic => Box::pin(anthropic_out::stream_response(events, id, model)),
        Protocol::OpenAi => Box::pin(openai_out::stream_response(events, id, model)),
    };
    // A streamed request is recorded once its last frame has been produced, which is the
    // only point at which the upstream's usage is known.
    let log = state.log.clone();
    // The stream owns its copies; the originals stay with this frame for the headers.
    let recorded_provider = provider_name.clone();
    let recorded_model = upstream_model.clone();
    let frames = async_stream::stream! {
        let mut frames = frames;
        while let Some(frame) = frames.next().await {
            yield frame;
        }
        log.record(Record {
            alias,
            provider: recorded_provider,
            upstream_model: recorded_model,
            stream: true,
            prompt,
            duration_ms: started.elapsed().as_millis() as u64,
            outcome: Outcome::Served { usage: logging::captured_usage(&usage) },
        })
        .await;
    };
    Ok(with_route(
        sse_response(frames),
        &provider_name,
        &upstream_model,
    ))
}

/// Drains a normalized stream into the response shape the calling protocol expects.
async fn complete(
    protocol: Protocol,
    events: gateway_core::ir::EventStream,
    id: &str,
    model: &str,
) -> gateway_core::Result<Value> {
    match protocol {
        Protocol::Anthropic => anthropic_out::complete(events, id, model).await,
        Protocol::OpenAi => openai_out::complete(events, id, model).await,
    }
}

/// What is known about a request that produced no response.
struct FailedRequest {
    alias: String,
    provider_name: String,
    upstream_model: String,
    streaming: bool,
    prompt: PromptShape,
    started: std::time::Instant,
}

/// Records a request that produced no response, and tells the operator when the failure
/// is theirs to fix.
///
/// A 5xx means nothing answered usefully — a dead route, a proxy that is not running, an
/// upstream that fell over. Refusals (401/403/429) stay quiet: another provider already
/// had its chance, and the log has them.
async fn record_failure(state: &Arc<AppState>, request: FailedRequest, error: &Error) {
    if error.http_status() >= 500 {
        crate::notify::network_problem(
            "无法连接上游，请检查网络连接或代理设置。网关会在恢复后自动重试。",
        );
    }
    state
        .log
        .record(Record {
            alias: request.alias,
            provider: request.provider_name,
            upstream_model: request.upstream_model,
            stream: request.streaming,
            prompt: request.prompt,
            duration_ms: request.started.elapsed().as_millis() as u64,
            outcome: Outcome::Failed {
                status: error.http_status(),
                message: error.to_string(),
            },
        })
        .await;
}

/// Tells the caller which provider and upstream model actually answered.
///
/// Aliases can be served by a fallback provider when the configured one refuses or
/// stalls, so the client cannot infer this from its request. Values are percent-encoded
/// because provider names are free-form and HTTP headers are not Unicode.
fn with_route(mut response: Response, provider: &str, model: &str) -> Response {
    for (name, value) in [
        ("x-llm-gateway-provider", provider),
        ("x-llm-gateway-upstream", model),
    ] {
        if let Ok(value) = HeaderValue::from_str(&percent_encode(value)) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b' ') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
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

/// The clocks a call runs under: how long its opening may take, and how long its stream
/// may stay silent.
#[derive(Clone, Copy)]
struct CallLimits {
    opening: Option<Duration>,
    idle: Option<Duration>,
}

/// One upstream call, optionally abandoned when the first event takes too long.
///
/// The deadline is what makes "working but slow" visible: a provider like that never
/// returns an error, so only a clock can tell the difference between thinking and stalling.
async fn call_upstream(
    state: &Arc<AppState>,
    provider_id: &str,
    config: &ProviderConfig,
    request: &ChatRequest,
    model: &str,
    client: reqwest::Client,
    limits: CallLimits,
) -> gateway_core::Result<gateway_core::ir::EventStream> {
    // Quota-aware ordering puts the account with the most usable credit first. Without
    // quota data the cursor still rotates, so equal accounts share the load.
    let (api_keys, ranked) = state.order_keys(provider_id, config.api_keys.clone());
    let key_start = if ranked { 0 } else { state.next_key_start() };
    let refusal_state = state.clone();
    let refusal_provider = provider_id.to_owned();
    let call = UpstreamCall {
        client,
        config: ProviderConfig {
            api_keys,
            ..config.clone()
        },
        request: request.clone(),
        model: model.to_owned(),
        key_start,
        idle: limits.idle,
        on_key_refused: Some(Arc::new(move |key: &str, status: u16| {
            refusal_state.record_key_refusal(&refusal_provider, key, status);
        })),
    };
    let Some(limit) = limits.opening else {
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
        Err(_) => Err(Error::Timeout(format!(
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

/// The request as a provider other than the one the alias names can accept.
///
/// Reasoning blocks belong to the model that issued them: a signature is only valid at
/// its issuer, and an endpoint running in thinking mode demands its own blocks back
/// (DeepSeek answers a replayed conversation with "The `content[].thinking` in the
/// thinking mode must be passed back to the API"). A fallback therefore gets the
/// conversation without reasoning and with thinking turned off, rather than a request the
/// new provider cannot validate.
fn for_alternate_provider(request: &ChatRequest) -> ChatRequest {
    fn strip(parts: &mut Vec<ContentPart>) {
        parts.retain(|part| {
            !matches!(
                part,
                ContentPart::Thinking { .. } | ContentPart::RedactedThinking { .. }
            )
        });
        for part in parts.iter_mut() {
            if let ContentPart::ToolResult { content, .. } = part {
                strip(content);
            }
        }
    }

    let mut request = request.clone();
    request.thinking = None;
    for message in &mut request.messages {
        strip(&mut message.content);
    }
    request
}

/// Watches a served stream so its route is remembered for what it did.
///
/// Any error is what the client receives, and recording it is what makes the next request
/// avoid the same route for longer. A route that reaches `Done` clears its stall history,
/// so a provider that recovered is trusted again immediately.
fn watch_route(
    state: Arc<AppState>,
    route_key: String,
    mut events: gateway_core::ir::EventStream,
) -> gateway_core::ir::EventStream {
    Box::pin(async_stream::stream! {
        while let Some(result) = events.next().await {
            match &result {
                Ok(ModelEvent::Done(_)) => state.mark_healthy(&route_key),
                Err(_) => state.mark_slow(&route_key),
                _ => {}
            }
            let failed = result.is_err();
            yield result;
            if failed {
                break;
            }
        }
    })
}

/// A provider that answered in place of the one the alias named.
struct FallbackAnswer {
    events: gateway_core::ir::EventStream,
    provider_name: String,
    upstream_model: String,
    provider_id: String,
    route_key: String,
}

/// Asks the other enabled providers, in configuration order, when the one the alias
/// names refuses the request or its stream fails before anything reached the client.
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
    idle: Option<Duration>,
) -> Option<FallbackAnswer> {
    let request = for_alternate_provider(request);
    for candidate in &config.providers {
        if !candidate.enabled || candidate.id == primary_provider_id {
            continue;
        }
        let Some(model) = candidate.models.iter().find(|model| model.enabled) else {
            continue;
        };
        match call_upstream(
            state,
            &candidate.id,
            &provider_config(candidate, model),
            &request,
            &model.upstream_model,
            state.http.clone(),
            CallLimits {
                opening: None,
                idle,
            },
        )
        .await
        {
            Ok(events) => {
                tracing::warn!(
                    provider = %candidate.name,
                    model = %model.upstream_model,
                    "the named provider did not answer; a fallback did"
                );
                return Some(FallbackAnswer {
                    events,
                    provider_name: candidate.name.clone(),
                    upstream_model: model.upstream_model.clone(),
                    provider_id: candidate.id.clone(),
                    route_key: format!("{}/{}", candidate.id, model.id),
                });
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
