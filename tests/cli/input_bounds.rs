use super::*;

fn rejects_before_mutation(repo: &Path, fixture: &Fixture, plan: &Path, needle: &str) {
    let head = git(repo, &["rev-parse", "HEAD"]);
    let branch = git(repo, &["branch", "--show-current"]);
    for mode in [None, Some("--resume-review"), Some("--refresh-review")] {
        let mut command = recovery_command(repo, fixture, plan);
        remove_auth(&mut command);
        if let Some(mode) = mode {
            command.args([mode, "11111111-1111-4111-8111-111111111111"]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{mode:?} accepted invalid input");
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(error.contains(needle), "{mode:?}: {error}");
        assert!(
            !error.contains("codex login"),
            "credentials checked before input: {error}"
        );
        assert_eq!(git(repo, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(repo, &["branch", "--show-current"]), branch);
        assert!(git(repo, &["status", "--porcelain"]).is_empty());
        assert!(!repo.join(".git/crabber-implement").exists());
        assert!(fixture.history.lock().unwrap().is_empty());
    }
}

#[test]
fn all_entry_modes_bound_plan_bytes_including_rendered_headers_before_mutation() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/plan.md");
    std::fs::File::create(&plan)
        .unwrap()
        .set_len(2_000_001)
        .unwrap();
    rejects_before_mutation(repo.path(), &fixture, &plan, "plan");
    std::fs::write(&plan, "x".repeat(1_999_999)).unwrap();
    rejects_before_mutation(repo.path(), &fixture, &plan, "plan");
}

#[test]
fn all_entry_modes_bound_skill_bytes_before_mutation() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/plan.md");
    std::fs::write(&plan, "Implement the fixture.").unwrap();
    std::fs::File::create(repo.path().join(".git/fixture-skills/review/SKILL.md"))
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    rejects_before_mutation(repo.path(), &fixture, &plan, "skill");
}

#[test]
fn ignored_directory_entries_count_toward_the_configuration_limit() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/plan");
    std::fs::create_dir(&plan).unwrap();
    std::fs::write(plan.join("00-overview.md"), "Implement fixture.").unwrap();
    for index in 0..4096 {
        std::fs::create_dir(plan.join(format!("ignored-{index}"))).unwrap();
    }
    rejects_before_mutation(repo.path(), &fixture, &plan, "entr");
}

#[test]
fn selected_plan_files_have_a_separate_count_limit() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/plan");
    std::fs::create_dir(&plan).unwrap();
    std::fs::write(plan.join("00-overview.md"), "Implement fixture.").unwrap();
    for index in 1..=256 {
        std::fs::write(plan.join(format!("{index:03}-part.md")), "Requirement.").unwrap();
    }
    rejects_before_mutation(repo.path(), &fixture, &plan, "256");
}

#[test]
fn special_plan_and_skill_files_fail_without_opening_a_blocking_stream() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/plan.md");
    assert!(
        Command::new("mkfifo")
            .arg(&plan)
            .status()
            .unwrap()
            .success()
    );
    rejects_before_mutation(repo.path(), &fixture, &plan, "regular");
    std::fs::remove_file(&plan).unwrap();
    std::fs::write(&plan, "Implement fixture.").unwrap();
    let skill = repo.path().join(".git/fixture-skills/review/SKILL.md");
    std::fs::remove_file(&skill).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&skill)
            .status()
            .unwrap()
            .success()
    );
    rejects_before_mutation(repo.path(), &fixture, &plan, "regular");
}

#[test]
fn maximum_skill_reaches_real_review_through_snapshot_references() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let plan = repo.path().join(".git/large-plan.md");
    std::fs::write(
        &plan,
        "Implement the fixture and fix the observed verification failure.",
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".git/fixture-skills/review/SKILL.md"),
        "S".repeat(16 * 1024 * 1024),
    )
    .unwrap();
    let output = recovery_command(repo.path(), &fixture, &plan)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let artifacts = run_artifacts(repo.path());
    for round in [1, 2] {
        let checkpoint = artifacts.join(format!("review-checkpoints/{round}"));
        for role in ["correctness", "maintainability"] {
            let prompt = std::fs::read(checkpoint.join(format!("{role}.prompt.txt"))).unwrap();
            assert!(prompt.len() <= 256 * 1024);
        }
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(checkpoint.join("snapshot/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(
            manifest["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "skill" && entry["bytes"] == 16 * 1024 * 1024)
        );
        assert!(
            manifest["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "plan" && entry["bytes"].as_u64().unwrap() > 0)
        );
    }
}
