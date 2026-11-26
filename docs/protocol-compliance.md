# Protocol compliance — `codex-agent-hints`

Target: **`agent-hint-protocol` v1.0**, level **L1 + L2** (L3 not claimed).

Harness: [Codex CLI](https://github.com/openai/codex) (`codex-rs`, commit
`9e51c74`, 2026-11-26). Wire format: **Responses API** (`input[]`).

## 1. L1 checklist

### Transport (W)

| ID | Requirement | Status | Where |
|---|---|---|---|
| W-01 | Hint only in `X-Agent-Hint`, matched case-insensitively | ✅ | `protocol::HEADER_NAME`; `tests/protocol.rs` |
| W-02 | Request body carries no hint field | ✅ | `examples/wire_capture.rs` asserts `body_has_hint_fields = false` |
| W-03 | base64url, no padding, UTF-8 JSON | ✅ | `encoder::encode` / `decode` |
| W-04 | Single header value, ≤ 4 KB | ✅ | `encoder::encode` rejects `> MAX_HEADER_BYTES` |
| W-05 | Auxiliary requests carry no hint | ✅ | `RequestKind::is_auxiliary`; `tests/adapter.rs` |
| W-06 | No session identity → no hint | ✅ | empty `thread_id` returns `None`; `tests/adapter.rs` |
| W-07 | Retry reuses the same hint, `call_index` unchanged | ✅ | fingerprint reuse in `HintSession::build`; `tests/adapter.rs` |

### Shape

| ID | Requirement | Status |
|---|---|---|
| S-01 | Top level contains only `session` / `context_management` / `priority_control` / `cache_control` | ✅ `protocol::validate` |
| S-02 | Ordinary request: `session.state ∈ {start, append}` | ✅ |
| S-03 | Ordinary request never carries `context_management` | ✅ |
| S-04 | Manage request: `manage_request` is boolean `true` and exactly one edit | ✅ |
| S-05 | `edits[0].target == "session"`, type matches the verb | ✅ `State::edit` |
| S-06 | `priority.level` ∈ {1,3}, paired with the matching reason | ✅ `Priority` |
| S-07 | `cache_control` uses only `type` / `ttl_s` / `msg_offset` | ✅ |

### Values

| ID | Requirement | Status |
|---|---|---|
| V-01 | `call_index` starts at 1 and is monotonic (retries excluded) | ✅ |
| V-02 | `msg_offset` is the **wire** item count, not a token offset | ✅ `RequestView::messages.len()` over `input[]` |
| V-03 | `ttl_s` within [60, 3600] | ✅ clamped |
| V-04 | No `cache_control` on the first request after a rewrite | ✅ `after_rewrite` |
| V-05 | No static prefix claim on a cold signature | ✅ `last_signature` check |

## 2. L2 checklist

| ID | Requirement | Status |
|---|---|---|
| M-01 | `pause` → `offload`, `resume` → `prefetch`, `stop`/`compact` → `evict` | ✅ |
| M-02 | Manage body is the fixed placeholder `{model, input: [], stream: false}` | ✅ `ManageClient::build` |
| M-03 | Manage requests carry no `priority_control` / `cache_control` | ✅ |
| M-04 | `compact` carries the **superseded** epoch; the next ordinary request uses `epoch + 1` | ✅ `ManageClient::compact` |
| M-05 | `resume` uses `call_index + 1`, other verbs `max(1, call_index)` | ✅ |
| M-06 | An unresolved manage result isolates the session | ✅ `HintStore::isolate` / `assert_ready` |
| M-07 | HTTP 2xx is not treated as KV completion (state is not advanced by transport) | ✅ |

## 3. L3 — not claimed

Codex exposes no token-level draft/prefetch signal to the transport layer and no
archive event for a live conversation. An early `resume` could be emitted around
the HTTP call, but there is no honest prefetch lead time, so L3 is out of scope.

## 4. §7.5 Trigger source register

| state | Trigger in this harness | Required | Degradation when missing |
|---|---|---|---|
| `start` / `append` | `stream_responses_http` dispatch; `start` only for a fresh thread (`call_index == 1`, no history, not adopted) | yes | none |
| `pause` | no idle/away signal in Codex; emitted only by an explicit host call | level-dependent | not sent |
| `resume` | explicit host call before re-entering a paused session | level-dependent | session continues with `append` |
| `stop` | session teardown (`/clear`, process exit); host calls the manage client | level-dependent | only sent when the host destroys the session |
| `compact` | host calls `ManageClient::compact` after a committed remote history rewrite (`compact_remote_v2`); Codex's own compaction request never carries a hint | level-dependent | KV is not evicted |

## 5. §12.4 Degradation profile

| Missing capability | Degradation used here | Impact |
|---|---|---|
| No manage egress | Not applicable — manage requests are supported | — |
| No persistent event log | In-process counters, keyed by `thread_id` | `call_index` resets on restart; `epoch` is never fabricated; snapshot/restore is available |
| No user-input boundary | Backwards walk over `input[]` (`derive_turn_opening`) | A turn with no trailing user item (e.g. a resumed thread) is prioritised as 1 |
| No interval observation | Host passes `now`; otherwise TTL = 600 | Adaptive TTL is opt-in per request |
| No draft signal | `resume` only immediately before the request | No prefetch lead time |
| No archive event | `stop` only on session teardown | Archived sessions keep KV until deleted |
| Cannot estimate prefix reliably | Local CJK/4-chars-per-token heuristic, 128-token floor | May under-claim `cache_control` for unusual scripts |

## 6. Harness-specific facts

1. **Responses, not chat.** The body is `{model, input[], tools[], ...}`. There
   is no `instructions` field: Codex prepends base instructions as the first
   `input[]` item. `msg_offset` therefore counts every item — user message,
   developer message, reasoning summary, function call and function output each
   occupy one wire slot.
2. **Auxiliary calls are labelled by the host.** `CodexResponsesMetadata.request_kind`
   already distinguishes `Turn`, `Prewarm`, `Compaction(_)` and `Memory`. Only
   `Turn` is annotated; the patch still calls the hook for every request and the
   adapter rejects the rest before serialising the body.
3. **WebSocket must be disabled.** Codex prefers a long-lived Responses
   WebSocket whose handshake headers are fixed for the life of the connection.
   A per-request hint cannot ride it, so `responses_websocket_enabled()` returns
   `false` while a provider is installed. Removing the provider restores the
   upstream transport choice.
4. **One process, many conversations.** Codex keeps a `thread_id` per
   conversation and can run subagents in the same process. The store is keyed by
   `thread_id`, so `call_index` / `epoch` never leak between threads.
5. **The header hook sees the body in-process.** `HintHeadersContext::body` is
   the already-built JSON body. The adapter only reads item types, roles, tool
   names and text lengths; no prompt text is placed in the header.
6. **Retries reuse the hint.** `HintSession::build` fingerprints the logical
   request (model + tools + message views) and returns the previous value until
   the endpoint accepts it, satisfying W-07 even though Codex retries at the
   transport layer.
