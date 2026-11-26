use codex_agent_hints::protocol::State;
use codex_agent_hints::state::{
    derive_turn_opening, estimate_prefix_tokens, HintConfig, HintError, HintSession, MessageView,
    RequestView,
};
use serde_json::Value;

fn config() -> HintConfig {
    HintConfig {
        cache_min_prefix_tokens: 0,
        ..HintConfig::default()
    }
}

fn user(text: &str) -> MessageView {
    MessageView::new("user", text)
}

fn assistant(text: &str) -> MessageView {
    MessageView::new("assistant", text)
}

fn request(messages: Vec<MessageView>) -> RequestView {
    RequestView {
        messages,
        model: "claude-sonnet-4".to_string(),
        tools: vec!["Read".to_string(), "Bash".to_string()],
        ..RequestView::default()
    }
}

fn make_session() -> HintSession {
    HintSession::new(Some("ah-test".to_string()), config()).expect("session")
}

#[test]
fn first_call_is_start_then_append() {
    let mut session = make_session();
    let first = session.build(&request(vec![user("hi")])).unwrap().unwrap();
    assert_eq!(first["session"]["state"], Value::String("start".into()));
    assert_eq!(first["session"]["call_index"], Value::from(1));
    assert_eq!(first["session"]["epoch"], Value::from(0));

    session.note_accepted();
    let second = session
        .build(&request(vec![user("hi"), assistant("hello")]))
        .unwrap()
        .unwrap();
    assert_eq!(second["session"]["state"], Value::String("append".into()));
    assert_eq!(second["session"]["call_index"], Value::from(2));
}

#[test]
fn a_tool_continuation_is_append_and_critical_path() {
    let mut session = make_session();
    session.note_accepted();
    let mut tool_result = MessageView::new("tool", "file contents");
    tool_result.has_tool_result = true;
    let messages = vec![
        user("read the file"),
        MessageView {
            role: "assistant".into(),
            text: String::new(),
            has_tool_result: false,
            has_tool_call: true,
        },
        tool_result,
    ];
    let hint = session.build(&request(messages)).unwrap().unwrap();
    assert_eq!(hint["session"]["state"], Value::String("append".into()));
    assert_eq!(
        hint["priority_control"]["priority"]["level"],
        Value::from(1)
    );
    assert_eq!(
        hint["priority_control"]["priority"]["reason"],
        Value::String("critical_path".into())
    );
}

#[test]
fn a_retry_reuses_the_exact_hint_until_accepted() {
    let mut session = make_session();
    let req = request(vec![user("hi")]);
    let first = session.build(&req).unwrap().unwrap();
    let retry = session.build(&req).unwrap().unwrap();
    assert_eq!(first, retry);
    assert_eq!(retry["session"]["call_index"], Value::from(1));

    session.note_accepted();
    let next = session.build(&req).unwrap().unwrap();
    assert_eq!(next["session"]["call_index"], Value::from(2));
    assert_ne!(first["session"]["state"], next["session"]["state"]);
}

#[test]
fn auxiliary_requests_never_carry_a_hint() {
    let mut session = make_session();
    let mut req = request(vec![user("summarise the thread")]);
    req.auxiliary = true;
    assert!(session.build(&req).unwrap().is_none());
    // The omission must not consume a call index.
    assert_eq!(session.snapshot().call_index, 0);
}

#[test]
fn cache_control_reports_the_wire_message_count() {
    let mut session = make_session();
    let hint = session
        .build(&request(vec![user("a"), assistant("b"), user("c")]))
        .unwrap()
        .unwrap();
    assert_eq!(hint["cache_control"]["msg_offset"], Value::from(3));
    assert_eq!(
        hint["cache_control"]["type"],
        Value::String("ephemeral".into())
    );
    assert_eq!(hint["cache_control"]["ttl_s"], Value::from(600));
}

#[test]
fn a_short_prefix_suppresses_cache_control() {
    let mut targeted = HintSession::new(Some("ah-test".into()), HintConfig::default()).unwrap();
    let hint = targeted.build(&request(vec![user("hi")])).unwrap().unwrap();
    assert!(hint.get("cache_control").is_none());
}

