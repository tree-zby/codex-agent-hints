//! Wire schema and validation for `agent-hint-protocol` v1.0.
//!
//! The sender is the only party that needs this module at runtime: a receiver
//! MUST NOT reject a model request because a hint is malformed, so validation
//! here exists to keep *us* honest, never to police the network.

use serde_json::Value;

/// HTTP header that carries the base64url-encoded hint.
pub const HEADER_NAME: &str = "X-Agent-Hint";

/// Sender budget from protocol section 3.3.
pub const MAX_HEADER_BYTES: usize = 4096;

/// The six lifecycle values. Ordinary requests use `start` / `append`; every
/// other value is a manage request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Start,
    Append,
    Pause,
    Resume,
    Stop,
    Compact,
}

impl State {
    pub const ALL: [State; 6] = [
        State::Start,
        State::Append,
        State::Pause,
        State::Resume,
        State::Stop,
        State::Compact,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Start => "start",
            State::Append => "append",
            State::Pause => "pause",
            State::Resume => "resume",
            State::Stop => "stop",
            State::Compact => "compact",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        State::ALL.into_iter().find(|state| state.as_str() == value)
    }

    #[must_use]
    pub const fn is_ordinary(self) -> bool {
        matches!(self, State::Start | State::Append)
    }

    #[must_use]
    pub const fn is_manage(self) -> bool {
        !self.is_ordinary()
    }

    /// The single edit type that a manage request for this state must carry.
    #[must_use]
    pub const fn edit(self) -> Option<&'static str> {
        match self {
            State::Pause => Some("offload"),
            State::Resume => Some("prefetch"),
            State::Stop | State::Compact => Some("evict"),
            State::Start | State::Append => None,
        }
    }
}

/// Priority level 3 always pairs with `user_waiting`; level 1 with
/// `critical_path` (protocol section 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    UserWaiting,
    CriticalPath,
}

impl Priority {
    #[must_use]
    pub const fn level(self) -> i64 {
        match self {
            Priority::UserWaiting => 3,
            Priority::CriticalPath => 1,
        }
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Priority::UserWaiting => "user_waiting",
            Priority::CriticalPath => "critical_path",
        }
    }

    #[must_use]
    pub fn from_level(level: i64) -> Option<Self> {
        match level {
            3..=i64::MAX => Some(Priority::UserWaiting),
            1..=2 => Some(Priority::CriticalPath),
            _ => None,
        }
    }
}

