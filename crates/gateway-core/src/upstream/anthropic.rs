//! Speaks the Anthropic Messages API upstream, both directions.
//!
//! The outbound half turns an IR request into a `/v1/messages` body; the inbound half
//! turns that endpoint's SSE stream back into normalized events. Because the IR follows
//! Anthropic's content-block model this adapter is mostly a structural pass-through, but
//! it still has to drop what Anthropic will not accept: unsigned reasoning blocks and
//! images outside user messages.
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use eventsource_stream::Eventsource;
use futures_util::{pin_mut, Stream, StreamExt};
use serde_json::{json, Value};

use super::{send_with_keys, UpstreamCall};
use crate::ir::{
    ChatRequest, ContentPart, EventStream, FinishReason, ModelEvent, Role, ThinkingConfig,
    ToolChoice, Usage,
};
use crate::{Error, Result};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Used when neither the client nor the provider config caps output length, since
/// Anthropic rejects a request without `max_tokens`.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// The largest upstream error body echoed back to the caller.
const MAX_ERROR_BODY_CHARS: usize = 500;

pub async fn stream(call: UpstreamCall) -> Result<EventStream> {
    let body = request_body(&call)?;
    let response = send_with_keys(&call.config.api_keys, call.key_start, |key| {
        let mut request = call
            .client
            .post(call.config.endpoint("/v1/messages"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("x-api-key", key)
            .json(&body);
        for (name, value) in &call.config.custom_headers {
            request = request.header(name, value);
        }
        request
    })
    .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::upstream(status, truncate(body)));
    }

    Ok(Box::pin(parse_stream(response)))
}

fn request_body(call: &UpstreamCall) -> Result<Value> {
    let request = &call.request;
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(call.model));
    // The adapter always streams; the gateway buffers a stream when a non-streaming
    // client asked for one, which keeps a single upstream behaviour to reason about.
    body.insert("stream".into(), json!(true));
    body.insert(
        "max_tokens".into(),
        json!(request
            .max_tokens
            .or(call.config.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)),
    );
    if let Some(system) = &request.system {
        body.insert("system".into(), json!([{"type": "text", "text": system}]));
    }
    body.insert(
        "messages".into(),
        json!(messages(
            request,
            requires_signature(&call.config.base_url)
        )?),
    );
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !request.stop_sequences.is_empty() {
        body.insert("stop_sequences".into(), json!(request.stop_sequences));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!(request
                .tools
                .iter()
                .map(|tool| {
                    let mut value = json!({
                        "name": tool.name,
                        "input_schema": tool.input_schema,
                    });
                    if let Some(description) = &tool.description {
                        value["description"] = json!(description);
                    }
                    value
                })
                .collect::<Vec<_>>()),
        );
    }
    if let Some(choice) = &request.tool_choice {
        body.insert("tool_choice".into(), tool_choice(choice));
    }
    if let Some(thinking) = thinking(&request.thinking) {
        body.insert("thinking".into(), thinking);
    }
    call.config.merge_extra(&mut body)?;
    Ok(Value::Object(body))
}

fn tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::Any => json!({"type": "any"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Tool(name) => json!({"type": "tool", "name": name}),
    }
}

/// Anthropic requires `budget_tokens` alongside an explicit `enabled` thinking config, so a
/// config without one cannot be expressed and is left out. An adaptive config is a shape of
/// its own and is passed on as the client wrote it.
fn thinking(config: &Option<ThinkingConfig>) -> Option<Value> {
    let config = config.as_ref().filter(|config| config.enabled)?;
    if config.adaptive {
        return Some(json!({"type": "adaptive"}));
    }
    config
        .budget_tokens
        .map(|budget| json!({"type": "enabled", "budget_tokens": budget}))
}

/// Whether this endpoint demands a signature on every thinking block.
///
/// Only Anthropic's own API does; compatible vendors accept a block without one, and
/// some require the block to be present.
fn requires_signature(base_url: &str) -> bool {
    let host = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("");
    host.eq_ignore_ascii_case("api.anthropic.com")
        || host.to_ascii_lowercase().ends_with(".anthropic.com")
}

fn messages(request: &ChatRequest, strict: bool) -> Result<Vec<Value>> {
    let mut output: Vec<Value> = Vec::new();
    for message in &request.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content = blocks(role, &message.content, strict)?;
        if content.is_empty() {
            continue;
        }
        push_message(&mut output, role, content);
    }
    Ok(output)
}

/// Appends to the previous message when the role repeats.
///
/// Anthropic requires strictly alternating roles, but clients legitimately send two
/// consecutive user messages (for instance a text turn followed by tool results).
fn push_message(output: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if let Some(last) = output.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(content);
                return;
            }
        }
    }
    output.push(json!({"role": role, "content": content}));
}

