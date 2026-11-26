# Changelog

## 0.1.0

First release. Implements the sender side of `agent-hint-protocol` v1.0
(L1 + L2, L3 not claimed) for Codex CLI (`codex-rs`, commit `9e51c74`).

- `protocol`: header name, 4 KB budget, six states, priority fold to `{1, 3}`,
  full shape validation (four top-level blocks only).
- `encoder`: base64url without padding, padded input accepted on decode.
- `state`: `start`/`append` derivation, `call_index`, `epoch`, fingerprint-based
  retry reuse, adaptive TTL, wire `msg_offset`, prefix heuristic that treats
  absent text as `None`.
- `manage`: fixed placeholder body `{model, input: [], stream: false}`,
  `offload`/`prefetch`/`evict` mapping, superseded epoch on `compact`.
- `adapter`: Responses `input[]` projection, per-`thread_id` session registry,
  `turn` / `prewarm` / `compaction` / `memory` classification, fail-open header
  generation, bounded registry.
- `patches/0001-core-hint-header-hook.patch`: generic outbound header hook for
  `codex-rs/core`, plus a WebSocket gate (`git apply --check` and
  `cargo check -p codex-core` verified).
- `examples/wire_capture`: real TCP capture of ten requests, decoded with the
  same codec a gateway uses.
