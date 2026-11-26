//! Deterministic hint derivation (protocol sections 5-6).
//!
//! The state machine is the sender side of the contract: one request in, one
//! hint out, with every mutable fact held on [`HintSession`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::protocol::{Priority, State};

/// Raised when a session has an unresolved manage mutation.
///
/// Protocol section 8.4: after a manage request with an unknown result the
/// sender MUST refuse new main-line requests until the operator clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIsolated {
    pub session_id: String,
}

impl std::fmt::Display for SessionIsolated {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "session {:?} is isolated after an unresolved manage request; \
             clear it once the backend state is confirmed, or start a new session",
            self.session_id
        )
    }
}

impl std::error::Error for SessionIsolated {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintError {
    InvalidSessionId,
    InvalidConfig(String),
    UnknownVerb(String),
    Isolated(SessionIsolated),
}

impl std::fmt::Display for HintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HintError::InvalidSessionId => {
                write!(
                    formatter,
                    "session_id must be a non-empty string <= 256 chars"
                )
            }
            HintError::InvalidConfig(message) => {
                write!(formatter, "invalid hint config: {message}")
            }
            HintError::UnknownVerb(verb) => write!(
                formatter,
                "manage verb must be one of pause/resume/stop/compact, got {verb:?}"
            ),
            HintError::Isolated(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HintError {}

impl From<SessionIsolated> for HintError {
    fn from(value: SessionIsolated) -> Self {
        HintError::Isolated(value)
    }
}

/// `dynamic` derives a block from the facts; `omit` leaves it out entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Derive {
    #[default]
    Dynamic,
    Omit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HintConfig {
    pub enabled: bool,
    pub priority: Derive,
    pub cache: Derive,
    pub priority_level: Option<i64>,
    pub priority_reason: Option<String>,
    pub cache_min_prefix_tokens: i64,
    pub default_ttl_s: i64,
    pub ttl_min_s: i64,
    pub ttl_max_s: i64,
    pub parent_session_id: Option<String>,
    pub has_history: bool,
    pub assume_existing_session: bool,
}

impl Default for HintConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: Derive::Dynamic,
            cache: Derive::Dynamic,
            priority_level: None,
            priority_reason: None,
            cache_min_prefix_tokens: 128,
            default_ttl_s: 600,
            ttl_min_s: 60,
            ttl_max_s: 3600,
            parent_session_id: None,
            has_history: false,
            assume_existing_session: false,
        }
    }
}