/// True when `context_management.manage_request` is the boolean `true`.
#[must_use]
pub fn is_manage_hint(hint: &Value) -> bool {
    hint.get("context_management")
        .and_then(|context| context.get("manage_request"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Return every contract violation found in `hint`.
#[must_use]
pub fn validate(hint: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(object) = hint.as_object() else {
        return vec!["hint must be a JSON object".to_string()];
    };

    const TOP_LEVEL: [&str; 4] = [
        "session",
        "context_management",
        "priority_control",
        "cache_control",
    ];
    for key in object.keys() {
        if !TOP_LEVEL.contains(&key.as_str()) {
            errors.push(format!("unexpected top-level key: {key:?}"));
        }
    }

    let Some(session) = object.get("session").and_then(Value::as_object) else {
        errors.push("session is required and must be an object".to_string());
        return errors;
    };
    validate_session(session, &mut errors);

    let state = session.get("state").and_then(Value::as_str);
    let manage = is_manage_hint(hint);

    if let Some(context) = object.get("context_management") {
        validate_context_management(context, &mut errors);
    }

    if manage {
        match state.and_then(State::parse) {
            Some(parsed) if parsed.is_manage() => {
                if let Some(expected) = parsed.edit() {
                    let actual = object
                        .get("context_management")
                        .and_then(|context| context.get("edits"))
                        .and_then(Value::as_array)
                        .and_then(|edits| edits.first())
                        .and_then(|edit| edit.get("type"))
                        .and_then(Value::as_str);
                    if actual != Some(expected) {
                        errors.push(format!(
                            "edits[0].type must be {expected:?} for state {:?}",
                            parsed.as_str()
                        ));
                    }
                }
            }
            _ => errors.push(format!(
                "manage request state must be one of pause/resume/stop/compact, got {state:?}"
            )),
        }
        for forbidden in ["priority_control", "cache_control"] {
            if object.contains_key(forbidden) {
                errors.push(format!("{forbidden} MUST NOT appear on a manage request"));
            }
        }
    } else {
        if object.contains_key("context_management") {
            errors.push("context_management MUST NOT appear on an ordinary request".to_string());
        }
        match state.and_then(State::parse) {
            Some(parsed) if parsed.is_ordinary() => {}
            _ => errors.push(format!(
                "ordinary request state must be one of start/append, got {state:?}"
            )),
        }
    }

    if let Some(control) = object.get("priority_control") {
        validate_priority_control(control, &mut errors);
    }
    if let Some(cache) = object.get("cache_control") {
        validate_cache_control(cache, &mut errors);
    }

    errors
}

fn validate_session(session: &serde_json::Map<String, Value>, errors: &mut Vec<String>) {
    const ALLOWED: [&str; 5] = [
        "session_id",
        "parent_session_id",
        "call_index",
        "epoch",
        "state",
    ];
    for key in session.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            errors.push(format!("unexpected session key: {key:?}"));
        }
    }

    match session.get("session_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() && id.chars().count() <= 256 => {}
        Some(_) => {
            errors.push("session.session_id must be between 1 and 256 characters".to_string())
        }
        None => errors.push("session.session_id must be a non-empty string".to_string()),
    }

    if let Some(parent) = session.get("parent_session_id") {
        if !parent.is_null() && parent.as_str().map_or(true, str::is_empty) {
            errors.push(
                "session.parent_session_id must be a non-empty string when present".to_string(),
            );
        }
    }

    match session.get("call_index").and_then(Value::as_u64) {
        Some(index) if index >= 1 => {}
        _ => errors.push("session.call_index must be an integer >= 1".to_string()),
    }

    match session.get("epoch").and_then(Value::as_u64) {
        Some(_) => {}
        None => errors.push("session.epoch must be an integer >= 0".to_string()),
    }

    match session.get("state").and_then(Value::as_str) {
        Some(value) if State::parse(value).is_some() => {}
        _ => errors.push(format!(
            "session.state must be one of start/append/pause/resume/stop/compact, got {:?}",
            session.get("state")
        )),
    }
}

fn validate_context_management(context: &Value, errors: &mut Vec<String>) {
    let Some(object) = context.as_object() else {
        errors.push("context_management must be an object".to_string());
        return;
    };
    for key in object.keys() {
        if key != "manage_request" && key != "edits" {
            errors.push(format!("unexpected context_management key: {key:?}"));
        }
    }
    if object.get("manage_request").and_then(Value::as_bool) != Some(true) {
        errors.push("context_management.manage_request must be the boolean true".to_string());
    }
    let Some(edits) = object.get("edits").and_then(Value::as_array) else {
        errors.push("context_management.edits must be an array".to_string());
        return;
    };
    if edits.len() != 1 {
        errors.push("context_management.edits must contain exactly one edit".to_string());
        return;
    }
    let Some(edit) = edits[0].as_object() else {
        errors.push("context_management.edits[0] must be an object".to_string());
        return;
    };
    for key in edit.keys() {
        if key != "type" && key != "target" {
            errors.push(format!("unexpected edit key: {key:?}"));
        }
    }
    match edit.get("type").and_then(Value::as_str) {
        Some("offload" | "prefetch" | "evict") => {}
        _ => errors.push("edit.type must be one of offload/prefetch/evict".to_string()),
    }
    if edit.get("target").and_then(Value::as_str) != Some("session") {
        errors.push("edit.target must be 'session'".to_string());
    }
}

fn validate_priority_control(control: &Value, errors: &mut Vec<String>) {
    let Some(object) = control.as_object() else {
        errors.push("priority_control must be an object".to_string());
        return;
    };
    for key in object.keys() {
        if key != "priority" {
            errors.push(format!("unexpected priority_control key: {key:?}"));
        }
    }
    let Some(priority) = object.get("priority").and_then(Value::as_object) else {
        errors.push("priority_control.priority must be an object".to_string());
        return;
    };
    for key in priority.keys() {
        if key != "level" && key != "reason" {
            errors.push(format!("unexpected priority key: {key:?}"));
        }
    }
    let level = priority.get("level").and_then(Value::as_i64);
    let reason = priority.get("reason").and_then(Value::as_str);
    if !matches!(level, Some(1 | 3)) {
        errors.push("priority.level must be one of 1 or 3".to_string());
    }
    if !matches!(reason, Some("user_waiting" | "critical_path")) {
        errors.push("priority.reason must be one of user_waiting or critical_path".to_string());
    }
    if let (Some(level), Some(reason)) = (level, reason) {
        let expected = if level >= 3 {
            "user_waiting"
        } else {
            "critical_path"
        };
        if reason != expected {
            errors.push(format!(
                "priority.level {level} must pair with reason {expected:?}, got {reason:?}"
            ));
        }
    }
}

fn validate_cache_control(cache: &Value, errors: &mut Vec<String>) {
    let Some(object) = cache.as_object() else {
        errors.push("cache_control must be an object".to_string());
        return;
    };
    for key in object.keys() {
        if key != "type" && key != "ttl_s" && key != "msg_offset" {
            errors.push(format!("unexpected cache_control key: {key:?}"));
        }
    }
    if object.get("type").and_then(Value::as_str) != Some("ephemeral") {
        errors.push("cache_control.type must be 'ephemeral'".to_string());
    }
    match object.get("ttl_s").and_then(Value::as_i64) {
        Some(ttl) if (60..=3600).contains(&ttl) => {}
        _ => errors.push("cache_control.ttl_s must be an integer in [60, 3600]".to_string()),
    }
    match object.get("msg_offset").and_then(Value::as_u64) {
        Some(offset) if offset >= 1 => {}
        _ => errors.push("cache_control.msg_offset must be an integer >= 1".to_string()),
    }
}