#[test]
fn a_rewrite_bumps_the_epoch_and_suppresses_the_first_cache_block() {
    let mut session = make_session();
    let _ = session.build(&request(vec![user("hi")])).unwrap();
    session.note_accepted();

    let superseded = session.note_rewrite();
    assert_eq!(superseded, 0);
    let hint = session
        .build(&request(vec![user("hi"), assistant("x")]))
        .unwrap()
        .unwrap();
    assert_eq!(hint["session"]["epoch"], Value::from(1));
    assert!(hint.get("cache_control").is_none());

    session.note_accepted();
    let next = session
        .build(&request(vec![user("hi"), assistant("x"), user("y")]))
        .unwrap()
        .unwrap();
    assert!(next.get("cache_control").is_some());
}

#[test]
fn manage_requests_follow_the_call_index_convention() {
    let mut session = make_session();
    let _ = session.build(&request(vec![user("hi")])).unwrap();
    session.note_accepted();
    let _ = session
        .build(&request(vec![user("hi"), assistant("x")]))
        .unwrap();

    let pause = session.build_manage(State::Pause, None, None).unwrap();
    assert_eq!(pause["session"]["call_index"], Value::from(2));
    assert_eq!(
        pause["context_management"]["edits"][0]["type"],
        Value::String("offload".into())
    );

    let resume = session.build_manage(State::Resume, None, None).unwrap();
    assert_eq!(resume["session"]["call_index"], Value::from(3));

    let compact = session.build_manage(State::Compact, None, None).unwrap();
    assert_eq!(compact["session"]["call_index"], Value::from(2));
    assert!(pause.get("priority_control").is_none());
    assert!(pause.get("cache_control").is_none());
}

#[test]
fn an_unresolved_manage_request_isolates_the_session() {
    let mut session = make_session();
    session.isolate();
    let error = session.build(&request(vec![user("hi")])).unwrap_err();
    assert!(matches!(error, HintError::Isolated(_)));
    session.clear_isolation();
    assert!(session.build(&request(vec![user("hi")])).unwrap().is_some());
}

#[test]
fn snapshots_round_trip() {
    let mut session = make_session();
    let _ = session.build(&request(vec![user("hi")])).unwrap();
    session.note_accepted();
    session.note_rewrite();
    session.note_response_end(Some(12.5));
    let snapshot = session.snapshot();

    let mut restored = make_session();
    restored.restore(&snapshot).unwrap();
    assert_eq!(restored.snapshot(), snapshot);
}

#[test]
fn turn_opening_walks_back_over_empty_assistant_placeholders() {
    let opening = vec![user("hi"), assistant("")];
    assert!(derive_turn_opening(&opening));

    let in_progress = vec![user("hi"), assistant("partial")];
    assert!(!derive_turn_opening(&in_progress));

    let after_tool = vec![
        user("hi"),
        MessageView {
            role: "assistant".into(),
            text: String::new(),
            has_tool_result: false,
            has_tool_call: true,
        },
    ];
    assert!(!derive_turn_opening(&after_tool));
    assert!(!derive_turn_opening(&[]));
}

#[test]
fn prefix_tokens_are_none_without_text() {
    assert_eq!(estimate_prefix_tokens(&[]), None);
    assert_eq!(
        estimate_prefix_tokens(&[MessageView::new("user", "")]),
        None
    );
    // Four ASCII characters per token, one token per CJK character.
    assert_eq!(
        estimate_prefix_tokens(&[MessageView::new("user", "abcd")]),
        Some(1)
    );
    assert_eq!(
        estimate_prefix_tokens(&[MessageView::new("user", "中文")]),
        Some(2)
    );
}

#[test]
fn invalid_config_is_rejected_early() {
    let bad_ttl = HintConfig {
        ttl_min_s: 10,
        ..HintConfig::default()
    };
    assert!(HintSession::new(Some("ah-test".into()), bad_ttl).is_err());

    let bad_pair = HintConfig {
        priority_level: Some(3),
        priority_reason: Some("critical_path".into()),
        ..HintConfig::default()
    };
    assert!(HintSession::new(Some("ah-test".into()), bad_pair).is_err());
}
