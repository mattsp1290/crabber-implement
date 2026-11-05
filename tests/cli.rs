#[path = "cli/bounded.rs"]
mod bounded;

#[path = "cli/admission.rs"]
mod admission;
#[path = "cli/input_bounds.rs"]
mod input_bounds;

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.name", "Fixture"]);
    git(repo.path(), &["config", "user.email", "fixture@localhost"]);
    std::fs::write(repo.path().join("README.md"), "Fixture project.\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "initial"]);
    for (name, instructions) in [
        (
            "review",
            "Review independently. Identify Critical, Important, and Suggestion findings.",
        ),
        (
            "fix-review",
            "Apply the selected findings automatically. Preserve needs-manual handling and emit the auto-summary block.",
        ),
    ] {
        let folder = repo.path().join(".git/fixture-skills").join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), instructions).unwrap();
    }
    repo
}

struct Fixture {
    address: String,
    stop: Arc<AtomicBool>,
    implementation_requests: Arc<AtomicUsize>,
    stream_release: Arc<AtomicBool>,
    fail_review: Arc<AtomicBool>,
    review_release: Arc<AtomicBool>,
    history: Arc<Mutex<Vec<Value>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(repo: &Path, invalid_review: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let implementation_requests = Arc::new(AtomicUsize::new(0));
        let requests = implementation_requests.clone();
        let stream_release = Arc::new(AtomicBool::new(false));
        let release = stream_release.clone();
        let fail_review = Arc::new(AtomicBool::new(true));
        let fail_review_worker = fail_review.clone();
        let review_release = Arc::new(AtomicBool::new(true));
        let released_review = review_release.clone();
        let history = Arc::new(Mutex::new(Vec::new()));
        let histories = history.clone();
        let repo = repo.to_path_buf();
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let repo = repo.clone();
                        let requests = requests.clone();
                        let release = release.clone();
                        let fail_review = fail_review_worker.clone();
                        let released_review = released_review.clone();
                        let stopped = stopped.clone();
                        let histories = histories.clone();
                        connections.push(thread::spawn(move || {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let body = request(&mut stream);
                        histories.lock().unwrap().push(body.clone());
                        let serialized = body.to_string();
                        let has_result = body["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|message| message["role"] == "tool");
                        if serialized.contains("TRANSPORT_OUTAGE") || (serialized.contains("TRANSPORT_RECOVER") && has_result
                            && !serialized.contains("Perform your assigned independent review")
                            && !serialized.contains("Execute the following fix-review skill")) {
                            let attempt = requests.fetch_add(1, Ordering::Relaxed);
                            if serialized.contains("TRANSPORT_OUTAGE") || attempt < 4 {
                                if serialized.contains("TRANSPORT_RECOVER") {
                                    assert_eq!(std::fs::read_to_string(repo.join("hello.txt")).unwrap(), "initial\n");
                                }
                                return;
                            }
                        }
                        if serialized.contains("COMPLETION_BUDGET_REJECT") && body["max_tokens"].as_u64().unwrap() > 16384 {
                            let payload = r#"{"error":{"message":"fixture rejects requested output cap","type":"invalid_request_error"}}"#;
                            write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                            return;
                        }
                        let response = if serialized.contains("Summarize this context for continuation") {
                            if serialized.contains("COMPLETION_SUMMARY_LENGTH") {
                                output_length()
                            } else if serialized.contains("COMPLETION_STREAM_VISIBLE") {
                                text("COMPLETION_STREAM_VISIBLE The assigned task wrote hello.txt with initial greeting. Complete verification and report the result.")
                            } else {
                                text("COMPLETION_STREAM_HIDDEN Complete the assigned task by writing hello.txt with initial greeting, then report the result.")
                            }
                        } else if serialized.contains("COMPLETION_BUDGET_STAGES") && body["max_tokens"].as_u64().unwrap() < 32768 {
                            output_length()
                        } else if serialized
                            .contains("Perform your assigned independent review")
                        {
                            assert!(!serialized.contains("external-secret-sentinel"), "host followed a committed symlink outside the review snapshot");
                            let tools = body["tools"].as_array().unwrap();
                            assert!(tools.iter().all(|tool| !matches!(tool["function"]["name"].as_str(), Some("shell" | "file_write" | "file_edit" | "apply_patch"))));

                            if serialized.contains("REVIEW_RECOVERY") && serialized.contains("slug maintainability,") && fail_review.load(Ordering::Relaxed) {
                                return;
                            }
                            if serialized.contains("REVIEW_BROWSER") && serialized.contains("slug maintainability,") && !fail_review.load(Ordering::Relaxed) {
                                while !released_review.load(Ordering::Relaxed) && !stopped.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(10)); }
                            }
                            if invalid_review || (serialized.contains("REVIEW_LATER_ROUND") && serialized.contains("Review round 2.") && serialized.contains("slug maintainability,") && fail_review.load(Ordering::Relaxed)) || (serialized.contains("REVIEW_BAD_REPORT") && serialized.contains("slug maintainability,") && fail_review.load(Ordering::Relaxed)) {
                                text("not a review")
                            } else {
                                let target = if serialized.contains("AUTONOMOUS_CONFIG") { "config.toml" } else { "hello.txt" };
                                let approved = serialized.contains("EXTERNAL_SYMLINK") || std::fs::read_to_string(repo.join(target))
                                    .unwrap_or_default()
                                    == if serialized.contains("AUTONOMOUS_CONFIG") { "mode = \"fixed\"\n" } else if serialized.contains("REVIEW_LATER_ROUND") { "final\n" } else { "fixed\n" };
                                let mut review = report(approved);
                                if !approved && serialized.contains("AUTONOMOUS_CONFIG") {
                                    review["findings"][0]["path"] = "config.toml".into();
                                    review["findings"][0]["description"] = "Application config has the initial value instead of the required fixed value.".into();
                                }
                                text(&review.to_string())
                            }
                        } else if serialized.contains("Execute the following fix-review skill") {
                            if serialized.contains("AUTONOMOUS_BLOCKER") || (serialized.contains("AUTONOMOUS_CONFIG") && !serialized.contains("name: fix-review-autonomous")) {
                                text("The requested fix needs a product decision. <!-- auto-summary fixed:0 skipped:0 needs-manual:1 -->")
                            } else if has_result {
                                text("<!-- auto-summary fixed:1 skipped:0 needs-manual:0 -->")
                            } else {
                                let second_fix = serialized.contains("REVIEW_LATER_ROUND") && std::fs::read_to_string(repo.join("hello.txt")).unwrap() == "fixed\n";
                                tool(
                                    "file_edit",
                                    json!({"path": if serialized.contains("AUTONOMOUS_CONFIG") { "config.toml" } else { "hello.txt" },"anchor":if second_fix { "fixed" } else { "initial" },"replacement":if second_fix { "final" } else { "fixed" }}),
                                )
                            }
                        } else if serialized.contains("AUTONOMOUS_CONFIG") {
                            if has_result {
                                text("Implemented application config; verification complete.")
                            } else {
                                tool("file_write", json!({"path":"config.toml","content":"mode = \"initial\"\n"}))
                            }
                        } else if serialized.contains("COMPLETION_") {
                            let attempt = requests.fetch_add(1, Ordering::Relaxed);
                            let continued = body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "user" && !message["content"].to_string().contains("COMPLETION_"));
                            if serialized.contains("COMPLETION_DUPLICATE_START") {
                                json!({"choices":[{"index":0,"delta":{"tool_calls":[
                                    {"index":0,"id":"duplicate-call","function":{"name":"file_read","arguments":"{}"}},
                                    {"index":1,"id":"duplicate-call","function":{"name":"file_read","arguments":"{}"}}
                                ]},"finish_reason":"tool_calls"}]})
                            } else if serialized.contains("COMPLETION_BUDGET_PRESERVE") && !has_result {
                                tool("file_write", json!({"path":"hello.txt","content":"partial work\n"}))
                            } else if (serialized.contains("COMPLETION_BUDGET_MIXED") && !serialized.contains("COMPLETION_BUDGET_MIXED_EXHAUST") && attempt == 0)
                                || (serialized.contains("COMPLETION_BUDGET_MIXED_EXHAUST") && attempt == 1) {
                                json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private empty budget sentinel"},"finish_reason":"stop"}]})
                            } else if serialized.contains("COMPLETION_BUDGET_ALWAYS")
                                || serialized.contains("COMPLETION_BUDGET_PRESERVE")
                                || serialized.contains("COMPLETION_BUDGET_MIXED_EXHAUST")
                                || serialized.contains("COMPLETION_BUDGET_REJECT")
                                || serialized.contains("COMPLETION_BUDGET_DELAY")
                                || serialized.contains("COMPLETION_BUDGET_CANCEL")
                                || (serialized.contains("COMPLETION_BUDGET_MIXED") && body["max_tokens"].as_u64().unwrap() < 32768)
                                || (serialized.contains("COMPLETION_BUDGET_THIRD") && body["max_tokens"].as_u64().unwrap() < 65536) {
                                output_length()
                            } else if serialized.contains("COMPLETION_CAP_RECOVER") && body["max_tokens"].as_u64().unwrap() < 32768 {
                                json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private output budget sentinel"},"finish_reason":"length"}]})
                            } else if serialized.contains("COMPLETION_STREAM_VISIBLE") && attempt == 0 {
                                tool("file_write", json!({"path":"hello.txt","content":"initial\n"}))
                            } else if serialized.contains("COMPLETION_STREAM_HIDDEN") && attempt == 1 {
                                json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private retry reasoning."},"finish_reason":"stop"}]})
                            } else if serialized.contains("COMPLETION_STREAM_HIDDEN") && attempt == 2 {
                                tool("file_write", json!({"path":"hello.txt","content":"initial\n"}))
                            } else if serialized.contains("COMPLETION_BLOCKED") {
                                text("BLOCKED: The required fixture dependency is unavailable.")
                            } else if serialized.contains("COMPLETION_TRUNCATED_TOOL") {
                                let mut response = tool("file_write", json!({"path":"unsafe.txt","content":"must not run"}));
                                response["choices"][0]["finish_reason"] = "length".into();
                                response
                            } else if serialized.contains("COMPLETION_FILTERED") {
                                json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})
                            } else if serialized.contains("COMPLETION_EMPTY") || serialized.contains("COMPLETION_DELAY") || serialized.contains("COMPLETION_CANCEL") || (serialized.contains("COMPLETION_RECOVER") && attempt == 1) {
                                json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private fixture reasoning."},"finish_reason":"stop"}]})
                            } else if serialized.contains("COMPLETION_LENGTH") && !continued {
                                json!({"choices":[{"index":0,"delta":{"content":"Partial narration"},"finish_reason":"length"}]})
                            } else if serialized.contains("COMPLETION_RECOVER") && attempt == 0 {
                                {
                                    let mut response = tool("file_read", json!({"path":"README.md"}));
                                    response["choices"][0]["delta"]["content"] = "Researching the fixture repository. ".into();
                                    response
                                }
                            } else if !serialized.contains("hello.txt") {
                                tool("file_write", json!({"path":"hello.txt","content":"initial\n"}))
                            } else {
                                text("Implemented the plan. The BLOCKED marker is documented in README.md.")
                            }
                        } else if serialized.contains("LONG_TURNS") {
                            requests.fetch_add(1, Ordering::Relaxed);
                            let tool_results = body["messages"].as_array().unwrap().iter()
                                .filter(|message| message["role"] == "tool").count();
                            if tool_results == 0 {
                                tool("file_write", json!({"path":"hello.txt","content":"initial\n"}))
                            } else if tool_results < 65 {
                                tool("file_read", json!({"path":"hello.txt"}))
                            } else {
                                text("Implemented the plan after 65 tool responses.")
                            }
                        } else if !has_result && serialized.contains("DRIFT_BRANCH") {
                            tool("shell", json!({"cmd":"git switch -c unexpected"}))
                        } else if !has_result && serialized.contains("EXTERNAL_SYMLINK") {
                            tool("shell", json!({"cmd":"ln -s .git/external-secret leak.txt"}))
                        } else if has_result {
                            text("Implemented the plan and verified hello.txt.")
                        } else {
                            tool(
                                "file_write",
                                json!({"path":"hello.txt","content":"initial\n"}),
                            )
                        };
                        if (serialized.contains("COMPLETION_STREAM_HIDDEN") && requests.load(Ordering::Relaxed) == 1 && !serialized.contains("Summarize this context for continuation"))
                            || ((serialized.contains("COMPLETION_STREAM_VISIBLE") || serialized.contains("COMPLETION_SUMMARY_LENGTH")) && requests.load(Ordering::Relaxed) == 2 && !serialized.contains("Summarize this context for continuation")) {
                            let partial = json!({"choices":[{"index":0,"delta":{"content":"BLOCKED: stale failed response. "},"finish_reason":null}]});
                            let error = json!({"error":{"code":400,"message":"fixture overflow","metadata":{"error_type":"context_length_exceeded"}}});
                            let payload = format!("data: {partial}\n\ndata: {error}\n\n");
                            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                            return;
                        }
                        if has_result && serialized.contains("LIVE_ARTIFACTS")
                            && !serialized.contains("Perform your assigned independent review")
                            && !serialized.contains("Execute the following fix-review skill") {
                            let first = format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":{"content":"Implemented live "},"finish_reason":null}]}));
                            let last = format!("data: {}\n\ndata: [DONE]\n\n", text("artifacts and verified hello.txt."));
                            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", first.len() + last.len(), first).unwrap();
                            stream.flush().unwrap();
                            while !release.load(Ordering::Relaxed) && !stopped.load(Ordering::Relaxed) {
                                thread::sleep(Duration::from_millis(10));
                            }
                            if !stopped.load(Ordering::Relaxed) {
                                stream.write_all(last.as_bytes()).unwrap();
                            }
                            return;
                        }
                        if serialized.contains("COMPLETION_DELAY") || serialized.contains("COMPLETION_BUDGET_DELAY") {
                            thread::sleep(Duration::from_millis(if requests.load(Ordering::Relaxed) == 1 { 900 } else { 2500 }));
                        }
                        if (serialized.contains("COMPLETION_CANCEL") || serialized.contains("COMPLETION_BUDGET_CANCEL")) && requests.load(Ordering::Relaxed) > 1 {
                            while !stopped.load(Ordering::Relaxed) {
                                thread::sleep(Duration::from_millis(10));
                            }
                            return;
                        }
                        let payload = format!("data: {}\n\ndata: [DONE]\n\n", response);
                        if let Err(error) = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload) {
                            assert!((serialized.contains("COMPLETION_DELAY") || serialized.contains("COMPLETION_BUDGET_DELAY") || serialized.contains("REVIEW_BROWSER"))
                                && matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset), "fixture response: {error}");
                        }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture listener: {error}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            address,
            stop,
            implementation_requests,
            stream_release,
            fail_review,
            review_release,
            history,
            worker: Some(worker),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            assert!(headers.starts_with("POST /chat/completions "));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn text(content: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":3}})
}

