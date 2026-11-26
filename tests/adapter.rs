//! Adapter tests: Responses request facts in, `X-Agent-Hint` out.

use codex_agent_hints::adapter::{HintStore, RequestFacts, RequestKind};
use codex_agent_hints::manage::{parse_verb, ManageClient};
use codex_agent_hints::protocol::{is_manage_hint, validate, State};
use codex_agent_hints::state::{HintConfig, HintSession};
use codex_agent_hints::{decode, HEADER_NAME};
use serde_json::{json, Value};

fn long_text() -> String {
    "the quick brown fox jumps over the lazy dog. ".repeat(60)
}

fn user_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

fn assistant_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    })
}

fn turn_body(items: Vec<Value>) -> Value {
    json!({
        "model": "gpt-5",
        "input": items,
        "tools": [
            {"type": "function", "name": "shell", "parameters": {}},
            {"type": "function", "name": "read_file", "parameters": {}},
        ],
        "stream": true,
    })
}

fn store() -> HintStore {
    HintStore::new(HintConfig::default())
}

#[test]
fn only_turn_requests_carry_a_hint() {
    let store = store();
    let body = turn_body(vec![user_item("hello")]);

    for label in ["prewarm", "compaction", "memory", "something-new"] {
        let facts = RequestFacts::from_body("thread-1", "gpt-5", label, &body);
        assert!(
            store.headers(&facts).is_none(),
            "{label} must stay unannotated"
        );
    }

    let facts = RequestFacts::from_body("thread-1", "gpt-5", "turn", &body);
    assert_eq!(facts.kind, RequestKind::Turn);
    assert!(store.headers(&facts).is_some());
}

#[test]
fn start_then_append_and_retry_reuses_the_same_hint() {
    let store = store();

    let first = turn_body(vec![user_item(&long_text())]);
    let facts = RequestFacts::from_body("thread-1", "gpt-5", "turn", &first);
    let header = store.headers(&facts).expect("turn must be annotated");
    assert_eq!(header.name, HEADER_NAME);

    let hint = decode(&header.value).expect("header is valid base64url JSON");
    assert!(validate(&hint).is_empty(), "hint must satisfy v1.0");
    assert_eq!(hint["session"]["state"], "start");
    assert_eq!(hint["session"]["call_index"], 1);
    assert_eq!(hint["priority_control"]["priority"]["level"], 3);
    assert_eq!(
        hint["priority_control"]["priority"]["reason"],
        "user_waiting"
    );
    // One logical input item on the wire, so msg_offset is 1.
    assert_eq!(hint["cache_control"]["msg_offset"], 1);

    // A retry of the same logical request must reuse the exact header.
    let retry = store.headers(&facts).expect("retry keeps its hint");
    assert_eq!(retry.value, header.value);

    // The next turn appends history.
    let second = turn_body(vec![
        user_item(&long_text()),
        assistant_item("I will look."),
        user_item("thanks"),
    ]);
    let facts = RequestFacts::from_body("thread-1", "gpt-5", "turn", &second);
    let header = store.headers(&facts).expect("second turn is annotated");
    let hint = decode(&header.value).expect("valid hint");
    assert!(validate(&hint).is_empty());
    assert_eq!(hint["session"]["state"], "append");
    assert_eq!(hint["session"]["call_index"], 2);
}

#[test]
fn session_id_is_stable_per_thread_and_distinct_across_threads() {
    let store = store();
    let body = turn_body(vec![user_item("hello")]);

    let a1 = store
        .headers(&RequestFacts::from_body("thread-a", "gpt-5", "turn", &body))
        .unwrap();
    let a2 = store
        .headers(&RequestFacts::from_body("thread-a", "gpt-5", "turn", &body))
        .unwrap();
    let b1 = store
        .headers(&RequestFacts::from_body("thread-b", "gpt-5", "turn", &body))
        .unwrap();

    let id = |value: &str| decode(value).unwrap()["session"]["session_id"].clone();
    assert_eq!(id(&a1.value), id(&a2.value));
    assert_ne!(id(&a1.value), id(&b1.value));
    assert!(id(&a1.value).as_str().unwrap().starts_with("ah-"));
}

#[test]
fn resumed_conversation_starts_as_append() {
    let store = store();
    // History is already on the wire: assistant output counts as history even
    // though this is the first request this process sees.
    let body = turn_body(vec![user_item("hello"), assistant_item("hi there")]);
    let facts = RequestFacts::from_body("thread-resumed", "gpt-5", "turn", &body);
    let header = store.headers(&facts).unwrap();
    let hint = decode(&header.value).unwrap();
    assert_eq!(hint["session"]["state"], "append");
}

