//! Parses OpenAI Chat Completions requests into the shared IR.
//!
//! Chat Completions is the less expressive of the two protocols, so this direction has to
//! reconstruct structure the IR carries natively: system messages collapse into the single
//! system prompt, and `role: "tool"` messages fold back into the tool-result blocks that
//! live inside Anthropic-style user messages.
use serde_json::{json, Value};

use super::required_str;
use crate::ir::{ChatRequest, ContentPart, Message, ThinkingConfig, ToolChoice, ToolDef};
use crate::{Error, Result};

pub fn parse_request(value: &Value) -> Result<ChatRequest> {
    let (messages, system) = parse_messages(value.get("messages"))?;
    let mut request = ChatRequest {
        model: required_str(value, "model")?.to_owned(),
        system,
        messages,
        tools: parse_tools(value.get("tools"))?,
        tool_choice: parse_tool_choice(value.get("tool_choice"))?,
        // Chat Completions accepts either name, and OpenAI's own models are split
        // between them, so both are read.
        max_tokens: number(value, "max_tokens").or_else(|| number(value, "max_completion_tokens")),
        temperature: float(value, "temperature"),
        top_p: float(value, "top_p"),
        stop_sequences: parse_stop(value.get("stop")),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Reasoning is requested through `reasoning_effort`, whose values do not map onto
        // Anthropic's token budget, so only the fact that it was asked for is preserved.
        thinking: value
            .get("reasoning_effort")
            .filter(|effort| !effort.is_null())
            .map(|_| ThinkingConfig {
                enabled: true,
                adaptive: false,
                budget_tokens: None,
            }),
    };
    request.normalize_tool_ids();
    Ok(request)
}

/// Splits Chat Completions' flat message list into the IR's block-structured messages and
/// the single system prompt the IR carries alongside them.
///
/// The IR keeps a tool result inside the user message that carries it, so consecutive
/// `tool` messages are collected and flushed as one user message before whatever comes
/// next. That also preserves the ordering an assistant turn's results must keep.
fn parse_messages(value: Option<&Value>) -> Result<(Vec<Message>, Option<String>)> {
    let messages = value
        .and_then(Value::as_array)
        .ok_or_else(|| Error::bad_request("`messages` must be an array"))?;

    let mut output: Vec<Message> = Vec::new();
    let mut system: Vec<String> = Vec::new();
    let mut pending_results: Vec<ContentPart> = Vec::new();
    let mut seen_conversation = false;

    for message in messages {
        let role = required_str(message, "role")?;
        if matches!(role, "system" | "developer") {
            // OpenAI tolerates a system message anywhere, Anthropic's top-level system
            // prompt cannot, so a late one has no representation.
            if seen_conversation {
                return Err(Error::bad_request(
                    "system messages must precede the conversation",
                ));
            }
            system.push(
                text_content(message.get("content")).ok_or_else(|| {
                    Error::bad_request("a system message must carry text content")
                })?,
            );
            continue;
        }

        if !pending_results.is_empty() {
            output.push(Message::user(std::mem::take(&mut pending_results)));
        }
        seen_conversation = true;

        match role {
            "user" => output.push(Message::user(parse_content(message.get("content"))?)),
            "assistant" => output.push(Message::assistant(parse_assistant(message)?)),
            "tool" => {
                pending_results.push(ContentPart::ToolResult {
                    tool_use_id: required_str(message, "tool_call_id")?.to_owned(),
                    content: vec![ContentPart::Text {
                        text: text_content(message.get("content")).unwrap_or_default(),
                    }],
                    is_error: false,
                });
            }
            other => {
                return Err(Error::bad_request(format!(
                    "unsupported message role: {other}"
                )))
            }
        }
    }

    if !pending_results.is_empty() {
        output.push(Message::user(pending_results));
    }

    let system = system.join("\n");
    Ok((output, Some(system).filter(|system| !system.is_empty())))
}

fn parse_assistant(message: &Value) -> Result<Vec<ContentPart>> {
    let mut parts = Vec::new();
    if let Some(text) = text_content(message.get("content")) {
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }
    }
    // Unsigned reasoning: OpenAI has no signature concept, so a block that came from an
    // OpenAI-compatible upstream is exactly what must be replayed to one.
    for field in ["reasoning_content", "reasoning"] {
        if let Some(reasoning) = message
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            parts.push(ContentPart::thinking(reasoning, None));
            break;
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let function = call
                .get("function")
                .ok_or_else(|| Error::bad_request("a tool call is missing `function`"))?;
            parts.push(ContentPart::ToolUse {
                id: required_str(call, "id")?.to_owned(),
                name: required_str(function, "name")?.to_owned(),
                input: parse_arguments(function.get("arguments"))?,
            });
        }
    }
    Ok(parts)
}

