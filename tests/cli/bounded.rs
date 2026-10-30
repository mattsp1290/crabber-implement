use super::*;
use std::{collections::BTreeMap, path::PathBuf, process::Output};

struct ChildGuard(Option<std::process::Child>);
impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
impl ChildGuard {
    fn output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Default)]
struct Observations {
    mutable_sessions: usize,
    review_prompts: Vec<(u32, usize)>,
    evidence_reads: usize,
    plan_reads: usize,
    source_reads: usize,
    inherited_reads: usize,
    maximum_tool_message_bytes: usize,
    forbidden_tools: Vec<String>,
}

struct BoundedFixture {
    address: String,
    stopped: Arc<AtomicBool>,
    hold_reviews: Arc<AtomicBool>,
    output_calls: Arc<AtomicUsize>,
    fail_round: Arc<AtomicUsize>,
    request_changes: Arc<AtomicBool>,
    observations: Arc<Mutex<Observations>>,
    worker: Option<thread::JoinHandle<()>>,
}

fn message_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| message_text(&item["text"]))
            .collect(),
        _ => String::new(),
    }
}

fn tool_values(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| serde_json::from_str(&message_text(&message["content"])).ok())
        .collect()
}

fn batch(calls: Vec<(&str, Value)>) -> Value {
    static ID: AtomicUsize = AtomicUsize::new(0);
    let calls: Vec<_> = calls.into_iter().enumerate().map(|(index, (name, arguments))| {
        json!({"index":index,"id":format!("bounded-{}", ID.fetch_add(1, Ordering::Relaxed)),"type":"function","function":{"name":name,"arguments":arguments.to_string()}})
    }).collect();
    json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]})
}

