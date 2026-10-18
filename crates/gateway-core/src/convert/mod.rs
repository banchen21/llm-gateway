//! Converts between client-facing protocol JSON/SSE and the shared IR.

pub mod anthropic_in;
pub mod anthropic_out;
pub mod openai_in;
pub mod openai_out;

use serde_json::Value;

use crate::{Error, Result};

/// Reads a required string field, naming the field on failure.
pub(crate) fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::bad_request(format!("`{field}` must be a string")))
}