/// Tool arguments are a JSON string in the wire format, though some clients send an
/// object directly.
fn parse_arguments(value: Option<&Value>) -> Result<Value> {
    match value {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(json!({})),
        Some(Value::String(text)) => serde_json::from_str(text).map_err(|error| {
            Error::bad_request(format!("tool call arguments are not valid JSON: {error}"))
        }),
        Some(value) => Ok(value.clone()),
    }
}

/// Message content is a bare string or an array of parts.
fn parse_content(value: Option<&Value>) -> Result<Vec<ContentPart>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if let Some(text) = value.as_str() {
        return Ok(vec![ContentPart::Text {
            text: text.to_owned(),
        }]);
    }
    let parts = value
        .as_array()
        .ok_or_else(|| Error::bad_request("`content` must be a string or an array of parts"))?;
    parts.iter().map(parse_part).collect()
}

fn parse_part(part: &Value) -> Result<ContentPart> {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::bad_request("a content part is missing `type`"))?;
    match kind {
        "text" | "input_text" | "output_text" => Ok(ContentPart::Text {
            text: required_str(part, "text")?.to_owned(),
        }),
        "image_url" | "input_image" => parse_image(part),
        other => Err(Error::bad_request(format!(
            "unsupported content part type: {other}"
        ))),
    }
}

/// Only base64 data URLs are accepted: fetching a remote URL would make the gateway an
/// SSRF vector, and neither upstream accepts a URL in a content block.
fn parse_image(part: &Value) -> Result<ContentPart> {
    let url = part
        .pointer("/image_url/url")
        .or_else(|| part.get("image_url"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::bad_request("an image part must carry `image_url.url`"))?;
    let (header, data_base64) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .ok_or_else(|| {
            Error::bad_request("only base64 data URL images are supported; send the image inline")
        })?;
    let mime_type = header.strip_suffix(";base64").ok_or_else(|| {
        Error::bad_request("only base64 data URL images are supported; send the image inline")
    })?;
    Ok(ContentPart::Image {
        mime_type: mime_type.to_owned(),
        data_base64: data_base64.to_owned(),
    })
}

fn parse_tools(value: Option<&Value>) -> Result<Vec<ToolDef>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| Error::bad_request("`tools` must be an array"))?;
    tools
        .iter()
        .map(|tool| {
            // Tools are wrapped in `{type: "function", function: {...}}`, but a bare
            // function object is also seen in the wild.
            let function = tool.get("function").unwrap_or(tool);
            let input_schema = function
                .get("parameters")
                .or_else(|| function.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Ok(ToolDef {
                name: required_str(function, "name")?.to_owned(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                input_schema,
            })
        })
        .collect()
}

fn parse_tool_choice(value: Option<&Value>) -> Result<Option<ToolChoice>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(name) => Ok(Some(match name.as_str() {
            "auto" => ToolChoice::Auto,
            // OpenAI calls it `required`; some compatible servers use Anthropic's `any`.
            "required" | "any" => ToolChoice::Any,
            "none" => ToolChoice::None,
            other => {
                return Err(Error::bad_request(format!(
                    "unsupported tool_choice: {other}"
                )))
            }
        })),
        Value::Object(_) => {
            let name = value
                .pointer("/function/name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::bad_request("a named tool_choice must carry `function.name`")
                })?;
            Ok(Some(ToolChoice::Tool(name.to_owned())))
        }
        _ => Err(Error::bad_request("`tool_choice` has an unsupported shape")),
    }
}

/// `stop` is either one string or an array of them.
fn parse_stop(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(stop)) if !stop.is_empty() => vec![stop.clone()],
        Some(Value::Array(stops)) => stops
            .iter()
            .filter_map(|stop| stop.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Text from content that is either a string or an array of text parts.
fn text_content(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.as_str())
                })
                .collect::<Vec<_>>()
                .join("");
            Some(text)
        }
        _ => None,
    }
}

fn number(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .map(|number| number.min(u32::MAX as u64) as u32)
}

fn float(value: &Value, field: &str) -> Option<f32> {
    value.get(field).and_then(Value::as_f64).map(|n| n as f32)
}
