//! Defines the normalized streaming events every upstream adapter produces and
//! every outbound serializer consumes.
use std::pin::Pin;

use futures_util::Stream;

use crate::Result;

/// The normalized event stream a provider adapter hands to an outbound serializer.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolUse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Total input tokens *including* cached ones.
    ///
    /// The protocols disagree on this field's meaning — OpenAI's `prompt_tokens`
    /// includes cached tokens while Anthropic's `input_tokens` excludes them — so the
    /// normalized form takes the inclusive reading and each serializer converts back.
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl Usage {
    /// Merges usage reported across several events.
    ///
    /// Anthropic splits usage across `message_start` (input) and `message_delta`
    /// (output) and repeats overlapping fields in both, so the accumulated value
    /// is the field-wise maximum rather than a sum.
    pub fn merge(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    /// Opens the response. `message_id` is the upstream's id when it has one.
    Start {
        message_id: Option<String>,
    },
    TextStart,
    TextDelta(String),
    TextEnd,
    ThinkingStart,
    ThinkingDelta(String),
    /// Closes the reasoning block. `signature` is the upstream's signature when it
    /// provides one, which is what lets the caller replay the block later.
    ThinkingEnd {
        signature: Option<String>,
    },
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
    },
    Usage(Usage),
    Done(FinishReason),
}

impl ModelEvent {
    /// Whether this event carries an actual model response, as opposed to transport
    /// markers (`Start`), bookkeeping (`Usage`), or termination (`Done`).
    pub fn is_response_content(&self) -> bool {
        matches!(
            self,
            ModelEvent::TextStart
                | ModelEvent::TextDelta(_)
                | ModelEvent::TextEnd
                | ModelEvent::ThinkingStart
                | ModelEvent::ThinkingDelta(_)
                | ModelEvent::ThinkingEnd { .. }
                | ModelEvent::ToolCallStart { .. }
                | ModelEvent::ToolCallArgumentsDelta { .. }
                | ModelEvent::ToolCallEnd { .. }
        )
    }
}
