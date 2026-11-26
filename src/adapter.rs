//! Codex CLI adapter: Responses request facts in, `X-Agent-Hint` out.
//!
//! Codex has no per-request header hook, so this crate is paired with a small
//! patch that adds one process-wide provider (see `patches/`). Nothing in this
//! module knows about Codex types: [`RequestFacts`] is the seam, and the body
//! is the JSON that is about to be sent.
//!
//! One process can host many conversations. Hints are kept per `thread_id`,
//! because that is Codex's conversation identity across turns.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde_json::Value;

use crate::encoder::encode;
use crate::protocol::HEADER_NAME;
use crate::state::{
    new_session_id, tool_names, HintConfig, HintError, HintSession, MessageView, RequestView,
    SessionSnapshot,
};

/// Where a model call sits in the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestKind {
    /// The agent turn itself, including tool-result continuations.
    #[default]
    Turn,
    /// A warmup/prewarm request that does not generate output.
    Prewarm,
    /// A remote compaction summary over older history.
    Compaction,
    /// Memory extraction / consolidation.
    Memory,
    /// A request the host did not classify.
    Unknown,
}

impl RequestKind {
    /// Parse the label handed over by the patched host.
    #[must_use]
    pub fn from_label(label: &str) -> Self {
        match label {
            "turn" => Self::Turn,
            "prewarm" => Self::Prewarm,
            "compaction" => Self::Compaction,
            "memory" => Self::Memory,
            _ => Self::Unknown,
        }
    }

    /// Auxiliary calls never carry a hint (protocol section 7.2).
    #[must_use]
    pub const fn is_auxiliary(self) -> bool {
        !matches!(self, Self::Turn)
    }
}

/// Everything the adapter needs from one outbound request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestFacts {
    /// Codex conversation id.
    pub thread_id: String,
    pub model: String,
    /// Logical input items (`input[]` on the wire), in order.
    pub input: Vec<MessageView>,
    pub tools: Vec<String>,
    pub kind: RequestKind,
    /// Seconds on a monotonic clock; used for the adaptive TTL.
    pub now: Option<f64>,
}

impl RequestFacts {
    /// Build facts from the exact JSON body that is about to be sent.
    ///
    /// `kind_label` is `turn` / `prewarm` / `compaction` / `memory`, as
    /// classified by the host. The body is the Responses API payload:
    /// `input[]` items plus a `tools` array.
    #[must_use]
    pub fn from_body(
        thread_id: &str,
        model: &str,
        kind_label: &str,
        body: &Value,
    ) -> Self {
        let input = body
            .get("input")
            .map_or_else(Vec::new, responses_message_views);
        Self {
            thread_id: thread_id.to_string(),
            model: model.to_string(),
            input,
            tools: body.get("tools").map_or_else(Vec::new, tool_names),
            kind: RequestKind::from_label(kind_label),
            now: None,
        }
    }

    /// Project the facts into the provider-neutral view used by the state machine.
    #[must_use]
    pub fn view(&self) -> RequestView {
        RequestView {
            messages: self.input.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            auxiliary: self.kind.is_auxiliary(),
            turn_opening: None,
            fingerprint: None,
            // Codex keeps the base instructions inside `input[]`, so there is
            // no out-of-band system text to add to the cache floor.
            system_text: None,
            now: self.now,
        }
    }

    /// True when this looks like a resumed conversation rather than a new one.
    ///
    /// Base instructions arrive as `developer`/`system` items and do not count
    /// as history; assistant output or tool traffic does.
    #[must_use]
    pub fn has_history(&self) -> bool {
        self.input.iter().any(|message| {
            (message.role != "user" && message.role != "system" && message.role != "developer")
                || message.has_tool_call
                || message.has_tool_result
        })
    }
}

/// A ready-to-send header pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintHeader {
    pub name: &'static str,
    pub value: String,
}

