//! Wire-level capture for the Codex CLI plugin.
//!
//! Codex has no per-request outbound-header hook, so the sender ships with a
//! small generic header patch (`patches/0001-core-hint-header-hook.patch`).
//! This example exercises the exact header bytes that patch emits: a real
//! `TcpListener` receives the request, and the captured header is decoded with
//! the same codec a gateway uses.
//!
//! Run with `cargo run --example wire_capture`; the evidence lands in
//! `e2e/evidence/codex.json`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use codex_agent_hints::adapter::{HintStore, RequestFacts};
use codex_agent_hints::{
    decode, validate, HintConfig, ManageClient, State, HEADER_NAME,
};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Captured> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..position]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = position + 4;
            while buffer.len() < body_start + length {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return Ok(parse_request(&buffer, body_start, length));
        }
    }
    Ok(parse_request(&buffer, buffer.len(), 0))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_request(buffer: &[u8], body_start: usize, length: usize) -> Captured {
    let head = String::from_utf8_lossy(&buffer[..body_start.saturating_sub(4)]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let end = (body_start + length).min(buffer.len());
    Captured {
        request_line,
        headers,
        body: String::from_utf8_lossy(&buffer[body_start.min(end)..end]).to_string(),
    }
}

fn serve(listener: TcpListener, expected: usize) -> Receiver<Captured> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..expected {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            if let Ok(request) = read_request(&mut stream) {
                let _ = sender.send(request);
            }
            let response =
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}";
            let _ = stream.write_all(response);
        }
    });
    receiver
}

fn post(addr: SocketAddr, headers: &[(String, String)], body: &str) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    let mut request = format!(
        "POST /v1/responses HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes())?;
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink);
    Ok(())
}

fn message(role: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{"type": "input_text", "text": text}],
    })
}

