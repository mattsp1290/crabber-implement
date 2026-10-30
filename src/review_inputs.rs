use crate::{config::RunConfig, durable, git, review_checkpoint::ROLES};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub const PROMPT_LIMIT: usize = 256 * 1024;
pub const EXCERPT_LIMIT: usize = 64 * 1024;
pub const ARTIFACT_LIMIT: usize = 16 * 1024 * 1024;
pub const TOTAL_LIMIT: usize = 128 * 1024 * 1024;
pub const ENTRY_LIMIT: usize = 4096;
pub const RESPONSE_LIMIT: usize = 32 * 1024;

pub fn validate_prompt(prompt: &str) -> Result<()> {
    ensure!(
        prompt.len() <= PROMPT_LIMIT,
        "final rendered review prompt exceeds 256 KiB"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotIdentity {
    pub manifest: String,
    pub policy: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub bytes: usize,
    pub digest: String,
    pub kind: String,
    pub workflow: String,
    pub stage: String,
    pub round: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub round: u32,
    pub head: String,
    pub branch: String,
    pub base: String,
    pub entries: Vec<Artifact>,
}

#[derive(Serialize)]
pub struct InputMetrics {
    pub prompt_bytes: [usize; 2],
    pub snapshot_bytes: usize,
    pub excerpt_bytes: usize,
    pub omitted_bytes: usize,
}
pub struct RenderedInputs {
    pub prompts: [String; 2],
    pub metrics: InputMetrics,
}

struct Inner {
    directory: fs::File,
    manifest: Manifest,
    identity: SnapshotIdentity,
    poisoned: AtomicBool,
}
#[derive(Clone)]
pub struct ReviewSnapshot(Arc<Inner>);

fn pin_directory(path: &Path) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    ensure!(path.is_absolute(), "snapshot directory must be absolute");
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut fd = open("/", flags, Mode::empty())?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => fd = openat(&fd, name, flags, Mode::empty())?,
            _ => anyhow::bail!("unsafe snapshot ancestry"),
        }
    }
    Ok(fd.into())
}

fn read_at(directory: &fs::File, name: &str, limit: usize) -> Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, openat};
    ensure!(
        !name.contains('/') && name != "." && name != "..",
        "invalid snapshot filename"
    );
    let fd = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = fs::File::from(fd);
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit as u64,
        "invalid snapshot file size or kind"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "snapshot file exceeds limit");
    Ok(bytes)
}

pub fn snapshot_directory(root: &Path, round: u32) -> PathBuf {
    root.join("review-checkpoints")
        .join(round.to_string())
        .join("snapshot")
}

