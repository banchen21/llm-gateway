//! Serializes normalized events into the Anthropic Messages API wire format.
//!
//! Two entry points share one interpretation of the event stream: [`stream_response`]
//! emits SSE frames as events arrive, and [`complete`] drains the same stream into a
//! single JSON body for non-streaming callers.
use std::collections::BTreeMap;

use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::ir::{EventStream, FinishReason, ModelEvent, Usage};
use crate::{Error, Result};

/// Assigns Anthropic `content_block` indices.
///
/// Anthropic requires block indices to be contiguous and to start at 0, and the
/// response's leading reasoning block shifts every later block up by one. The
/// upstream's tool indices are its own numbering, so they are mapped rather than reused.
#[derive(Default)]
struct Blocks {
    thinking: Option<usize>,
    text: Option<usize>,
    tools: BTreeMap<usize, usize>,
    next: usize,
}

impl Blocks {
    fn allocate(&mut self) -> usize {
        let index = self.next;
        self.next += 1;
        index
    }
}

pub fn stream_response(
    mut events: EventStream,
    message_id: String,
    model: String,
) -> impl Stream<Item = Result<String>> + Send {
    async_stream::stream! {
        let mut blocks = Blocks::default();
        let mut usage = Usage::default();
        let mut finish: Option<FinishReason> = None;
        let mut saw_tool_use = false;

        yield Ok(sse("message_start", json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }
        })));

        while let Some(result) = events.next().await {
            let event = match result {
                Ok(event) => event,
                Err(error) => {
                    yield Ok(sse("error", json!({
                        "type": "error",
                        "error": {
                            "type": error.anthropic_error_type(),
                            "message": error.to_string(),
                        },
                    })));
                    return;
                }
            };

            match event {
                ModelEvent::Start { .. } => {}
                ModelEvent::ThinkingStart => {
                    if blocks.thinking.is_none() {
                        let index = blocks.allocate();
                        blocks.thinking = Some(index);
                        yield Ok(sse("content_block_start", json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "thinking", "thinking": ""},
                        })));
                    }
                }
                ModelEvent::ThinkingDelta(delta) => {
                    if let Some(index) = blocks.thinking {
                        yield Ok(sse("content_block_delta", json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "thinking_delta", "thinking": delta},
                        })));
                    }
                }
                ModelEvent::ThinkingEnd { signature } => {
                    if let Some(index) = blocks.thinking.take() {
                        if let Some(signature) = signature.filter(|s| !s.is_empty()) {
                            yield Ok(sse("content_block_delta", json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {"type": "signature_delta", "signature": signature},
                            })));
                        }
                        yield Ok(sse("content_block_stop", json!({
                            "type": "content_block_stop",
                            "index": index,
                        })));
                    }
                }
                ModelEvent::TextStart => {
                    if blocks.text.is_none() {
                        yield Ok(open_text_block(&mut blocks));
                    }
                }
                ModelEvent::TextDelta(delta) => {
                    // Upstreams are not required to send an explicit start, so a delta
                    // arriving first opens the block itself.
                    if blocks.text.is_none() {
                        yield Ok(open_text_block(&mut blocks));
                    }
                    let index = blocks.text.expect("text block was just opened");
                    yield Ok(sse("content_block_delta", json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": delta},
                    })));
                }
                ModelEvent::TextEnd => {
                    if let Some(index) = blocks.text.take() {
                        yield Ok(close_block(index));
                    }
                }
                ModelEvent::ToolCallStart { index: tool, call_id, name } => {
                    // Anthropic opens and closes one block at a time, and tool blocks come
                    // last, so any block still open has to be closed before this one opens.
                    // Open-in-compatible upstreams never send an explicit end for text.
                    if let Some(index) = blocks.thinking.take() {
                        yield Ok(close_block(index));
                    }
                    if let Some(index) = blocks.text.take() {
                        yield Ok(close_block(index));
                    }
                    let block = blocks.allocate();
                    blocks.tools.insert(tool, block);
                    saw_tool_use = true;
                    yield Ok(sse("content_block_start", json!({
                        "type": "content_block_start",
                        "index": block,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": {},
                        },
                    })));
                }
                ModelEvent::ToolCallArgumentsDelta { index: tool, delta } => {
                    if let Some(block) = blocks.tools.get(&tool) {
                        yield Ok(sse("content_block_delta", json!({
                            "type": "content_block_delta",
                            "index": block,
                            "delta": {"type": "input_json_delta", "partial_json": delta},
                        })));
                    }
                }
                ModelEvent::ToolCallEnd { index: tool } => {
                    if let Some(block) = blocks.tools.remove(&tool) {
                        yield Ok(close_block(block));
                    }
                }
                ModelEvent::Usage(reported) => usage.merge(reported),
                ModelEvent::Done(reason) => finish = Some(reason),
            }

            if finish.is_some() {
                break;
            }
        }

        // A stream that ended without closing its blocks would leave the client
        // waiting, so anything still open is closed here.
        if let Some(index) = blocks.thinking.take() {
            yield Ok(close_block(index));
        }
        if let Some(index) = blocks.text.take() {
            yield Ok(close_block(index));
        }
        for (_, index) in std::mem::take(&mut blocks.tools) {
            yield Ok(close_block(index));
        }

        let stop_reason = match finish {
            Some(FinishReason::Length) => "max_tokens",
            // A tool call that upstream reported as a plain stop is still a tool use;
            // the client has to run the tool to continue the conversation.
            Some(FinishReason::ToolUse) | None if saw_tool_use => "tool_use",
            Some(FinishReason::ToolUse) => "tool_use",
            _ => "end_turn",
        };

        yield Ok(sse("message_delta", json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": usage_json(usage),
        })));
        yield Ok(sse("message_stop", json!({"type": "message_stop"})));
    }
}

