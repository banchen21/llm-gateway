//! Normalizes tool-call identifiers across protocols.

/// Anthropic caps `tool_use_id` at 64 characters while OpenAI-generated ids
/// routinely exceed it, so the same conversation could not otherwise survive a
/// round trip through both. Every tool-call and tool-result id is truncated on
/// the way into the pipeline, which keeps both sides agreeing on the identity.
const PROVIDER_TOOL_CALL_ID_MAX_CHARS: usize = 64;

/// The id used when an upstream omits one. OpenAI-compatible servers are allowed
/// to send `tool_calls` entries without an id in the first chunk.
pub fn synthetic_tool_call_id(index: usize) -> String {
    format!("call-{index}")
}

pub fn normalize_tool_call_id(id: &mut String) {
    if let Some((end, _)) = id.char_indices().nth(PROVIDER_TOOL_CALL_ID_MAX_CHARS) {
        id.truncate(end);
    }
}

pub fn normalized_tool_call_id(id: &str) -> String {
    let mut id = id.to_owned();
    normalize_tool_call_id(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_at_or_below_the_cap_are_untouched() {
        let id = "a".repeat(PROVIDER_TOOL_CALL_ID_MAX_CHARS);
        assert_eq!(normalized_tool_call_id(&id), id);
    }

    #[test]
    fn ids_above_the_cap_are_truncated() {
        let id = "a".repeat(PROVIDER_TOOL_CALL_ID_MAX_CHARS + 10);
        let normalized = normalized_tool_call_id(&id);
        assert_eq!(normalized.chars().count(), PROVIDER_TOOL_CALL_ID_MAX_CHARS);
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let id = "é".repeat(PROVIDER_TOOL_CALL_ID_MAX_CHARS + 1);
        let normalized = normalized_tool_call_id(&id);
        assert_eq!(normalized.chars().count(), PROVIDER_TOOL_CALL_ID_MAX_CHARS);
        assert_eq!(normalized, "é".repeat(PROVIDER_TOOL_CALL_ID_MAX_CHARS));
    }
}
