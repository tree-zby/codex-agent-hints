//! Install the Agent Hint sender into a patched Codex CLI.
//!
//! This file is not compiled by this crate; it is the snippet referenced from
//! `README.md`. Drop it into `codex-rs/core/src/...` or call
//! [`install`] once from the CLI startup path, before the first model request.
//!
//! Requires `patches/0001-core-hint-header-hook.patch`.

use std::sync::Arc;

use codex_agent_hints::adapter::{headers_for, RequestFacts};
use codex_core::agent_hint_headers::{self, HintHeadersContext};

/// Installs the process-wide provider when `CODEX_AGENT_HINTS` is truthy.
///
/// Returns `true` when hints are active. Called again, it is a no-op.
pub fn install() -> bool {
    if !codex_agent_hints::install_from_env() {
        return false;
    }
    agent_hint_headers::set_hint_headers_provider(Some(Arc::new(
        |ctx: &HintHeadersContext<'_>| {
            let facts = RequestFacts::from_body(
                ctx.thread_id,
                ctx.model,
                ctx.request_kind,
                ctx.body,
            );
            // Fail-open: a hint problem must never fail the model request.
            headers_for(&facts).into_iter().collect()
        },
    )));
    true
}

/// Removes the provider; Codex returns to its normal WebSocket transport.
pub fn uninstall() {
    agent_hint_headers::clear_hint_headers_provider();
}
