use super::*;
use crate::config::PlanBundle;
use std::{num::NonZeroUsize, process::Command, time::Duration};

pub(crate) fn fixture() -> (tempfile::TempDir, RunConfig) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().canonicalize().unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@localhost"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fs::write(repo.join("README.md"), "fixture\n").unwrap();
    git::output(&repo, &["add", "README.md"]).unwrap();
    git::output(&repo, &["commit", "-m", "fixture"]).unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    let artifacts = repo.join(".git/crabber-implement").join(&run_id);
    durable::directory(&artifacts).unwrap();
    let config = RunConfig {
        repo: repo.clone(),
        plan: PlanBundle {
            text: "test plan".into(),
        },
        review_skill: "review".into(),
        base: "main".into(),
        model: crate::config::MODEL.into(),
        reasoning_effort: crabber::ReasoningEffort::Medium,
        codex_auth_file: repo.join(".git/fixture-auth.json"),
        base_url: "http://127.0.0.1".into(),
        max_reviews: 3,
        max_turns: NonZeroUsize::new(20).unwrap(),
        session_timeout: Duration::from_secs(30),
        artifacts,
        run_id,
        _repo_lock: fs::File::open(repo.join("README.md")).unwrap(),
        executable_digest: "fixture-binary".into(),
        generation: 1,
        predecessor_trace: None,
        resume: None,
        refresh_source: None,
        recovery_origin: None,
    };
    (temp, config)
}

fn state(config: &RunConfig) -> ReviewCheckpoint {
    durable::write_json(&config.artifacts.join("implementation.json"), &serde_json::json!({"commit":git::output(&config.repo, &["rev-parse", "HEAD"]).unwrap(),"plan":durable::digest(config.plan.text.as_bytes())}), false).unwrap();
    let mut state = ReviewCheckpoint::create(
        config,
        1,
        git::output(&config.repo, &["rev-parse", "HEAD"]).unwrap(),
        &snapshot(config, 1),
    )
    .unwrap();
    state.reserve_role(&config.artifacts, 0).unwrap();
    state.reserve_role(&config.artifacts, 1).unwrap();
    state
}

const APPROVE: &str =
    r#"{"verdict":"APPROVE","summary":"reviewed","findings":[],"positive_notes":[]}"#;

#[test]
fn successful_role_is_reusable_without_sibling_or_coordinator_ack() {
    let (_temp, config) = fixture();
    let state = state(&config);
    state
        .publish_role(&config, 0, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
    let loaded = ReviewCheckpoint::load(&config.artifacts).unwrap();
    assert_eq!(loaded.phase, CheckpointPhase::ReviewPending);
    assert!(loaded.report(&config.artifacts, 0).unwrap().is_some());
    assert!(loaded.report(&config.artifacts, 1).unwrap().is_none());
    assert!(loaded.needs_credentials(&config.artifacts).unwrap());
    let path = loaded
        .directory(&config.artifacts)
        .join("correctness.report.json");
    let mut corrupt: serde_json::Value = durable::json(&path, durable::REPORT_LIMIT).unwrap();
    corrupt["report"]["summary"] = "modified".into();
    durable::write_json(&path, &corrupt, true).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_err());
}

#[test]
fn identity_and_mutation_boundaries_fail_closed() {
    let (_temp, mut config) = fixture();
    let mut state = state(&config);
    assert!(state.inputs.verify(&config).is_ok());
    config.model = "different".into();
    assert!(state.inputs.verify(&config).is_err());
    config.model = "fixture".into();
    state.phase = CheckpointPhase::FixIntentRecorded;
    state.persist(&config.artifacts).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_err());
    assert!(state.reserve_role(&config.artifacts, 1).is_err());
}

#[test]
fn complete_reports_are_required_and_approval_needs_no_provider() {
    let (_temp, config) = fixture();
    let mut state = state(&config);
    state.phase = CheckpointPhase::ReviewReportsReady;
    state.persist(&config.artifacts).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_err());
    state.phase = CheckpointPhase::ReviewPending;
    state
        .publish_role(&config, 0, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
    state
        .publish_role(&config, 1, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
    assert!(!state.needs_credentials(&config.artifacts).unwrap());
    assert!(next_phase(state.phase, PhaseEvent::Reports([true, false])).is_none());
    assert!(next_phase(state.phase, PhaseEvent::Fix([true, true])).is_none());
}

#[test]
fn rendering_is_clean_and_idempotent_after_partial_staging() {
    let (_temp, config) = fixture();
    let mut state = state(&config);
    let reports = [
        state
            .publish_role(&config, 0, APPROVE, &saved_snapshot(&config, &state))
            .unwrap(),
        state
            .publish_role(&config, 1, APPROVE, &saved_snapshot(&config, &state))
            .unwrap(),
    ];
    state.phase = CheckpointPhase::ReviewReportsReady;
    state.staging_generation = Some(config.generation);
    state.persist(&config.artifacts).unwrap();
    let staging = config
        .repo
        .join("reviews")
        .join(format!(".{}-1.staging-1", config.run_id));
    let exclude = config.repo.join(".git/info/exclude");
    fs::write(
        &exclude,
        format!(
            "/reviews/{}-1/\n/reviews/.{}-1.staging-1/\n",
            config.run_id, config.run_id
        ),
    )
    .unwrap();
    durable::directory(&staging.join("correctness")).unwrap();
    durable::bytes(
        &staging.join("correctness/00-overview.md"),
        b"partial",
        false,
    )
    .unwrap();
    git::require_clean(&config.repo).unwrap();
    let dir = crate::review::publish_review(&config, &mut state, &reports, "stats").unwrap();
    git::require_clean(&config.repo).unwrap();
    let manifest = fs::read(dir.join("manifest.json")).unwrap();
    assert_eq!(
        crate::review::publish_review(&config, &mut state, &reports, "stats").unwrap(),
        dir
    );
    assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), manifest);
    fs::write(dir.join("correctness/00-overview.md"), "corrupt").unwrap();
    assert!(crate::review::publish_review(&config, &mut state, &reports, "stats").is_err());
}