fn tool(name: &str, arguments: Value) -> Value {
    static CALL_ID: AtomicUsize = AtomicUsize::new(0);
    let id = format!("fixture-call-{}", CALL_ID.fetch_add(1, Ordering::Relaxed));
    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
}

fn report(approved: bool) -> Value {
    json!({"verdict":if approved {"APPROVE"} else {"REQUEST_CHANGES"},"summary":"Reviewed the greeting implementation.","positive_notes":[],"findings":if approved {json!([])} else {json!([{"severity":"Important","path":"hello.txt","line":1,"description":"Greeting is initial.","suggested_fix":"Replace initial with fixed."}])}})
}

#[test]
fn cli_implements_fixes_and_requires_fresh_review() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan_dir = tempfile::tempdir().unwrap();
    let plan = plan_dir.path().join("plan.md");
    std::fs::write(&plan, "Create hello.txt with the requested greeting.\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(&plan)
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("20")
        .env("HOME", repo.path())
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "fixed\n"
    );
    assert!(
        git(repo.path(), &["status", "--porcelain"])
            .trim()
            .is_empty()
    );
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "2"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("fix-review 1"));
    assert!(stdout.contains("review 2"));
    assert!(stdout.contains("done "));
    assert_eq!(
        std::fs::read_dir(repo.path().join("reviews"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn malformed_review_cannot_complete_workflow() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), true);
    let plan_dir = tempfile::tempdir().unwrap();
    let plan = plan_dir.path().join("plan.md");
    std::fs::write(&plan, "Create hello.txt.\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(&plan)
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("20")
        .env("HOME", repo.path())
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("structured JSON report"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("done "));
}

#[test]
fn missing_plan_and_dirty_repository_fail_before_network() {
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg("/does/not/exist.md")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("plan path does not exist"));
    let repo = repository();
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "Implement the task.").unwrap();
    std::fs::write(repo.path().join("uncommitted.txt"), "keep me").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("uncommitted changes"));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("uncommitted.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn exhausted_review_budget_never_runs_fixes_or_claims_done() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "Create hello.txt.").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--max-reviews")
        .arg("1")
        .arg("--session-timeout")
        .arg("20")
        .env("HOME", repo.path())
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("review budget exhausted"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fix-review 1"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("done "));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "initial\n"
    );
}

#[test]
fn agent_branch_drift_stops_before_host_commit() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "DRIFT_BRANCH").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("20")
        .env("HOME", repo.path())
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("changed branch or HEAD"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "0"
    );
}

