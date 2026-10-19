//! Shared harness for driving the gateway against a stand-in upstream.
//!
//! Streaming is only testable deterministically against recorded bytes, so every test
//! replays a fixed SSE fixture and asserts on what the gateway produced — and on what it
//! sent upstream, which is how the request-side conversions are pinned down.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::routing::post;
use axum::Router;
use gateway_core::config::{Config, ProtocolMode, Provider, ProviderModel, ServerConfig};
use gateway_core::upstream::{MaxTokensField, ProviderProtocol};
use gateway_server::state::AppState;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// A stand-in upstream: it replays one fixed SSE body and records what it was sent.
pub struct Upstream {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

impl Upstream {
    /// The body of the most recent request the gateway sent upstream.
    pub fn last_request(&self) -> Value {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the gateway should have sent a request upstream")
    }
}

/// Replays `sse` for every request to `path`.
pub async fn start_upstream(path: &'static str, sse: &'static str) -> Upstream {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let app = Router::new().route(
        path,
        post(move |body: Bytes| {
            let recorded = recorded.clone();
            async move {
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    recorded.lock().unwrap().push(value);
                }
                ([(CONTENT_TYPE, "text/event-stream")], sse)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Upstream {
        base_url: format!("http://{address}"),
        requests,
    }
}

/// Starts the gateway in front of `upstream` and returns its base URL.
pub async fn start_gateway(upstream: &Upstream, protocol: ProviderProtocol) -> String {
    start_gateway_with(
        upstream,
        protocol,
        ProtocolMode::PrimaryAnthropic,
        MaxTokensField::default(),
    )
    .await
}

pub async fn start_gateway_with(
    upstream: &Upstream,
    protocol: ProviderProtocol,
    mode: ProtocolMode,
    max_tokens_field: MaxTokensField,
) -> String {
    let config = Config {
        protocol_mode: mode,
        providers: vec![Provider {
            id: "p1".into(),
            name: "mock".into(),
            protocol,
            base_url: upstream.base_url.clone(),
            api_key: "test-key".into(),
            api_keys: Vec::new(),
            enabled: true,
            custom_headers: BTreeMap::new(),
            models: vec![ProviderModel {
                id: "m1".into(),
                alias: "test-model".into(),
                upstream_model: "upstream-model".into(),
                enabled: true,
                max_output_tokens: None,
                max_tokens_field,
                extra: BTreeMap::new(),
            }],
        }],
        server: ServerConfig {
            bind: "127.0.0.1".into(),
            port: 0,
            api_key: None,
            api_keys: Vec::new(),
        },
        ..Config::default()
    };

    serve_app(config, temp_config_path()).await
}

/// A provider pointing at `upstream`, for tests that need to add or replace providers.
pub fn provider_for(upstream: &Upstream, protocol: ProviderProtocol) -> Provider {
    Provider {
        id: "p1".into(),
        name: "mock".into(),
        protocol,
        base_url: upstream.base_url.clone(),
        api_key: "test-key".into(),
        api_keys: Vec::new(),
        enabled: true,
        custom_headers: BTreeMap::new(),
        models: vec![ProviderModel {
            id: "m1".into(),
            alias: "test-model".into(),
            upstream_model: "upstream-model".into(),
            enabled: true,
            max_output_tokens: None,
            max_tokens_field: MaxTokensField::default(),
            extra: BTreeMap::new(),
        }],
    }
}

/// A unique configuration path per test.
///
/// Each test gets its own directory, not just its own file name, because the request log
/// lives beside the configuration and tests would otherwise share one log.
pub fn temp_config_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "llm-gateway-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .join("config.json")
}

/// Serves `app` on an ephemeral port and returns its base URL.
pub async fn serve_app(config: Config, config_path: PathBuf) -> String {
    let state = AppState::new(config, config_path).unwrap();
    serve_state(state).await
}

pub async fn serve_state(state: Arc<AppState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, gateway_server::app(state))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

/// The `(event, data)` pairs in an SSE body, in order.
pub fn parse_sse(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = Value::Null;
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_owned();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(rest).unwrap_or(Value::Null);
                }
            }
            (name, data)
        })
        .collect()
}

/// The event names in order, which is what most assertions care about.
pub fn event_names(frames: &[(String, Value)]) -> Vec<&str> {
    frames.iter().map(|(name, _)| name.as_str()).collect()
}

pub fn messages_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
    })
}

pub async fn post_messages(gateway: &str, payload: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/messages"))
        .json(payload)
        .send()
        .await
        .unwrap()
}

pub async fn post_chat(gateway: &str, payload: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(payload)
        .send()
        .await
        .unwrap()
}

pub fn chat_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
    })
}