fn blocks(role: &str, parts: &[ContentPart], strict: bool) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for part in parts {
        match part {
            // Anthropic rejects empty text blocks.
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => output.push(json!({"type": "text", "text": text})),
            ContentPart::Thinking {
                text,
                signature: Some(signature),
            } => output.push(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            })),
            // Reasoning that arrived from an OpenAI-compatible upstream has no
            // signature. The official API rejects such a block, but DeepSeek-compatible
            // endpoints not only accept it — they reject the follow-up when it is
            // missing, so dropping it would break every conversation that changed
            // providers mid-way.
            ContentPart::Thinking {
                text,
                signature: None,
            } if !strict => output.push(json!({"type": "thinking", "thinking": text})),
            ContentPart::Thinking {
                signature: None, ..
            } => {}
            ContentPart::RedactedThinking { data } => {
                output.push(json!({"type": "redacted_thinking", "data": data}))
            }
            ContentPart::ToolUse { id, name, input } => output.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": tool_result_content(content)?,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                output.push(block);
            }
            ContentPart::Image {
                mime_type,
                data_base64,
            } if role == "user" => output.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime_type, "data": data_base64},
            })),
            ContentPart::Image { .. } => {
                return Err(Error::bad_request(
                    "images are only supported in user messages",
                ))
            }
        }
    }
    Ok(output)
}

/// A tool result's content is a bare string when it is a single text block, which is
/// both what clients send and what Anthropic prefers.
fn tool_result_content(parts: &[ContentPart]) -> Result<Value> {
    match parts {
        [] => Ok(json!("")),
        [ContentPart::Text { text }] => Ok(json!(text)),
        parts => Ok(json!(blocks("user", parts, false)?)),
    }
}

/// Parses the upstream SSE stream into normalized events.
fn parse_stream(response: reqwest::Response) -> impl Stream<Item = Result<ModelEvent>> {
    async_stream::try_stream! {
        let chunks = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from));
        let source = chunks.eventsource();
        pin_mut!(source);

        yield ModelEvent::Start { message_id: None };

        // Block index -> block kind, so a delta or a stop knows what it belongs to.
        let mut block_kinds = BTreeMap::<usize, String>::new();
        let mut thinking_signatures = BTreeMap::<usize, String>::new();
        let mut saw_tool = false;
        let mut finish: Option<FinishReason> = None;
        let mut usage = Usage::default();

        loop {
            let Some(event) = source.next().await else { break };
            let event = event.map_err(sse_error)?;
            let value: Value = serde_json::from_str(&event.data).map_err(|error| {
                Error::bad_request(format!("Anthropic sent unparseable event data: {error}"))
            })?;
            if let Some(error) = event_error(&value) {
                Err(error)?;
            }
            // Some servers leave the SSE `event:` field empty and rely on the JSON type.
            let kind = match event.event.as_str() {
                "" | "message" => value.get("type").and_then(Value::as_str).unwrap_or(""),
                kind => kind,
            };

            match kind {
                "message_start" => {
                    if let Some(usage_value) = value.pointer("/message/usage") {
                        usage.merge(parse_usage(usage_value));
                    }
                }
                "content_block_start" => {
                    let index = index_of(&value)?;
                    let block = value.get("content_block").unwrap_or(&Value::Null);
                    let block_kind = required_str(block, "type")?;
                    block_kinds.insert(index, block_kind.to_owned());
                    match block_kind {
                        "text" => yield ModelEvent::TextStart,
                        "thinking" => {
                            thinking_signatures.insert(index, String::new());
                            yield ModelEvent::ThinkingStart;
                        }
                        "tool_use" => {
                            saw_tool = true;
                            yield ModelEvent::ToolCallStart {
                                index,
                                call_id: required_str(block, "id")?.to_owned(),
                                name: required_str(block, "name")?.to_owned(),
                            };
                        }
                        // Opaque and unsigned; nothing to stream, but it must survive a
                        // round trip, which `complete` handles from the request side.
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let index = index_of(&value)?;
                    let delta = value.get("delta").unwrap_or(&Value::Null);
                    let delta_kind = required_str(delta, "type")?;
                    // A delta can arrive without a preceding start block, in which case the
                    // delta's own kind is the only clue to what the block is.
                    let inferred = match block_kinds.entry(index) {
                        Entry::Vacant(entry) => match delta_kind {
                            "text_delta" => {
                                entry.insert("text".into());
                                Some(ModelEvent::TextStart)
                            }
                            "thinking_delta" | "signature_delta" => {
                                entry.insert("thinking".into());
                                thinking_signatures.insert(index, String::new());
                                Some(ModelEvent::ThinkingStart)
                            }
                            _ => None,
                        },
                        Entry::Occupied(_) => None,
                    };
                    if let Some(event) = inferred {
                        yield event;
                    }
                    match delta_kind {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                yield ModelEvent::TextDelta(text.to_owned());
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                yield ModelEvent::ThinkingDelta(text.to_owned());
                            }
                        }
                        // The signature is not content: it is accumulated so the
                        // completed block can be replayed on the next turn.
                        "signature_delta" => {
                            if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                                thinking_signatures.entry(index).or_default().push_str(signature);
                            }
                        }
                        "input_json_delta" => {
                            if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                                yield ModelEvent::ToolCallArgumentsDelta {
                                    index,
                                    delta: fragment.to_owned(),
                                };
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = index_of(&value)?;
                    if let Some(block_kind) = block_kinds.remove(&index) {
                        yield close_block(index, &block_kind, &mut thinking_signatures);
                    }
                }
                "message_delta" => {
                    if let Some(usage_value) = value.get("usage") {
                        usage.merge(parse_usage(usage_value));
                    }
                    if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        finish = Some(finish_reason(reason, saw_tool));
                    }
                }
                "message_stop" => {
                    // Close anything the upstream left open before terminating.
                    for (index, block_kind) in std::mem::take(&mut block_kinds) {
                        yield close_block(index, &block_kind, &mut thinking_signatures);
                    }
                    yield ModelEvent::Usage(usage);
                    yield ModelEvent::Done(resolve_finish(finish, saw_tool));
                    return;
                }
                // `ping` and any future bookkeeping event.
                _ => {}
            }
        }

        // The connection closed without `message_stop`; close what is open and report
        // what was seen rather than failing an otherwise complete response.
        for (index, block_kind) in std::mem::take(&mut block_kinds) {
            yield close_block(index, &block_kind, &mut thinking_signatures);
        }
        yield ModelEvent::Usage(usage);
        yield ModelEvent::Done(resolve_finish(finish, saw_tool));
    }
}