/// Per-process, per-conversation hint state.
#[derive(Debug)]
pub struct HintStore {
    sessions: Mutex<HashMap<String, HintSession>>,
    config: HintConfig,
}

impl HintStore {
    #[must_use]
    pub fn new(config: HintConfig) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            config,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, HintSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Build and encode the hint for one ordinary request.
    pub fn try_headers(&self, facts: &RequestFacts) -> Result<Option<HintHeader>, HintError> {
        if facts.kind.is_auxiliary() || !self.config.enabled {
            return Ok(None);
        }
        // No conversation identity means no session to annotate (W-06).
        if facts.thread_id.trim().is_empty() {
            return Ok(None);
        }
        let view = facts.view();
        let mut sessions = self.lock();
        if !sessions.contains_key(&facts.thread_id) && sessions.len() >= 256 {
            // Keep the process bounded; old conversations are cold.
            sessions.clear();
        }
        let session = match sessions.entry(facts.thread_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut config = self.config.clone();
                config.has_history |= facts.has_history();
                entry.insert(HintSession::new(None, config)?)
            }
        };
        let hint = session.build(&view)?;
        let Some(hint) = hint else {
            return Ok(None);
        };
        let value = encode(&hint).map_err(|error| HintError::InvalidConfig(error.to_string()))?;
        Ok(Some(HintHeader {
            name: HEADER_NAME,
            value,
        }))
    }

    /// Fail-open variant: a hint problem must never break the model call.
    #[must_use]
    pub fn headers(&self, facts: &RequestFacts) -> Option<HintHeader> {
        self.try_headers(facts).ok().flatten()
    }

    #[must_use]
    pub fn snapshot(&self, thread_id: &str) -> Option<SessionSnapshot> {
        self.lock().get(thread_id).map(HintSession::snapshot)
    }

    /// Idempotent: a second call keeps the first session id.
    pub fn session_id(&self, thread_id: &str) -> Option<String> {
        self.lock().get(thread_id).map(|session| session.session_id.clone())
    }

    pub fn note_accepted(&self, thread_id: &str) {
        if let Some(session) = self.lock().get_mut(thread_id) {
            session.note_accepted();
        }
    }

    pub fn note_response_end(&self, thread_id: &str, now: Option<f64>) {
        if let Some(session) = self.lock().get_mut(thread_id) {
            session.note_response_end(now);
        }
    }

    /// Record a committed history rewrite; return the superseded epoch.
    pub fn note_rewrite(&self, thread_id: &str) -> Option<u64> {
        self.lock().get_mut(thread_id).map(HintSession::note_rewrite)
    }

    pub fn isolate(&self, thread_id: &str) {
        if let Some(session) = self.lock().get_mut(thread_id) {
            session.isolate();
        }
    }

    pub fn clear_isolation(&self, thread_id: &str) {
        if let Some(session) = self.lock().get_mut(thread_id) {
            session.clear_isolation();
        }
    }

    /// Session for manage requests; created on demand so a verb works even if
    /// no model call happened yet in this process.
    pub fn session(&self, thread_id: &str, has_history: bool) -> Result<String, HintError> {
        let mut sessions = self.lock();
        if !sessions.contains_key(thread_id) {
            let mut config = self.config.clone();
            config.has_history |= has_history;
            sessions.insert(thread_id.to_string(), HintSession::new(None, config)?);
        }
        Ok(sessions
            .get(thread_id)
            .map(|session| session.session_id.clone())
            .unwrap_or_default())
    }

    pub fn with_session<R>(&self, thread_id: &str, f: impl FnOnce(&mut HintSession) -> R) -> Option<R> {
        self.lock().get_mut(thread_id).map(f)
    }
}

static INSTALLED: OnceLock<HintStore> = OnceLock::new();