impl ReviewSnapshot {
    pub fn open(path: &Path, identity: &SnapshotIdentity) -> Result<Self> {
        ensure!(identity.policy == 1, "unsupported review rendering policy");
        let directory = pin_directory(path)?;
        let bytes = read_at(&directory, "manifest.json", durable::METADATA_LIMIT)?;
        ensure!(
            durable::digest(&bytes) == identity.manifest,
            "snapshot manifest digest mismatch"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.version == 1 && manifest.entries.len() <= ENTRY_LIMIT,
            "unsupported or excessive snapshot manifest"
        );
        let mut ids = BTreeSet::new();
        let mut total = 0usize;
        for entry in &manifest.entries {
            ensure!(
                entry.id.len() == 64
                    && entry.id.bytes().all(|b| b.is_ascii_hexdigit())
                    && ids.insert(entry.id.clone()),
                "invalid artifact identity"
            );
            ensure!(
                entry.id
                    == durable::digest(&serde_json::to_vec(&(
                        &entry.workflow,
                        &entry.stage,
                        &entry.name,
                        &entry.digest
                    ))?),
                "artifact provenance mismatch"
            );
            ensure!(
                matches!(
                    entry.kind.as_str(),
                    "plan"
                        | "skill"
                        | "diff"
                        | "commits"
                        | "source"
                        | "binary"
                        | "deleted"
                        | "symlink"
                        | "gitlink"
                        | "evidence"
                ),
                "unknown artifact kind"
            );
            ensure!(
                entry.bytes <= ARTIFACT_LIMIT,
                "snapshot artifact exceeds limit"
            );
            total = total
                .checked_add(entry.bytes)
                .context("snapshot size overflow")?;
            ensure!(total <= TOTAL_LIMIT, "snapshot total exceeds limit");
        }
        let snapshot = Self(Arc::new(Inner {
            directory,
            manifest,
            identity: identity.clone(),
            poisoned: AtomicBool::new(false),
        }));
        snapshot.verify()?;
        Ok(snapshot)
    }
    pub fn identity(&self) -> &SnapshotIdentity {
        &self.0.identity
    }
    pub fn manifest(&self) -> &Manifest {
        &self.0.manifest
    }
    pub fn bytes(&self, artifact: &Artifact) -> Result<Vec<u8>> {
        let result = (|| {
            let bytes = read_at(&self.0.directory, &artifact.id, ARTIFACT_LIMIT)?;
            ensure!(
                bytes.len() == artifact.bytes && durable::digest(&bytes) == artifact.digest,
                "snapshot artifact integrity failure"
            );
            Ok(bytes)
        })();
        if result.is_err() {
            self.0.poisoned.store(true, Ordering::SeqCst);
        }
        result
    }
    pub fn range(&self, artifact: &Artifact, offset: usize, length: usize) -> Result<Vec<u8>> {
        use rustix::fs::{Mode, OFlags, openat};
        use sha2::{Digest, Sha256};
        let result = (|| {
            self.verify_manifest()?;
            let fd = openat(
                &self.0.directory,
                artifact.id.as_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let mut file = fs::File::from(fd);
            ensure!(
                file.metadata()?.is_file() && file.metadata()?.len() == artifact.bytes as u64,
                "snapshot artifact length changed"
            );
            let end = offset
                .checked_add(length)
                .context("artifact range overflow")?
                .min(artifact.bytes);
            ensure!(offset <= end, "artifact offset out of range");
            let mut selected = Vec::with_capacity(end - offset);
            let mut hash = Sha256::new();
            let mut total = 0usize;
            let mut buffer = [0u8; 16384];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let next = total.checked_add(count).context("artifact size overflow")?;
                ensure!(next <= artifact.bytes, "snapshot artifact grew during read");
                hash.update(&buffer[..count]);
                if total < end && next > offset {
                    selected.extend_from_slice(
                        &buffer[offset.saturating_sub(total)..(end - total).min(count)],
                    );
                }
                total = next;
            }
            ensure!(
                total == artifact.bytes && format!("{:x}", hash.finalize()) == artifact.digest,
                "snapshot artifact integrity failure"
            );
            self.verify_manifest()?;
            Ok(selected)
        })();
        if result.is_err() {
            self.0.poisoned.store(true, Ordering::SeqCst);
        }
        result
    }
    pub fn verify_manifest(&self) -> Result<()> {
        ensure!(
            !self.0.poisoned.load(Ordering::SeqCst),
            "snapshot previously failed integrity validation"
        );
        let result = (|| {
            let bytes = read_at(&self.0.directory, "manifest.json", durable::METADATA_LIMIT)?;
            ensure!(
                durable::digest(&bytes) == self.0.identity.manifest,
                "snapshot manifest changed"
            );
            Ok(())
        })();
        if result.is_err() {
            self.0.poisoned.store(true, Ordering::SeqCst);
        }
        result
    }
    pub fn verify(&self) -> Result<()> {
        self.verify_manifest()?;
        for entry in &self.0.manifest.entries {
            self.bytes(entry)?;
        }
        Ok(())
    }
    pub fn capture(
        config: &RunConfig,
        round: u32,
        cancel: &dmt::CancellationToken,
    ) -> Result<Self> {
        let deadline = Instant::now() + config.session_timeout;
        let head = git::output(&config.repo, &["rev-parse", "HEAD"])?;
        let branch = git::output(&config.repo, &["branch", "--show-current"])?;
        let resolved_base = git::output(
            &config.repo,
            &["rev-parse", &format!("{}^{{commit}}", config.base)],
        )?;
        let base = git::output(&config.repo, &["merge-base", &resolved_base, &head])?;
        let range = format!("{base}..{head}");
        let capture =
            |args: &[&str], cap| git::capture(&config.repo, args, cap, Some((cancel, deadline)));
        let names = changed_paths(config, &range, Some((cancel, deadline)))?;
        let mut builder = SnapshotBuilder::new(
            snapshot_directory(&config.artifacts, round),
            round,
            head.clone(),
        )?;
        builder.bind_revision(branch.clone(), resolved_base.clone());
        builder.add(
            config,
            "plan",
            "plan.md",
            "input",
            round,
            config.plan.text.as_bytes(),
        )?;
        builder.add(
            config,
            "skill",
            "review/SKILL.md",
            "input",
            round,
            config.review_skill.as_bytes(),
        )?;
        builder.add(
            config,
            "diff",
            "changes.diff",
            "git",
            round,
            &capture(
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    &range,
                ],
                ARTIFACT_LIMIT,
            )?,
        )?;
        builder.add(
            config,
            "commits",
            "commits.txt",
            "git",
            round,
            &capture(&["log", &range, "--oneline"], ARTIFACT_LIMIT)?,
        )?;
        for raw_name in names {
            check_active(Some((cancel, deadline)))?;
            let name = match std::str::from_utf8(&raw_name) {
                Ok(name) => name.to_owned(),
                Err(_) => format!(
                    "git-path-hex:{}",
                    raw_name
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                ),
            };
            let arguments = [
                std::ffi::OsString::from("ls-tree"),
                "-z".into(),
                head.clone().into(),
                "--".into(),
                std::ffi::OsString::from_vec(raw_name),
            ];
            let tree = git::capture(
                &config.repo,
                &arguments,
                durable::METADATA_LIMIT,
                Some((cancel, deadline)),
            )?;
            if tree.is_empty() {
                builder.add(config, "deleted", &name, "git", round, b"")?;
                continue;
            }
            let metadata_end = tree
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid Git tree record")?;
            let metadata = std::str::from_utf8(&tree[..metadata_end])?
                .split_whitespace()
                .collect::<Vec<_>>();
            ensure!(metadata.len() == 3, "invalid Git tree metadata");
            let body = if metadata[0] == "160000" {
                metadata[2].as_bytes().to_vec()
            } else {
                capture(&["cat-file", "blob", metadata[2]], ARTIFACT_LIMIT)?
            };
            let kind = match metadata[0] {
                "120000" => "symlink",
                "160000" => "gitlink",
                _ if std::str::from_utf8(&body).is_err() || body.contains(&0) => "binary",
                _ => "source",
            };
            builder.add(config, kind, &name, "git", round, &body)?;
        }
        if let Some(origin) = &config.recovery_origin {
            let inherited = Self::open(
                &config.artifacts.join("inherited-evidence"),
                &origin.inherited,
            )?;
            for artifact in &inherited.manifest().entries {
                check_active(Some((cancel, deadline)))?;
                builder.copy(artifact.clone(), &inherited.bytes(artifact)?)?;
            }
        }
        let sources = add_local_evidence(
            &mut builder,
            config,
            &config.artifacts,
            &config.run_id,
            Some((cancel, deadline)),
        )?;
        for (path, digest) in sources {
            check_active(Some((cancel, deadline)))?;
            ensure!(
                durable::digest(&durable::read(&path, ARTIFACT_LIMIT)?) == digest,
                "execution evidence changed during snapshot capture"
            );
        }
        ensure!(
            git::output(&config.repo, &["rev-parse", "HEAD"])? == head,
            "HEAD changed during snapshot capture"
        );
        ensure!(
            git::output(&config.repo, &["branch", "--show-current"])? == branch
                && git::output(
                    &config.repo,
                    &["rev-parse", &format!("{}^{{commit}}", config.base)]
                )? == resolved_base,
            "branch or base changed during capture"
        );
        git::require_clean(&config.repo)?;
        ensure!(
            !cancel.is_cancelled() && Instant::now() < deadline,
            "review input capture cancelled or expired"
        );
        builder.finish()
    }

    #[cfg(test)]
    pub fn prompts(&self) -> Result<[String; 2]> {
        Ok(self.render()?.prompts)
    }
    pub fn render(&self) -> Result<RenderedInputs> {
        let manifest = self.manifest();
        let mut context = format!(
            "HEAD {}\nSnapshot manifest {}. {} artifacts. Use review_artifact_catalog for the complete paginated inventory. Only IDs are accepted by review_artifact_read. Offsets and lengths are bytes. Every omitted range remains available through that tool. Inspect omitted evidence relevant to findings and verification. If required evidence cannot be inspected, return NEEDS_DISCUSSION. Model-authored logs are claims, not proof of passing tests; tool logs are unstructured evidence unless a verified schema establishes an outcome.\n",
            manifest.head,
            self.identity().manifest,
            manifest.entries.len()
        );
        let mut inventory_bytes = 0;
        let mut shown = 0;
        for entry in &manifest.entries {
            let line = format!("Artifact {}\n", serde_json::to_string(entry)?);
            if inventory_bytes + line.len() > 48 * 1024 {
                break;
            }
            context.push_str(&line);
            inventory_bytes += line.len();
            shown += 1;
        }
        if manifest.entries.len() > shown {
            context.push_str(&format!("Additional inventory entries omitted; use the catalog starting at cursor {shown}.\n"));
        }
        context.push_str("Verification status is unknown until inspected. Execution evidence is never automatically treated as a passing command.\n");
        let mut excerpt_bytes = 0;
        for kind in ["plan", "skill", "diff", "evidence"] {
            let mut remaining = EXCERPT_LIMIT / 4;
            for entry in manifest.entries.iter().filter(|e| e.kind == kind) {
                if remaining == 0 {
                    break;
                }
                let bytes = self.bytes(entry)?;
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                let mut end = text.len().min(remaining).min(16 * 1024);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                context.push_str(&format!("\nPartial {} excerpt, artifact {}, bytes 0..{} of {}. Remaining bytes {}..{} omitted.\n{}\n", kind, entry.id, end, bytes.len(), end, bytes.len(), &text[..end]));
                remaining -= end;
                excerpt_bytes += end;
            }
        }
        let render = |i: usize| -> Result<String> {
            let prompt = format!(
                "Review round {}. Perform your assigned independent review using the review skill in the snapshot. The host handles prerequisites, Git context, reviewer concurrency, directory creation, manifest, and writing your five files. You have readonly tools. Do not invoke other agents or write reports yourself. Your assigned display name is {}, slug {}, role {}.\n\nReturn ONLY a JSON object with keys verdict, summary, findings, positive_notes. verdict is APPROVE, REQUEST_CHANGES, or NEEDS_DISCUSSION. summary is a factual paragraph. findings is an array of objects with exactly severity, path, line, description, suggested_fix. severity is Critical, Important, or Suggestion. path is repository-relative, line is a positive integer. positive_notes is an array of strings referencing specific code. Return empty arrays when there are none. Never approve an unimplemented requirement or failed verification.\n\n{}",
                manifest.round, ROLES[i].1, ROLES[i].0, ROLES[i].2, context
            );
            validate_prompt(&prompt)?;
            Ok(prompt)
        };
        let prompts = [render(0)?, render(1)?];
        let snapshot_bytes = manifest.entries.iter().map(|e| e.bytes).sum::<usize>();
        let metrics = InputMetrics {
            prompt_bytes: prompts.each_ref().map(|p| p.len()),
            snapshot_bytes,
            excerpt_bytes,
            omitted_bytes: snapshot_bytes - excerpt_bytes,
        };
        Ok(RenderedInputs { prompts, metrics })
    }
}

