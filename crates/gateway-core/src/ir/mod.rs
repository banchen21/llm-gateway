//! Defines the protocol-independent representation every converter shares.

mod event;
pub mod ids;
mod request;

pub use event::{EventStream, FinishReason, ModelEvent, Usage};
pub use ids::{normalize_tool_call_id, normalized_tool_call_id, synthetic_tool_call_id};
pub use request::{ChatRequest, ContentPart, Message, Role, ThinkingConfig, ToolChoice, ToolDef};