#[test]
fn checkpoint_rejects_symlinked_inputs_and_oversized_records() {
    let (_temp, config) = fixture();
    let state = state(&config);
    let path = state
        .directory(&config.artifacts)
        .join("correctness.prompt.txt");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(config.repo.join("README.md"), &path).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_err());
    let path = config.artifacts.join("review-state.json");
    fs::write(path, vec![b' '; durable::METADATA_LIMIT + 1]).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_err());
}

#[test]
fn writer_faults_never_report_success() {
    for step in [
        "directory_create",
        "directory_sync",
        "file_write",
        "file_sync",
        "publish",
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        durable::FAIL.with(|f| *f.borrow_mut() = Some(step));
        let result = durable::bytes(&root.join("nested/file.json"), b"data", false);
        durable::FAIL.with(|f| *f.borrow_mut() = None);
        assert!(result.is_err(), "fault {step} did not fail");
    }
}

#[test]
fn rendering_subprocess_fixture() {
    let Some(root) = std::env::var_os("CRABBER_TEST_RENDER_ROOT") else {
        return;
    };
    let (temp, mut config) = fixture();
    config.artifacts = PathBuf::from(root);
    config.repo = config.artifacts.ancestors().nth(3).unwrap().to_owned();
    config.run_id = config
        .artifacts
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    drop(temp);
    let mut state = ReviewCheckpoint::load(&config.artifacts).unwrap();
    let reports = [
        state.report(&config.artifacts, 0).unwrap().unwrap(),
        state.report(&config.artifacts, 1).unwrap().unwrap(),
    ];
    crate::review::publish_review(&config, &mut state, &reports, "stats").unwrap();
}