pub struct SnapshotBuilder {
    path: PathBuf,
    manifest: Manifest,
    total: usize,
}
impl SnapshotBuilder {
    pub fn new(path: PathBuf, round: u32, head: String) -> Result<Self> {
        ensure!(!path.try_exists()?, "snapshot already exists");
        durable::directory(&path)?;
        Ok(Self {
            path,
            manifest: Manifest {
                version: 1,
                round,
                head,
                branch: String::new(),
                base: String::new(),
                entries: Vec::new(),
            },
            total: 0,
        })
    }
    pub fn bind_revision(&mut self, branch: String, base: String) {
        self.manifest.branch = branch;
        self.manifest.base = base;
    }
    pub fn add(
        &mut self,
        config: &RunConfig,
        kind: &str,
        name: &str,
        stage: &str,
        round: u32,
        bytes: &[u8],
    ) -> Result<()> {
        self.add_origin(&config.run_id, kind, name, stage, round, bytes)
    }
    pub fn add_origin(
        &mut self,
        workflow: &str,
        kind: &str,
        name: &str,
        stage: &str,
        round: u32,
        bytes: &[u8],
    ) -> Result<()> {
        let digest = durable::digest(bytes);
        let id = durable::digest(&serde_json::to_vec(&(workflow, stage, name, &digest))?);
        self.copy(
            Artifact {
                id,
                name: name.into(),
                bytes: bytes.len(),
                digest,
                kind: kind.into(),
                workflow: workflow.into(),
                stage: stage.into(),
                round,
            },
            bytes,
        )
    }
    pub fn copy(&mut self, artifact: Artifact, bytes: &[u8]) -> Result<()> {
        if let Some(existing) = self.manifest.entries.iter().find(|e| e.id == artifact.id) {
            ensure!(
                *existing == artifact,
                "conflicting inherited evidence identity"
            );
            return Ok(());
        }
        ensure!(
            bytes.len() <= ARTIFACT_LIMIT
                && bytes.len() == artifact.bytes
                && durable::digest(bytes) == artifact.digest,
            "artifact size or digest mismatch"
        );
        ensure!(
            self.manifest.entries.len() < ENTRY_LIMIT,
            "snapshot exceeds 4096 artifacts"
        );
        self.total = self
            .total
            .checked_add(bytes.len())
            .context("snapshot size overflow")?;
        ensure!(self.total <= TOTAL_LIMIT, "snapshot exceeds 128 MiB total");
        durable::bytes(&self.path.join(&artifact.id), bytes, false)?;
        self.manifest.entries.push(artifact);
        if self.manifest.entries.len() == 1
            && self.path.file_name().is_some_and(|name| name == "snapshot")
        {
            durable::barrier("snapshot_entry")?;
        }
        ensure!(
            serde_json::to_vec(&self.manifest)?.len() <= durable::METADATA_LIMIT,
            "snapshot manifest exceeds 1 MiB"
        );
        Ok(())
    }
    pub fn finish(mut self) -> Result<ReviewSnapshot> {
        self.manifest.entries.sort_by(|a, b| {
            (&a.kind, &a.workflow, &a.stage, &a.name).cmp(&(
                &b.kind,
                &b.workflow,
                &b.stage,
                &b.name,
            ))
        });
        let bytes = serde_json::to_vec(&self.manifest)?;
        ensure!(
            bytes.len() <= durable::METADATA_LIMIT,
            "snapshot manifest exceeds 1 MiB"
        );
        durable::bytes(&self.path.join("manifest.json"), &bytes, false)?;
        ReviewSnapshot::open(
            &self.path,
            &SnapshotIdentity {
                manifest: durable::digest(&bytes),
                policy: 1,
            },
        )
    }
}

