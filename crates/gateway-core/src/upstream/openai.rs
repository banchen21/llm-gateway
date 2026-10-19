//! Speaks the OpenAI Chat Completions API upstream, both directions.
//!
//! The outbound half flattens the IR's content blocks into OpenAI's message model: the
//! system prompt becomes a leading `system` message, assistant tool calls become
//! `tool_calls`, and tool results become standalone `role: "tool"` messages. The inbound
//! half rebuilds the block structure from the delta stream.
use std::collections::BTreeMap;

use eventsource_stream::Eventsource;
use futures_util::{pin_mut, Stream, StreamExt};
use serde_json::{json, Map, Value};

use super::{send_with_keys, UpstreamCall};
use crate::ir::{
    normalized_tool_call_id, synthetic_tool_call_id, ChatRequest, ContentPart, EventStream,
    FinishReason, ModelEvent, Role, ToolChoice, Usage,
};
use crate::{Error, Result};

/// Used when neither the client nor the provider config caps output length.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

const MAX_ERROR_BODY_CHARS: usize = 500;

pub async fn stream(call: UpstreamCall) -> Result<EventStream> {
    let body = request_body(&call)?;
    let response = send_with_keys(&call.config.api_keys, call.key_start, |key| {
        let mut request = call
            .client
            .post(call.config.endpoint("/v1/chat/completions"))
            .bearer_auth(key)
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
    let mut body = Map::new();
    body.insert("model".into(), json!(call.model));
    body.insert("stream".into(), json!(true));
    // Without this an OpenAI-compatible upstream reports no usage for a streamed
    // response at all, and the gateway would report zero tokens.
    body.insert("stream_options".into(), json!({"include_usage": true}));
    body.insert(
        call.config.max_tokens_field.name().into(),
        json!(request
            .max_tokens
            .or(call.config.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)),
    );
    body.insert("messages".into(), json!(messages(request)?));
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !request.stop_sequences.is_empty() {
        body.insert("stop".into(), json!(request.stop_sequences));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!(request
                .tools
                .iter()
                .map(|tool| {
                    let mut function = Map::new();
                    function.insert("name".into(), json!(tool.name));
                    function.insert("parameters".into(), tool.input_schema.clone());
                    if let Some(description) = &tool.description {
                        function.insert("description".into(), json!(description));
                    }
                    json!({"type": "function", "function": function})
                })
                .collect::<Vec<_>>()),
        );
    }
    if let Some(choice) = &request.tool_choice {
        body.insert("tool_choice".into(), tool_choice(choice));
    }
    call.config.merge_extra(&mut body)?;
    Ok(Value::Object(body))
}

fn tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        // Anthropic's `any` means "you must call some tool", which OpenAI calls
        // `required`.
        ToolChoice::Any => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

fn messages(request: &ChatRequest) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    if let Some(system) = &request.system {
        output.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::User => user_messages(message, &mut output)?,
            Role::Assistant => assistant_message(message, &mut output)?,
        }
    }
    Ok(output)
}

/// Expands one user message, whose tool results belong in separate `tool` messages.
///
/// Chat Completions requires tool results to be their own messages and to follow the
/// assistant turn that requested them, so they are emitted before any accompanying text.
fn user_messages(message: &crate::ir::Message, output: &mut Vec<Value>) -> Result<()> {
    for part in &message.content {
        if let ContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = part
        {
            let mut text = tool_result_text(content);
            if *is_error && !text.is_empty() {
                text.insert_str(0, "Error: ");
            }
            output.push(json!({
                "role": "tool",
                "tool_call_id": normalized_tool_call_id(tool_use_id),
                "content": text,
            }));
        }
    }

    let content = user_content(&message.content)?;
    if !content_is_empty(&content) {
        output.push(json!({"role": "user", "content": content}));
    }
    Ok(())
}

/// User content is absent when there is neither text nor an image, which happens for a
/// message that carried nothing but tool results.
fn content_is_empty(content: &Value) -> bool {
    match content {
        Value::String(text) => text.is_empty(),
        Value::Array(parts) => parts.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

/// A tool result's text. Images have no representation in a `tool` message, so they are
/// marked in place rather than dropped silently.
fn tool_result_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
            ContentPart::Image { .. } => text.push_str("[image omitted]"),
            _ => {}
        }
    }
    text
}

/// User content is a bare string when it carries no images, which is what most
/// OpenAI-compatible servers expect, and a part array otherwise.
fn user_content(parts: &[ContentPart]) -> Result<Value> {
    let has_image = parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image { .. }));
    if !has_image {
        let mut text = String::new();
        for part in parts {
            if let ContentPart::Text { text: part } = part {
                text.push_str(part);
            }
        }
        return Ok(json!(text));
    }

    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                content.push(json!({"type": "text", "text": text}))
            }
            ContentPart::Text { .. } => {}
            ContentPart::Image {
                mime_type,
                data_base64,
            } => content.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime_type};base64,{data_base64}")},
            })),
            _ => {}
        }
    }
    Ok(Value::Array(content))
}

