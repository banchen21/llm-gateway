//! The OpenAI Chat Completions endpoint and model discovery.
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use super::{authorize, openai_error, serve, Protocol};
use crate::state::AppState;

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, &headers, &body, Protocol::OpenAi).await
}

#[derive(Deserialize)]
pub struct ModelsQuery {
    /// Present so an Anthropic-flavoured caller that sends `?limit=` is not rejected.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Lists every model alias the gateway can route, in the OpenAI listing shape.
///
/// Available in either mode: a client that speaks only one protocol still needs to
/// discover what the gateway offers.
pub async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(_query): Query<ModelsQuery>,
) -> Response {
    let snapshot = state.snapshot();
    if let Err(error) = authorize(&snapshot.config, &headers) {
        return openai_error(error);
    }
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    Json(json!({
        "object": "list",
        "data": snapshot
            .router
            .aliases()
            .into_iter()
            .map(|alias| json!({
                "id": alias,
                "object": "model",
                "created": created,
                "owned_by": "llm-gateway",
            }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}