#[test]
fn killed_renderer_reuses_paired_reports_and_cleans_ignored_staging() {
    let (_temp, config) = fixture();
    let mut state = state(&config);
    state
        .publish_role(&config, 0, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
    state
        .publish_role(&config, 1, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
    state.phase = CheckpointPhase::ReviewReportsReady;
    state.persist(&config.artifacts).unwrap();
    let report = fs::read(
        state
            .directory(&config.artifacts)
            .join("correctness.report.json"),
    )
    .unwrap();
    let marker = config.artifacts.join("render-paused");
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review_checkpoint::tests::rendering_subprocess_fixture",
                "--nocapture",
            ])
            .env("CRABBER_TEST_RENDER_ROOT", &config.artifacts)
            .env("CRABBER_TEST_RENDER_PAUSE", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "renderer exited before barrier"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "renderer did not reach publication barrier"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    git::require_clean(&config.repo).unwrap();
    let mut loaded = ReviewCheckpoint::load(&config.artifacts).unwrap();
    assert!(loaded.staging_generation.is_some());
    let reports = [
        loaded.report(&config.artifacts, 0).unwrap().unwrap(),
        loaded.report(&config.artifacts, 1).unwrap().unwrap(),
    ];
    let directory = crate::review::publish_review(&config, &mut loaded, &reports, "stats").unwrap();
    assert!(directory.join("manifest.json").is_file());
    assert_eq!(
        fs::read(
            loaded
                .directory(&config.artifacts)
                .join("correctness.report.json")
        )
        .unwrap(),
        report
    );
    assert!(loaded.staging_generation.is_none());
    git::require_clean(&config.repo).unwrap();
}

pub(crate) fn snapshot(config: &RunConfig, round: u32) -> crate::review_inputs::ReviewSnapshot {
    let mut builder = crate::review_inputs::SnapshotBuilder::new(
        crate::review_inputs::snapshot_directory(&config.artifacts, round),
        round,
        git::output(&config.repo, &["rev-parse", "HEAD"]).unwrap(),
    )
    .unwrap();
    builder.bind_revision(
        git::output(&config.repo, &["branch", "--show-current"]).unwrap(),
        git::output(&config.repo, &["rev-parse", &config.base]).unwrap(),
    );
    builder
        .add(
            config,
            "plan",
            "plan.md",
            "input",
            round,
            config.plan.text.as_bytes(),
        )
        .unwrap();
    builder.finish().unwrap()
}
fn saved_snapshot(
    config: &RunConfig,
    state: &ReviewCheckpoint,
) -> crate::review_inputs::ReviewSnapshot {
    crate::review_inputs::ReviewSnapshot::open(
        &crate::review_inputs::snapshot_directory(&config.artifacts, state.round),
        state.inputs.snapshot.as_ref().unwrap(),
    )
    .unwrap()
}

#[test]
fn refresh_publication_subprocess_fixture() {
    let Some(source) = std::env::var_os("CRABBER_TEST_REFRESH_SOURCE") else {
        return;
    };
    let source = PathBuf::from(source);
    let (_temporary, mut config) = fixture();
    config.repo = source.ancestors().nth(3).unwrap().to_owned();
    config.artifacts = PathBuf::from(std::env::var_os("CRABBER_TEST_REFRESH_CHILD").unwrap());
    config.run_id = config
        .artifacts
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .into();
    config.refresh_source = Some(RefreshSource::load(&config, &source).unwrap());
    begin_execution(&mut config).unwrap();
    let mut state = config.resume.take().unwrap();
    state.reserve_role(&config.artifacts, 0).unwrap();
    state
        .publish_role(&config, 0, APPROVE, &saved_snapshot(&config, &state))
        .unwrap();
}

#[test]
fn killed_refresh_initialization_never_promotes_partial_inputs_or_reuses_old_reports() {
    for stage in ["origin", "snapshot_entry", "ready", "role_report"] {
        let (_temp, config) = fixture();
        git::output(&config.repo, &["switch", "-c", "feature"]).unwrap();
        fs::write(config.repo.join("hello.txt"), "implemented\n").unwrap();
        git::output(&config.repo, &["add", "hello.txt"]).unwrap();
        git::output(&config.repo, &["commit", "-m", "implementation"]).unwrap();
        let source = state(&config);
        let source_bytes = fs::read(config.artifacts.join("review-state.json")).unwrap();
        let source_inputs =
            fs::read(source.directory(&config.artifacts).join("inputs.json")).unwrap();
        let child_root = config
            .artifacts
            .parent()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        let marker = config
            .repo
            .join(".git")
            .join(format!("refresh-{stage}-paused"));
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "review_checkpoint::tests::refresh_publication_subprocess_fixture",
                    "--nocapture",
                ])
                .env("CRABBER_TEST_REFRESH_SOURCE", &config.artifacts)
                .env("CRABBER_TEST_REFRESH_CHILD", &child_root)
                .env("CRABBER_TEST_REFRESH_BARRIER", stage)
                .env("CRABBER_TEST_REFRESH_MARKER", &marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "refresh exited before {stage}"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "refresh did not reach {stage}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert_eq!(
            fs::read(config.artifacts.join("review-state.json")).unwrap(),
            source_bytes
        );
        assert_eq!(
            fs::read(source.directory(&config.artifacts).join("inputs.json")).unwrap(),
            source_inputs
        );
        if matches!(stage, "origin" | "snapshot_entry") {
            assert!(!child_root.join("review-state.json").exists());
            assert!(ReviewCheckpoint::load(&child_root).is_err());
        } else {
            let checkpoint = ReviewCheckpoint::load(&child_root).unwrap();
            assert_eq!(checkpoint.phase, CheckpointPhase::ReviewPending);
            assert_eq!(checkpoint.round, source.round);
            assert_eq!(
                checkpoint.report(&child_root, 0).unwrap().is_some(),
                stage == "role_report"
            );
            assert!(checkpoint.report(&child_root, 1).unwrap().is_none());
            assert_eq!(checkpoint.reused, [false, false]);
        }
        git::require_clean(&config.repo).unwrap();
    }
}

#[test]
fn formal_legacy_checkpoint_versions_are_rejected() {
    let (_temp, config) = fixture();
    let checkpoint = state(&config);
    for version in [1, 2] {
        let mut wire = serde_json::to_value(&checkpoint).unwrap();
        wire["version"] = version.into();
        durable::write_json(&config.artifacts.join("review-state.json"), &wire, true).unwrap();
        let rejected = ReviewCheckpoint::load(&config.artifacts)
            .err()
            .is_some_and(|error| {
                error
                    .to_string()
                    .contains("only Codex native-subagent checkpoints")
            });
        assert!(
            rejected,
            "legacy checkpoint accepted as native subagent recovery"
        );
        assert!(RefreshSource::load(&config, &config.artifacts).is_err());
    }
    checkpoint.persist(&config.artifacts).unwrap();
    assert!(ReviewCheckpoint::load(&config.artifacts).is_ok());
}