#[test]
fn input_views_cover_every_wire_slot() {
    let body = json!({
        "input": [
            user_item("hi"),
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]},
            {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        ],
    });
    let facts = RequestFacts::from_body("t", "gpt-5", "turn", &body);
    assert!(facts.has_history());
    let views = facts.input;
    assert_eq!(views.len(), 4);
    assert_eq!(views[0].role, "user");
    assert!(views[1].text.contains("thinking"));
    assert!(views[2].has_tool_call);
    assert!(views[3].has_tool_result);
}

#[test]
fn unknown_item_kinds_still_occupy_a_slot() {
    let body = json!({"input": [
        user_item("hi"),
        {"type": "web_search_call", "id": "ws1"},
        {"type": "brand_new_item", "id": "x1"},
    ]});
    let facts = RequestFacts::from_body("t", "gpt-5", "turn", &body);
    assert_eq!(facts.view().messages.len(), 3);
    assert!(facts.view().messages[2].has_tool_call);
}

#[test]
fn manage_requests_use_responses_placeholder_body() {
    let mut session =
        HintSession::new(Some("ah-manage".to_string()), HintConfig::default()).unwrap();
    let client =
        ManageClient::new("https://gateway.example.com/v1/", "gpt-5").with_bearer_token("sk-test");

    let request = client.compact(&mut session).expect("compact builds");
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://gateway.example.com/v1/responses");
    assert_eq!(session.epoch, 1, "compact supersedes the old epoch");

    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(
        body,
        json!({"model": "gpt-5", "input": [], "stream": false})
    );

    let header = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(HEADER_NAME))
        .expect("manage request carries the header")
        .1
        .clone();
    let hint = decode(&header).unwrap();
    assert!(is_manage_hint(&hint));
    assert!(validate(&hint).is_empty());
    assert_eq!(hint["session"]["state"], "compact");
    assert_eq!(hint["context_management"]["edits"][0]["type"], "evict");
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "authorization" && value == "Bearer sk-test"));
}

#[test]
fn lifecycle_verbs_do_not_touch_the_epoch() {
    let mut session = HintSession::new(Some("ah-life".to_string()), HintConfig::default()).unwrap();
    let client = ManageClient::new("http://127.0.0.1:1/v1", "gpt-5");

    for (verb, edit) in [
        ("pause", "offload"),
        ("resume", "prefetch"),
        ("stop", "evict"),
    ] {
        let state = parse_verb(verb).expect("known verb");
        let request = client.lifecycle(&mut session, state).unwrap();
        assert_eq!(session.epoch, 0);
        let header = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(HEADER_NAME))
            .unwrap()
            .1
            .clone();
        let hint = decode(&header).unwrap();
        assert!(validate(&hint).is_empty());
        assert_eq!(hint["context_management"]["edits"][0]["type"], edit);
    }

    assert_eq!(parse_verb("start"), None);
    assert!(matches!(
        client.lifecycle(&mut session, State::Start),
        Err(codex_agent_hints::state::HintError::UnknownVerb(_))
    ));
}

#[test]
fn isolated_sessions_refuse_new_main_line_requests() {
    let store = store();
    let body = turn_body(vec![user_item("hello")]);
    let facts = RequestFacts::from_body("thread-iso", "gpt-5", "turn", &body);
    assert!(store.headers(&facts).is_some());

    store.isolate("thread-iso");
    assert!(
        store.headers(&facts).is_none(),
        "fail-open: isolating must drop the hint, not the request"
    );

    store.clear_isolation("thread-iso");
    assert!(store.headers(&facts).is_some());
}

#[test]
fn disabled_config_emits_nothing() {
    let config = HintConfig {
        enabled: false,
        ..HintConfig::default()
    };
    let store = HintStore::new(config);
    let body = turn_body(vec![user_item("hello")]);
    let facts = RequestFacts::from_body("t", "gpt-5", "turn", &body);
    assert!(store.headers(&facts).is_none());
}

#[test]
fn missing_thread_identity_emits_nothing() {
    let store = store();
    let body = turn_body(vec![user_item("hello")]);
    for thread_id in ["", "   "] {
        let facts = RequestFacts::from_body(thread_id, "gpt-5", "turn", &body);
        assert!(store.headers(&facts).is_none());
    }
}
