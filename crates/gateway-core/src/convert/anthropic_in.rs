//! Parses Anthropic Messages API requests into the shared IR.

use serde_json::{json, Value};

use super::required_str;
use crate::ir::{ChatRequest, ContentPart, Message, Role, ThinkingConfig, ToolChoice, ToolDef};
use crate::{Error, Result};

pub fn parse_request(value: &Value) -> Result<ChatRequest> {
    let (messages, late_system) = parse_messages(value.get("messages"))?;
    let mut request = ChatRequest {
        model: required_str(value, "model")?.to_owned(),
        system: merge_system(parse_system(value.get("system"))?, late_system),
        messages,
        tools: parse_tools(value.get("tools"))?,
        tool_choice: parse_tool_choice(value.get("tool_choice"))?,
        max_tokens: value.get("max_tokens").and_then(Value::as_u64).map(as_u32),
        temperature: number(value, "temperature"),
        top_p: number(value, "top_p"),
        stop_sequences: parse_stop_sequences(value.get("stop_sequences")),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        thinking: parse_thinking(value.get("thinking"))?,
    };
    request.normalize_tool_ids();
    Ok(request)
}

/// Folds `system`-role messages into the request's system prompt.
///
/// Claude Code sends its environment preamble as a `system`-role message inside `messages`
/// rather than in the top-level `system` field, and neither upstream protocol has a place
/// for one mid-conversation: the IR carries a single system prompt, so it is appended there
/// in the order it arrived.
fn merge_system(base: Option<String>, late_system: Vec<String>) -> Option<String> {
    if late_system.is_empty() {
        return base;
    }
    let late = late_system.join("\n");
    Some(match base {
        Some(base) => format!("{base}\n{late}"),
        None => late,
    })
}

/// `system` is either a plain string or an array of text blocks; both collapse
/// into the single system prompt the IR carries.
fn parse_system(value: Option<&Value>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let joined = match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let mut texts = Vec::with_capacity(blocks.len());
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
                if kind != "text" {
                    return Err(Error::bad_request(format!(
                        "unsupported system block type: {kind}"
                    )));
                }
                texts.push(required_str(block, "text")?.to_owned());
            }
            texts.join("\n")
        }
        _ => {
            return Err(Error::bad_request(
                "`system` must be a string or an array of text blocks",
            ))
        }
    };
    Ok(Some(joined).filter(|text| !text.is_empty()))
}

/// Splits the message list into the IR's messages and the text of any `system`-role
/// messages, which the caller folds into the system prompt.
fn parse_messages(value: Option<&Value>) -> Result<(Vec<Message>, Vec<String>)> {
    let messages = value
        .and_then(Value::as_array)
        .ok_or_else(|| Error::bad_request("`messages` must be an array"))?;
    let mut parsed = Vec::with_capacity(messages.len());
    let mut system = Vec::new();
    for message in messages {
        let content = message
            .get("content")
            .ok_or_else(|| Error::bad_request("message `content` is required"))?;
        let role = match required_str(message, "role")? {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => {
                // A system message carries text and nothing else, which is the same shape
                // the top-level `system` field takes, so it is parsed by the same function.
                if let Some(text) = parse_system(Some(content))? {
                    system.push(text);
                }
                continue;
            }
            other => {
                return Err(Error::bad_request(format!(
                    "unsupported message role: {other}"
                )))
            }
        };
        parsed.push(Message {
            role,
            content: parse_content(content)?,
        });
    }
    Ok((parsed, system))
}

/// Message content is either a bare string or an array of content blocks.
fn parse_content(value: &Value) -> Result<Vec<ContentPart>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![ContentPart::Text {
            text: text.to_owned(),
        }]);
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| Error::bad_request("`content` must be a string or an array of blocks"))?;
    blocks.iter().map(parse_block).collect()
}

fn parse_block(block: &Value) -> Result<ContentPart> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::bad_request("content block is missing `type`"))?;
    match kind {
        "text" => Ok(ContentPart::Text {
            text: required_str(block, "text")?.to_owned(),
        }),
        "thinking" => Ok(ContentPart::Thinking {
            text: required_str(block, "thinking")?.to_owned(),
            // An empty signature is indistinguishable from a missing one, and only a
            // non-empty signature makes the block replayable upstream.
            signature: non_empty(
                block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
        }),
        "redacted_thinking" => Ok(ContentPart::RedactedThinking {
            data: required_str(block, "data")?.to_owned(),
        }),
        "tool_use" => Ok(ContentPart::ToolUse {
            id: required_str(block, "id")?.to_owned(),
            name: required_str(block, "name")?.to_owned(),
            input: block
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        }),
        "tool_result" => Ok(ContentPart::ToolResult {
            tool_use_id: required_str(block, "tool_use_id")?.to_owned(),
            content: parse_tool_result_content(block.get("content"))?,
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "image" => parse_image(block),
        other => Err(Error::bad_request(format!(
            "unsupported content block type: {other}"
        ))),
    }
}

/// A tool result's content is a bare string, or an array of blocks (typically text).
fn parse_tool_result_content(value: Option<&Value>) -> Result<Vec<ContentPart>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => parse_content(value),
    }
}

fn parse_image(block: &Value) -> Result<ContentPart> {
    let source = block
        .get("source")
        .ok_or_else(|| Error::bad_request("image block is missing `source`"))?;
    match required_str(source, "type")? {
        "base64" => Ok(ContentPart::Image {
            mime_type: required_str(source, "media_type")?.to_owned(),
            data_base64: required_str(source, "data")?.to_owned(),
        }),
        // Fetching a remote URL on the gateway's behalf would make it an SSRF vector,
        // and neither upstream accepts a URL in a content block.
        other => Err(Error::bad_request(format!(
            "unsupported image source type `{other}`; send base64 data instead"
        ))),
    }
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
            Ok(ToolDef {
                name: required_str(tool, "name")?.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                input_schema: tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            })
        })
        .collect()
}

fn parse_tool_choice(value: Option<&Value>) -> Result<Option<ToolChoice>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(match required_str(value, "type")? {
        "auto" => ToolChoice::Auto,
        "any" => ToolChoice::Any,
        "none" => ToolChoice::None,
        "tool" => ToolChoice::Tool(required_str(value, "name")?.to_owned()),
        other => {
            return Err(Error::bad_request(format!(
                "unsupported tool_choice type: {other}"
            )))
        }
    }))
}

fn parse_thinking(value: Option<&Value>) -> Result<Option<ThinkingConfig>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(match required_str(value, "type")? {
        // Claude Code asks for this on models it believes pick their own budget. It carries
        // no `budget_tokens`, and inventing one here would change what the client asked for,
        // so the flag travels through to the upstream request builder as it stands.
        "adaptive" => ThinkingConfig {
            enabled: true,
            adaptive: true,
            budget_tokens: None,
        },
        "enabled" => ThinkingConfig {
            enabled: true,
            adaptive: false,
            budget_tokens: value
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .map(as_u32),
        },
        "disabled" => ThinkingConfig {
            enabled: false,
            adaptive: false,
            budget_tokens: None,
        },
        other => {
            return Err(Error::bad_request(format!(
                "unsupported thinking type: {other}"
            )))
        }
    }))
}

fn parse_stop_sequences(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|sequences| {
            sequences
                .iter()
                .filter_map(|sequence| sequence.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn number(value: &Value, field: &str) -> Option<f32> {
    value.get(field).and_then(Value::as_f64).map(|n| n as f32)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

fn as_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}
