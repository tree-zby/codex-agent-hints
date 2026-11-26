//! Agent Hint protocol (`X-Agent-Hint`, v1.0) sender for
//! [Codex CLI](https://github.com/openai/codex).
//!
//! The crate turns one model request into one HTTP header and never touches
//! the request body. Codex's wire format is the Responses API, so the logical
//! input is `input[]`, not `messages[]`.
//!
//! ```no_run
//! use codex_agent_hints::{headers_for, RequestFacts};
//!
//! let facts = RequestFacts::from_body(
//!     "thread-1",
//!     "gpt-5",
//!     "turn",
//!     &serde_json::json!({"input": [{"type": "message", "role": "user",
//!         "content": [{"type": "input_text", "text": "hi"}]}]}),
//! );
//! if let Some((name, value)) = headers_for(&facts) {
//!     let _ = (name, value); // add to the outbound request headers
//! }
//! ```
//!
//! Hints are enabled with `CODEX_AGENT_HINTS=1`; without it every call is a
//! no-op. See `README.md` for the two-patch wire-up.

pub mod adapter;
pub mod encoder;
pub mod manage;
pub mod protocol;
pub mod state;

pub use adapter::{
    headers_for, install_from_env, installed_store, opaque_session_id, responses_message_views,
    HintHeader, HintStore, RequestFacts, RequestKind,
};
pub use encoder::{decode, encode, DecodeError, EncodeError};
pub use manage::{parse_verb, ManageClient, ManageRequest};
pub use protocol::{is_manage_hint, validate, Priority, State, HEADER_NAME, MAX_HEADER_BYTES};
pub use state::{
    derive_turn_opening, estimate_prefix_tokens, estimate_text_tokens, new_session_id,
    request_fingerprint, tool_names, Derive, HintConfig, HintError, HintSession, MessageView,
    RequestView, SessionIsolated, SessionSnapshot,
};

/// Crate version, kept in sync with the Git tag.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
