//! Defines the protocol-independent request representation.
//!
//! The shape follows Anthropic's content-block model, which is a superset of the
//! OpenAI message model: OpenAI's `system` messages collapse into [`ChatRequest::system`],
//! and OpenAI's `tool_calls` / `role: "tool"` messages become [`ContentPart::ToolUse`]
//! and [`ContentPart::ToolResult`] blocks. Every converter in this crate reads and
//! writes this one representation.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// A reasoning block.
    ///
    /// `signature` is what makes a block replayable: Anthropic signs its thinking
    /// blocks and rejects unsigned ones, so only signed blocks are sent back to an
    /// Anthropic upstream. OpenAI has no signature concept, so its `reasoning_content`
    /// round-trips as an unsigned block and is only ever sent back to an OpenAI upstream.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Anthropic's opaque, pre-encrypted reasoning block. Replayed verbatim.
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentPart>,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        mime_type: String,
        data_base64: String,
    },
}

impl ContentPart {
    pub fn thinking(text: impl Into<String>, signature: Option<String>) -> Self {
        ContentPart::Thinking {
            text: text.into(),
            signature,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    /// The plain text this part contributes when a target protocol has no
    /// structured equivalent. Non-textual parts contribute nothing.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            ContentPart::Thinking { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn user(content: Vec<ContentPart>) -> Self {
        Message {
            role: Role::User,
            content,
        }
    }

    pub fn assistant(content: Vec<ContentPart>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The client left the token budget to the model, which Anthropic spells
    /// `{"type": "adaptive"}`.
    ///
    /// It is carried separately from `enabled` because the two render differently upstream:
    /// collapsing it into `budget_tokens` would either invent a budget the client never
    /// sent or drop the request to reason altogether.
    #[serde(default)]
    pub adaptive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The model name the client asked for. Resolved to a provider model by the router.
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

impl ChatRequest {
    /// Truncates every tool-call and tool-result id to the length both protocols accept.
    ///
    /// Applied on the way in and on the way out, so a client that received a
    /// truncated id from us and echoes it back still matches the upstream's id.
    pub fn normalize_tool_ids(&mut self) {
        for message in &mut self.messages {
            for part in &mut message.content {
                match part {
                    ContentPart::ToolUse { id, .. } => super::ids::normalize_tool_call_id(id),
                    ContentPart::ToolResult { tool_use_id, .. } => {
                        super::ids::normalize_tool_call_id(tool_use_id)
                    }
                    _ => {}
                }
            }
        }
    }
}
