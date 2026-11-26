//! Base64url codec for the `X-Agent-Hint` header (protocol section 3.2).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use crate::protocol::MAX_HEADER_BYTES;

#[derive(Debug)]
pub enum EncodeError {
    Serialize(serde_json::Error),
    TooLarge { bytes: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Serialize(error) => write!(formatter, "cannot serialise hint: {error}"),
            EncodeError::TooLarge { bytes } => write!(
                formatter,
                "encoded hint is {bytes} bytes, above the {MAX_HEADER_BYTES}-byte budget"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug)]
pub enum DecodeError {
    Empty,
    Base64(base64::DecodeError),
    Json(serde_json::Error),
    NotAnObject,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Empty => write!(formatter, "header value is empty"),
            DecodeError::Base64(error) => write!(formatter, "invalid base64url: {error}"),
            DecodeError::Json(error) => write!(formatter, "invalid JSON payload: {error}"),
            DecodeError::NotAnObject => write!(formatter, "hint payload must be a JSON object"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Serialise a hint: UTF-8 JSON -> base64url without padding.
pub fn encode(hint: &Value) -> Result<String, EncodeError> {
    let raw = serde_json::to_vec(hint).map_err(EncodeError::Serialize)?;
    let encoded = URL_SAFE_NO_PAD.encode(raw);
    if encoded.len() > MAX_HEADER_BYTES {
        return Err(EncodeError::TooLarge {
            bytes: encoded.len(),
        });
    }
    Ok(encoded)
}

/// Decode a header value. Padded and unpadded base64url are both accepted.
pub fn decode(value: &str) -> Result<Value, DecodeError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(DecodeError::Empty);
    }
    // `URL_SAFE_NO_PAD` rejects padding, receivers tolerate it: strip it first.
    let stripped = text.trim_end_matches('=');
    let raw = URL_SAFE_NO_PAD
        .decode(stripped)
        .map_err(DecodeError::Base64)?;
    let parsed: Value = serde_json::from_slice(&raw).map_err(DecodeError::Json)?;
    if !parsed.is_object() {
        return Err(DecodeError::NotAnObject);
    }
    Ok(parsed)
}