fn close_block(
    index: usize,
    block_kind: &str,
    thinking_signatures: &mut BTreeMap<usize, String>,
) -> ModelEvent {
    match block_kind {
        "text" => ModelEvent::TextEnd,
        "thinking" => ModelEvent::ThinkingEnd {
            signature: thinking_signatures
                .remove(&index)
                .filter(|signature| !signature.is_empty()),
        },
        _ => ModelEvent::ToolCallEnd { index },
    }
}

fn finish_reason(reason: &str, saw_tool: bool) -> FinishReason {
    match reason {
        "tool_use" => FinishReason::ToolUse,
        // A truncated response is reported as truncation even if tool calls were seen,
        // because those calls are incomplete.
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "end_turn" | "stop_sequence" | "pause_turn" | "refusal" => FinishReason::Stop,
        // Any unrecognized reason falls back to whether tools were emitted.
        _ if saw_tool => FinishReason::ToolUse,
        _ => FinishReason::Stop,
    }
}

fn resolve_finish(finish: Option<FinishReason>, saw_tool: bool) -> FinishReason {
    match finish {
        Some(FinishReason::Length) => FinishReason::Length,
        _ if saw_tool => FinishReason::ToolUse,
        Some(finish) => finish,
        None => FinishReason::Stop,
    }
}

fn parse_usage(value: &Value) -> Usage {
    let cache_read = tokens(value, "cache_read_input_tokens");
    let cache_write = tokens(value, "cache_creation_input_tokens");
    Usage {
        // Anthropic's `input_tokens` excludes cached tokens; the normalized field
        // includes them, so the cached counts are folded back in here.
        input_tokens: tokens(value, "input_tokens")
            .saturating_add(cache_read)
            .saturating_add(cache_write),
        output_tokens: tokens(value, "output_tokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

fn tokens(value: &Value, field: &str) -> u32 {
    value
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

fn index_of(value: &Value) -> Result<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| Error::bad_request("Anthropic event is missing `index`"))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        Error::bad_request(format!("Anthropic event field `{field}` must be a string"))
    })
}

/// Detects an upstream error delivered inside the stream, which happens after the
/// HTTP status has already been sent as a success.
fn event_error(value: &Value) -> Option<Error> {
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("Anthropic reported an error event");
    Some(Error::provider(format!(
        "Anthropic upstream error: {message}"
    )))
}

fn sse_error(error: eventsource_stream::EventStreamError<Error>) -> Error {
    match error {
        eventsource_stream::EventStreamError::Transport(error) => error,
        eventsource_stream::EventStreamError::Utf8(error) => {
            Error::bad_request(format!("Anthropic SSE was not valid UTF-8: {error}"))
        }
        eventsource_stream::EventStreamError::Parser(error) => {
            Error::bad_request(format!("Anthropic SSE could not be parsed: {error}"))
        }
    }
}

fn truncate(body: String) -> String {
    if body.chars().count() <= MAX_ERROR_BODY_CHARS {
        return body;
    }
    let end = body
        .char_indices()
        .nth(MAX_ERROR_BODY_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(body.len());
    format!("{}…", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::requires_signature;

    #[test]
    fn only_the_official_api_requires_a_signature() {
        assert!(requires_signature("https://api.anthropic.com"));
        assert!(requires_signature("https://api.anthropic.com/v1"));
        assert!(requires_signature("https://eu.anthropic.com"));
        assert!(!requires_signature("https://api.deepseek.com/anthropic"));
        assert!(!requires_signature("http://127.0.0.1:8080"));
        assert!(!requires_signature(""));
    }
}