#[test]
fn review_reads_committed_symlink_text_without_target_contents() {
    let repo = repository();
    std::fs::write(
        repo.path().join(".git/external-secret"),
        "external-secret-sentinel",
    )
    .unwrap();
    let fixture = Fixture::start(repo.path(), false);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "EXTERNAL_SYMLINK").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("20")
        .env("HOME", repo.path())
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_link(repo.path().join("leak.txt")).unwrap(),
        Path::new(".git/external-secret")
    );
}

fn long_turn_run(
    repo: &Path,
    fixture: &Fixture,
    extra_args: &[&str],
    max_turns_env: Option<&str>,
) -> std::process::Output {
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(
        plan.path(),
        "LONG_TURNS Create hello.txt after a long tool journey.",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabber-implement"));
    command
        .arg(plan.path())
        .arg("--repo")
        .arg(repo)
        .arg("--skills-dir")
        .arg(repo.join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("30")
        .args(extra_args)
        .env_remove("CRABBER_IMPLEMENT_MAX_TURNS")
        .env("HOME", repo)
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true");
    if let Some(value) = max_turns_env {
        command.env("CRABBER_IMPLEMENT_MAX_TURNS", value);
    }
    command.output().unwrap()
}

fn run_artifacts(repo: &Path) -> std::path::PathBuf {
    std::fs::read_dir(repo.join(".git/crabber-implement"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

fn session_evidence(repo: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(run_artifacts(repo).join("implement.session.json")).unwrap(),
    )
    .unwrap()
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for live artifacts"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn export_artifacts(artifacts: &Path, phase: &str) {
    let Some(destination) = std::env::var_os("CRABBER_VERIFY_ARTIFACTS") else {
        return;
    };
    let root = Path::new(&destination);
    let run_id = artifacts.file_name().unwrap();
    let snapshot = root.join(phase).join(run_id);
    std::fs::create_dir_all(&snapshot).unwrap();
    fn copy_evidence(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                copy_evidence(&entry.path(), &destination.join(entry.file_name()));
            } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(".tmp") {
                std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
            }
        }
    }
    copy_evidence(artifacts, &snapshot);
    let temporary = root.join("snapshot.json.tmp");
    std::fs::write(
        &temporary,
        json!({"run_id":run_id.to_str().unwrap(),"artifacts":artifacts,"phase":phase}).to_string(),
    )
    .unwrap();
    std::fs::rename(temporary, root.join("snapshot.json")).unwrap();
}

#[test]
fn cli_streams_live_artifacts_before_completion() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "LIVE_ARTIFACTS Create hello.txt.").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path())
        .arg("--repo")
        .arg(repo.path())
        .arg("--skills-dir")
        .arg(repo.path().join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .arg("--session-timeout")
        .arg("180")
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(|| {
        repo.path()
            .join(".git/crabber-implement")
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some())
    });
    let artifacts = run_artifacts(repo.path());
    wait_until(|| {
        std::fs::read_to_string(artifacts.join("implement.txt"))
            .is_ok_and(|text| text == "Implemented live ")
    });
    assert!(
        child.try_wait().unwrap().is_none(),
        "CLI exited before stream completed"
    );
    assert!(!artifacts.join("result.json").exists());
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["status"], "running");
    assert_eq!(evidence["name"], "implement");
    assert!(evidence["observed_turns"].as_u64().unwrap() > 0);
    assert!(!evidence["session_id"].as_str().unwrap().is_empty());
    let tool_log = std::fs::read_to_string(artifacts.join("implement.tools.txt")).unwrap();
    assert!(
        !tool_log.trim().is_empty(),
        "tool results must be visible during a session"
    );
    for line in tool_log.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    let read_metadata = || {
        serde_json::from_slice::<Value>(&std::fs::read(artifacts.join("run.json")).unwrap())
            .unwrap()
    };
    let metadata = read_metadata();
    assert_eq!(metadata["version"], 2);
    assert_eq!(
        metadata["run_id"],
        artifacts.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(
        metadata["repo"],
        repo.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        metadata["branch"],
        git(repo.path(), &["branch", "--show-current"]).trim()
    );
    assert_eq!(metadata["model"], "glm-5.2");
    let first_heartbeat = metadata["heartbeat_at_ms"].as_u64().unwrap();
    wait_until(|| read_metadata()["heartbeat_at_ms"].as_u64().unwrap() > first_heartbeat);
    export_artifacts(&artifacts, "live");
    if let Some(hold_file) = std::env::var_os("CRABBER_VERIFY_HOLD_FILE") {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !Path::new(&hold_file).exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for browser verification release"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "CLI exited during browser verification"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
    fixture.stream_release.store(true, Ordering::Relaxed);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(artifacts.join("implement.txt")).unwrap(),
        "Implemented live artifacts and verified hello.txt."
    );
    assert_eq!(session_evidence(repo.path())["status"], "completed");
    let result: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["status"], "completed");
    assert!(String::from_utf8_lossy(&output.stdout).contains("review 2"));
    let final_heartbeat = read_metadata()["heartbeat_at_ms"].as_u64().unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        read_metadata()["heartbeat_at_ms"].as_u64().unwrap(),
        final_heartbeat,
        "heartbeat continued after the workflow exited"
    );
    export_artifacts(&artifacts, "completed");
    if let Some(done_file) = std::env::var_os("CRABBER_VERIFY_DONE_FILE") {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Path::new(&done_file).exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for completed browser verification"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn default_turn_budget_allows_more_than_64_model_turns() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = long_turn_run(repo.path(), &fixture, &[], None);
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}\nimplementation requests {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fixture.implementation_requests.load(Ordering::Relaxed)
    );
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 66);
    assert!(String::from_utf8_lossy(&output.stdout).contains("done "));
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["max_turns"], 256);
    assert_eq!(evidence["observed_turns"], 66);
    assert_eq!(evidence["status"], "completed");
    assert!(
        evidence["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(evidence["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(evidence["error"].is_null());
    assert_eq!(evidence["runs"][0]["response_count"], 66);
    assert_eq!(evidence["runs"][0]["responses_truncated"], true);
    assert_eq!(
        evidence["runs"][0]["responses"].as_array().unwrap().len(),
        8
    );
    let artifacts = run_artifacts(repo.path());
    assert!(
        std::fs::metadata(artifacts.join("implement.session.json"))
            .unwrap()
            .len()
            < 65_536
    );
    let responses = std::fs::read_to_string(artifacts.join("implement.responses.jsonl")).unwrap();
    assert_eq!(responses.lines().count(), 66);
    for line in responses.lines() {
        let response: Value = serde_json::from_str(line).unwrap();
        assert_eq!(response["run_id"], evidence["run_id"]);
        assert!(response["stream_completed"].as_bool().unwrap());
    }
}

#[test]
fn explicit_turn_budget_fails_with_preserved_evidence() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = long_turn_run(repo.path(), &fixture, &["--max-turns", "3"], Some("2"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("turn limit of 3 after 3 observed model turns")
            && stderr.contains("--max-turns"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("done "));
    assert!(!stdout.contains("review 1"));
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 3);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "initial\n"
    );
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "0"
    );
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["max_turns"], 3);
    assert_eq!(evidence["observed_turns"], 3);
    assert_eq!(evidence["status"], "failed");
    assert!(evidence["error"].as_str().unwrap().contains("turn limit"));
    assert!(!evidence.to_string().contains("fixture-key"));
    let artifacts = run_artifacts(repo.path());
    assert!(artifacts.join("implement.txt").is_file());
    let tool_log = std::fs::read_to_string(artifacts.join("implement.tools.txt")).unwrap();
    assert_eq!(tool_log.lines().count(), 3);
}

#[test]
fn environment_turn_budget_is_used_and_zero_is_rejected() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = long_turn_run(repo.path(), &fixture, &[], Some("2"));
    assert!(!output.status.success());
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 2);
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["max_turns"], 2);
    assert_eq!(evidence["observed_turns"], 2);
    for args in [vec!["--max-turns", "0"], vec![]] {
        let output = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
            .arg("/does/not/exist.md")
            .args(args)
            .env("CRABBER_IMPLEMENT_MAX_TURNS", "0")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value '0'"));
    }
}

fn completion_run(
    repo: &Path,
    fixture: &Fixture,
    scenario: &str,
    args: &[&str],
) -> std::process::Output {
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), scenario).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabber-implement"));
    if !args.contains(&"--session-timeout") {
        command.args(["--session-timeout", "20"]);
    }
    command
        .arg(plan.path())
        .arg("--repo")
        .arg(repo)
        .arg("--skills-dir")
        .arg(repo.join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .args(args)
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
        .output()
        .unwrap()
}

