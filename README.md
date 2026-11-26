# codex-agent-hints

[Agent Hint protocol](https://github.com/tree-zby/agent-hint-research) (`X-Agent-Hint`, v1.0) sender for
[Codex CLI](https://github.com/openai/codex).

The crate turns one model request into one HTTP header, so a hint-aware
inference gateway can tell **which conversation a call belongs to**, where it
sits in the session's lifecycle, how urgent it is, and how much of the prefix is
still cacheable. The request body is never touched.

```
POST /v1/responses
X-Agent-Hint: eyJzZXNzaW9uIjp7InNlc3Npb25faWQiOiJhaC0...
```

Codex speaks the **Responses API** (`input[]`), not chat completions, and may
prefer a long-lived WebSocket. Both facts shape this adapter: logical messages
are projected from `input[]` items, and the small host patch disables the
WebSocket while hints are active.

## What it sends

| Block | Content |
|---|---|
| `session` | `session_id`, `call_index`, `epoch`, `state` (`start` / `append`) |
| `priority_control` | `level` 3 + `user_waiting` while the user waits; 1 + `critical_path` otherwise |
| `cache_control` | `ephemeral`, adaptive `ttl_s`, `msg_offset` = **wire** `input[]` item count |
| `context_management` | only on manage requests: `manage_request: true` + one explicit `edits[]` |

Lifecycle verbs (`pause` / `resume` / `stop` / `compact`) are **separate HTTP
requests** with a placeholder body — never flags on a normal call. Auxiliary
generations (`prewarm`, remote compaction, memory) never carry a hint.

## Install

Codex's crates are `publish = false`, so use a git/path dependency:

```toml
[dependencies]
codex-agent-hints = { git = "https://github.com/tree-zby/codex-agent-hints", tag = "v0.1.0" }
```

## Wire-up

Codex has no per-request outbound-header hook in `codex-rs/core`, so this
repository ships one small, protocol-agnostic patch (188 lines added, 0 removed;
no behaviour change when nothing is installed):

```bash
git clone https://github.com/openai/codex
cd codex
git checkout 9e51c74
git apply /path/to/codex-agent-hints/patches/0001-core-hint-header-hook.patch
```

The patch adds `core::agent_hint_headers`, a process-wide provider that receives
the exact `ResponsesApiRequest` about to be sent plus its `thread_id` and
`request_kind`, and applies the returned pairs to `options.extra_headers` in the
HTTP generation path. The same provider makes `responses_websocket_enabled()`
return `false`: a WebSocket pins its handshake headers, so a per-request hint
cannot ride it.

Then install the sender in the CLI startup path:

```rust
use std::sync::Arc;
use codex_agent_hints::{headers_for, install_from_env, RequestFacts};
use codex_core::agent_hint_headers::{self, HintHeadersContext};

if install_from_env() { // reads CODEX_AGENT_HINTS
    agent_hint_headers::set_hint_headers_provider(Some(Arc::new(
        |ctx: &HintHeadersContext<'_>| {
            let facts = RequestFacts::from_body(
                ctx.thread_id,
                ctx.model,
                ctx.request_kind, // turn / prewarm / compaction / memory
                ctx.body,         // the JSON body, read-only
            );
            headers_for(&facts).into_iter().collect()
        },
    )));
}
```

`integration/codex-wiring.rs` is the same snippet in one file.

Because the hook lives in the HTTP branch, it sees exactly one request per
generation attempt, including empty-input prewarms and compaction summaries.
Only `request_kind == "turn"` produces a hint; the adapter returns before
serialising any other body.

## Configuration

| Variable | Meaning |
|---|---|
| `CODEX_AGENT_HINTS` | `1` / `true` / `yes` / `on` installs the sender |
| `CODEX_AGENT_HINTS_CACHE` | `omit` / `off` / `0` never sends `cache_control` |
| `CODEX_AGENT_HINTS_PRIORITY_LEVEL` | pin `1` or `3` (pairs the reason automatically) |
| `CODEX_AGENT_HINTS_DEFAULT_TTL_S` | fallback TTL, default `600` |
| `CODEX_AGENT_HINTS_TTL_MIN_S` / `_MAX_S` | adaptive TTL bounds, default `60` / `3600` |

A malformed value disables hints instead of failing requests.

## Verified behaviour

Two proofs, both at the **seam** (the header hook and the HTTP transport), not
full CLI sessions:

1. **Raw TCP capture** — `e2e/evidence/codex.json` is produced by
   `cargo run --example wire_capture`: a `TcpListener` records the exact
   HTTP/1.1 bytes for ten requests, and the hints are decoded with the same
   codec a gateway uses.
2. **Host-hook probe** — `e2e/probe/` compiles the patched `codex-core` on its
   own (the full workspace pulls the V8/Chromium submodule chain) and asserts
   the process-wide provider contract, including the state that makes
   `responses_websocket_enabled()` return `false`.
   Result: [`e2e/evidence/codex-upstream-check.txt`](e2e/evidence/codex-upstream-check.txt).

| # | Stage | Header | state | call_index | epoch | cache offset | priority |
|---|---|---|---|---|---|---|---|
| 1 | first turn | yes | `start` | 1 | 0 | 1 | 3 |
| 2 | tool continuation | yes | `append` | 2 | 0 | 3 | 1 |
| 3 | prewarm | **no** | — | — | — | — | — |
| 4 | remote compaction | **no** | — | — | — | — | — |
| 5 | memory extraction | **no** | — | — | — | — | — |
| 6 | `compact` manage | yes | `compact` | 2 | **0** (superseded) | — | — |
| 7 | next turn | yes | `append` | 3 | **1** | **suppressed** | 3 |
| 8–10 | `pause` / `resume` / `stop` | yes | verb | 3 / 4 / 3 | 1 | — | — |

Checks in the capture: `every_hint_valid`, `body_has_no_hint_fields`,
`auxiliary_request_has_no_hint` — all `true`.

This is deliberately **not** claimed as a framework-wide e2e: the capture comes
from this crate's HTTP stub, and the probe stops at the host-hook contract. No
in-tree wiremock test of `ModelClientSession::stream` was executed — the
workspace cannot resolve its V8 dependencies here.

## Compliance

- **L1** (core wire): complete — ordinary requests only, exact four-block shape.
- **L2** (lifecycle): complete — four verbs, explicit `edits`, placeholder body.
- **L3**: not claimed — Codex exposes no token-level draft signal and no archive
  event, so there is no honest prefetch lead time.
- Full checklist, degradation profile (§12.4) and trigger table (§7.5):
  [`docs/protocol-compliance.md`](docs/protocol-compliance.md).

Notable harness facts:

- Base instructions are the first `input[]` item, not an out-of-band `system`
  field, so every wire item counts towards `msg_offset`.
- `CodexResponsesMetadata.request_kind` already classifies `Turn`, `Prewarm`,
  `Compaction(_)` and `Memory`; no guessing from the body is needed.
- One process can host many `thread_id`s (main thread + subagents); hint state is
  keyed by `thread_id`, so counters never leak between conversations.
- Retries reuse the same hint via a logical-request fingerprint until the
  endpoint accepts it.

## Compatibility

| | |
|---|---|
| Protocol | `agent-hint-protocol` v1.0 |
| Codex CLI | `codex-rs` commit `9e51c74` (2026-11-26) |
| Rust | 1.75+ (no nightly) |
| Dependencies | `serde_json`, `base64`, `sha2` — nothing from the harness |

## Privacy

The crate sends no prompt text, tool output or user content. It only emits an
opaque session id, counters and cache offsets. See [PRIVACY.md](PRIVACY.md).

## License

Apache-2.0.