fn header_value(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(HEADER_NAME))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    // 5 ordinary turns/aux calls + compact + 3 lifecycle verbs + 1 post-rewrite turn.
    let total = 10;
    let captured = serve(listener, total);

    let store = HintStore::new(HintConfig {
        cache_min_prefix_tokens: 0,
        ..HintConfig::default()
    });
    let client = ManageClient::new(format!("http://{addr}/v1"), "gpt-5");

    // 1. First main-line call.
    let body_turn_1 = json!({
        "model": "gpt-5",
        "input": [message("user", "Summarise the repository layout.")],
        "tools": [{"type": "function", "name": "shell"}, {"type": "function", "name": "read_file"}],
        "stream": true,
    })
    .to_string();
    let facts_turn_1 = RequestFacts::from_body(
        "thread-capture",
        "gpt-5",
        "turn",
        &serde_json::from_str(&body_turn_1)?,
    );
    let mut sent: Vec<(&str, String, String)> = Vec::new();

    let header = store
        .headers(&facts_turn_1)
        .ok_or("expected a hint on the first turn")?;
    post(addr, &[(header.name.into(), header.value.clone())], &body_turn_1)?;
    sent.push(("turn:start", header.value, body_turn_1.clone()));
    store.note_accepted("thread-capture");
    store.note_response_end("thread-capture", Some(1.0));

    // 2. Tool continuation: three wire items, append + critical_path.
    let body_tool = json!({
        "model": "gpt-5",
        "input": [
            message("user", "Summarise the repository layout."),
            {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": "codex-rs/core/src/lib.rs"},
        ],
        "tools": [{"type": "function", "name": "shell"}, {"type": "function", "name": "read_file"}],
        "stream": true,
    })
    .to_string();
    let mut facts_tool = RequestFacts::from_body(
        "thread-capture",
        "gpt-5",
        "turn",
        &serde_json::from_str(&body_tool)?,
    );
    facts_tool.now = Some(3.0);
    let header = store
        .headers(&facts_tool)
        .ok_or("expected a hint on the tool continuation")?;
    post(addr, &[(header.name.into(), header.value.clone())], &body_tool)?;
    sent.push(("turn:tool-continuation", header.value, body_tool.clone()));
    store.note_accepted("thread-capture");

    // 3. Auxiliary calls: prewarm, remote compaction, memory. No hint at all.
    for (label, kind) in [
        ("auxiliary:prewarm", "prewarm"),
        ("auxiliary:compaction", "compaction"),
        ("auxiliary:memory", "memory"),
    ] {
        let body = json!({
            "model": "gpt-5",
            "input": [message("user", "Summarise this conversation for the next turn.")],
            "stream": false,
        })
        .to_string();
        let facts = RequestFacts::from_body(
            "thread-capture",
            "gpt-5",
            kind,
            &serde_json::from_str(&body)?,
        );
        assert!(store.headers(&facts).is_none(), "{label} must stay unannotated");
        post(addr, &[], &body)?;
        sent.push((label, String::new(), body));
    }

    // 4. compact manage request: placeholder body, superseded epoch.
    let compact = store
        .with_session("thread-capture", |session| client.compact(session))
        .transpose()?
        .ok_or("session must exist")?;
    post(addr, &compact.headers, &compact.body)?;
    sent.push((
        "manage:compact",
        header_value(&compact.headers),
        compact.body.clone(),
    ));

    // 5. First turn after the rewrite: epoch 1, no cache_control yet.
    let body_turn_2 = json!({
        "model": "gpt-5",
        "input": [
            message("user", "Summarise the repository layout."),
            message("assistant", "Here is the layout."),
            message("user", "Now list the crates."),
        ],
        "tools": [{"type": "function", "name": "shell"}, {"type": "function", "name": "read_file"}],
        "stream": true,
    })
    .to_string();
    let mut facts_turn_2 = RequestFacts::from_body(
        "thread-capture",
        "gpt-5",
        "turn",
        &serde_json::from_str(&body_turn_2)?,
    );
    facts_turn_2.now = Some(600.0);
    let header = store
        .headers(&facts_turn_2)
        .ok_or("expected a hint after the rewrite")?;
    post(addr, &[(header.name.into(), header.value.clone())], &body_turn_2)?;
    sent.push(("turn:after-compact", header.value, body_turn_2.clone()));
    store.note_accepted("thread-capture");

    // 6. Lifecycle manage requests.
    for verb in [State::Pause, State::Resume, State::Stop] {
        let request = store
            .with_session("thread-capture", |session| client.lifecycle(session, verb))
            .transpose()?
            .ok_or("session must exist")?;
        post(addr, &request.headers, &request.body)?;
        sent.push((
            verb.as_str(),
            header_value(&request.headers),
            request.body.clone(),
        ));
    }

    let mut requests = Vec::new();
    for (index, (label, value, body)) in sent.iter().enumerate() {
        let captured = captured.recv_timeout(std::time::Duration::from_secs(5))?;
        let observed = captured.header(HEADER_NAME).unwrap_or_default().to_string();
        let decoded = if observed.is_empty() {
            None
        } else {
            Some(decode(&observed)?)
        };
        requests.push(json!({
            "index": index + 1,
            "stage": label,
            "request_line": captured.request_line,
            "hint_header_present": !observed.is_empty(),
            "hint": decoded,
            "body_has_hint_fields": body.contains("X-Agent-Hint") || body.contains("\"session\""),
            "sent_header_matches_capture": observed == *value,
            "captured_request_body": captured.body,
        }));
    }

    let hints: Vec<&Value> = requests
        .iter()
        .filter_map(|request| {
            request
                .get("hint")
                .and_then(|hint| hint.as_object().map(|_| hint))
        })
        .collect();
    let every_hint_valid = hints.iter().all(|hint| validate(hint).is_empty());
    let body_has_no_hint_fields = requests
        .iter()
        .all(|request| request["body_has_hint_fields"] == Value::Bool(false));
    let auxiliary_request_has_no_hint = requests
        .iter()
        .filter(|request| {
            request["stage"]
                .as_str()
                .is_some_and(|stage| stage.starts_with("auxiliary:"))
        })
        .all(|request| request["hint_header_present"] == Value::Bool(false));
    let states: Vec<&str> = hints
        .iter()
        .map(|hint| hint["session"]["state"].as_str().unwrap_or(""))
        .collect();
    let epochs: Vec<i64> = hints
        .iter()
        .map(|hint| hint["session"]["epoch"].as_i64().unwrap_or(-1))
        .collect();
    let offsets: Vec<&Value> = hints
        .iter()
        .filter_map(|hint| {
            hint.get("cache_control")
                .and_then(|cache| cache.get("msg_offset"))
        })
        .collect();

    let evidence = json!({
        "harness": "codex-cli",
        "harness_commit": "9e51c74",
        "protocol": "agent-hint-protocol v1.0",
        "capture": "raw HTTP/1.1 over 127.0.0.1, server-side byte capture",
        "generated_at_unix": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "requests": requests,
        "checks": {
            "every_hint_valid": every_hint_valid,
            "body_has_no_hint_fields": body_has_no_hint_fields,
            "auxiliary_request_has_no_hint": auxiliary_request_has_no_hint,
            "states": states,
            "epochs": epochs,
            "cache_msg_offsets": offsets,
        }
    });

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("e2e/evidence/codex.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&evidence)?)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}
