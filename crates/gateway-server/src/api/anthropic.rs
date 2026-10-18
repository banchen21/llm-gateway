//! The Anthropic Messages endpoints.
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use gateway_core::convert::anthropic_in;
use gateway_core::ir::{ChatRequest, ContentPart};
use gateway_core::Error;
use serde_json::{json, Value};

use super::{anthropic_error, authorize, mode_name, serve, Protocol};
use crate::state::AppState;

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, &headers, &body, Protocol::Anthropic).await
}

/// Estimates the token count of an Anthropic request.
///
/// Anthropic's endpoint exists so clients can size a prompt before sending it. The count
/// here is derived from the request's characters rather than a real tokenizer, and the
/// response says so. It is deliberately not forwarded upstream: providers do not agree on
/// how to count, so a forwarded number would be no more trustworthy than this one.
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let snapshot = state.snapshot();
    if !snapshot.config.protocol_mode.serves_anthropic() {
        return anthropic_error(Error::not_found(format!(
            "this gateway is in `{}` mode and does not serve the Anthropic Messages API",
            mode_name(&snapshot.config.protocol_mode)
        )));
    }
    if let Err(error) = authorize(&snapshot.config, &headers) {
        return anthropic_error(error);
    }
    let request = match serde_json::from_slice::<Value>(&body)
        .map_err(|error| Error::bad_request(format!("the request body is not valid JSON: {error}")))
        .and_then(|value| anthropic_in::parse_request(&value))
    {
        Ok(request) => request,
        Err(error) => return anthropic_error(error),
    };
    (
        [("x-token-count", "approximate")],
        Json(json!({"input_tokens": estimate_tokens(&request)})),
    )
        .into_response()
}

/// Roughly four characters per token, the usual heuristic for mixed prose.
fn estimate_tokens(request: &ChatRequest) -> u32 {
    fn count(text: &str) -> u64 {
        text.chars().count() as u64
    }

    let mut total = request.system.as_deref().map_or(0, count);
    for message in &request.messages {
        for part in &message.content {
            total += match part {
                ContentPart::Text { text } | ContentPart::Thinking { text, .. } => count(text),
                ContentPart::RedactedThinking { data } => count(data),
                ContentPart::ToolUse { name, input, .. } => count(name) + count(&input.to_string()),
                ContentPart::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(ContentPart::as_text)
                    .map(count)
                    .sum(),
                // An image really costs far more than this, but the estimate is crude and
                // labelled as such in the response header.
                ContentPart::Image { data_base64, .. } => count(data_base64) / 4,
            };
        }
    }
    for tool in &request.tools {
        total += count(&tool.name) + count(&tool.input_schema.to_string());
        total += tool.description.as_deref().map_or(0, count);
    }
    (total / 4).min(u32::MAX as u64) as u32
}