/// Collapses one assistant message into Chat Completions form.
fn assistant_message(message: &crate::ir::Message, output: &mut Vec<Value>) -> Result<()> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
            // Reasoning is replayed whatever its origin: providers that think in Chat
            // Completions mode (DeepSeek among them) reject a follow-up that omits the
            // previous turn's `reasoning_content`. The signature means nothing to them,
            // so only the text travels.
            ContentPart::Thinking { text: part, .. } => reasoning.push_str(part),
            ContentPart::ToolUse { id, name, input } => calls.push(json!({
                "id": normalized_tool_call_id(id),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input)?,
                },
            })),
            _ => {}
        }
    }

    // Chat Completions rejects an empty assistant content string: an assistant turn
    // that only calls tools sends `null`, and one with nothing at all is not sent.
    if text.is_empty() && calls.is_empty() && reasoning.is_empty() {
        return Ok(());
    }
    let mut value = Map::new();
    value.insert("role".into(), json!("assistant"));
    value.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !reasoning.is_empty() {
        value.insert("reasoning_content".into(), json!(reasoning));
    }
    if !calls.is_empty() {
        value.insert("tool_calls".into(), Value::Array(calls));
    }
    output.push(Value::Object(value));
    Ok(())
}

/// A tool call being reassembled across deltas.
#[derive(Default)]
struct ToolState {
    call_id: String,
    name: String,
    arguments: String,
    /// How much of `arguments` has already been forwarded downstream.
    emitted: usize,
    started: bool,
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

        let mut tools = BTreeMap::<usize, ToolState>::new();
        let mut text_open = false;
        let mut thinking_open = false;
        let mut usage = Usage::default();
        let mut finish: Option<FinishReason> = None;
        let mut saw_done = false;

        loop {
            let Some(event) = source.next().await else { break };
            let event = event.map_err(sse_error)?;
            if event.data.trim() == "[DONE]" {
                saw_done = true;
                break;
            }
            let value: Value = serde_json::from_str(&event.data).map_err(|error| {
                Error::bad_request(format!("OpenAI sent unparseable event data: {error}"))
            })?;
            if let Some(error) = event_error(&value) {
                Err(error)?;
            }
            // Usage arrives on the final chunk, whose `choices` array is empty.
            if let Some(reported) = value.get("usage").filter(|usage| !usage.is_null()) {
                usage.merge(parse_usage(reported));
            }

            let Some(choice) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
            else {
                continue;
            };
            let delta = choice.get("delta").unwrap_or(&Value::Null);

            // Reasoning has no standard field name: OpenAI-compatible servers use
            // either `reasoning_content` or `reasoning`.
            let reasoning = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty());
            if let Some(reasoning) = reasoning {
                if !thinking_open {
                    thinking_open = true;
                    yield ModelEvent::ThinkingStart;
                }
                yield ModelEvent::ThinkingDelta(reasoning.to_owned());
            }

            if let Some(content) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                if thinking_open {
                    thinking_open = false;
                    yield ModelEvent::ThinkingEnd { signature: None };
                }
                if !text_open {
                    text_open = true;
                    yield ModelEvent::TextStart;
                }
                yield ModelEvent::TextDelta(content.to_owned());
            }

            if let Some(deltas) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, tool) in deltas.iter().enumerate() {
                    // Servers that omit `index` are addressed by array position.
                    let index = tool
                        .get("index")
                        .and_then(Value::as_u64)
                        .map_or(position, |index| index as usize);
                    let function = tool.get("function").unwrap_or(&Value::Null);
                    for event in update_tool(
                        index,
                        tool.get("id").and_then(Value::as_str),
                        function.get("name").and_then(Value::as_str),
                        function.get("arguments").and_then(Value::as_str),
                        &mut tools,
                    ) {
                        yield event;
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                finish = Some(finish_reason(reason, !tools.is_empty()));
            }
        }

        // Close whatever the upstream left open.
        if thinking_open {
            yield ModelEvent::ThinkingEnd { signature: None };
        }
        if text_open {
            yield ModelEvent::TextEnd;
        }
        for (index, tool) in tools.iter_mut() {
            if !tool.started {
                if tool.name.is_empty() {
                    Err(Error::provider("OpenAI tool call arrived without a name"))?;
                }
                if tool.call_id.is_empty() {
                    tool.call_id = synthetic_tool_call_id(*index);
                }
                tool.started = true;
                yield ModelEvent::ToolCallStart {
                    index: *index,
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                };
                // Arguments that arrived before the call was identifiable were buffered
                // and are released here.
                if !tool.arguments.is_empty() {
                    tool.emitted = tool.arguments.len();
                    yield ModelEvent::ToolCallArgumentsDelta {
                        index: *index,
                        delta: tool.arguments.clone(),
                    };
                }
            }
            yield ModelEvent::ToolCallEnd { index: *index };
        }

        yield ModelEvent::Usage(usage);
        yield ModelEvent::Done(resolve_finish(finish, saw_done, !tools.is_empty()));
    }
}

