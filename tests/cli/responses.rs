use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{ffi::OsStr, path::PathBuf, sync::OnceLock};

fn auth_directory() -> &'static tempfile::TempDir {
    static DIRECTORY: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIRECTORY.get_or_init(|| tempfile::tempdir().unwrap())
}

fn fixture_token() -> String {
    format!(
        "{}.{}.fixture-signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"exp":4102444800}"#)
    )
}

pub(super) fn fixture_command(binary: impl AsRef<OsStr>) -> Command {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let path = auth_directory()
        .path()
        .join(format!("auth-{}.json", ID.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(
        &path,
        json!({"auth_mode":"chatgpt","tokens":{"access_token":fixture_token(),"account_id":"fixture-account"}}).to_string(),
    )
    .unwrap();
    let mut command = Command::new(binary);
    command.arg("--codex-auth-file").arg(path);
    command
}

pub(super) fn remove_auth(command: &mut Command) {
    let args: Vec<_> = command.get_args().collect();
    let path = PathBuf::from(
        args.windows(2)
            .find(|pair| pair[0] == "--codex-auth-file")
            .unwrap()[1],
    );
    assert!(path.starts_with(auth_directory().path()));
    std::fs::remove_file(path).unwrap();
}

pub(super) fn assert_headers(headers: &str) {
    assert!(headers.starts_with("POST /responses "), "{headers}");
    let header = |name: &str| {
        headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
            .unwrap()
    };
    assert_eq!(header("chatgpt-account-id"), "fixture-account");
    assert_eq!(
        header("authorization"),
        format!("Bearer {}", fixture_token())
    );
}

pub(super) fn normalize(mut body: Value) -> Value {
    assert_eq!(body["model"], "gpt-5.6-terra");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("messages").is_none());
    let mut messages = Vec::new();
    if let Some(instructions) = body["instructions"].as_str() {
        messages.push(json!({"role":"system","content":instructions}));
    }
    for item in body["input"].as_array().unwrap() {
        match item["type"].as_str() {
            Some("function_call") => messages.push(json!({"role":"assistant","tool_calls":[{"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}}]})),
            Some("function_call_output") => messages.push(json!({"role":"tool","tool_call_id":item["call_id"],"content":item["output"]})),
            Some("reasoning") => {},
            _ => messages.push(json!({"role":item["role"],"content":item["content"]})),
        }
    }
    body["messages"] = messages.into();
    for tool in body["tools"].as_array_mut().unwrap() {
        assert_eq!(tool["type"], "function");
        assert!(tool.get("function").is_none());
        let native = tool.clone();
        tool["function"] = native;
    }
    body
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

pub(super) fn responses_sse(chunk: &Value) -> String {
    let choice = &chunk["choices"][0];
    let delta = &choice["delta"];
    let mut result = String::new();
    if let Some(content) = delta["content"].as_str() {
        result += &event(
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":content}),
        );
    }
    if let Some(content) = delta["reasoning_content"].as_str() {
        result += &event(
            json!({"type":"response.reasoning_text.delta","item_id":"fixture-reasoning","content_index":0,"delta":content}),
        );
        result += &event(
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"fixture-reasoning","encrypted_content":"fixture-encrypted-reasoning","summary":[]}}),
        );
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for (index, call) in calls.iter().enumerate() {
            let item = json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]});
            result += &event(
                json!({"type":"response.output_item.added","output_index":index,"item":item}),
            );
        }
        for (index, call) in calls.iter().enumerate().rev() {
            result += &event(
                json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":call["function"]["arguments"]}),
            );
        }
        for (index, call) in calls.iter().enumerate().rev() {
            let item = json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]});
            if choice["finish_reason"] != "length" {
                result += &event(
                    json!({"type":"response.output_item.done","output_index":index,"item":item}),
                );
            }
        }
    }
    match choice["finish_reason"].as_str() {
        None => {}
        Some("length" | "content_filter") => {
            result += &event(
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":if choice["finish_reason"] == "length" {"max_output_tokens"} else {"content_filter"}}}}),
            );
        }
        Some(_) => {
            result += &event(
                json!({"type":"response.completed","response":{"usage":{"input_tokens":chunk["usage"]["prompt_tokens"].as_u64().unwrap_or(0),"output_tokens":chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0)}}}),
            );
        }
    }
    result
}

#[test]
fn batched_tool_calls_replay_both_results_and_preserve_errors_in_call_order() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "BATCHED_TOOLS", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "fixed\n"
    );
    let history = fixture.history.lock().unwrap();
    let replay = &history[1];
    assert_eq!(replay["parallel_tool_calls"], true);
    let calls: Vec<_> = replay["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["call_id"], "batch-write");
    assert_eq!(calls[0]["name"], "file_write");
    assert_eq!(
        serde_json::from_str::<Value>(calls[0]["arguments"].as_str().unwrap()).unwrap(),
        json!({"path":"hello.txt","content":"initial\n"})
    );
    assert_eq!(calls[1]["call_id"], "batch-denied-read");
    assert_eq!(calls[1]["name"], "file_read");
    assert_eq!(
        serde_json::from_str::<Value>(calls[1]["arguments"].as_str().unwrap()).unwrap(),
        json!({"path":"../outside-batching"})
    );
    let results: Vec<_> = replay["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["call_id"], "batch-write");
    assert_eq!(results[1]["call_id"], "batch-denied-read");
    let written: Value = serde_json::from_str(results[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(written["outcome"], "succeeded");
    assert_eq!(written["bytes_written"], 8);
    let denied: Value = serde_json::from_str(results[1]["output"].as_str().unwrap()).unwrap();
    assert_eq!(denied["error"]["category"], "path_escape");
}

pub(super) fn assert_native_delegations(artifacts: &Path, expected_children: usize) {
    let sessions: Vec<Value> = std::fs::read_dir(artifacts)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".session.json"))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect();
    let children: Vec<_> = sessions
        .iter()
        .filter(|session| session.get("parent_delegation").is_some())
        .collect();
    assert_eq!(children.len(), expected_children);
    let mut call_ids = std::collections::BTreeSet::new();
    for child in children {
        let link = &child["parent_delegation"];
        let parent = sessions
            .iter()
            .find(|session| {
                session["provider"] == "local-review-dispatch"
                    && session["session_id"] == link["session_id"]
            })
            .expect("review child must link to the native dispatcher session");
        assert_eq!(parent["run_id"], link["run_id"]);
        assert_ne!(child["session_id"], parent["session_id"]);
        assert_eq!(child["review_round"], parent["review_round"]);
        let call_id = link["call_id"].as_str().unwrap();
        assert!(
            call_ids.insert(call_id.to_owned()),
            "review children reused a delegation call"
        );
        let tools = std::fs::read_to_string(
            artifacts.join(format!("{}.tools.txt", parent["name"].as_str().unwrap())),
        )
        .unwrap();
        assert!(
            tools.contains(call_id),
            "parent did not settle its child's delegate_task call"
        );
        assert!(tools.contains("delegate_task"));
    }
}
