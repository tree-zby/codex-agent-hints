# Seam probe against a patched `codex-rs`

The natural place for a seam test is `core/tests/`, next to the other wiremock
tests. In a constrained environment that is not runnable: resolving
`cargo test -p codex-core` inside the workspace pulls the whole Chromium/V8
submodule chain (`rusty_v8` → `chromium_build`, `icu`, `abseil-cpp`, …), which
is a multi-hundred-megabyte fetch unrelated to `core`.

This probe compiles the patched `codex-core` on its own instead:

```bash
# 1. patched host
git clone https://github.com/openai/codex
cd codex && git checkout 9e51c74
git apply /path/to/codex-agent-hints/patches/0001-core-hint-header-hook.patch

# 2. probe crate
cd /path/to/codex-agent-hints/e2e/probe
cp Cargo.toml.template Cargo.toml
$EDITOR Cargo.toml          # point codex-core at the patched checkout
cp /path/to/codex/codex-rs/Cargo.lock .
cargo test --test hint_headers -- --nocapture
```

`Cargo.toml.template` carries the workspace `[patch.crates-io]` table copied from
`codex-rs/Cargo.toml`; without it `tokio-tungstenite` resolves to the upstream
crate, which lacks the `proxy` feature `codex-api` requires.

## What this does and does not prove

It proves the patched module compiles against the real crate and that the
process-wide provider contract behaves as documented:

- `set_hint_headers_provider` → `hint_headers_active() == true`
  (this is what flips `responses_websocket_enabled()` to `false`);
- replacing returns the previous provider;
- clearing restores the default, and no provider means no headers.

It does **not** stand in for an in-tree wiremock test of
`ModelClientSession::stream`. Such a test would drive the patched HTTP path end
to end; it is not shipped because it cannot run in the environment this release
was prepared in, and a test nobody can execute is worse than no test. The
outbound-wire claim in the README therefore rests on the raw TCP capture from
`examples/wire_capture.rs` plus the compile check of the two client call sites.