impl BoundedFixture {
    fn start(repo: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stopped = Arc::new(AtomicBool::new(false));
        let hold_reviews = Arc::new(AtomicBool::new(false));
        let output_calls = Arc::new(AtomicUsize::new(18));
        let fail_round = Arc::new(AtomicUsize::new(0));
        let request_changes = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(Mutex::new(Observations::default()));
        let repo = repo.to_owned();
        let stop = stopped.clone();
        let hold = hold_reviews.clone();
        let count = output_calls.clone();
        let fail = fail_round.clone();
        let changes = request_changes.clone();
        let seen = observations.clone();
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let repo = repo.clone();
                        let stop = stop.clone();
                        let hold = hold.clone();
                        let count = count.clone();
                        let fail = fail.clone();
                        let changes = changes.clone();
                        let seen = seen.clone();
                        connections.push(thread::spawn(move || {
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                            let body = request(&mut stream);
                            let prompt = body["messages"].as_array().unwrap().iter()
                                .find(|message| message["role"] == "user")
                                .map(|message| message_text(&message["content"])).unwrap();
                            let tools = tool_values(&body);
                            let review = prompt.starts_with("Review round ");
                            let response = if review {
                                let round: u32 = prompt.strip_prefix("Review round ").unwrap().split('.').next().unwrap().parse().unwrap();
                                let bounded = body["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "review_artifact_read");
                                if bounded {
                                    for message in body["input"].as_array().unwrap().iter().filter(|message| message["type"] == "function_call_output") {
                                        let bytes = serde_json::to_vec(message).unwrap().len();
                                        assert!(bytes <= 32 * 1024, "provider-facing artifact tool message exceeds 32 KiB: {bytes}");
                                        let mut observations = seen.lock().unwrap();
                                        observations.maximum_tool_message_bytes = observations.maximum_tool_message_bytes.max(bytes);
                                    }
                                }
                                if tools.is_empty() {
                                    seen.lock().unwrap().review_prompts.push((round, prompt.len()));
                                    for tool in body["tools"].as_array().unwrap() {
                                        let name = tool["function"]["name"].as_str().unwrap();
                                        if matches!(name, "shell" | "file_write" | "file_edit" | "apply_patch") {
                                            seen.lock().unwrap().forbidden_tools.push(name.into());
                                        }
                                    }
                                }
                                if fail.load(Ordering::Relaxed) == round as usize {
                                    text("fixture failed report after committed revision")
                                } else if !bounded {
                                    text(&report(round == 4).to_string())
                                } else if tools.is_empty() {
                                    tool("review_artifact_catalog", json!({"cursor":0,"limit":32}))
                                } else {
                                    let catalog = tools.iter().find(|value| value["entries"].is_array())
                                        .unwrap_or_else(|| panic!("catalog tool failed: {}", body["messages"].as_array().unwrap().last().unwrap()));
                                    let entries = catalog["entries"].as_array().unwrap();
                                    let evidence_name = if round == 1 { "implement.tools.txt".into() } else { format!("fix-review-{}.tools.txt", round - 1) };
                                    let desired = match tools.len() { 1 => Some(("evidence", evidence_name.as_str())), 2 => Some(("plan", "plan.md")), 3 => Some(("source", "hello.txt")), 4 => Some(("evidence", "implement.tools.txt")), _ => None };
                                    if let Some((kind, name)) = desired {
                                        if tools.len() == 2 {
                                            assert!(tools[1]["content"].as_str().unwrap_or("").contains(&format!("{}_STAGE_{}", if round == 4 { "PASS" } else { "FAIL" }, round - 1)), "omitted evidence sentinel missing from actual reader output: {}", tools[1]);
                                            seen.lock().unwrap().evidence_reads += 1;
                                        }
                                        if tools.len() == 3 {
                                            assert!(tools[2]["content"].as_str().unwrap().contains("BOUNDED_FIXTURE_PLAN"));
                                            seen.lock().unwrap().plan_reads += 1;
                                        }
                                        let entry = entries.iter().find(|entry| entry["kind"] == kind && entry["name"] == name).unwrap_or_else(|| panic!("missing {kind}/{name} in {catalog}"));
                                        let bytes = entry["bytes"].as_u64().unwrap();
                                        let offset = if kind == "evidence" { bytes.saturating_sub(4096) } else { 0 };
                                        tool("review_artifact_read", json!({"id":entry["id"],"offset":offset,"length":4096}))
                                    } else {
                                        assert!(tools[4]["content"].as_str().unwrap().contains("FAIL_STAGE_0"));
                                        seen.lock().unwrap().inherited_reads += 1;
                                        assert!(tools[3]["content"].as_str().unwrap().contains(&format!("stage{}", round - 1)));
                                        seen.lock().unwrap().source_reads += 1;
                                        while hold.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(10)); }
                                        let approved = tools[1]["content"].as_str().unwrap().contains("PASS_STAGE_3") && !changes.load(Ordering::Relaxed);
                                        let mut result = report(approved);
                                        result["summary"] = "Inspected omitted execution evidence, original plan, and pinned committed source through snapshot tools.".into();
                                        if !approved {
                                            result["findings"][0]["description"] = "The inspected execution evidence contains a failing verification stage.".into();
                                            result["findings"][0]["suggested_fix"] = "Advance the implementation and rerun the fixture verification.".into();
                                        }
                                        text(&result.to_string())
                                    }
                                }
                            } else if body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool") {
                                if prompt.starts_with("Execute the following fix-review skill") {
                                    text("<!-- auto-summary fixed:1 skipped:0 needs-manual:0 -->")
                                } else { text("Implemented stage0 and ran verification. The log contains the observed failures.") }
                            } else {
                                seen.lock().unwrap().mutable_sessions += 1;
                                let stage = if prompt.starts_with("Execute the following fix-review skill") {
                                    std::fs::read_to_string(repo.join("hello.txt")).unwrap().trim().strip_prefix("stage").unwrap().parse::<u32>().unwrap() + 1
                                } else { 0 };
                                let sentinel = format!("{}_STAGE_{stage}", if stage == 3 { "PASS" } else { "FAIL" });
                                let mut calls = vec![("file_write", json!({"path":"hello.txt","content":format!("stage{stage}\n")}))];
                                for _ in 0..count.load(Ordering::Relaxed) {
                                    calls.push(("shell", json!({"cmd":format!("printf '%060000d' 0; printf '\\n{sentinel}\\n'; test {stage} -eq 3")})));
                                }
                                batch(calls)
                            };
                            let payload = responses_sse(&response);
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("bounded fixture listener: {error}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            address,
            stopped,
            hold_reviews,
            output_calls,
            fail_round,
            request_changes,
            observations,
            worker: Some(worker),
        }
    }
    fn command(&self, binary: &Path, repo: &Path, plan: &Path, skills: &Path) -> Command {
        let mut command = fixture_command(binary);
        command
            .arg(plan)
            .arg("--repo")
            .arg(repo)
            .arg("--skills-dir")
            .arg(skills)
            .arg("--base-url")
            .arg(format!("http://{}", self.address))
            .args(["--session-timeout", "90", "--max-reviews", "4"])
            .env("CRABBER_IMPLEMENT_NO_TELEMETRY", "true");
        command
    }
}
impl Drop for BoundedFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn artifact_root(repo: &Path) -> PathBuf {
    PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"]).trim()).join("crabber-implement")
}
fn runs(repo: &Path) -> Vec<PathBuf> {
    let mut runs: Vec<_> = std::fs::read_dir(artifact_root(repo))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    runs.sort();
    runs
}
fn record(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn file_inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry.path(), result);
            } else {
                result.insert(
                    entry.path().strip_prefix(root).unwrap().into(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    collect(root, root, &mut result);
    result
}
fn plan_file() -> tempfile::NamedTempFile {
    let plan = tempfile::NamedTempFile::with_suffix(".md").unwrap();
    std::fs::write(plan.path(), format!("BOUNDED_FIXTURE_PLAN. Create hello.txt and fix all three observed verification failures.\n{}", "\\\"\u{0001}é\n".repeat(1000))).unwrap();
    plan
}
fn check_bounded_run(fixture: &BoundedFixture, artifacts: &Path) {
    responses::assert_native_delegations(artifacts, 8);
    let observations = fixture.observations.lock().unwrap();
    assert_eq!(observations.mutable_sessions, 4);
    assert_eq!(observations.evidence_reads, 8);
    assert_eq!(observations.plan_reads, 8);
    assert_eq!(observations.source_reads, 8);
    assert_eq!(observations.inherited_reads, 8);
    assert!(observations.forbidden_tools.is_empty());
    assert_eq!(observations.review_prompts.len(), 8);
    assert!(
        observations
            .review_prompts
            .iter()
            .all(|(_, bytes)| *bytes <= 256 * 1024)
    );
    let smallest = observations
        .review_prompts
        .iter()
        .map(|(_, bytes)| *bytes)
        .min()
        .unwrap();
    let largest = observations
        .review_prompts
        .iter()
        .map(|(_, bytes)| *bytes)
        .max()
        .unwrap();
    assert!(
        largest - smallest < 32 * 1024,
        "prompts grew with raw logs: {}",
        largest - smallest
    );
    for stage in ["implement", "fix-review-1", "fix-review-2", "fix-review-3"] {
        assert!(
            std::fs::metadata(artifacts.join(format!("{stage}.tools.txt")))
                .unwrap()
                .len()
                > 1_000_000
        );
        let log = std::fs::read_to_string(artifacts.join(format!("{stage}.tools.txt"))).unwrap();
        let expected = if stage == "fix-review-3" {
            "\"exit_code\":0"
        } else {
            "\"exit_code\":1"
        };
        assert!(
            log.contains(expected) || log.contains(&expected.replace('"', "\\\"")),
            "missing observed shell outcome {expected}"
        );
    }
    for round in 1..=4 {
        let manifest =
            record(&artifacts.join(format!("review-checkpoints/{round}/snapshot/manifest.json")));
        for entry in manifest["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "evidence")
        {
            let original = artifacts.join(entry["name"].as_str().unwrap());
            if original.exists() {
                assert_eq!(
                    std::fs::read(&original).unwrap(),
                    std::fs::read(artifacts.join(format!(
                        "review-checkpoints/{round}/snapshot/{}",
                        entry["id"].as_str().unwrap()
                    )))
                    .unwrap()
                );
            }
        }
    }
    println!(
        "bounded review measurements {:?}, evidence_reads={}, maximum_provider_tool_message_bytes={}",
        observations.review_prompts,
        observations.evidence_reads,
        observations.maximum_tool_message_bytes
    );
}

#[test]
fn bounded_reviews_read_omitted_evidence_across_four_rounds_and_linked_worktrees() {
    for linked in [false, true] {
        let repository = repository();
        let linked_directory = tempfile::tempdir().unwrap();
        let repo = if linked {
            let worktree = linked_directory.path().join("worktree");
            git(
                repository.path(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature-linked",
                    worktree.to_str().unwrap(),
                ],
            );
            worktree
        } else {
            repository.path().to_owned()
        };
        let fixture = BoundedFixture::start(&repo);
        let plan = plan_file();
        let result = fixture
            .command(
                Path::new(env!("CARGO_BIN_EXE_crabber-implement")),
                &repo,
                plan.path(),
                &repository.path().join(".git/fixture-skills"),
            )
            .output()
            .unwrap();
        assert_success(&result);
        let artifacts = runs(&repo).pop().unwrap();
        check_bounded_run(&fixture, &artifacts);
        assert_eq!(record(&artifacts.join("review-state.json"))["round"], 4);
        export_artifacts(
            &artifacts,
            if linked {
                "bounded-linked-complete"
            } else {
                "bounded-complete"
            },
        );
    }
}

#[test]
fn refresh_at_last_round_requires_fresh_reports_and_cannot_fix() {
    for changes in [false, true] {
        let repo = repository();
        let fixture = BoundedFixture::start(repo.path());
        let plan = plan_file();
        let skills = repo.path().join(".git/fixture-skills");
        let binary = Path::new(env!("CARGO_BIN_EXE_crabber-implement"));
        fixture.fail_round.store(4, Ordering::Relaxed);
        let first = fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .output()
            .unwrap();
        assert!(!first.status.success());
        let source = runs(repo.path()).pop().unwrap();
        let state = record(&source.join("review-state.json"));
        assert_eq!(
            state["round"],
            4,
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let original = file_inventory(&source);
        let head = git(repo.path(), &["rev-parse", "HEAD"]);
        if !changes {
            let mut command = fixture.command(binary, repo.path(), plan.path(), &skills);
            remove_auth(&mut command);
            let no_credentials = command
                .args(["--refresh-review", state["workflow_id"].as_str().unwrap()])
                .output()
                .unwrap();
            assert!(!no_credentials.status.success());
            assert!(String::from_utf8_lossy(&no_credentials.stderr).contains("codex login"));
            assert_eq!(runs(repo.path()).len(), 1);
            let changed_budget = fixture
                .command(binary, repo.path(), plan.path(), &skills)
                .args([
                    "--refresh-review",
                    state["workflow_id"].as_str().unwrap(),
                    "--max-turns",
                    "255",
                ])
                .output()
                .unwrap();
            assert!(!changed_budget.status.success());
            assert!(String::from_utf8_lossy(&changed_budget.stderr).contains("identity changed"));
            assert_eq!(runs(repo.path()).len(), 1);
            assert_eq!(file_inventory(&source), original);
        }
        fixture.fail_round.store(0, Ordering::Relaxed);
        fixture.request_changes.store(changes, Ordering::Relaxed);
        let refreshed = fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args(["--refresh-review", state["workflow_id"].as_str().unwrap()])
            .output()
            .unwrap();
        if changes {
            assert!(!refreshed.status.success());
            assert!(String::from_utf8_lossy(&refreshed.stderr).contains("review budget exhausted"));
        } else {
            assert_success(&refreshed);
        }
        assert_eq!(file_inventory(&source), original);
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(fixture.observations.lock().unwrap().mutable_sessions, 4);
        let child = runs(repo.path())
            .into_iter()
            .find(|path| *path != source)
            .unwrap();
        let child_state = record(&child.join("review-state.json"));
        assert_eq!(child_state["round"], 4);
        assert_eq!(child_state["attempts"], json!([1, 1]));
        assert_eq!(child_state["reused"], json!([false, false]));
        assert_eq!(child_state["generation"], 1);
        assert_eq!(
            record(&child.join("implementation.json")),
            record(&source.join("implementation.json"))
        );
        let origin = record(&child.join("recovery-origin.json"));
        assert_eq!(origin["parent_workflow_id"], state["workflow_id"]);
        assert_eq!(origin["source_round"], 4);
        assert!(!child.join("implement.session.json").exists());
        assert!(!child.join("fix-review-4.session.json").exists());
        export_artifacts(&source, "refresh-source-preserved");
        export_artifacts(
            &child,
            if changes {
                "refresh-last-round-changes"
            } else {
                "refresh-last-round-approved"
            },
        );
    }
}

#[test]
fn refresh_of_refresh_preserves_anchor_inventory_and_child_strict_resume() {
    let repo = repository();
    let fixture = BoundedFixture::start(repo.path());
    let plan = plan_file();
    let skills = repo.path().join(".git/fixture-skills");
    let binary = Path::new(env!("CARGO_BIN_EXE_crabber-implement"));
    fixture.fail_round.store(2, Ordering::Relaxed);
    assert!(
        !fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .output()
            .unwrap()
            .status
            .success()
    );
    let source = runs(repo.path()).pop().unwrap();
    let source_id = source.file_name().unwrap().to_str().unwrap();
    let original = file_inventory(&source);
    assert!(
        !fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args(["--refresh-review", source_id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let parent = runs(repo.path())
        .into_iter()
        .find(|path| *path != source)
        .unwrap();
    let parent_id = parent.file_name().unwrap().to_str().unwrap();
    let parent_original = file_inventory(&parent);
    let origin_path = parent.join("recovery-origin.json");
    let original_origin = std::fs::read(&origin_path).unwrap();
    let calls_before_corruption = fixture.observations.lock().unwrap().review_prompts.len();
    for key in ["parent_workflow_id", "root_workflow_id"] {
        let mut corrupt: Value = serde_json::from_slice(&original_origin).unwrap();
        corrupt[key] = uuid::Uuid::new_v4().to_string().into();
        std::fs::write(&origin_path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        for mode in ["--refresh-review", "--resume-review"] {
            let rejected = fixture
                .command(binary, repo.path(), plan.path(), &skills)
                .args([mode, parent_id])
                .output()
                .unwrap();
            assert!(
                !rejected.status.success(),
                "{mode} accepted changed recovery origin"
            );
            assert_eq!(
                runs(repo.path()).len(),
                2,
                "origin corruption initialized another child"
            );
            assert_eq!(
                fixture.observations.lock().unwrap().review_prompts.len(),
                calls_before_corruption
            );
        }
    }
    std::fs::write(&origin_path, original_origin).unwrap();

    assert!(
        !fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args(["--refresh-review", parent_id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let child = runs(repo.path())
        .into_iter()
        .find(|path| *path != source && *path != parent)
        .unwrap();
    let child_id = child.file_name().unwrap().to_str().unwrap();
    let origin = record(&child.join("recovery-origin.json"));
    assert_eq!(origin["parent_workflow_id"], parent_id);
    assert_eq!(origin["root_workflow_id"], source_id);
    assert_eq!(origin["source_round"], 2);
    let inventory = record(&child.join("inherited-evidence/manifest.json"));
    let ids: std::collections::BTreeSet<_> = inventory["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), inventory["entries"].as_array().unwrap().len());
    let saved_source = source.with_extension("removed-parent");
    let saved_parent = parent.with_extension("removed-parent");
    std::fs::rename(&source, &saved_source).unwrap();
    std::fs::rename(&parent, &saved_parent).unwrap();
    fixture.fail_round.store(0, Ordering::Relaxed);
    let resumed = fixture
        .command(binary, repo.path(), plan.path(), &skills)
        .args(["--resume-review", child_id])
        .output()
        .unwrap();
    assert_success(&resumed);
    assert_eq!(fixture.observations.lock().unwrap().mutable_sessions, 4);
    assert_eq!(record(&child.join("review-state.json"))["round"], 4);
    for round in [2, 3, 4] {
        let manifest =
            record(&child.join(format!("review-checkpoints/{round}/snapshot/manifest.json")));
        for entry in inventory["entries"].as_array().unwrap() {
            assert!(
                manifest["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|candidate| candidate["id"] == entry["id"]),
                "inherited artifact dropped in round {round}"
            );
        }
    }
    assert_eq!(
        record(&child.join("implementation.json")),
        record(&saved_source.join("implementation.json"))
    );
    assert_eq!(file_inventory(&saved_source), original);
    assert_eq!(file_inventory(&saved_parent), parent_original);
    assert!(child.join("fix-review-2.tools.txt").exists());
    assert!(child.join("fix-review-3.tools.txt").exists());
    assert!(!child.join("fix-review-1.tools.txt").exists());
    export_artifacts(&child, "refresh-flattened-resumed-complete");
}

fn wait_marker(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(180);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "marker {} timed out",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "browser-controlled refresh lifecycle"]
fn cli_review_refresh_browser_journey() {
    let export = PathBuf::from(std::env::var_os("CRABBER_VERIFY_ARTIFACTS").unwrap());
    let continue_file =
        PathBuf::from(std::env::var_os("CRABBER_VERIFY_REFRESH_CONTINUE_FILE").unwrap());
    let release_file =
        PathBuf::from(std::env::var_os("CRABBER_VERIFY_REFRESH_RELEASE_FILE").unwrap());
    let done_file = PathBuf::from(std::env::var_os("CRABBER_VERIFY_REFRESH_DONE_FILE").unwrap());
    let binary = Path::new(env!("CARGO_BIN_EXE_crabber-implement"));
    let repo = repository();
    let fixture = BoundedFixture::start(repo.path());
    let plan = plan_file();
    let skills = repo.path().join(".git/fixture-skills");
    fixture.fail_round.store(4, Ordering::Relaxed);
    let first = fixture
        .command(binary, repo.path(), plan.path(), &skills)
        .output()
        .unwrap();
    assert!(
        !first.status.success(),
        "expected source failure, stdout {}",
        String::from_utf8_lossy(&first.stdout)
    );
    let source = runs(repo.path()).pop().unwrap();
    let state = record(&source.join("review-state.json"));
    assert_eq!(
        state["round"],
        4,
        "source failed before round4: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let original = file_inventory(&source);
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    export_artifacts(&source, "refresh-browser-source-failed");
    wait_marker(&continue_file);
    fixture.fail_round.store(0, Ordering::Relaxed);
    fixture.hold_reviews.store(true, Ordering::Relaxed);
    let mut process = ChildGuard(Some(
        fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args(["--refresh-review", state["workflow_id"].as_str().unwrap()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(60);
    let child = loop {
        assert!(
            Instant::now() < deadline,
            "refresh child did not publish run metadata"
        );
        if let Some(status) = process.try_wait().unwrap() {
            let output = process.output();
            panic!(
                "refresh exited {status} before run metadata: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if let Some(child) = runs(repo.path())
            .into_iter()
            .find(|path| *path != source && path.join("run.json").exists())
        {
            break child;
        }
        thread::sleep(Duration::from_millis(20));
    };
    std::fs::write(export.join("refresh.json"), json!({"run_id":child.file_name().unwrap().to_str().unwrap(),"artifacts":child,"parent_workflow_id":state["workflow_id"]}).to_string()).unwrap();
    wait_marker(&release_file);
    fixture.hold_reviews.store(false, Ordering::Relaxed);
    let second = process.output();
    assert_success(&second);
    assert_eq!(file_inventory(&source), original);
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.observations.lock().unwrap().mutable_sessions, 4);
    assert_eq!(record(&child.join("review-state.json"))["round"], 4);
    export_artifacts(&child, "refresh-browser-completed");
    wait_marker(&done_file);
}

#[test]
fn refresh_rejects_stale_corrupt_and_mutable_sources_before_dispatch() {
    let repo = repository();
    let fixture = BoundedFixture::start(repo.path());
    fixture.output_calls.store(1, Ordering::Relaxed);
    fixture.fail_round.store(2, Ordering::Relaxed);
    let plan = plan_file();
    let skills = repo.path().join(".git/fixture-skills");
    let binary = Path::new(env!("CARGO_BIN_EXE_crabber-implement"));
    assert!(
        !fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .output()
            .unwrap()
            .status
            .success()
    );
    let source = runs(repo.path()).pop().unwrap();
    let source_id = source.file_name().unwrap().to_str().unwrap();
    let original_state = std::fs::read(source.join("review-state.json")).unwrap();
    let original = file_inventory(&source);
    let calls = fixture.observations.lock().unwrap().review_prompts.len();
    let rejected = || {
        let output = fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args(["--refresh-review", source_id])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            runs(repo.path()).len(),
            1,
            "rejection initialized a child: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fixture.observations.lock().unwrap().review_prompts.len(),
            calls
        );
    };
    for phase in ["fix_intent_recorded", "fix_committed", "completed"] {
        let mut state: Value = serde_json::from_slice(&original_state).unwrap();
        state["phase"] = phase.into();
        std::fs::write(
            source.join("review-state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        rejected();
    }
    let mut state: Value = serde_json::from_slice(&original_state).unwrap();
    state["version"] = 999.into();
    std::fs::write(
        source.join("review-state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    rejected();
    std::fs::write(source.join("review-state.json"), &original_state).unwrap();
    let prompt = source.join("review-checkpoints/2/correctness.prompt.txt");
    let original_prompt = std::fs::read(&prompt).unwrap();
    std::fs::write(&prompt, "corrupt input").unwrap();
    rejected();
    std::fs::write(&prompt, &original_prompt).unwrap();
    let manifest = record(&source.join("review-checkpoints/2/snapshot/manifest.json"));
    let snapshot_file = source
        .join("review-checkpoints/2/snapshot")
        .join(manifest["entries"][0]["id"].as_str().unwrap());
    let original_file = std::fs::read(&snapshot_file).unwrap();
    std::fs::write(&snapshot_file, "corrupt snapshot").unwrap();
    rejected();
    std::fs::remove_file(&snapshot_file).unwrap();
    std::os::unix::fs::symlink(repo.path().join("README.md"), &snapshot_file).unwrap();
    rejected();
    std::fs::remove_file(&snapshot_file).unwrap();
    std::fs::write(&snapshot_file, original_file).unwrap();
    let original_plan = std::fs::read(plan.path()).unwrap();
    std::fs::write(plan.path(), "changed plan").unwrap();
    rejected();
    std::fs::write(plan.path(), original_plan).unwrap();
    let review_skill = skills.join("review/SKILL.md");
    let original_skill = std::fs::read(&review_skill).unwrap();
    std::fs::write(&review_skill, "changed skill").unwrap();
    rejected();
    std::fs::write(&review_skill, original_skill).unwrap();
    let branch = git(repo.path(), &["branch", "--show-current"]);
    git(repo.path(), &["switch", "-c", "wrong-review-branch"]);
    rejected();
    git(repo.path(), &["switch", branch.trim()]);
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &["commit", "--allow-empty", "-m", "stale review head"],
    );
    rejected();
    git(repo.path(), &["reset", "--hard", head.trim()]);
    std::fs::write(repo.path().join("dirty.txt"), "dirty source").unwrap();
    rejected();
    std::fs::remove_file(repo.path().join("dirty.txt")).unwrap();
    assert_eq!(file_inventory(&source), original);
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
}

#[test]
fn killed_refresh_releases_repository_lock_and_restarts_only_reviewers() {
    let repo = repository();
    let fixture = BoundedFixture::start(repo.path());
    fixture.output_calls.store(1, Ordering::Relaxed);
    fixture.fail_round.store(4, Ordering::Relaxed);
    let plan = plan_file();
    let skills = repo.path().join(".git/fixture-skills");
    let binary = Path::new(env!("CARGO_BIN_EXE_crabber-implement"));
    assert!(
        !fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .output()
            .unwrap()
            .status
            .success()
    );
    let source = runs(repo.path()).pop().unwrap();
    let original = file_inventory(&source);
    fixture.fail_round.store(0, Ordering::Relaxed);
    fixture.hold_reviews.store(true, Ordering::Relaxed);
    let mut process = ChildGuard(Some(
        fixture
            .command(binary, repo.path(), plan.path(), &skills)
            .args([
                "--refresh-review",
                source.file_name().unwrap().to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    let child = loop {
        assert!(
            Instant::now() < deadline,
            "fresh reviewers did not reach the held response"
        );
        assert!(
            process.try_wait().unwrap().is_none(),
            "child exited before reviewer barrier"
        );
        if let Some(child) = runs(repo.path()).into_iter().find(|path| *path != source) {
            let both_running = ["correctness", "maintainability"].iter().all(|role| {
                let path = child.join(format!("review-4-{role}-attempt-1.session.json"));
                std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|value| value["status"] == "running")
            });
            if both_running && fixture.observations.lock().unwrap().source_reads >= 8 {
                break child;
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    process.kill().unwrap();
    process.wait().unwrap();
    assert_eq!(
        record(&child.join("review-state.json"))["phase"],
        "review_pending"
    );
    for role in ["correctness", "maintainability"] {
        assert!(
            !child
                .join(format!("review-checkpoints/4/{role}.report.json"))
                .exists()
        );
    }
    fixture.hold_reviews.store(false, Ordering::Relaxed);
    let resumed = fixture
        .command(binary, repo.path(), plan.path(), &skills)
        .args([
            "--resume-review",
            child.file_name().unwrap().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_success(&resumed);
    assert_eq!(
        record(&child.join("review-state.json"))["attempts"],
        json!([2, 2])
    );
    assert_eq!(fixture.observations.lock().unwrap().mutable_sessions, 4);
    assert_eq!(file_inventory(&source), original);
    export_artifacts(&child, "refresh-killed-and-resumed");
}