impl HintConfig {
    /// Validate TTL bounds and the priority/reason pairing.
    pub fn validate(&self) -> Result<(), HintError> {
        if self.ttl_min_s < 60 || self.ttl_max_s > 3600 || self.ttl_min_s > self.ttl_max_s {
            return Err(HintError::InvalidConfig(
                "TTL bounds must satisfy 60 <= min <= max <= 3600".to_string(),
            ));
        }
        if self.default_ttl_s < self.ttl_min_s || self.default_ttl_s > self.ttl_max_s {
            return Err(HintError::InvalidConfig(
                "default_ttl_s must be inside the TTL bounds".to_string(),
            ));
        }
        if let Some(level) = self.priority_level {
            if level < 1 {
                return Err(HintError::InvalidConfig(
                    "priority_level must be >= 1".to_string(),
                ));
            }
            let expected = Priority::from_level(level).expect("level >= 1");
            if let Some(reason) = &self.priority_reason {
                if reason != expected.reason() {
                    return Err(HintError::InvalidConfig(format!(
                        "priority_level {} must pair with reason {:?}",
                        expected.level(),
                        expected.reason()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Static priority folded to `{1, 3}`, when configured.
    pub fn static_priority(&self) -> Option<Priority> {
        self.priority_level.and_then(Priority::from_level)
    }
}

/// Provider-neutral view of one logical input message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageView {
    pub role: String,
    pub text: String,
    pub has_tool_result: bool,
    pub has_tool_call: bool,
}

impl MessageView {
    #[must_use]
    pub fn new(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            text: text.into(),
            has_tool_result: false,
            has_tool_call: false,
        }
    }
}

/// Everything the derivation needs to know about one model request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestView {
    pub messages: Vec<MessageView>,
    pub model: String,
    pub tools: Vec<String>,
    pub auxiliary: bool,
    pub turn_opening: Option<bool>,
    pub fingerprint: Option<String>,
    /// Out-of-band prefix text (Anthropic sends it as a top-level `system`
    /// field, not as a message). Counted for the cache floor, never for
    /// `msg_offset`.
    pub system_text: Option<String>,
    /// Seconds on a monotonic clock; `None` falls back to `default_ttl_s`.
    pub now: Option<f64>,
}

/// Serializable facts, for persistence across processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub call_index: u64,
    pub epoch: u64,
    pub first_accepted: bool,
    pub started: bool,
    pub isolated: bool,
    pub last_signature: Option<Vec<String>>,
    pub last_response_end: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pending,
    Accepted,
}

/// Generate an opaque, non-identifying session id (max 256 chars).
#[must_use]
pub fn new_session_id(prefix: &str) -> String {
    // No `rand` dependency: hash the clock, the address of a stack slot and a
    // process-local counter. The id is opaque, not a security boundary.
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}-{hex}")
}

/// Stable identity of a logical request, used to reuse a hint on retry.
#[must_use]
pub fn request_fingerprint(request: &RequestView) -> String {
    let mut tools = request.tools.clone();
    tools.sort();
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            json!([
                message.role,
                message.text,
                message.has_tool_result,
                message.has_tool_call
            ])
        })
        .collect();
    let payload = json!({
        "model": request.model,
        "tools": tools,
        "messages": messages,
    });
    let canonical = serde_json::to_vec(&payload).expect("request view is serialisable");
    let digest = Sha256::digest(canonical);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Approximate prefix size; `None` when there is no text to measure.
///
/// CJK characters count as one token each, other text as four characters per
/// token (protocol section 6.8).
#[must_use]
pub fn estimate_prefix_tokens(messages: &[MessageView]) -> Option<i64> {
    let mut saw_text = false;
    let mut total = 0.0_f64;
    for message in messages {
        if !message.text.is_empty() {
            saw_text = true;
        }
        total += estimate_text_tokens(&message.text);
    }
    if !saw_text {
        return None;
    }
    Some(total as i64)
}

/// Approximate token count for one piece of text.
///
/// CJK characters count as one token each, other text as four characters per
/// token (protocol section 6.8).
#[must_use]
pub fn estimate_text_tokens(text: &str) -> f64 {
    let mut cjk = 0_usize;
    let mut chars = 0_usize;
    for character in text.chars() {
        chars += 1;
        if ('\u{3400}'..='\u{9fff}').contains(&character) {
            cjk += 1;
        }
    }
    cjk as f64 + (chars - cjk) as f64 / 4.0
}

/// A turn opens when the latest user input has no reply after it.
///
/// Walks backwards: empty assistant placeholders are skipped, the first real
/// assistant message or tool call means the turn is already in progress, and a
/// plain user message means this is the first main-line call of the turn.
#[must_use]
pub fn derive_turn_opening(messages: &[MessageView]) -> bool {
    for message in messages.iter().rev() {
        if message.role == "assistant" {
            if message.text.is_empty() && !message.has_tool_call {
                continue;
            }
            return false;
        }
        if message.has_tool_call || message.has_tool_result {
            return false;
        }
        if message.role == "user" {
            return true;
        }
    }
    false
}

/// Mutable per-session facts plus the pure hint builders.
#[derive(Debug, Clone)]
pub struct HintSession {
    pub session_id: String,
    pub call_index: u64,
    pub epoch: u64,
    pub first_accepted: bool,
    pub started: bool,
    pub isolated: bool,
    pub after_rewrite: bool,
    pub config: HintConfig,
    last_signature: Option<Vec<String>>,
    last_response_end: Option<f64>,
    last_fingerprint: Option<String>,
    last_hint: Option<Value>,
    last_outcome: Outcome,
}

impl HintSession {
    pub fn new(session_id: Option<String>, config: HintConfig) -> Result<Self, HintError> {
        config.validate()?;
        let session_id = session_id.unwrap_or_else(|| new_session_id("ah"));
        if session_id.is_empty() || session_id.chars().count() > 256 {
            return Err(HintError::InvalidSessionId);
        }
        Ok(Self {
            session_id,
            call_index: 0,
            epoch: 0,
            first_accepted: false,
            started: false,
            isolated: false,
            after_rewrite: false,
            config,
            last_signature: None,
            last_response_end: None,
            last_fingerprint: None,
            last_hint: None,
            last_outcome: Outcome::Pending,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id.clone(),
            call_index: self.call_index,
            epoch: self.epoch,
            first_accepted: self.first_accepted,
            started: self.started,
            isolated: self.isolated,
            last_signature: self.last_signature.clone(),
            last_response_end: self.last_response_end,
        }
    }

    pub fn restore(&mut self, snapshot: &SessionSnapshot) -> Result<(), HintError> {
        if snapshot.session_id != self.session_id {
            return Err(HintError::InvalidConfig(
                "snapshot belongs to a different session".to_string(),
            ));
        }
        self.call_index = snapshot.call_index;
        self.epoch = snapshot.epoch;
        self.first_accepted = snapshot.first_accepted;
        self.started = snapshot.started;
        self.isolated = snapshot.isolated;
        self.last_signature = snapshot.last_signature.clone();
        self.last_response_end = snapshot.last_response_end;
        Ok(())
    }

    /// Record that the endpoint accepted the in-flight ordinary request.
    pub fn note_accepted(&mut self) {
        self.first_accepted = true;
        self.last_outcome = Outcome::Accepted;
    }

    pub fn note_response_end(&mut self, now: Option<f64>) {
        if let Some(now) = now {
            self.last_response_end = Some(now);
        }
    }

    /// Record a committed history rewrite; return the superseded epoch.
    pub fn note_rewrite(&mut self) -> u64 {
        let superseded = self.epoch;
        self.epoch += 1;
        self.after_rewrite = true;
        superseded
    }

    pub fn isolate(&mut self) {
        self.isolated = true;
    }

    pub fn clear_isolation(&mut self) {
        self.isolated = false;
        self.last_outcome = Outcome::Pending;
    }

    pub fn assert_ready(&self) -> Result<(), HintError> {
        if self.isolated {
            return Err(SessionIsolated {
                session_id: self.session_id.clone(),
            }
            .into());
        }
        Ok(())
    }

    /// Return the hint for an ordinary request, or `None` to send none.
    pub fn build(&mut self, request: &RequestView) -> Result<Option<Value>, HintError> {
        if !self.config.enabled || request.auxiliary {
            return Ok(None);
        }
        self.assert_ready()?;

        let fingerprint = request
            .fingerprint
            .clone()
            .unwrap_or_else(|| request_fingerprint(request));
        if self.last_fingerprint.as_deref() == Some(fingerprint.as_str())
            && self.last_outcome != Outcome::Accepted
        {
            // Same logical request again: a retry reuses the exact hint.
            return Ok(self.last_hint.clone());
        }

        self.call_index += 1;
        let opening = request
            .turn_opening
            .unwrap_or_else(|| derive_turn_opening(&request.messages));
        let state = self.ordinary_state();

        let mut session = Map::new();
        session.insert("session_id".into(), Value::String(self.session_id.clone()));
        session.insert("call_index".into(), json!(self.call_index));
        session.insert("epoch".into(), json!(self.epoch));
        session.insert("state".into(), Value::String(state.as_str().to_string()));
        if let Some(parent) = &self.config.parent_session_id {
            session.insert("parent_session_id".into(), Value::String(parent.clone()));
        }

        let mut hint = Map::new();
        hint.insert("session".into(), Value::Object(session));

        if self.config.priority == Derive::Dynamic {
            let priority = self.priority(opening);
            hint.insert(
                "priority_control".into(),
                json!({"priority": {"level": priority.level(), "reason": priority.reason()}}),
            );
        }
        if let Some(cache) = self.cache(request) {
            hint.insert("cache_control".into(), cache);
        }

        let hint = Value::Object(hint);
        self.last_fingerprint = Some(fingerprint);
        self.last_hint = Some(hint.clone());
        self.last_outcome = Outcome::Pending;
        self.after_rewrite = false;
        Ok(Some(hint))
    }

    /// Build a manage hint (protocol sections 5.3 and 8.2).
    pub fn build_manage(
        &mut self,
        verb: State,
        call_index: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<Value, HintError> {
        let Some(edit) = verb.edit() else {
            return Err(HintError::UnknownVerb(verb.as_str().to_string()));
        };
        let call_index = call_index.unwrap_or_else(|| self.manage_call_index(verb));
        let epoch = epoch.unwrap_or(self.epoch);

        let mut session = Map::new();
        session.insert("session_id".into(), Value::String(self.session_id.clone()));
        session.insert("call_index".into(), json!(call_index.max(1)));
        session.insert("epoch".into(), json!(epoch));
        session.insert("state".into(), Value::String(verb.as_str().to_string()));
        if let Some(parent) = &self.config.parent_session_id {
            session.insert("parent_session_id".into(), Value::String(parent.clone()));
        }

        Ok(json!({
            "session": Value::Object(session),
            "context_management": {
                "manage_request": true,
                "edits": [{"type": edit, "target": "session"}],
            },
        }))
    }

    fn ordinary_state(&mut self) -> State {
        let fresh = self.call_index == 1
            && !self.config.has_history
            && !self.config.assume_existing_session
            && !self.first_accepted
            && !self.started;
        if fresh {
            self.started = true;
            return State::Start;
        }
        State::Append
    }

    fn priority(&self, opening: bool) -> Priority {
        if let Some(static_priority) = self.config.static_priority() {
            return static_priority;
        }
        if opening {
            Priority::UserWaiting
        } else {
            Priority::CriticalPath
        }
    }

    fn cache(&mut self, request: &RequestView) -> Option<Value> {
        if self.config.cache == Derive::Omit || self.after_rewrite {
            return None;
        }

        let mut signature: Vec<String> = request.tools.clone();
        signature.sort();
        let signature = std::iter::once(request.model.clone())
            .chain(signature)
            .collect::<Vec<_>>();
        let stable = match &self.last_signature {
            Some(previous) => *previous == signature,
            None => self.call_index <= 1,
        };
        self.last_signature = Some(signature);
        if !stable {
            return None;
        }

        if let Some(tokens) = self.prefix_tokens(request) {
            if tokens < self.config.cache_min_prefix_tokens {
                return None;
            }
        }

        let msg_offset = request.messages.len();
        if msg_offset < 1 {
            return None;
        }

        let ttl = self.ttl(request.now);
        Some(json!({"type": "ephemeral", "ttl_s": ttl, "msg_offset": msg_offset}))
    }

    fn prefix_tokens(&self, request: &RequestView) -> Option<i64> {
        let body_tokens = estimate_prefix_tokens(&request.messages);
        let system_tokens = request
            .system_text
            .as_deref()
            .map(estimate_text_tokens)
            .unwrap_or(0.0);
        match (body_tokens, system_tokens) {
            (Some(tokens), system) => Some(tokens + system.round() as i64),
            (None, system) if system > 0.0 => Some(system.round() as i64),
            (None, _) => None,
        }
    }

    fn ttl(&self, now: Option<f64>) -> i64 {
        let (Some(now), Some(last_response_end)) = (now, self.last_response_end) else {
            return self.config.default_ttl_s;
        };
        let gap = (now - last_response_end).max(0.0);
        let ttl = (gap * 2.0).round() as i64;
        ttl.clamp(self.config.ttl_min_s, self.config.ttl_max_s)
    }

    fn manage_call_index(&self, verb: State) -> u64 {
        if verb == State::Resume {
            return self.call_index + 1;
        }
        self.call_index.max(1)
    }
}

/// Convert OpenAI-style chat messages (already parsed JSON) into views.
#[must_use]
pub fn openai_message_views(messages: &Value) -> Vec<MessageView> {
    let list: Vec<&Value> = match messages {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![messages],
        Value::String(text) => return vec![MessageView::new("user", text.clone())],
        _ => Vec::new(),
    };

    list.into_iter()
        .map(|message| {
            let Some(object) = message.as_object() else {
                return MessageView::new("user", message.to_string());
            };
            let role = object
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_string();
            let text = content_text(object.get("content"));
            let content = object.get("content");
            let has_tool_result = role == "tool"
                || object.get("tool_call_id").is_some()
                || content_has_type(content, "tool_result");
            let has_tool_call = object
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
                || content_has_type(content, "tool_use");
            MessageView {
                role,
                text,
                has_tool_result,
                has_tool_call,
            }
        })
        .collect()
}

fn content_has_type(content: Option<&Value>, wanted: &str) -> bool {
    content.and_then(Value::as_array).is_some_and(|parts| {
        parts
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some(wanted))
    })
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

/// Extract tool names from OpenAI or Anthropic tool schemas, sorted.
#[must_use]
pub fn tool_names(tools: &Value) -> Vec<String> {
    let mut names = BTreeSet::new();
    match tools {
        Value::Array(items) => {
            for tool in items {
                match tool {
                    Value::String(name) => {
                        names.insert(name.clone());
                    }
                    Value::Object(object) => {
                        if let Some(name) = object
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                        {
                            names.insert(name.to_string());
                        } else if let Some(name) = object.get("name").and_then(Value::as_str) {
                            names.insert(name.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => names.extend(map.keys().cloned()),
        _ => {}
    }
    names.into_iter().collect()
}