/// Install the process-wide store when `CODEX_AGENT_HINTS` is truthy.
///
/// Returns `true` only when this call (or an earlier one) installed a store.
/// A malformed configuration disables hints instead of failing requests.
pub fn install_from_env() -> bool {
    if let Some(_store) = INSTALLED.get() {
        return true;
    }
    if !env_flag("CODEX_AGENT_HINTS") {
        return false;
    }
    let Some(config) = config_from_env() else {
        return false;
    };
    INSTALLED.set(HintStore::new(config)).is_ok()
}

#[must_use]
pub fn installed_store() -> Option<&'static HintStore> {
    INSTALLED.get()
}

/// Entry point for the patched Codex header hook.
///
/// Returns `None` when no store is installed or the hint cannot be built: the
/// caller keeps its original headers and the request still goes out.
#[must_use]
pub fn headers_for(facts: &RequestFacts) -> Option<(String, String)> {
    let header = INSTALLED.get()?.headers(facts)?;
    Some((header.name.to_string(), header.value))
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().as_str(),
        "1" | "true" | "TRUE" | "yes" | "on"
    )
}

fn env_i64(name: &str) -> Option<i64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn config_from_env() -> Option<HintConfig> {
    let mut config = HintConfig::default();
    if let Ok(value) = std::env::var("CODEX_AGENT_HINTS_CACHE") {
        if matches!(value.as_str(), "omit" | "off" | "0") {
            config.cache = crate::state::Derive::Omit;
        }
    }
    if let Some(level) = env_i64("CODEX_AGENT_HINTS_PRIORITY_LEVEL") {
        config.priority_level = Some(level);
        config.priority_reason = crate::protocol::Priority::from_level(level)
            .map(|priority| priority.reason().to_string());
    }
    if let Some(ttl) = env_i64("CODEX_AGENT_HINTS_DEFAULT_TTL_S") {
        config.default_ttl_s = ttl;
    }
    if let Some(ttl) = env_i64("CODEX_AGENT_HINTS_TTL_MIN_S") {
        config.ttl_min_s = ttl;
    }
    if let Some(ttl) = env_i64("CODEX_AGENT_HINTS_TTL_MAX_S") {
        config.ttl_max_s = ttl;
    }
    config.validate().ok()?;
    Some(config)
}

/// Convert Responses `input[]` items into logical message views.
///
/// One item is one message, which is what `msg_offset` counts: instructions,
/// reasoning, calls and results each occupy a wire slot.
#[must_use]
pub fn responses_message_views(input: &Value) -> Vec<MessageView> {
    let Value::Array(items) = input else {
        return Vec::new();
    };
    items.iter().map(message_view).collect()
}

fn message_view(item: &Value) -> MessageView {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "message" => MessageView {
            role: item
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_string(),
            text: content_text(item.get("content")),
            has_tool_result: false,
            has_tool_call: false,
        },
        "function_call"
        | "custom_tool_call"
        | "local_shell_call"
        | "tool_search_call"
        | "image_generation_call"
        | "web_search_call" => MessageView {
            role: "assistant".to_string(),
            text: String::new(),
            has_tool_result: false,
            has_tool_call: true,
        },
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => MessageView {
            role: "tool".to_string(),
            text: String::new(),
            has_tool_result: true,
            has_tool_call: false,
        },
        "reasoning" => MessageView {
            role: "assistant".to_string(),
            text: summary_text(item.get("summary")),
            has_tool_result: false,
            has_tool_call: false,
        },
        "compaction" | "context_compaction" => MessageView {
            role: "system".to_string(),
            text: String::new(),
            has_tool_result: false,
            has_tool_call: false,
        },
        // Unknown item kinds still occupy a wire slot and belong to the
        // assistant side of the conversation.
        _ => MessageView {
            role: "assistant".to_string(),
            text: String::new(),
            has_tool_result: false,
            has_tool_call: true,
        },
    }
}

fn summary_text(summary: Option<&Value>) -> String {
    match summary {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(object) => object
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

/// Generate an opaque session id for a conversation.
#[must_use]
pub fn opaque_session_id() -> String {
    new_session_id("ah")
}