fn update_tool(
    index: usize,
    call_id: Option<&str>,
    name: Option<&str>,
    arguments: Option<&str>,
    tools: &mut BTreeMap<usize, ToolState>,
) -> Vec<ModelEvent> {
    let tool = tools.entry(index).or_default();
    if let Some(call_id) = call_id {
        merge_fragment(&mut tool.call_id, call_id);
    }
    if let Some(name) = name {
        merge_fragment(&mut tool.name, name);
    }
    if let Some(arguments) = arguments {
        tool.arguments.push_str(arguments);
    }

    let mut events = Vec::new();
    // A call cannot be announced until both its id and name are known.
    if !tool.started && !tool.call_id.is_empty() && !tool.name.is_empty() {
        tool.started = true;
        events.push(ModelEvent::ToolCallStart {
            index,
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
        });
    }
    if tool.started && tool.emitted < tool.arguments.len() {
        let delta = tool.arguments[tool.emitted..].to_owned();
        tool.emitted = tool.arguments.len();
        events.push(ModelEvent::ToolCallArgumentsDelta { index, delta });
    }
    events
}

/// Folds an id or name fragment into what has been accumulated so far.
///
/// Many OpenAI-compatible servers resend the whole id and name on every chunk instead
/// of sending them once, which would otherwise produce `call_abccall_abc`.
fn merge_fragment(target: &mut String, fragment: &str) {
    if target == fragment || target.ends_with(fragment) {
        return;
    }
    if fragment.starts_with(target.as_str()) {
        *target = fragment.to_owned();
    } else {
        target.push_str(fragment);
    }
}

fn finish_reason(reason: &str, saw_tool: bool) -> FinishReason {
    match reason {
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        // A truncated response is reported as truncation even when tool calls appeared,
        // because those calls are incomplete.
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Stop,
        "stop" if saw_tool => FinishReason::ToolUse,
        "stop" => FinishReason::Stop,
        // Open-source compatible servers emit `stop` for tool turns, and anything
        // unrecognized is resolved by whether tool calls were seen at all.
        _ if saw_tool => FinishReason::ToolUse,
        _ => FinishReason::Stop,
    }
}

fn resolve_finish(finish: Option<FinishReason>, saw_done: bool, saw_tool: bool) -> FinishReason {
    match finish {
        // Truncation outranks everything: the tool calls a truncated turn emitted are
        // incomplete, so the caller must not act on them.
        Some(FinishReason::Length) => FinishReason::Length,
        Some(FinishReason::ToolUse) => FinishReason::ToolUse,
        // A `finish_reason` of `stop` is resolved again here because some servers report
        // it on the same chunk that introduces tool calls, or before them.
        Some(FinishReason::Stop) if saw_tool => FinishReason::ToolUse,
        Some(FinishReason::Stop) => FinishReason::Stop,
        // No `finish_reason` at all, but the stream ended normally.
        None if saw_tool => FinishReason::ToolUse,
        None if saw_done => FinishReason::Stop,
        None => FinishReason::Stop,
    }
}

fn parse_usage(value: &Value) -> Usage {
    let cache_read = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    Usage {
        // `prompt_tokens` already includes cached tokens, which is the normalized
        // meaning of the field.
        input_tokens: tokens(value, "prompt_tokens"),
        output_tokens: tokens(value, "completion_tokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
    }
}

fn tokens(value: &Value, field: &str) -> u32 {
    value
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

/// Detects an error delivered inside the stream, after the status was already sent.
fn event_error(value: &Value) -> Option<Error> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("the OpenAI-compatible upstream reported an error event");
    Some(Error::provider(format!("upstream error: {message}")))
}

fn sse_error(error: eventsource_stream::EventStreamError<Error>) -> Error {
    match error {
        eventsource_stream::EventStreamError::Transport(error) => error,
        eventsource_stream::EventStreamError::Utf8(error) => {
            Error::bad_request(format!("OpenAI SSE was not valid UTF-8: {error}"))
        }
        eventsource_stream::EventStreamError::Parser(error) => {
            Error::bad_request(format!("OpenAI SSE could not be parsed: {error}"))
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
