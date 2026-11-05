use super::*;

fn builder(root: &Path) -> SnapshotBuilder {
    SnapshotBuilder::new(
        root.canonicalize().unwrap().join("snapshot"),
        1,
        "head".into(),
    )
    .unwrap()
}

#[test]
fn storage_limits_fail_without_publishing_a_snapshot_manifest() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = builder(root.path());
    let block = vec![b'x'; ARTIFACT_LIMIT];
    for index in 0..8 {
        snapshot
            .add_origin(
                "workflow",
                "evidence",
                &format!("log-{index}"),
                "implement",
                0,
                &block,
            )
            .unwrap();
    }
    let error = snapshot
        .add_origin("workflow", "evidence", "overflow", "implement", 0, b"x")
        .unwrap_err();
    assert!(error.to_string().contains("128 MiB"));
    assert!(!root.path().join("snapshot/manifest.json").exists());
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = builder(root.path());
    let oversized = vec![b'x'; ARTIFACT_LIMIT + 1];
    assert!(
        snapshot
            .add_origin(
                "workflow",
                "evidence",
                "oversized",
                "implement",
                0,
                &oversized
            )
            .is_err()
    );
    assert_eq!(
        fs::read_dir(root.path().join("snapshot")).unwrap().count(),
        0
    );
}

#[test]
fn manifest_bytes_and_entry_counts_fail_before_readiness() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = builder(root.path());
    let error = snapshot
        .add_origin(
            "workflow",
            "evidence",
            &"x".repeat(durable::METADATA_LIMIT),
            "implement",
            0,
            b"small",
        )
        .unwrap_err();
    assert!(error.to_string().contains("manifest exceeds"));
    assert!(!root.path().join("snapshot/manifest.json").exists());
    let manifest = Manifest {
        version: 1,
        round: 1,
        head: "head".into(),
        branch: "feature".into(),
        base: "base".into(),
        entries: (0..=ENTRY_LIMIT)
            .map(|index| Artifact {
                id: format!("{index:064x}"),
                name: String::new(),
                bytes: 0,
                digest: durable::digest(b""),
                kind: String::new(),
                workflow: String::new(),
                stage: String::new(),
                round: 0,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&manifest).unwrap();
    assert!(bytes.len() <= durable::METADATA_LIMIT);
    let path = root.path().canonicalize().unwrap().join("too-many");
    durable::directory(&path).unwrap();
    durable::bytes(&path.join("manifest.json"), &bytes, false).unwrap();
    let error = match ReviewSnapshot::open(
        &path,
        &SnapshotIdentity {
            manifest: durable::digest(&bytes),
            policy: 1,
        },
    ) {
        Ok(_) => panic!("too many entries became ready"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("excessive snapshot manifest"));
}

fn committed_change(config: &RunConfig, name: &str, bytes: &[u8]) {
    git::output(&config.repo, &["switch", "-c", "feature"]).unwrap();
    fs::write(config.repo.join(name), bytes).unwrap();
    git::output(&config.repo, &["add", "--", name]).unwrap();
    git::output(&config.repo, &["commit", "-m", "change"]).unwrap();
}

#[test]
fn credential_paths_and_cancellation_fail_before_snapshot_content_publication() {
    let (_root, config) = crate::review_checkpoint::tests::fixture();
    committed_change(&config, ".env", b"PRIVATE_CREDENTIAL_CAPTURE_SENTINEL");
    let error = match ReviewSnapshot::capture(&config, 1, &dmt::CancellationToken::new()) {
        Ok(_) => panic!("credential path captured"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("restriction"));
    assert!(!snapshot_directory(&config.artifacts, 1).exists());
    let (_root, config) = crate::review_checkpoint::tests::fixture();
    committed_change(&config, "source.txt", b"observable change");
    let cancel = dmt::CancellationToken::new();
    cancel.cancel();
    assert!(ReviewSnapshot::capture(&config, 1, &cancel).is_err());
    assert!(!snapshot_directory(&config.artifacts, 1).exists());
}

#[test]
fn capture_preserves_large_inputs_and_committed_file_classifications() {
    let (_root, mut config) = crate::review_checkpoint::tests::fixture();
    git::output(&config.repo, &["switch", "-c", "feature"]).unwrap();
    fs::write(
        config.repo.join("large.txt"),
        "large source line\n".repeat(100_000),
    )
    .unwrap();
    fs::write(config.repo.join("binary.bin"), [0, 255, 128, 1]).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), b"EXTERNAL_SYMLINK_CONTENT_SENTINEL").unwrap();
    std::os::unix::fs::symlink(outside.path(), config.repo.join("link")).unwrap();
    git::output(&config.repo, &["rm", "README.md"]).unwrap();
    git::output(&config.repo, &["add", "."]).unwrap();
    git::output(&config.repo, &["commit", "-m", "large mixed change"]).unwrap();
    let plan = config.repo.join(".git/large-plan.md");
    fs::write(&plan, "P".repeat(1_999_900)).unwrap();
    config.plan = crate::config::PlanBundle::load(&plan).unwrap();
    assert!((1_999_000..=2_000_000).contains(&config.plan.text.len()));
    config.review_skill = "S".repeat(ARTIFACT_LIMIT);
    let snapshot = ReviewSnapshot::capture(&config, 1, &dmt::CancellationToken::new()).unwrap();
    let artifact = |name: &str| {
        snapshot
            .manifest()
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
    };
    assert_eq!(artifact("README.md").kind, "deleted");
    assert_eq!(artifact("binary.bin").kind, "binary");
    assert_eq!(
        snapshot.bytes(artifact("binary.bin")).unwrap(),
        [0, 255, 128, 1]
    );
    assert_eq!(artifact("link").kind, "symlink");
    assert_eq!(
        snapshot.bytes(artifact("link")).unwrap(),
        outside.path().as_os_str().as_encoded_bytes()
    );
    assert!(artifact("changes.diff").bytes > 1_000_000);
    assert_eq!(
        snapshot.bytes(artifact("plan.md")).unwrap(),
        config.plan.text.as_bytes()
    );
    assert_eq!(artifact("review/SKILL.md").bytes, ARTIFACT_LIMIT);
    for prompt in snapshot.prompts().unwrap() {
        assert!(prompt.len() <= PROMPT_LIMIT);
        assert!(prompt.contains("omitted"));
        assert!(!prompt.contains("EXTERNAL_SYMLINK_CONTENT_SENTINEL"));
    }
}

#[test]
fn streaming_git_capture_refuses_an_oversized_committed_blob() {
    let (_root, config) = crate::review_checkpoint::tests::fixture();
    git::output(&config.repo, &["switch", "-c", "feature"]).unwrap();
    fs::File::create(config.repo.join("large.bin"))
        .unwrap()
        .set_len((ARTIFACT_LIMIT + 1) as u64)
        .unwrap();
    git::output(&config.repo, &["add", "large.bin"]).unwrap();
    git::output(&config.repo, &["commit", "-m", "oversized blob"]).unwrap();
    let error = match ReviewSnapshot::capture(&config, 1, &dmt::CancellationToken::new()) {
        Ok(_) => panic!("oversized committed blob was captured"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("Git input exceeds byte limit"),
        "{error:#}"
    );
    assert!(
        !snapshot_directory(&config.artifacts, 1)
            .join("manifest.json")
            .exists()
    );
    assert_eq!(
        fs::metadata(config.repo.join("large.bin")).unwrap().len(),
        (ARTIFACT_LIMIT + 1) as u64
    );
}