pub fn add_local_evidence(
    builder: &mut SnapshotBuilder,
    _config: &RunConfig,
    root: &Path,
    workflow: &str,
    active: Option<(&dmt::CancellationToken, Instant)>,
) -> Result<Vec<(PathBuf, String)>> {
    let mut paths = Vec::new();
    for (index, entry) in fs::read_dir(root)?.enumerate() {
        ensure!(index < ENTRY_LIMIT, "too many workflow artifact entries");
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 evidence filename"))?;
        if (name.starts_with("implement.") || name.starts_with("fix-review-"))
            && (name.ends_with(".txt") || name.ends_with(".responses.jsonl"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut sources = Vec::new();
    let directory = pin_directory(root)?;
    for path in paths {
        check_active(active)?;
        let name = path.file_name().unwrap().to_str().unwrap();
        let stage = name.split('.').next().unwrap();
        let round = stage
            .strip_prefix("fix-review-")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let bytes = read_at(&directory, name, ARTIFACT_LIMIT)?;
        check_active(active)?;
        builder.add_origin(workflow, "evidence", name, stage, round, &bytes)?;
        sources.push((path, durable::digest(&bytes)));
    }
    Ok(sources)
}

pub fn check_active(active: Option<(&dmt::CancellationToken, Instant)>) -> Result<()> {
    if let Some((cancel, deadline)) = active {
        ensure!(!cancel.is_cancelled(), "review input capture cancelled");
        ensure!(
            Instant::now() < deadline,
            "review input capture deadline exceeded"
        );
    }
    Ok(())
}

fn changed_paths(
    config: &RunConfig,
    range: &str,
    active: Option<(&dmt::CancellationToken, Instant)>,
) -> Result<Vec<Vec<u8>>> {
    let bytes = git::capture(
        &config.repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            range,
        ],
        durable::METADATA_LIMIT,
        active,
    )?;
    let names = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    ensure!(names.len() <= ENTRY_LIMIT, "too many changed paths");
    ensure!(!names.is_empty(), "review has no changes against base");
    let deny = crate::agent::path_deny_policy(config)?;
    for name in &names {
        ensure!(
            !deny.matches(Path::new(std::ffi::OsStr::from_bytes(name))),
            "review input restriction forbids a changed credential path"
        );
    }
    Ok(names)
}
pub fn check_changed_paths(config: &RunConfig) -> Result<()> {
    let head = git::output(&config.repo, &["rev-parse", "HEAD"])?;
    let base = git::output(&config.repo, &["merge-base", &config.base, &head])?;
    changed_paths(config, &format!("{base}..{head}"), None)?;
    Ok(())
}

#[cfg(test)]
mod tests;
