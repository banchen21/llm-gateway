//! Serializes normalized events into the OpenAI Chat Completions wire format.
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{Stream, StreamExt};
use serde_json::{json, Map, Value};

use crate::ir::{EventStream, FinishReason, ModelEvent, Usage};
use crate::{Error, Result};

pub fn stream_response(
    mut events: EventStream,
    completion_id: String,
    model: String,
) -> impl Stream<Item = Result<String>> + Send {
    async_stream::stream! {
        let created = unix_seconds();
        // The upstream's tool indices are its own numbering (Anthropic counts content
        // blocks, including any text block), so they are remapped to a dense 0-based run
        // as the caller sees them.
        let mut tool_indices = BTreeMap::<usize, usize>::new();
        let mut saw_tool_use = false;
        let mut usage = Usage::default();
        let mut finish: Option<FinishReason> = None;

        yield Ok(sse(&chunk(&completion_id, created, &model, json!({"role": "assistant", "content": ""}), Value::Null)));

        while let Some(result) = events.next().await {
            let event = match result {
                Ok(event) => event,
                Err(error) => {
                    yield Ok(sse(&json!({
                        "error": {
                            "message": error.to_string(),
                            "type": error.openai_error_type(),
                        },
                    })));
                    yield Ok(sse_done());
                    return;
                }
            };

            match event {
                ModelEvent::Start { .. }
                | ModelEvent::TextStart
                | ModelEvent::TextEnd
                | ModelEvent::ThinkingStart
                | ModelEvent::ThinkingEnd { .. }
                | ModelEvent::ToolCallEnd { .. } => {}
                ModelEvent::TextDelta(delta) => {
                    yield Ok(sse(&chunk(&completion_id, created, &model, json!({"content": delta}), Value::Null)));
                }
                ModelEvent::ThinkingDelta(delta) => {
                    yield Ok(sse(&chunk(&completion_id, created, &model, json!({"reasoning_content": delta}), Value::Null)));
                }
                ModelEvent::ToolCallStart { index, call_id, name } => {
                    let position = tool_indices.len();
                    tool_indices.insert(index, position);
                    saw_tool_use = true;
                    yield Ok(sse(&chunk(&completion_id, created, &model, json!({
                        "tool_calls": [{
                            "index": position,
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": ""},
                        }],
                    }), Value::Null)));
                }
                ModelEvent::ToolCallArgumentsDelta { index, delta } => {
                    if let Some(position) = tool_indices.get(&index) {
                        yield Ok(sse(&chunk(&completion_id, created, &model, json!({
                            "tool_calls": [{
                                "index": position,
                                "function": {"arguments": delta},
                            }],
                        }), Value::Null)));
                    }
                }
                ModelEvent::Usage(reported) => usage.merge(reported),
                ModelEvent::Done(reason) => finish = Some(reason),
            }

            if finish.is_some() {
                break;
            }
        }

        let reason = match finish {
            // A truncated response is reported as truncation even when tool calls were
            // streamed, because those calls are incomplete.
            Some(FinishReason::Length) => "length",
            Some(FinishReason::ToolUse) => "tool_calls",
            _ if saw_tool_use => "tool_calls",
            _ => "stop",
        };
        yield Ok(sse(&chunk(&completion_id, created, &model, json!({}), json!(reason))));

        // Usage rides on its own chunk with an empty `choices` array, which is the only
        // shape an OpenAI client reads it from in a stream.
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            yield Ok(sse(&json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [],
                "usage": usage_json(usage),
            })));
        }
        yield Ok(sse_done());
    }
}

/// Drains the event stream into a single non-streaming Chat Completions response.
pub async fn complete(mut events: EventStream, completion_id: &str, model: &str) -> Result<Value> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut finish: Option<FinishReason> = None;

    while let Some(event) = events.next().await {
        match event? {
            ModelEvent::Start { .. }
            | ModelEvent::TextStart
            | ModelEvent::TextEnd
            | ModelEvent::ThinkingStart
            | ModelEvent::ThinkingEnd { .. }
            | ModelEvent::ToolCallEnd { .. } => {}
            ModelEvent::TextDelta(delta) => text.push_str(&delta),
            ModelEvent::ThinkingDelta(delta) => reasoning.push_str(&delta),
            ModelEvent::ToolCallStart {
                index,
                call_id,
                name,
            } => {
                tools.insert(index, (call_id, name, String::new()));
            }
            ModelEvent::ToolCallArgumentsDelta { index, delta } => {
                if let Some(tool) = tools.get_mut(&index) {
                    tool.2.push_str(&delta);
                }
            }
            ModelEvent::Usage(reported) => usage.merge(reported),
            ModelEvent::Done(reason) => {
                finish = Some(reason);
                break;
            }
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    // Chat Completions uses `null` content for a turn that only calls tools.
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    let has_tools = !tools.is_empty();
    if has_tools {
        message.insert(
            "tool_calls".into(),
            Value::Array(
                tools
                    .into_values()
                    .map(|(call_id, name, arguments)| {
                        // Arguments are a JSON string on the wire; an unparseable fragment
                        // run is passed through rather than failing the whole response.
                        json!({
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": normalized_arguments(&arguments)},
                        })
                    })
                    .collect(),
            ),
        );
    }

    let finish_reason = match finish {
        Some(FinishReason::Length) => "length",
        Some(FinishReason::ToolUse) => "tool_calls",
        Some(FinishReason::Stop) if has_tools => "tool_calls",
        Some(FinishReason::Stop) => "stop",
        None => {
            return Err(Error::provider(
                "upstream stream ended without reporting a finish reason",
            ))
        }
    };

    Ok(json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": unix_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": usage_json(usage),
    }))
}

/// A tool call's arguments, coerced to the JSON string the wire format expects.
fn normalized_arguments(arguments: &str) -> String {
    if arguments.is_empty() {
        return "{}".to_owned();
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value.to_string(),
        Err(_) => arguments.to_owned(),
    }
}

fn chunk(id: &str, created: u64, model: &str, delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
}

/// `prompt_tokens` includes cached tokens, which is how the normalized field is defined.
fn usage_json(usage: Usage) -> Value {
    let mut value = json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens.saturating_add(usage.output_tokens),
    });
    if usage.cache_read_tokens > 0 {
        value["prompt_tokens_details"] = json!({"cached_tokens": usage.cache_read_tokens});
    }
    value
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn sse(data: &Value) -> String {
    format!("data: {data}\n\n")
}

/// The sentinel that ends an OpenAI stream.
///
/// It has to travel as a `data:` line like every other frame; without the prefix an
/// OpenAI client does not recognize the end of the response.
fn sse_done() -> String {
    "data: [DONE]\n\n".to_owned()
}
