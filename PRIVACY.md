# Privacy

`codex-agent-hints` adds one HTTP header to model requests that Codex already
sends. It does not proxy, duplicate or store those requests.

## What leaves the process

Only the `X-Agent-Hint` header value:

- an opaque `session_id` generated locally (for example `ah-9f3c…`), never a
  user id, email, hostname, thread title or file path;
- integer counters: `call_index`, `epoch`;
- a lifecycle state word (`start`, `append`, `pause`, `resume`, `stop`, `compact`);
- a priority level and reason;
- cache metadata: `type`, `ttl_s`, `msg_offset` (a wire-item count, not text).

The header is sent to the same endpoint the model request already targets. No
telemetry, analytics, crash reporting or "phone home" endpoint is used.

## What never leaves the process

- Prompt text, tool arguments, tool output, file contents and images.
- Credentials. The crate never reads API keys; it does not touch the body or the
  authorization headers built by Codex.

The adapter reads `input[].type`, `role`, summary text length, `tools[].name`
and the item count to derive the header. None of that content is copied into the
header.

## Local state

`HintStore` keeps per-thread counters in memory, bounded at 256 conversations.
`snapshot()` / `restore()` exist for hosts that want to persist them; nothing is
written to disk by this crate.

## Opt-out

`HintConfig { enabled: false, .. }` returns `None` for every request. So does
leaving `CODEX_AGENT_HINTS` unset: no provider is installed and the patched host
behaves exactly as upstream.
