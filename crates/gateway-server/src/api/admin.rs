//! The administrative API the GUI drives.
//!
//! The configuration is read and written as one document. Editing it as a whole keeps the
//! GUI free of nested create/update/delete plumbing for providers and their models, and
//! the gateway is a single-user local tool, so a lost update is not a real risk.
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use gateway_core::config::{self, Config};
use gateway_core::Error;
use serde::Deserialize;
use serde_json::json;

use super::openai_error;
use crate::state::AppState;

/// Guards every administrative route.
///
/// Enabling the web GUI is what makes these routes reachable from elsewhere, and the
/// configuration holds provider API keys, so access is gated on the configured token
/// whenever one is set. Loopback-only access is allowed without one.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Error> {
    let snapshot = state.snapshot();
    let Some(expected) = snapshot
        .config
        .web_ui
        .token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return Ok(());
    };
    let presented = headers
        .get("x-gateway-token")
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token)
        });
    if presented == Some(expected) {
        Ok(())
    } else {
        Err(Error::unauthorized("a valid gateway token is required"))
    }
}

/// The whole configuration, with a summary the GUI can render without recomputing it.
pub async fn get_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&state, &headers) {
        return openai_error(error);
    }
    let snapshot = state.snapshot();
    Json(json!({
        "config": snapshot.config,
        "aliases": snapshot.router.aliases(),
        "path": state.config_path.to_string_lossy(),
    }))
    .into_response()
}

/// Replaces the configuration, publishing it only if validation and the write both succeed.
pub async fn put_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut config): Json<Config>,
) -> Response {
    if let Err(error) = authorize(&state, &headers) {
        return openai_error(error);
    }
    assign_missing_ids(&mut config);
    match state.replace(config) {
        Ok(()) => Json(json!({"saved": true})).into_response(),
        // A rejected configuration is the caller's to fix, even though the error type
        // describes a gateway problem.
        Err(error) => openai_error(Error::Request {
            status: 400,
            message: error.to_string(),
        }),
    }
}

/// Gives newly added providers and models an identity.
///
/// The GUI does not generate identifiers, so anything it sends without one is assigned
/// here; without stable ids a later rename would orphan the alias index.
fn assign_missing_ids(config: &mut Config) {
    for provider in &mut config.providers {
        if provider.id.trim().is_empty() {
            provider.id = uuid::Uuid::new_v4().simple().to_string();
        }
        for model in &mut provider.models {
            if model.id.trim().is_empty() {
                model.id = uuid::Uuid::new_v4().simple().to_string();
            }
        }
        for key in &mut provider.api_keys {
            if key.id.trim().is_empty() {
                key.id = uuid::Uuid::new_v4().simple().to_string();
            }
        }
    }
    for key in &mut config.server.api_keys {
        if key.id.trim().is_empty() {
            key.id = uuid::Uuid::new_v4().simple().to_string();
        }
    }
    if let Some(default) = config.default_provider_id.as_deref() {
        let known = config
            .providers
            .iter()
            .any(|provider| provider.id == default);
        if !known {
            config.default_provider_id = None;
        }
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The most recent request records, oldest first.
pub async fn logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    if let Err(error) = authorize(&state, &headers) {
        return openai_error(error);
    }
    // Bounded so a caller cannot ask the gateway to materialize an unbounded log.
    let limit = query.limit.unwrap_or(200).min(2_000);
    Json(json!({"entries": state.log.recent(limit).await})).into_response()
}

/// Whether the config file on disk is currently readable, for the GUI's status line.
pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    let loaded = config::load(&state.config_path).is_ok();
    Json(json!({
        "ok": true,
        "protocol_mode": snapshot.config.protocol_mode,
        "providers": snapshot.config.providers.len(),
        "aliases": snapshot.router.aliases().len(),
        "web_ui_enabled": snapshot.config.web_ui.enabled,
        // False means the binary was built without the GUI assets, which would make the
        // web GUI answer 404 no matter how it is configured.
        "web_ui_assets": crate::webui::assets_are_embedded(),
        "config_readable": loaded,
        "inbound_api_key_required": !super::inbound_keys(&snapshot.config).is_empty(),
    }))
    .into_response()
}

/// Empties the request log.
pub async fn clear_logs(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&state, &headers) {
        return openai_error(error);
    }
    let cleared = state.log.clear().await;
    Json(json!({"cleared": cleared})).into_response()
}

/// The provider fields a model listing needs, which may be unsaved drafts from the GUI.
#[derive(Deserialize)]
pub struct ProviderProbe {
    pub protocol: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_keys: Vec<gateway_core::config::ApiKey>,
    #[serde(default)]
    pub custom_headers: std::collections::BTreeMap<String, String>,
}

/// Asks the upstream endpoint which models it serves.
///
/// The one call every compatible endpoint answers, which is what makes it the cheapest
/// way to fill the model table without typing names by hand.
pub async fn provider_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(probe): Json<ProviderProbe>,
) -> Response {
    if let Err(error) = authorize(&state, &headers) {
        return openai_error(error);
    }
    let keys: Vec<String> = std::iter::once(probe.api_key.as_str())
        .chain(probe.api_keys.iter().map(|key| key.key.as_str()))
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if keys.is_empty() {
        return openai_error(Error::bad_request(
            "fill in an API key before fetching models",
        ));
    }
    if probe.base_url.trim().is_empty() {
        return openai_error(Error::bad_request("fill in a base URL first"));
    }

    let url = format!("{}/v1/models", probe.base_url.trim_end_matches('/'));
    let anthropic = probe.protocol == "anthropic";
    let response = match gateway_core::upstream::send_with_keys(&keys, 0, |key| {
        // A model listing is a convenience, not a request worth waiting minutes for.
        let mut request = state
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(30));
        request = if anthropic {
            request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
        } else {
            request.bearer_auth(key)
        };
        for (name, value) in &probe.custom_headers {
            request = request.header(name, value);
        }
        request
    })
    .await
    {
        Ok(response) => response,
        Err(error) => return openai_error(error),
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message: String = body.chars().take(500).collect();
        return openai_error(Error::upstream(status, message));
    }

    let value: serde_json::Value = match response.json().await {
        Ok(value) => value,
        Err(error) => {
            return openai_error(Error::upstream(
                reqwest::StatusCode::BAD_GATEWAY,
                format!("the model list was not JSON: {error}"),
            ))
        }
    };

    // Both protocols answer with {"data":[{"id": …}]}; a bare array is accepted too.
    let list = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());
    let mut models: Vec<String> = list
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    models.dedup();
    Json(json!({"models": models})).into_response()
}