#[test]
fn reasoning_only_completion_continues_the_same_stage_session() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_RECOVER", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "fixed\n"
    );
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["continuation_count"], 1);
    assert_eq!(evidence["runs"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["observed_turns"], 4);
    assert_eq!(evidence["runs"][0]["responses"][1]["finish_reason"], "stop");
    assert_eq!(
        evidence["runs"][0]["responses"][1]["has_visible_text"],
        false
    );
    assert_eq!(
        evidence["runs"][0]["responses"][1]["stream_completed"],
        true
    );
    assert_eq!(
        evidence["runs"][1]["responses"][1]["usage"]["input_tokens"],
        17
    );
    assert_ne!(evidence["runs"][0]["run_id"], evidence["runs"][1]["run_id"]);
    export_artifacts(&run_artifacts(repo.path()), "completion");
    let history = fixture.history.lock().unwrap();
    let resumed = &history[2]["messages"];
    assert_eq!(
        resumed
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .count(),
        2,
        "continuation prompt missing from same-session history"
    );
    assert!(
        resumed
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"),
        "continuation lost prior research tool result"
    );
    assert_eq!(
        resumed
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user"
                && message.to_string().contains("COMPLETION_RECOVER"))
            .count(),
        1,
        "replayed original task"
    );
    assert!(
        !std::fs::read_to_string(run_artifacts(repo.path()).join("implement.txt"))
            .unwrap()
            .contains("Private fixture reasoning")
    );
}

#[test]
fn empty_completion_has_a_bounded_continuation_budget() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_EMPTY", &[]);
    assert!(!output.status.success());
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 3);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no final answer"));
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["continuation_count"], 2);
    assert_eq!(evidence["status"], "failed");
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "0"
    );
}

#[test]
fn continuation_does_not_reset_the_turn_budget() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(
        repo.path(),
        &fixture,
        "COMPLETION_RECOVER",
        &["--max-turns", "3"],
    );
    assert!(!output.status.success());
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 3);
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["observed_turns"], 3);
    assert!(evidence["error"].as_str().unwrap().contains("turn limit"));
}

#[test]
fn truncated_tools_and_filtered_responses_never_retry_or_execute_tools() {
    for scenario in ["COMPLETION_TRUNCATED_TOOL", "COMPLETION_FILTERED"] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &[]);
        assert!(!output.status.success());
        assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 1);
        assert!(!repo.path().join("unsafe.txt").exists());
        assert_eq!(
            git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
            "0"
        );
    }
}

#[test]
fn length_without_tool_calls_can_continue_and_complete() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_LENGTH", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(session_evidence(repo.path())["continuation_count"], 1);
}

#[test]
fn explicit_blocker_preserves_its_explanation_and_never_commits() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_BLOCKED", &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("required fixture dependency is unavailable")
    );
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "0"
    );
}

#[test]
fn continuations_share_one_absolute_deadline() {
    for scenario in ["COMPLETION_DELAY", "COMPLETION_BUDGET_DELAY"] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &["--session-timeout", "3"]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exceeded 3 seconds"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 2);
        let evidence = session_evidence(repo.path());
        assert_eq!(evidence["continuation_count"], 1);
        assert_eq!(evidence["status"], "failed");
    }
}

#[test]
fn cancellation_during_continuation_never_replays_the_stage() {
    for scenario in ["COMPLETION_CANCEL", "COMPLETION_BUDGET_CANCEL"] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
        std::fs::write(plan.path(), scenario).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
            .arg(plan.path())
            .arg("--repo")
            .arg(repo.path())
            .arg("--skills-dir")
            .arg(repo.path().join(".git/fixture-skills"))
            .arg("--base-url")
            .arg(format!("http://{}", fixture.address))
            .arg("--session-timeout")
            .arg("20")
            .env("OPENCODE_GO_API_KEY", "fixture-key")
            .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_until(|| fixture.implementation_requests.load(Ordering::Relaxed) == 2);
        let recovering = session_evidence(repo.path());
        assert_eq!(recovering["status"], "running");
        assert_eq!(recovering["continuation_count"], 1);
        assert_eq!(recovering["runs"].as_array().unwrap().len(), 2);
        assert!(
            Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 2);
        assert_eq!(session_evidence(repo.path())["status"], "failed");
        assert_eq!(
            git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
            "0"
        );
    }
}

#[test]
fn failed_stream_text_cannot_become_the_final_answer() {
    for (scenario, continuations) in [
        ("COMPLETION_STREAM_HIDDEN", 1),
        ("COMPLETION_STREAM_VISIBLE", 0),
    ] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &[]);
        assert!(
            output.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
            "fixed\n"
        );
        assert_eq!(
            session_evidence(repo.path())["continuation_count"],
            continuations
        );
        if scenario == "COMPLETION_STREAM_HIDDEN" {
            let history = fixture.history.lock().unwrap();
            let resumed = history
                .iter()
                .filter(|request| {
                    !request
                        .to_string()
                        .contains("Summarize this context for continuation")
                })
                .nth(2)
                .unwrap();
            assert!(
                resumed["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "user"
                        && message["content"]
                            .to_string()
                            .contains("Continue the assigned task from the existing session")),
                "compaction omitted the continuation nudge"
            );
        }
        assert!(
            std::fs::read_to_string(run_artifacts(repo.path()).join("implement.txt"))
                .unwrap()
                .contains("stale failed response")
        );
    }
}

#[test]
fn output_budget_recovers_only_after_transmitting_a_larger_cap() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_CAP_RECOVER", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "fixed\n"
    );
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["continuation_count"], 1);
    export_artifacts(&run_artifacts(repo.path()), "output-budget-success");
    let history = fixture.history.lock().unwrap();
    let implementation: Vec<_> = history
        .iter()
        .filter(|body| {
            let text = body.to_string();
            text.contains("COMPLETION_CAP_RECOVER")
                && !text.contains("Perform your assigned independent review")
                && !text.contains("Execute the following fix-review skill")
                && !text.contains("Summarize this context for continuation")
        })
        .collect();
    assert_eq!(implementation[0]["max_tokens"], 16384);
    assert_eq!(implementation[1]["max_tokens"], 32768);
    assert!(
        implementation[1]["messages"]
            .to_string()
            .contains("COMPLETION_CAP_RECOVER")
    );
    assert!(
        !std::fs::read_to_string(run_artifacts(repo.path()).join("implement.txt"))
            .unwrap()
            .contains("Private output budget sentinel")
    );
}

fn output_length() -> Value {
    json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private output budget sentinel"},"finish_reason":"length"}],"usage":{"prompt_tokens":20,"completion_tokens":16384}})
}

fn output_caps(evidence: &Value) -> Vec<u64> {
    evidence["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["requested_max_output_tokens"].as_u64().unwrap())
        .collect()
}

#[test]
fn output_budget_reaches_the_third_attempt_and_mixed_recovery_keeps_its_cap() {
    for (scenario, caps) in [
        ("COMPLETION_BUDGET_THIRD", vec![16384, 32768, 65536]),
        ("COMPLETION_BUDGET_MIXED", vec![16384, 16384, 32768]),
    ] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evidence = session_evidence(repo.path());
        assert_eq!(output_caps(&evidence), caps);
        assert_eq!(evidence["continuation_count"], 2);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
            "fixed\n"
        );
    }
}

#[test]
fn output_budget_exhaustion_is_bounded_and_preserves_successful_edits() {
    for (scenario, args, caps, reason) in [
        (
            "COMPLETION_BUDGET_ALWAYS",
            vec![],
            vec![16384, 32768, 65536],
            "output ceiling",
        ),
        (
            "COMPLETION_BUDGET_ALWAYS",
            vec![
                "--max-output-tokens",
                "20000",
                "--max-recovery-output-tokens",
                "50000",
            ],
            vec![20000, 40000, 50000],
            "output ceiling",
        ),
        (
            "COMPLETION_BUDGET_ALWAYS",
            vec![
                "--max-output-tokens",
                "32768",
                "--max-recovery-output-tokens",
                "32768",
            ],
            vec![32768],
            "output ceiling",
        ),
        (
            "COMPLETION_BUDGET_ALWAYS",
            vec!["--max-recovery-output-tokens", "262144"],
            vec![16384, 32768, 65536],
            "continuation allowance",
        ),
        (
            "COMPLETION_BUDGET_PRESERVE",
            vec![],
            vec![16384, 32768, 65536],
            "output ceiling",
        ),
        (
            "COMPLETION_BUDGET_MIXED_EXHAUST",
            vec![],
            vec![16384, 32768, 32768],
            "continuation allowance",
        ),
    ] {
        let repo = repository();
        let original_head = git(repo.path(), &["rev-parse", "HEAD"]);
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &args);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("exhausted output-limit recovery"), "{error}");
        assert!(error.contains(reason), "{error}");
        assert!(error.contains("--max-recovery-output-tokens"), "{error}");
        let evidence = session_evidence(repo.path());
        assert_eq!(output_caps(&evidence), caps);
        assert_eq!(
            evidence["runs"].as_array().unwrap().last().unwrap()["status"],
            "failed"
        );
        assert_eq!(
            evidence["runs"].as_array().unwrap().last().unwrap()["continuation_cause"],
            "output_limit"
        );
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), original_head);
        let history = fixture.history.lock().unwrap();
        assert!(!history.iter().any(|request| {
            request
                .to_string()
                .contains("Perform your assigned independent review")
        }));
        let transmitted: Vec<_> = history
            .iter()
            .map(|request| request["max_tokens"].as_u64().unwrap())
            .collect();
        let expected = if scenario.contains("PRESERVE") {
            vec![16384, 16384, 32768, 65536]
        } else {
            caps
        };
        assert_eq!(transmitted, expected);
        if scenario.contains("PRESERVE") {
            export_artifacts(&run_artifacts(repo.path()), "output-budget-exhausted");
            assert_eq!(
                std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
                "partial work\n"
            );
            assert!(git(repo.path(), &["status", "--porcelain"]).contains("hello.txt"));
        }
        assert!(
            !serde_json::to_string(&evidence)
                .unwrap()
                .contains("Private output budget sentinel")
        );
    }
}