/// Drains the event stream into a single non-streaming Messages API response.
pub async fn complete(mut events: EventStream, message_id: &str, model: &str) -> Result<Value> {
    let mut content = Vec::new();
    let mut thinking = String::new();
    let mut signature: Option<String> = None;
    let mut text = String::new();
    let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut finish: Option<FinishReason> = None;

    while let Some(event) = events.next().await {
        match event? {
            ModelEvent::Start { .. } => {}
            ModelEvent::ThinkingStart | ModelEvent::TextStart => {}
            ModelEvent::ThinkingDelta(delta) => thinking.push_str(&delta),
            ModelEvent::ThinkingEnd {
                signature: block_signature,
            } => signature = block_signature,
            ModelEvent::TextDelta(delta) => text.push_str(&delta),
            ModelEvent::TextEnd => {}
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
            ModelEvent::ToolCallEnd { .. } => {}
            ModelEvent::Usage(reported) => usage.merge(reported),
            ModelEvent::Done(reason) => {
                finish = Some(reason);
                break;
            }
        }
    }

    if !thinking.is_empty() {
        let mut block = json!({"type": "thinking", "thinking": thinking});
        if let Some(signature) = signature {
            block["signature"] = json!(signature);
        }
        content.push(block);
    }
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    let has_tools = !tools.is_empty();
    for (_, (call_id, name, arguments)) in tools {
        // `input_json_delta` fragments concatenate into a JSON object; an unparseable
        // result is surfaced as an empty input rather than failing the whole response.
        let input = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": input,
        }));
    }

    let stop_reason = match finish {
        Some(FinishReason::Length) => "max_tokens",
        Some(FinishReason::ToolUse) => "tool_use",
        Some(FinishReason::Stop) if has_tools => "tool_use",
        Some(FinishReason::Stop) => "end_turn",
        None => {
            return Err(Error::provider(
                "upstream stream ended without reporting a finish reason",
            ))
        }
    };

    Ok(json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage_json(usage),
    }))
}

fn open_text_block(blocks: &mut Blocks) -> String {
    let index = blocks.allocate();
    blocks.text = Some(index);
    sse(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        }),
    )
}

fn close_block(index: usize) -> String {
    sse(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

/// Anthropic reports `input_tokens` excluding cached tokens, while the normalized
/// usage carries the caller's inclusive total, so the cached counts are subtracted out.
fn usage_json(usage: Usage) -> Value {
    let uncached = usage
        .input_tokens
        .saturating_sub(usage.cache_read_tokens)
        .saturating_sub(usage.cache_write_tokens);
    let mut value = json!({
        "input_tokens": uncached,
        "output_tokens": usage.output_tokens,
    });
    if usage.cache_read_tokens > 0 {
        value["cache_read_input_tokens"] = json!(usage.cache_read_tokens);
    }
    if usage.cache_write_tokens > 0 {
        value["cache_creation_input_tokens"] = json!(usage.cache_write_tokens);
    }
    value
}

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}
