//! Seam-level smoke test for the patched host hook.
//!
//! Depends on the patched `codex-core` by path. The full in-tree wiremock test
//! (`codex-rs/core/tests/agent_hints_wire.rs`) cannot run in this environment:
//! resolving `codex-core`'s workspace pulls the Chromium/V8 submodule chain, so
//! this probe crate reuses `codex-rs/Cargo.lock` and exercises the public half
//! of the hook directly.

use std::sync::{Arc, Mutex};

use codex_core::agent_hint_headers::{
    HintHeadersContext, clear_hint_headers_provider, hint_headers_active,
    set_hint_headers_provider,
};

#[test]
fn provider_plumbing_is_process_wide_and_optional() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!hint_headers_active());

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    set_hint_headers_provider(Some(Arc::new(
        move |ctx: &HintHeadersContext<'_>| {
            sink.lock().unwrap().push(format!(
                "{}|{}|{}",
                ctx.thread_id, ctx.request_kind, ctx.model
            ));
            vec![("X-Agent-Hint".to_string(), "probe".to_string())]
        },
    )));

    // `ModelClient::responses_websocket_enabled` consults exactly this.
    assert!(hint_headers_active());

    // Replacing returns the previous provider; clearing restores the default.
    let replaced = set_hint_headers_provider(None);
    assert!(replaced.is_some());
    assert!(!hint_headers_active());

    assert!(clear_hint_headers_provider().is_none());
    // The first provider was never called: the client invokes it per request,
    // and no request was made here.
    assert!(seen.lock().unwrap().is_empty());
}

static LOCK: Mutex<()> = Mutex::new(());