#[test]
fn output_budget_invalid_flags_do_not_mutate_repository_or_contact_provider() {
    for args in [
        vec!["--max-output-tokens", "0"],
        vec!["--max-recovery-output-tokens", "0"],
        vec!["--max-output-tokens", "-1"],
        vec!["--max-output-tokens", "4294967296"],
        vec!["--max-output-tokens", "65537"],
    ] {
        let repo = repository();
        let head = git(repo.path(), &["rev-parse", "HEAD"]);
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, "COMPLETION_CAP_RECOVER", &args);
        assert!(!output.status.success());
        assert!(fixture.history.lock().unwrap().is_empty());
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(repo.path(), &["branch", "--show-current"]).trim(),
            "main"
        );
        assert!(!repo.path().join(".git/crabber-implement").exists());
    }
}

#[test]
fn output_budget_provider_rejection_and_turn_exhaustion_never_retry_again() {
    for (scenario, args, expected) in [
        ("COMPLETION_BUDGET_REJECT", vec![], vec![16384, 32768]),
        (
            "COMPLETION_BUDGET_ALWAYS",
            vec!["--max-turns", "1"],
            vec![16384],
        ),
    ] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, scenario, &args);
        assert!(!output.status.success());
        let evidence = session_evidence(repo.path());
        assert_eq!(output_caps(&evidence), expected);
        assert_eq!(fixture.history.lock().unwrap().len(), expected.len());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if scenario.contains("REJECT") {
                "fixture rejects requested output cap"
            } else {
                "turn limit"
            }),
            "{error}"
        );
        assert_eq!(
            evidence["runs"].as_array().unwrap().last().unwrap()["status"],
            "failed"
        );
    }
}

#[test]
fn output_budget_is_independent_in_implementation_review_and_fix_sessions() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_BUDGET_STAGES", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let artifacts = run_artifacts(repo.path());
    let mut count = 0;
    for file in std::fs::read_dir(&artifacts).unwrap() {
        let file = file.unwrap().path();
        if file.to_string_lossy().ends_with(".session.json") {
            let evidence: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            assert_eq!(output_caps(&evidence), vec![16384, 32768]);
            assert_eq!(evidence["runs"][0]["continuation_cause"], "output_limit");
            assert_eq!(evidence["runs"][1]["continuation_cause"], Value::Null);
            count += 1;
        }
    }
    assert_eq!(count, 6);
}

#[test]
fn output_budget_never_escalates_compaction_summary_truncation() {
    let repo = repository();
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_SUMMARY_LENGTH", &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("model output limit reached"), "{error}");
    let evidence = session_evidence(repo.path());
    assert_eq!(output_caps(&evidence), vec![16384]);
    assert_eq!(evidence["continuation_count"], 0);
    assert_eq!(evidence["runs"][0]["continuation_cause"], Value::Null);
    assert_eq!(evidence["runs"][0]["status"], "failed");
    let history = fixture.history.lock().unwrap();
    assert_eq!(history.len(), 3);
    assert!(
        history
            .last()
            .unwrap()
            .to_string()
            .contains("Summarize this context for continuation")
    );
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
}

#[test]
fn autonomous_config_fixes_use_bundled_policy_and_require_fresh_approval() {
    for external_skill_exists in [true, false] {
        let repo = repository();
        let main = git(repo.path(), &["rev-parse", "main"]);
        let external = repo.path().join(".git/fixture-skills/fix-review/SKILL.md");
        if external_skill_exists {
            std::fs::write(
                &external,
                "EXTERNAL_INTERACTIVE_POLICY: configuration must be needs-manual.",
            )
            .unwrap();
        } else {
            std::fs::remove_file(&external).unwrap();
        }
        let fixture = Fixture::start(repo.path(), false);
        let output = completion_run(repo.path(), &fixture, "AUTONOMOUS_CONFIG", &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("config.toml")).unwrap(),
            "mode = \"fixed\"\n"
        );
        assert_eq!(git(repo.path(), &["rev-parse", "main"]), main);
        assert_eq!(
            git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
            "2"
        );
        assert!(
            git(repo.path(), &["status", "--porcelain"])
                .trim()
                .is_empty()
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("review 2"));
        assert!(stdout.contains("done "));
        let history = fixture.history.lock().unwrap();
        let fix_requests: Vec<_> = history
            .iter()
            .filter(|request| {
                request
                    .to_string()
                    .contains("Execute the following fix-review skill")
            })
            .collect();
        assert!(!fix_requests.is_empty());
        for request in fix_requests {
            let prompt = request["messages"].to_string();
            assert!(prompt.contains("name: fix-review-autonomous"));
            assert!(!prompt.contains("EXTERNAL_INTERACTIVE_POLICY"));
        }
        let head = git(repo.path(), &["rev-parse", "HEAD"]);
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(
                repo.path()
                    .join("reviews")
                    .join(format!(
                        "{}-2",
                        run_artifacts(repo.path())
                            .file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                    ))
                    .join("manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["head_sha"].as_str().unwrap(), head.trim());
        assert_eq!(manifest["reviewers"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn autonomous_real_blockers_still_stop_before_fix_commit_or_approval() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "AUTONOMOUS_BLOCKER", &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fix-review requires manual attention")
    );
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "1"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("review 2"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("done "));
}

use opentelemetry_proto::tonic::{
    collector::trace::v1::{
        ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
    },
    common::v1::{KeyValue, any_value},
    trace::v1::Span as OtlpSpan,
};
use prost::Message as _;

#[derive(Clone, Copy)]
enum TraceReply {
    Accepted,
    Rejected,
    Hanging,
}

struct TraceIntake {
    endpoint: String,
    batches: Arc<Mutex<Vec<ExportTraceServiceRequest>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TraceIntake {
    fn start(reply: TraceReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let batches = Arc::new(Mutex::new(Vec::new()));
        let stopped = stop.clone();
        let captured = batches.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 8192];
                let header_end = loop {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() < 8_000_000);
                    if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.starts_with("POST /v1/traces "));
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("content-type: application/x-protobuf")
                );
                assert!(!headers.to_ascii_lowercase().contains("dd-api-key"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 8_000_000);
                while bytes.len() < header_end + length {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                captured.lock().unwrap().push(
                    ExportTraceServiceRequest::decode(&bytes[header_end..header_end + length])
                        .unwrap(),
                );
                if matches!(reply, TraceReply::Hanging) {
                    while !stopped.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(5));
                    }
                } else {
                    let body = match reply {
                        TraceReply::Rejected => ExportTraceServiceResponse {
                            partial_success: Some(ExportTracePartialSuccess {
                                rejected_spans: 1,
                                error_message: "PRIVATE_COLLECTOR_SENTINEL".into(),
                            }),
                        }
                        .encode_to_vec(),
                        _ => vec![],
                    };
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(&body);
                }
            }
        });
        Self {
            endpoint,
            batches,
            stop,
            worker: Some(worker),
        }
    }

    fn spans(&self) -> Vec<OtlpSpan> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|batch| &batch.resource_spans)
            .flat_map(|resource| &resource.scope_spans)
            .flat_map(|scope| scope.spans.clone())
            .collect()
    }
}

impl Drop for TraceIntake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn trace_attribute(attributes: &[KeyValue], name: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key == name)
        .and_then(
            |attribute| match attribute.value.as_ref()?.value.as_ref()? {
                any_value::Value::StringValue(value) => Some(value.clone()),
                any_value::Value::IntValue(value) => Some(value.to_string()),
                _ => None,
            },
        )
}

fn telemetry_command(repo: &Path, fixture: &Fixture, plan: &Path, endpoint: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabber-implement"));
    command
        .arg(plan)
        .arg("--repo")
        .arg(repo)
        .arg("--skills-dir")
        .arg(repo.join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .args(["--session-timeout", "20"])
        .env("OPENCODE_GO_API_KEY", "PRIVATE_TELEMETRY_KEY_SENTINEL")
        .env("DD_API_KEY", "PRIVATE_AGENTLESS_KEY_SENTINEL")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "false")
        .env("OTEL_SDK_DISABLED", "false")
        .env("OTEL_TRACES_SAMPLER", "always_on")
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint)
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "150")
        .env_remove("OTEL_EXPORTER_OTLP_HEADERS")
        .env("DD_SERVICE", "crabber-implement")
        .env("DD_ENV", "verification");
    command
}

