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
                        let response = if serialized
                            .contains("Perform your assigned independent review")
                        {
                            assert!(!serialized.contains("external-secret-sentinel"), "host followed a committed symlink outside the review snapshot");
                            let tools = body["tools"].as_array().unwrap();
                            assert!(tools.iter().all(|tool| !matches!(tool["function"]["name"].as_str(), Some("shell" | "file_write" | "file_edit" | "apply_patch"))));

                            if invalid_review {
                                text("not a review")
                            } else {
                                let approved = serialized.contains("EXTERNAL_SYMLINK") || std::fs::read_to_string(repo.join("hello.txt"))
                                    .unwrap_or_default()
                                    == "fixed\n";
                                text(&report(approved).to_string())
                            }
                        } else if serialized.contains("Execute the following fix-review skill") {
                            if has_result {
                                text("<!-- auto-summary fixed:1 skipped:0 needs-manual:0 -->")
                            } else {
                                tool(
                                    "file_edit",
                                    json!({"path":"hello.txt","anchor":"initial","replacement":"fixed"}),
                                )
                            }
                        } else if serialized.contains("COMPLETION_") {
                            let attempt = requests.fetch_add(1, Ordering::Relaxed);
                            let continued = body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "user" && message["content"].as_str().is_some_and(|value| !value.contains("COMPLETION_")));
                            if serialized.contains("COMPLETION_BLOCKED") {
                                text("BLOCKED: The required fixture dependency is unavailable.")
                            } else if serialized.contains("COMPLETION_TRUNCATED_TOOL") {
                                let mut response = tool("file_write", json!({"path":"unsafe.txt","content":"must not run"}));
                                response["choices"][0]["finish_reason"] = "length".into();
                                response
                            } else if serialized.contains("COMPLETION_FILTERED") {
                                json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})
                            } else if serialized.contains("COMPLETION_EMPTY") || (serialized.contains("COMPLETION_RECOVER") && attempt == 1) {
                                json!({"choices":[{"index":0,"delta":{"reasoning_content":"Private fixture reasoning."},"finish_reason":"stop"}]})
                            } else if serialized.contains("COMPLETION_LENGTH") && !continued {
                                json!({"choices":[{"index":0,"delta":{"content":"Partial narration"},"finish_reason":"length"}]})
                            } else if serialized.contains("COMPLETION_RECOVER") && attempt == 0 {
                                tool("file_read", json!({"path":"README.md"}))
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
                        let payload = format!("data: {}\n\ndata: [DONE]\n\n", response);
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
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
    json!({"choices":[{"index":0,"delta":{"content":content},"finish_reason":"stop"}]})
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
        .env("OPENCODE_GO_API_KEY", "fixture-key");
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
    for entry in std::fs::read_dir(artifacts).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file()
            && !entry.file_name().to_string_lossy().ends_with(".tmp")
        {
            std::fs::copy(entry.path(), snapshot.join(entry.file_name())).unwrap();
        }
    }
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
    assert_eq!(metadata["version"], 1);
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

fn completion_run(repo: &Path, fixture: &Fixture, scenario: &str, args: &[&str]) -> std::process::Output {
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), scenario).unwrap();
    Command::new(env!("CARGO_BIN_EXE_crabber-implement"))
        .arg(plan.path()).arg("--repo").arg(repo)
        .arg("--skills-dir").arg(repo.join(".git/fixture-skills"))
        .arg("--base-url").arg(format!("http://{}", fixture.address))
        .arg("--session-timeout").arg("20").args(args)
        .env("HOME", repo).env("OPENCODE_GO_API_KEY", "fixture-key")
        .output().unwrap()
}

#[test]
fn reasoning_only_completion_continues_the_same_stage_session() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_RECOVER", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read_to_string(repo.path().join("hello.txt")).unwrap(), "fixed\n");
    let evidence = session_evidence(repo.path());
    assert_eq!(evidence["continuation_count"], 1);
    assert_eq!(evidence["runs"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["observed_turns"], 4);
    let history = fixture.history.lock().unwrap();
    let resumed = &history[2]["messages"];
    assert!(resumed.as_array().unwrap().iter().any(|message| message["role"] == "tool"), "continuation lost prior research tool result");
    assert_eq!(resumed.as_array().unwrap().iter().filter(|message| message["role"] == "user" && message.to_string().contains("COMPLETION_RECOVER")).count(), 1, "replayed original task");
    assert!(!std::fs::read_to_string(run_artifacts(repo.path()).join("implement.txt")).unwrap().contains("Private fixture reasoning"));
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
    assert_eq!(git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(), "0");
}

#[test]
fn continuation_does_not_reset_the_turn_budget() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_RECOVER", &["--max-turns", "3"]);
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
        assert_eq!(git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(), "0");
    }
}

#[test]
fn length_without_tool_calls_can_continue_and_complete() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_LENGTH", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(session_evidence(repo.path())["continuation_count"], 1);
}

#[test]
fn explicit_blocker_preserves_its_explanation_and_never_commits() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let output = completion_run(repo.path(), &fixture, "COMPLETION_BLOCKED", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required fixture dependency is unavailable"));
    assert_eq!(fixture.implementation_requests.load(Ordering::Relaxed), 1);
    assert_eq!(git(repo.path(), &["rev-list", "--count", "main..HEAD"]).trim(), "0");
}
