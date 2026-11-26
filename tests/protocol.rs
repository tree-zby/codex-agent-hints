use codex_agent_hints::protocol::{is_manage_hint, validate, Priority, State, HEADER_NAME};
use codex_agent_hints::{decode, encode};
use serde_json::json;

fn start_hint() -> serde_json::Value {
    json!({
        "session": {
            "session_id": "ah-test",
            "call_index": 1,
            "epoch": 0,
            "state": "start",
        },
        "priority_control": {"priority": {"level": 3, "reason": "user_waiting"}},
        "cache_control": {"type": "ephemeral", "ttl_s": 600, "msg_offset": 1},
    })
}

#[test]
fn encodes_without_padding_and_round_trips() {
    let hint = start_hint();
    let encoded = encode(&hint).expect("encode");
    assert!(!encoded.contains('='));
    assert!(encoded.is_ascii());
    assert_eq!(decode(&encoded).expect("decode"), hint);
    assert_eq!(decode(&format!("{encoded}==")).expect("padded"), hint);
}

#[test]
fn header_name_matches_the_contract() {
    assert_eq!(HEADER_NAME, "X-Agent-Hint");
}

#[test]
fn rejects_non_object_payloads() {
    // base64url of `[1,2,3]`, no padding.
    assert!(decode("WzEsMiwzXQ").is_err());
    assert!(decode("  ").is_err());
}

#[test]
fn accepts_a_minimal_start_hint() {
    assert!(validate(&start_hint()).is_empty());
}

#[test]
fn accepts_manage_compact_and_detects_it() {
    let hint = json!({
        "session": {"session_id": "ah-test", "call_index": 7, "epoch": 2, "state": "compact"},
        "context_management": {
            "manage_request": true,
            "edits": [{"type": "evict", "target": "session"}],
        },
    });
    assert!(validate(&hint).is_empty());
    assert!(is_manage_hint(&hint));
    assert!(State::Compact.is_manage());
    assert!(!State::Compact.is_ordinary());
}

#[test]
fn rejects_a_context_management_block_on_an_ordinary_request() {
    let mut hint = start_hint();
    hint["context_management"] =
        json!({"manage_request": true, "edits": [{"type": "evict", "target": "session"}]});
    let errors = validate(&hint);
    assert!(
        errors.iter().any(|error| error.contains("MUST NOT appear")),
        "{errors:?}"
    );
}

#[test]
fn rejects_priority_or_cache_on_a_manage_request() {
    let hint = json!({
        "session": {"session_id": "ah-test", "call_index": 7, "epoch": 2, "state": "pause"},
        "context_management": {
            "manage_request": true,
            "edits": [{"type": "offload", "target": "session"}],
        },
        "priority_control": {"priority": {"level": 1, "reason": "critical_path"}},
    });
    let errors = validate(&hint);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("priority_control")),
        "{errors:?}"
    );
}

#[test]
fn rejects_the_wrong_edit_for_the_state() {
    let hint = json!({
        "session": {"session_id": "ah-test", "call_index": 7, "epoch": 2, "state": "resume"},
        "context_management": {
            "manage_request": true,
            "edits": [{"type": "offload", "target": "session"}],
        },
    });
    let errors = validate(&hint);
    assert!(
        errors.iter().any(|error| error.contains("prefetch")),
        "{errors:?}"
    );
}

#[test]
fn rejects_mismatched_priority_pairs_and_bad_ttl() {
    let mut hint = start_hint();
    hint["priority_control"] = json!({"priority": {"level": 3, "reason": "critical_path"}});
    hint["cache_control"] = json!({"type": "ephemeral", "ttl_s": 30, "msg_offset": 0});
    let errors = validate(&hint);
    assert!(
        errors.iter().any(|error| error.contains("must pair")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("ttl_s")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("msg_offset")),
        "{errors:?}"
    );
}

#[test]
fn rejects_unknown_top_level_and_session_keys() {
    let mut hint = start_hint();
    hint["extra"] = json!(true);
    hint["session"]["note"] = json!("nope");
    let errors = validate(&hint);
    assert!(
        errors.iter().any(|error| error.contains("top-level")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("session key")),
        "{errors:?}"
    );
}

#[test]
fn priority_folds_to_the_two_allowed_levels() {
    assert_eq!(Priority::from_level(1).unwrap().reason(), "critical_path");
    assert_eq!(Priority::from_level(2).unwrap().reason(), "critical_path");
    assert_eq!(Priority::from_level(3).unwrap().reason(), "user_waiting");
    assert_eq!(Priority::from_level(9).unwrap().level(), 3);
    assert!(Priority::from_level(0).is_none());
    assert_eq!(State::parse("append"), Some(State::Append));
    assert_eq!(State::parse("nope"), None);
}