fn telemetry_evidence(repo: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(run_artifacts(repo).join("telemetry.json")).unwrap())
        .unwrap()
}

#[test]
fn telemetry_exports_correlated_workflow_sessions_attempts_and_sdk_spans_without_content() {
    let repo = repository();
    std::fs::write(
        repo.path().join("README.md"),
        "PRIVATE_TELEMETRY_CONTENT_SENTINEL\n",
    )
    .unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-m", "private fixture"]);
    let fixture = Fixture::start(repo.path(), false);
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(
        plan.path(),
        "COMPLETION_RECOVER PRIVATE_TELEMETRY_CONTENT_SENTINEL",
    )
    .unwrap();
    let output = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = telemetry_evidence(repo.path());
    assert_eq!(report["status"], "flushed");
    let spans = intake.spans();
    let root = spans
        .iter()
        .find(|span| span.name == "implement.workflow")
        .unwrap();
    assert!(root.parent_span_id.is_empty());
    assert_eq!(
        trace_attribute(&root.attributes, "outcome").as_deref(),
        Some("completed")
    );
    assert_eq!(
        trace_attribute(&root.attributes, "workflow.run_id").unwrap(),
        run_artifacts(repo.path())
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(
        report["trace_id"],
        root.trace_id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    for name in [
        "implement.stage",
        "implement.session",
        "implement.attempt",
        "crabber.run",
        "crabber.model_call",
        "crabber.tool_call",
    ] {
        assert!(spans.iter().any(|span| span.name == name), "missing {name}");
    }
    for span in &spans {
        if span.name.starts_with("implement.")
            || matches!(
                span.name.as_str(),
                "crabber.run" | "crabber.model_call" | "crabber.tool_call"
            )
        {
            assert_eq!(span.trace_id, root.trace_id, "disconnected {}", span.name);
        }
        if matches!(
            span.name.as_str(),
            "implement.stage"
                | "implement.reviewer"
                | "implement.session"
                | "implement.attempt"
                | "crabber.run"
        ) {
            let parent = spans
                .iter()
                .find(|parent| parent.span_id == span.parent_span_id)
                .unwrap();
            let expected = match span.name.as_str() {
                "implement.stage" => "implement.workflow",
                "implement.session"
                    if trace_attribute(&span.attributes, "session.role")
                        .is_some_and(|role| role.starts_with("review-")) =>
                {
                    "implement.reviewer"
                }
                "implement.session" | "implement.reviewer" => "implement.stage",
                "implement.attempt" => "implement.session",
                _ => "implement.attempt",
            };
            assert_eq!(parent.name, expected, "parent of {}", span.name);
        }
    }
    let sessions: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "implement.session")
        .collect();
    assert_eq!(sessions.len(), 6);
    let attempts: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "implement.attempt")
        .collect();
    assert_eq!(attempts.len(), 7);
    assert_eq!(
        attempts
            .iter()
            .filter(
                |span| trace_attribute(&span.attributes, "continuation.reason").as_deref()
                    == Some("empty_completion")
            )
            .count(),
        1
    );
    for attempt in attempts {
        assert!(trace_attribute(&attempt.attributes, "session.id").is_some());
        assert!(trace_attribute(&attempt.attributes, "run.id").is_some());
    }
    for batch in intake.batches.lock().unwrap().iter() {
        let body = batch.encode_to_vec();
        let body = String::from_utf8_lossy(&body);
        for sentinel in [
            "PRIVATE_TELEMETRY",
            "PRIVATE_AGENTLESS",
            "Private fixture reasoning",
            "Fixture project",
            "Read AGENTS.md",
        ] {
            assert!(!body.contains(sentinel), "export leaked {sentinel}");
        }
        for resource in &batch.resource_spans {
            assert_eq!(
                trace_attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "workflow.run_id"
                )
                .unwrap(),
                run_artifacts(repo.path())
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            );
            assert_eq!(
                trace_attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "service.name"
                )
                .as_deref(),
                Some("crabber-implement")
            );
        }
    }
    if let Some(path) = std::env::var_os("CRABBER_VERIFY_ARTIFACTS") {
        let root = Path::new(&path);
        std::fs::create_dir_all(root).unwrap();
        let batches = intake.batches.lock().unwrap();
        std::fs::write(
            root.join("otlp-sample.bin"),
            batches.last().unwrap().encode_to_vec(),
        )
        .unwrap();
    }
}

#[test]
fn telemetry_exports_duplicate_tool_start_failure_before_process_exit() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "COMPLETION_DUPLICATE_START").unwrap();
    let output = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate tool call start"));
    assert_eq!(telemetry_evidence(repo.path())["status"], "flushed");
    let spans = intake.spans();
    for name in [
        "implement.workflow",
        "implement.stage",
        "implement.session",
        "implement.attempt",
    ] {
        let span = spans
            .iter()
            .find(|span| {
                span.name == name
                    && (name != "implement.stage"
                        || trace_attribute(&span.attributes, "stage.name").as_deref()
                            == Some("implement"))
            })
            .unwrap();
        assert_eq!(
            trace_attribute(&span.attributes, "outcome").as_deref(),
            Some("failed")
        );
        assert!(span.status.as_ref().is_some_and(|status| status.code == 2));
    }
}

#[test]
fn telemetry_disable_and_invalid_configuration_preserve_workflow_results() {
    for disabled in [true, false] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let intake = TraceIntake::start(TraceReply::Accepted);
        let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
        std::fs::write(plan.path(), "Create hello.txt").unwrap();
        let mut command = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint);
        if disabled {
            command.arg("--no-telemetry");
        } else {
            command.env(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://PRIVATE_ENDPOINT_SENTINEL",
            );
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evidence = telemetry_evidence(repo.path());
        assert_eq!(
            evidence["status"],
            if disabled { "disabled" } else { "failed" }
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_ENDPOINT_SENTINEL"));
        assert!(intake.spans().is_empty());
    }
}

#[test]
fn telemetry_unavailable_or_hanging_collector_cannot_fail_successful_workflow() {
    for hang in [false, true] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let intake = TraceIntake::start(if hang {
            TraceReply::Hanging
        } else {
            TraceReply::Accepted
        });
        let endpoint = if hang {
            intake.endpoint.clone()
        } else {
            let socket = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/v1/traces", socket.local_addr().unwrap())
        };
        let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
        std::fs::write(plan.path(), "Create hello.txt").unwrap();
        let start = Instant::now();
        let output = telemetry_command(repo.path(), &fixture, plan.path(), &endpoint)
            .output()
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(telemetry_evidence(repo.path())["error"], "export");
        assert!(String::from_utf8_lossy(&output.stderr).contains("workflow result is unchanged"));
    }
}

#[test]
fn telemetry_cancellation_exports_closed_workflow_with_cancelled_status() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "COMPLETION_CANCEL").unwrap();
    let child = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(|| fixture.implementation_requests.load(Ordering::Relaxed) == 2);
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(telemetry_evidence(repo.path())["status"], "flushed");
    let root = intake
        .spans()
        .into_iter()
        .find(|span| span.name == "implement.workflow")
        .unwrap();
    assert_eq!(
        trace_attribute(&root.attributes, "workflow.status").as_deref(),
        Some("cancelled")
    );
}

#[test]
#[ignore = "submits one synthetic failure trace to the local Datadog agent"]
fn telemetry_local_agent_smoke() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "COMPLETION_DUPLICATE_START").unwrap();
    let output = telemetry_command(
        repo.path(),
        &fixture,
        plan.path(),
        "http://127.0.0.1:4318/v1/traces",
    )
    .env("OTEL_EXPORTER_OTLP_TIMEOUT", "2000")
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate tool call start"));
    let evidence = telemetry_evidence(repo.path());
    export_artifacts(&run_artifacts(repo.path()), "datadog-smoke");
    assert_eq!(evidence["status"], "flushed", "{evidence}");
    println!(
        "workflow_run_id={} trace_id={}",
        run_artifacts(repo.path())
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        evidence["trace_id"].as_str().unwrap()
    );
}

#[test]
fn telemetry_health_does_not_claim_delivery_when_sampled_out_or_partially_rejected() {
    for sampled_out in [true, false] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let intake = TraceIntake::start(if sampled_out {
            TraceReply::Accepted
        } else {
            TraceReply::Rejected
        });
        let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
        std::fs::write(plan.path(), "Create hello.txt").unwrap();
        let mut command = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint);
        if sampled_out {
            command.env("OTEL_TRACES_SAMPLER", "always_off");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evidence = telemetry_evidence(repo.path());
        assert_eq!(evidence["status"], "flushed");
        assert_eq!(evidence["delivery"], "unverified");
        assert_eq!(intake.spans().is_empty(), sampled_out);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_COLLECTOR_SENTINEL"));
    }
}

#[test]
fn transport_outage_recovers_without_replaying_tools_or_starting_a_continuation() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let started = std::time::Instant::now();
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), "TRANSPORT_RECOVER").unwrap();
    let output = telemetry_command(repo.path(), &fixture, plan.path(), &intake.endpoint)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(started.elapsed() >= Duration::from_secs(15));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(),
        "fixed\n"
    );
    let history = fixture.history.lock().unwrap();
    assert!(history.len() >= 6);
    assert!(history[1..6].windows(2).all(|pair| pair[0] == pair[1]));
    drop(history);
    let artifact = run_artifacts(repo.path());
    let session: Value =
        serde_json::from_slice(&std::fs::read(artifact.join("implement.session.json")).unwrap())
            .unwrap();
    assert_eq!(session["continuation_count"], 0);
    assert_eq!(session["observed_turns"], 2);
    export_artifacts(&artifact, "transport-recovered");
    let spans = intake.spans();
    let failed: Vec<_> = spans
        .iter()
        .filter(|span| {
            span.name == "crabber.provider.request"
                && trace_attribute(&span.attributes, "outcome").as_deref()
                    == Some("transport_error")
        })
        .collect();
    assert_eq!(failed.len(), 4);
    for span in failed {
        assert_eq!(
            trace_attribute(&span.attributes, "transport.phase").as_deref(),
            Some("request")
        );
        assert_eq!(
            trace_attribute(&span.attributes, "transport.cause").as_deref(),
            Some("request")
        );
        assert_eq!(
            trace_attribute(&span.attributes, "transport.protocol_code").as_deref(),
            Some("http_incomplete_message")
        );
        assert_eq!(
            trace_attribute(&span.attributes, "transport.chain_status").as_deref(),
            Some("complete")
        );
    }
    let exported = format!("{spans:?}");
    for secret in [
        fixture.address.as_str(),
        "PRIVATE_TELEMETRY_KEY_SENTINEL",
        "PRIVATE_AGENTLESS_KEY_SENTINEL",
        "TRANSPORT_RECOVER",
    ] {
        assert!(!exported.contains(secret));
    }
}

#[test]
fn session_deadline_interrupts_transport_backoff() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let started = std::time::Instant::now();
    let output = completion_run(
        repo.path(),
        &fixture,
        "TRANSPORT_OUTAGE",
        &["--session-timeout", "2"],
    );
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(String::from_utf8_lossy(&output.stderr).contains("session exceeded 2 seconds"));
    assert!(!repo.path().join("hello.txt").exists());
}

fn recovery_command(repo: &Path, fixture: &Fixture, plan: &Path) -> Command {
    recovery_command_timeout(repo, fixture, plan, "30")
}

fn recovery_command_timeout(repo: &Path, fixture: &Fixture, plan: &Path, timeout: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabber-implement"));
    command
        .arg(plan)
        .arg("--repo")
        .arg(repo)
        .arg("--skills-dir")
        .arg(repo.join(".git/fixture-skills"))
        .arg("--base-url")
        .arg(format!("http://{}", fixture.address))
        .args(["--session-timeout", timeout])
        .env("OPENCODE_GO_API_KEY", "fixture-key")
        .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true");
    command
}

#[test]
fn review_recovery_reuses_the_completed_role_and_requires_fresh_review_after_fix() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_RECOVERY").unwrap();
    let first = recovery_command(repo.path(), &fixture, &plan)
        .output()
        .unwrap();
    assert!(!first.status.success(), "first review must fail");
    let artifacts = run_artifacts(repo.path());
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    let checkpoint = artifacts.join("review-checkpoints/1/correctness.report.json");
    assert!(
        checkpoint.exists(),
        "completed role was not independently checkpointed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report = std::fs::read(&checkpoint).unwrap();
    let head = git(repo.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let initial_meta =
        std::fs::read(artifacts.join("review-1-correctness-attempt-1.session.json")).unwrap();
    fixture.fail_review.store(false, Ordering::Relaxed);
    let second = recovery_command(repo.path(), &fixture, &plan)
        .arg("--resume-review")
        .arg(state["workflow_id"].as_str().unwrap())
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("reusing review 1 correctness"));
    assert!(!String::from_utf8_lossy(&second.stdout).contains("implement 1"));
    assert_eq!(std::fs::read(checkpoint).unwrap(), report);
    assert_eq!(
        std::fs::read(artifacts.join("review-1-correctness-attempt-1.session.json")).unwrap(),
        initial_meta
    );
    assert!(
        artifacts
            .join("review-1-maintainability-attempt-2.session.json")
            .exists()
    );
    assert!(
        artifacts
            .join("review-2-correctness-attempt-1.session.json")
            .exists()
    );
    assert!(artifacts.join("executions/1/result.json").exists());
    assert_eq!(git(repo.path(), &["status", "--porcelain"]).trim(), "");
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "2"
    );
    let history = fixture.history.lock().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|body| {
                let s = body.to_string();
                s.contains("slug correctness,")
                    && s.contains(&head)
                    && s.contains("Review round 1.")
            })
            .count(),
        1
    );
    drop(history);
    export_artifacts(&artifacts, "review-recovered");
}

#[test]
fn review_recovery_rejects_stale_profiles_corruption_and_fix_intent_without_calls() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_BAD_REPORT").unwrap();
    assert!(
        !recovery_command(repo.path(), &fixture, &plan)
            .output()
            .unwrap()
            .status
            .success()
    );
    let artifacts = run_artifacts(repo.path());
    let original = std::fs::read(artifacts.join("review-state.json")).unwrap();
    let state: Value = serde_json::from_slice(&original).unwrap();
    let id = state["workflow_id"].as_str().unwrap();
    let requests = fixture.history.lock().unwrap().len();
    for field in ["model", "branch", "base", "executable", "plan", "skill"] {
        let mut changed = state.clone();
        changed["inputs"][field] = "different".into();
        std::fs::write(artifacts.join("review-state.json"), changed.to_string()).unwrap();
        let output = recovery_command(repo.path(), &fixture, &plan)
            .args(["--resume-review", id])
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted stale {field}");
        assert_eq!(fixture.history.lock().unwrap().len(), requests);
    }
    for phase in ["fix_intent_recorded", "fix_committed", "completed"] {
        let mut changed = state.clone();
        changed["phase"] = phase.into();
        std::fs::write(artifacts.join("review-state.json"), changed.to_string()).unwrap();
        assert!(
            !recovery_command(repo.path(), &fixture, &plan)
                .args(["--resume-review", id])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fixture.history.lock().unwrap().len(), requests);
    }
    std::fs::write(artifacts.join("review-state.json"), &original).unwrap();
    let report = artifacts.join("review-checkpoints/1/correctness.report.json");
    std::fs::write(&report, "broken").unwrap();
    assert!(
        !recovery_command(repo.path(), &fixture, &plan)
            .args(["--resume-review", id])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fixture.history.lock().unwrap().len(), requests);
}

#[test]
fn review_recovery_survives_process_kill_after_completed_role_publication() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_RECOVERY").unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = recovery_command(repo.path(), &fixture, &plan)
        .stdout(log.reopen().unwrap())
        .stderr(log.reopen().unwrap())
        .spawn()
        .unwrap();
    let artifact_root = repo.path().join(".git/crabber-implement");
    wait_until(|| {
        artifact_root.is_dir()
            && std::fs::read_dir(&artifact_root).unwrap().any(|e| {
                e.unwrap()
                    .path()
                    .join("review-checkpoints/1/correctness.report.json")
                    .exists()
            })
    });
    let artifacts = run_artifacts(repo.path());
    let report =
        std::fs::read(artifacts.join("review-checkpoints/1/correctness.report.json")).unwrap();
    child.kill().unwrap();
    let _ = child.wait().unwrap();
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    fixture.fail_review.store(false, Ordering::Relaxed);
    let output = recovery_command(repo.path(), &fixture, &plan)
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("reusing review 1 correctness"));
    assert_eq!(
        std::fs::read(artifacts.join("review-checkpoints/1/correctness.report.json")).unwrap(),
        report
    );
    export_artifacts(&artifacts, "review-killed-and-recovered");
}

#[test]
fn telemetry_review_recovery_reuses_without_fabricating_model_spans() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_BAD_REPORT").unwrap();
    let first = telemetry_command(repo.path(), &fixture, &plan, &intake.endpoint)
        .output()
        .unwrap();
    assert!(!first.status.success());
    let artifacts = run_artifacts(repo.path());
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    fixture.fail_review.store(false, Ordering::Relaxed);
    let second = telemetry_command(repo.path(), &fixture, &plan, &intake.endpoint)
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let spans = intake.spans();
    let roots: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "implement.workflow")
        .collect();
    assert_eq!(roots.len(), 2);
    assert_ne!(roots[0].trace_id, roots[1].trace_id);
    let predecessor: Value = serde_json::from_slice(
        &std::fs::read(artifacts.join("executions/1/telemetry.json")).unwrap(),
    )
    .unwrap();
    let resumed = roots
        .iter()
        .find(|span| {
            trace_attribute(&span.attributes, "execution.generation").as_deref() == Some("2")
        })
        .unwrap();
    assert_eq!(
        trace_attribute(&resumed.attributes, "predecessor.trace_id").as_deref(),
        predecessor["trace_id"].as_str()
    );
    let reused = spans
        .iter()
        .find(|s| {
            s.name == "implement.reviewer"
                && trace_attribute(&s.attributes, "outcome").as_deref() == Some("reused")
        })
        .unwrap();
    assert!(
        !spans.iter().any(|s| s.parent_span_id == reused.span_id),
        "cached role fabricated an agent/model session"
    );
    assert_eq!(
        trace_attribute(&reused.attributes, "review.role").as_deref(),
        Some("correctness")
    );
    assert_eq!(
        trace_attribute(&reused.attributes, "execution.generation").as_deref(),
        Some("2")
    );
    let rendered = format!("{spans:?}");
    for secret in [
        "PRIVATE_TELEMETRY_KEY_SENTINEL",
        "PRIVATE_AGENTLESS_KEY_SENTINEL",
        "REVIEW_BAD_REPORT",
    ] {
        assert!(!rendered.contains(secret));
    }
    export_artifacts(&artifacts, "review-telemetry-recovered");
}

#[test]
#[ignore = "browser-controlled real CLI recovery journey"]
fn cli_review_recovery_browser_journey() {
    let continue_file =
        std::path::PathBuf::from(std::env::var_os("CRABBER_VERIFY_REVIEW_CONTINUE_FILE").unwrap());
    let release_file =
        std::path::PathBuf::from(std::env::var_os("CRABBER_VERIFY_REVIEW_RELEASE_FILE").unwrap());
    let done_file =
        std::path::PathBuf::from(std::env::var_os("CRABBER_VERIFY_REVIEW_DONE_FILE").unwrap());
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_RECOVERY REVIEW_BROWSER").unwrap();
    let first = recovery_command_timeout(repo.path(), &fixture, &plan, "90")
        .output()
        .unwrap();
    assert!(!first.status.success());
    let artifacts = run_artifacts(repo.path());
    export_artifacts(&artifacts, "review-browser-failed");
    let wait_file = |path: &Path| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "browser marker {} timed out",
                path.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    wait_file(&continue_file);
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    fixture.fail_review.store(false, Ordering::Relaxed);
    fixture.review_release.store(false, Ordering::Relaxed);
    let child = recovery_command_timeout(repo.path(), &fixture, &plan, "90")
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&release_file);
    fixture.review_release.store(true, Ordering::Relaxed);
    let second = child.wait_with_output().unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    export_artifacts(&artifacts, "review-browser-completed");
    wait_file(&done_file);
}

#[test]
fn review_recovery_later_round_preserves_earlier_fix_artifacts() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_LATER_ROUND").unwrap();
    let first = recovery_command(repo.path(), &fixture, &plan)
        .output()
        .unwrap();
    assert!(!first.status.success(), "must fail in second review");
    let artifacts = run_artifacts(repo.path());
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["round"],
        2,
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let originals: Vec<_> = ["session.json", "txt", "tools.txt", "responses.jsonl"]
        .into_iter()
        .map(|suffix| {
            let path = artifacts.join(format!("fix-review-1.{suffix}"));
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    fixture.fail_review.store(false, Ordering::Relaxed);
    let second = recovery_command(repo.path(), &fixture, &plan)
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    for (path, bytes) in originals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert!(artifacts.join("fix-review-2.session.json").exists());
    assert!(
        artifacts
            .join("review-3-correctness-attempt-1.session.json")
            .exists()
    );
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(),
        "3"
    );
}

#[test]
fn review_recovery_cached_reports_require_credentials_only_for_fixes() {
    use sha2::{Digest, Sha256};
    for approved in [false, true] {
        let repo = repository();
        let fixture = Fixture::start(repo.path(), false);
        let plan = repo.path().join(".git/recovery.md");
        std::fs::write(&plan, "REVIEW_BAD_REPORT").unwrap();
        let first = recovery_command(repo.path(), &fixture, &plan)
            .output()
            .unwrap();
        assert!(!first.status.success());
        let artifacts = run_artifacts(repo.path());
        let state_bytes = std::fs::read(artifacts.join("review-state.json")).unwrap();
        let state: Value = serde_json::from_slice(&state_bytes).unwrap();
        let directory = artifacts.join("review-checkpoints/1");
        let mut role: Value = serde_json::from_slice(
            &std::fs::read(directory.join("correctness.report.json")).unwrap(),
        )
        .unwrap();
        if approved {
            let report =
                r#"{"verdict":"APPROVE","summary":"reviewed","findings":[],"positive_notes":[]}"#;
            role["report"] = serde_json::from_str(report).unwrap();
            role["report_digest"] = format!("{:x}", Sha256::digest(report.as_bytes())).into();
            std::fs::write(
                directory.join("correctness.report.json"),
                serde_json::to_vec(&role).unwrap(),
            )
            .unwrap();
        }
        role["role"] = "maintainability".into();
        std::fs::write(
            directory.join("maintainability.report.json"),
            serde_json::to_vec(&role).unwrap(),
        )
        .unwrap();
        let calls = fixture.history.lock().unwrap().len();
        let second = recovery_command(repo.path(), &fixture, &plan)
            .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
            .env_remove("OPENCODE_GO_API_KEY")
            .output()
            .unwrap();
        assert_eq!(
            second.status.success(),
            approved,
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(
            fixture.history.lock().unwrap().len(),
            calls,
            "cached reports made model calls"
        );
        if !approved {
            assert!(String::from_utf8_lossy(&second.stderr).contains("export OPENCODE_GO_API_KEY"));
            assert_eq!(
                std::fs::read(artifacts.join("review-state.json")).unwrap(),
                state_bytes
            );
            fixture.fail_review.store(false, Ordering::Relaxed);
            let third = recovery_command(repo.path(), &fixture, &plan)
                .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
                .output()
                .unwrap();
            assert!(
                third.status.success(),
                "{}",
                String::from_utf8_lossy(&third.stderr)
            );
        }
    }
}

#[test]
fn review_recovery_last_round_cannot_reset_budget_or_start_a_fix() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_BAD_REPORT").unwrap();
    let first = recovery_command(repo.path(), &fixture, &plan)
        .args(["--max-reviews", "1"])
        .output()
        .unwrap();
    assert!(!first.status.success());
    let artifacts = run_artifacts(repo.path());
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    fixture.fail_review.store(false, Ordering::Relaxed);
    let second = recovery_command(repo.path(), &fixture, &plan)
        .args([
            "--max-reviews",
            "1",
            "--resume-review",
            state["workflow_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("review budget exhausted"));
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
    assert!(!artifacts.join("fix-review-1.session.json").exists());
    let history = fixture.history.lock().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|request| request.to_string().contains("slug correctness,"))
            .count(),
        1
    );
}

#[test]
fn review_recovery_cancellation_drains_and_allows_a_later_readonly_attempt() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/recovery.md");
    std::fs::write(&plan, "REVIEW_BAD_REPORT REVIEW_BROWSER").unwrap();
    let first = recovery_command(repo.path(), &fixture, &plan)
        .output()
        .unwrap();
    assert!(!first.status.success());
    let artifacts = run_artifacts(repo.path());
    let state: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("review-state.json")).unwrap())
            .unwrap();
    let original =
        std::fs::read(artifacts.join("review-checkpoints/1/correctness.report.json")).unwrap();
    fixture.fail_review.store(false, Ordering::Relaxed);
    fixture.review_release.store(false, Ordering::Relaxed);
    let child = recovery_command(repo.path(), &fixture, &plan)
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(|| {
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                let text = request.to_string();
                text.contains("slug maintainability,") && text.contains("Review round 1.")
            })
            .count()
            >= 2
            && artifacts
                .join("review-1-maintainability-attempt-2.session.json")
                .exists()
    });
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let second = child.wait_with_output().unwrap();
    assert!(!second.status.success());
    let result: Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(
        std::fs::read(artifacts.join("review-checkpoints/1/correctness.report.json")).unwrap(),
        original
    );
    fixture.review_release.store(true, Ordering::Relaxed);
    let third = recovery_command(repo.path(), &fixture, &plan)
        .args(["--resume-review", state["workflow_id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert!(
        artifacts
            .join("review-1-maintainability-attempt-3.session.json")
            .exists()
    );
    assert!(artifacts.join("executions/2/result.json").exists());
}
