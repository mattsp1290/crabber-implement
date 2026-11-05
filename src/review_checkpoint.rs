use crate::{
    config::RunConfig,
    durable, git,
    review::{ReviewReport, ReviewVerdict, parse_report},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const VERSION: u32 = 3;
pub const ROLES: [(&str, &str, &str); 2] = [
    (
        "correctness",
        "Correctness",
        "Check correctness, plan completeness, failures, and observable behavior.",
    ),
    (
        "maintainability",
        "Maintainability",
        "Independently check edge cases, hidden assumptions, and long-term maintainability.",
    ),
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPhase {
    ReviewPending,
    ReviewReportsReady,
    FixIntentRecorded,
    FixCommitted,
    Completed,
}

pub fn can_resume(phase: CheckpointPhase) -> bool {
    matches!(
        phase,
        CheckpointPhase::ReviewPending | CheckpointPhase::ReviewReportsReady
    )
}

pub fn review_round(first: u32, occurrence: u32, budget: u32) -> Option<u32> {
    occurrence
        .checked_sub(1)?
        .checked_add(first)
        .filter(|round| *round > 0 && *round <= budget)
}

pub fn reports_ready(valid: [bool; 2]) -> bool {
    valid.into_iter().all(|v| v)
}

#[derive(Clone, Copy)]
pub enum PhaseEvent {
    Reports([bool; 2]),
    Fix([bool; 2]),
    Commit,
    Complete([bool; 2], bool),
}

pub fn next_phase(phase: CheckpointPhase, event: PhaseEvent) -> Option<CheckpointPhase> {
    use CheckpointPhase::*;
    match event {
        PhaseEvent::Reports(valid) if can_resume(phase) && reports_ready(valid) => {
            Some(ReviewReportsReady)
        }
        PhaseEvent::Fix(valid) if phase == ReviewReportsReady && reports_ready(valid) => {
            Some(FixIntentRecorded)
        }
        PhaseEvent::Commit if phase == FixIntentRecorded => Some(FixCommitted),
        PhaseEvent::Complete(valid, approved)
            if phase == ReviewReportsReady && reports_ready(valid) && approved =>
        {
            Some(Completed)
        }
        _ => None,
    }
}

pub fn role_start(phase: CheckpointPhase, same_inputs: bool, valid_report: bool) -> Option<bool> {
    if !can_resume(phase)
        || !same_inputs
        || (phase == CheckpointPhase::ReviewReportsReady && !valid_report)
    {
        return None;
    }
    Some(!valid_report)
}

pub fn identity_matches(saved: &ReviewInputs, current: &ReviewInputs) -> bool {
    saved == current
}

pub fn session_identity(name: &str) -> Option<(u32, usize, u32)> {
    let parts: Vec<_> = name.strip_prefix("review-")?.split('-').collect();
    if parts.len() != 4 || parts[2] != "attempt" {
        return None;
    }
    let round = parts[0].parse::<u32>().ok().filter(|r| *r > 0)?;
    let role = ROLES.iter().position(|r| r.0 == parts[1])?;
    let attempt = parts[3].parse::<u32>().ok().filter(|r| *r > 0)?;
    Some((round, role, attempt))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewInputs {
    pub repo: PathBuf,
    pub git_dir: PathBuf,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub plan: String,
    pub skill: String,
    pub model: String,
    pub provider_profile: String,
    pub protocol: String,
    pub reasoning_effort: String,
    pub endpoint: String,
    pub turns: usize,
    pub budget: u32,
    pub executable: String,
    pub prompts: [String; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<crate::review_inputs::SnapshotIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl ReviewInputs {
    pub fn current(config: &RunConfig, prompts: [String; 2]) -> Result<Self> {
        git::require_clean(&config.repo)?;
        Ok(Self {
            repo: config.repo.clone(),
            git_dir: PathBuf::from(git::output(
                &config.repo,
                &["rev-parse", "--absolute-git-dir"],
            )?)
            .canonicalize()?,
            branch: git::output(&config.repo, &["branch", "--show-current"])?,
            base: git::output(
                &config.repo,
                &["rev-parse", &format!("{}^{{commit}}", config.base)],
            )?,
            head: git::output(&config.repo, &["rev-parse", "HEAD"])?,
            plan: durable::digest(config.plan.text.as_bytes()),
            skill: durable::digest(config.review_skill.as_bytes()),
            model: config.model.clone(),
            provider_profile: crate::config::PROVIDER_PROFILE.into(),
            protocol: "responses".into(),
            reasoning_effort: config.reasoning_effort.as_str().into(),
            endpoint: durable::digest(config.base_url.as_bytes()),
            turns: config.max_turns.get(),
            budget: config.max_reviews,
            executable: config.executable_digest.clone(),
            prompts,
            snapshot: None,
            origin: config
                .recovery_origin
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map(|bytes| durable::digest(&bytes)),
        })
    }
    pub fn digest(&self) -> Result<String> {
        Ok(durable::digest(&serde_json::to_vec(self)?))
    }
    pub fn verify(&self, config: &RunConfig) -> Result<()> {
        ensure!(
            identity_matches(self, &{
                let mut current = Self::current(config, self.prompts.clone())?;
                current.snapshot = self.snapshot.clone();
                current
            }),
            "review inputs changed; restore the original revision, plan, skill, model, executable and budget settings"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCheckpoint {
    pub version: u32,
    pub workflow_id: String,
    pub implementation_commit: String,
    pub round: u32,
    pub generation: u64,
    pub phase: CheckpointPhase,
    pub inputs: ReviewInputs,
    pub attempts: [u32; 2],
    pub sessions: [Option<String>; 2],
    pub reused: [bool; 2],
    pub staging_generation: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleCheckpoint {
    version: u32,
    inputs: String,
    role: String,
    attempt: u32,
    report_digest: String,
    report: ReviewReport,
}

impl ReviewCheckpoint {
    pub fn create(
        config: &RunConfig,
        round: u32,
        implementation_commit: String,
        snapshot: &crate::review_inputs::ReviewSnapshot,
    ) -> Result<Self> {
        ensure!(
            review_round(round, 1, config.max_reviews).is_some(),
            "invalid review round"
        );
        snapshot.verify()?;
        let rendered = snapshot.render()?;
        let prompts = rendered.prompts;
        let mut inputs = ReviewInputs::current(
            config,
            prompts.each_ref().map(|p| durable::digest(p.as_bytes())),
        )?;
        ensure!(
            snapshot.manifest().head == inputs.head
                && snapshot.manifest().branch == inputs.branch
                && snapshot.manifest().base == inputs.base
                && snapshot.manifest().round == round,
            "snapshot revision differs from checkpoint inputs"
        );
        inputs.snapshot = Some(snapshot.identity().clone());
        let (attempts, sessions, reused) = fresh_roles();
        let state = Self {
            version: VERSION,
            workflow_id: config.run_id.clone(),
            implementation_commit,
            round,
            generation: config.generation,
            phase: CheckpointPhase::ReviewPending,
            inputs,
            attempts,
            sessions,
            reused,
            staging_generation: None,
        };
        let dir = state.directory(&config.artifacts);
        durable::directory(&dir)?;
        for (i, prompt) in prompts.iter().enumerate() {
            crate::review_inputs::validate_prompt(prompt)?;
            durable::bytes(
                &dir.join(format!("{}.prompt.txt", ROLES[i].0)),
                prompt.as_bytes(),
                false,
            )?;
        }
        durable::write_json(&dir.join("input-metrics.json"), &rendered.metrics, false)?;
        durable::write_json(&dir.join("inputs.json"), &state.inputs, false)?;
        state.persist(&config.artifacts)?;
        Ok(state)
    }

    pub fn load(root: &Path) -> Result<Self> {
        durable::directory_existing(root)?;
        let wire: serde_json::Value =
            durable::json(&root.join("review-state.json"), durable::METADATA_LIMIT)?;
        ensure!(
            wire["version"] == VERSION,
            "unsupported review checkpoint version; only Codex native-subagent checkpoints can be resumed or refreshed; start a new run"
        );
        let state: Self =
            serde_json::from_value(wire).context("invalid Codex review checkpoint")?;
        ensure!(
            uuid::Uuid::parse_str(&state.workflow_id)?.to_string()
                == root
                    .file_name()
                    .context("missing workflow ID")?
                    .to_string_lossy(),
            "checkpoint workflow identity mismatch"
        );
        ensure!(
            review_round(state.round, 1, state.inputs.budget).is_some() && state.generation > 0,
            "invalid review checkpoint counters"
        );
        ensure!(
            can_resume(state.phase),
            "only an incomplete review can resume; implementation, fix intent and completed workflows cannot be replayed"
        );
        ensure!(
            state.inputs.snapshot.is_some()
                && state.inputs.provider_profile == crate::config::PROVIDER_PROFILE
                && state.inputs.protocol == "responses"
                && !state.inputs.model.trim().is_empty()
                && state
                    .inputs
                    .reasoning_effort
                    .parse::<crabber::ReasoningEffort>()
                    .is_ok(),
            "checkpoint snapshot protocol mismatch"
        );
        if let Some(identity) = &state.inputs.snapshot {
            crate::review_inputs::ReviewSnapshot::open(
                &crate::review_inputs::snapshot_directory(root, state.round),
                identity,
            )?;
        }
        let anchor: serde_json::Value =
            durable::json(&root.join("implementation.json"), durable::METADATA_LIMIT)?;
        ensure!(
            anchor["commit"].as_str() == Some(&state.implementation_commit)
                && anchor["plan"].as_str() == Some(&state.inputs.plan),
            "implementation anchor record mismatch"
        );
        let dir = state.directory(root);
        durable::directory_existing(&dir)?;
        let saved: ReviewInputs = durable::json(&dir.join("inputs.json"), durable::METADATA_LIMIT)?;
        ensure!(saved == state.inputs, "review input record mismatch");
        for i in 0..2 {
            let _ = state.prompt(root, i)?;
            ensure!(
                state.sessions[i].is_some() == (state.attempts[i] > 0),
                "review session counter mismatch"
            );
            if let Some(name) = &state.sessions[i] {
                ensure!(
                    *name == state.session_name(i),
                    "invalid reviewer attempt identity"
                );
            }
            let _ = state.report(root, i)?;
        }
        if state.phase == CheckpointPhase::ReviewReportsReady {
            ensure!(
                reports_ready([
                    state.report(root, 0)?.is_some(),
                    state.report(root, 1)?.is_some()
                ]),
                "completed review reports are missing"
            );
        }
        Ok(state)
    }

    pub fn directory(&self, root: &Path) -> PathBuf {
        root.join("review-checkpoints").join(self.round.to_string())
    }
    pub fn persist(&self, root: &Path) -> Result<()> {
        durable::write_json(&root.join("review-state.json"), self, true)
    }
    pub fn session_name(&self, role: usize) -> String {
        format!(
            "review-{}-{}-attempt-{}",
            self.round, ROLES[role].0, self.attempts[role]
        )
    }

    pub fn prompt(&self, root: &Path, role: usize) -> Result<String> {
        let bytes = durable::read(
            &self
                .directory(root)
                .join(format!("{}.prompt.txt", ROLES[role].0)),
            crate::review_inputs::PROMPT_LIMIT,
        )?;
        ensure!(
            durable::digest(&bytes) == self.inputs.prompts[role],
            "review prompt digest mismatch"
        );
        let prompt = String::from_utf8(bytes)?;
        crate::review_inputs::validate_prompt(&prompt)?;
        Ok(prompt)
    }

    pub fn report(&self, root: &Path, role: usize) -> Result<Option<ReviewReport>> {
        let path = self
            .directory(root)
            .join(format!("{}.report.json", ROLES[role].0));
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        let saved: RoleCheckpoint = durable::json(&path, durable::REPORT_LIMIT)?;
        ensure!(
            saved.version == self.version
                && saved.role == ROLES[role].0
                && saved.inputs == self.inputs.digest()?
                && saved.attempt > 0
                && saved.attempt == self.attempts[role],
            "review report identity mismatch"
        );
        let bytes = serde_json::to_vec(&saved.report)?;
        ensure!(
            durable::digest(&bytes) == saved.report_digest,
            "review report digest mismatch"
        );
        let report = parse_report(std::str::from_utf8(&bytes)?)?;
        // A killed publisher may have linked the complete file before syncing its parent.
        durable::sync_dir(&self.directory(root))?;
        Ok(Some(report))
    }

    pub fn publish_role(
        &self,
        config: &RunConfig,
        role: usize,
        text: &str,
        snapshot: &crate::review_inputs::ReviewSnapshot,
    ) -> Result<ReviewReport> {
        ensure!(
            self.inputs.snapshot.as_ref() == Some(snapshot.identity()),
            "report snapshot identity mismatch"
        );
        snapshot.verify()?;
        crate::review_inputs::ReviewSnapshot::open(
            &crate::review_inputs::snapshot_directory(&config.artifacts, self.round),
            snapshot.identity(),
        )?;
        ensure!(
            snapshot.manifest().head == self.inputs.head
                && snapshot.manifest().branch == self.inputs.branch
                && snapshot.manifest().base == self.inputs.base
                && snapshot.manifest().round == self.round,
            "report snapshot revision mismatch"
        );
        self.inputs.verify(config)?;
        let report = parse_report(text)?;
        let saved = RoleCheckpoint {
            version: VERSION,
            inputs: self.inputs.digest()?,
            role: ROLES[role].0.into(),
            attempt: self.attempts[role],
            report_digest: durable::digest(&serde_json::to_vec(&report)?),
            report: report.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&saved)?;
        ensure!(
            bytes.len() <= durable::REPORT_LIMIT,
            "review report exceeds size limit"
        );
        durable::bytes(
            &self
                .directory(&config.artifacts)
                .join(format!("{}.report.json", ROLES[role].0)),
            &bytes,
            false,
        )?;
        durable::barrier("role_report")?;
        Ok(report)
    }

    pub fn reserve_role(&mut self, root: &Path, role: usize) -> Result<String> {
        ensure!(
            can_resume(self.phase),
            "review role cannot start after fix intent"
        );
        self.attempts[role] = self.attempts[role]
            .checked_add(1)
            .context("review attempt overflow")?;
        let name = self.session_name(role);
        self.sessions[role] = Some(name.clone());
        self.reused[role] = false;
        self.persist(root)?;
        Ok(name)
    }

    pub fn needs_credentials(&self, root: &Path) -> Result<bool> {
        let reports = [self.report(root, 0)?, self.report(root, 1)?];
        if reports.iter().any(Option::is_none) {
            return Ok(true);
        }
        let reports: Vec<_> = reports.iter().flatten().collect();
        Ok(self.round < self.inputs.budget
            && !reports
                .iter()
                .any(|r| r.verdict == ReviewVerdict::NeedsDiscussion)
            && reports.iter().any(|r| r.verdict != ReviewVerdict::Approve))
    }
}

#[cfg(test)]
pub fn begin_execution(config: &mut RunConfig) -> Result<()> {
    begin_execution_cancellable(config, &dmt::CancellationToken::new())
}
pub fn begin_execution_cancellable(
    config: &mut RunConfig,
    cancel: &dmt::CancellationToken,
) -> Result<()> {
    if let Some(source) = config.refresh_source.take() {
        return source.initialize(config, cancel);
    }
    if let Some(state) = &mut config.resume {
        let archive = config
            .artifacts
            .join("executions")
            .join(state.generation.to_string());
        durable::directory(&archive)?;
        let metadata: Option<serde_json::Value> =
            if config.artifacts.join("run.json").try_exists()? {
                Some(durable::json(
                    &config.artifacts.join("run.json"),
                    durable::METADATA_LIMIT,
                )?)
            } else {
                None
            };
        for name in ["result.json", "run.json", "telemetry.json", "graph.json"] {
            let source = config.artifacts.join(name);
            if !source.try_exists()? {
                continue;
            }
            let data = durable::read(&source, durable::METADATA_LIMIT)?;
            let json: serde_json::Value = serde_json::from_slice(&data)?;
            let generation = if name == "graph.json" {
                metadata
                    .as_ref()
                    .and_then(|m| m["execution_generation"].as_u64())
            } else {
                json["execution_generation"].as_u64()
            };
            if generation != Some(state.generation) {
                continue;
            }
            if name == "telemetry.json" {
                config.predecessor_trace = json["trace_id"]
                    .as_str()
                    .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_owned);
            }
            let target = archive.join(name);
            if target.try_exists()? {
                ensure!(
                    durable::read(&target, durable::METADATA_LIMIT)? == data,
                    "archived execution differs"
                );
            } else {
                durable::bytes(&target, &data, false)?;
            }
        }
        state.generation = config.generation;
        state.reused = [
            state.report(&config.artifacts, 0)?.is_some(),
            state.report(&config.artifacts, 1)?.is_some(),
        ];
        state.persist(&config.artifacts)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOrigin {
    pub version: u32,
    pub parent_workflow_id: String,
    pub root_workflow_id: String,
    pub source_checkpoint: String,
    pub source_round: u32,
    pub implementation_commit: String,
    pub reviewed_commit: String,
    pub source_executable: String,
    pub child_executable: String,
    pub inherited: crate::review_inputs::SnapshotIdentity,
}
impl RecoveryOrigin {
    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = root.join("recovery-origin.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => (),
        }
        let origin: Self = durable::json(&path, durable::METADATA_LIMIT)?;
        ensure!(
            origin.version == 1 && origin.source_round > 0 && origin.source_round <= 100,
            "invalid recovery origin"
        );
        for id in [&origin.parent_workflow_id, &origin.root_workflow_id] {
            ensure!(
                uuid::Uuid::parse_str(id)?.to_string() == *id,
                "invalid recovery lineage"
            );
        }
        crate::review_inputs::ReviewSnapshot::open(
            &root.join("inherited-evidence"),
            &origin.inherited,
        )?;
        Ok(Some(origin))
    }
}

pub fn refresh_round(source_round: u32, budget: u32) -> Option<u32> {
    review_round(source_round, 1, budget)
}
pub fn refresh_eligible(
    phase: CheckpointPhase,
    same_identity: bool,
    source_verified: bool,
    origin_published: bool,
    snapshot_verified: bool,
) -> bool {
    can_resume(phase) && same_identity && source_verified && origin_published && snapshot_verified
}

pub struct RefreshSource {
    root: PathBuf,
    state: ReviewCheckpoint,
    records: Vec<(PathBuf, Option<String>)>,
    anchor: serde_json::Value,
    origin: Option<RecoveryOrigin>,
}
impl RefreshSource {
    pub fn load(config: &RunConfig, root: &Path) -> Result<Self> {
        let state = ReviewCheckpoint::load(root)?;
        crate::review_inputs::check_changed_paths(config)?;
        let mut current = ReviewInputs::current(config, state.inputs.prompts.clone())?;
        current.executable = state.inputs.executable.clone();
        current.snapshot = state.inputs.snapshot.clone();
        current.origin = state.inputs.origin.clone();
        ensure!(
            identity_matches(&state.inputs, &current),
            "refresh source identity changed; restore revision, branch, base, plan, skill, model and budgets"
        );
        ensure!(
            refresh_round(state.round, config.max_reviews).is_some(),
            "invalid refresh global round"
        );
        ensure!(
            state.inputs.branch != config.base,
            "refresh requires a feature branch"
        );
        git::output(
            &config.repo,
            &[
                "merge-base",
                "--is-ancestor",
                &state.implementation_commit,
                "HEAD",
            ],
        )
        .context("implementation anchor is not an ancestor")?;
        let anchor: serde_json::Value =
            durable::json(&root.join("implementation.json"), durable::METADATA_LIMIT)?;
        ensure!(
            anchor["commit"].as_str() == Some(&state.implementation_commit)
                && anchor["plan"].as_str() == Some(&state.inputs.plan),
            "invalid original implementation anchor"
        );
        let origin = RecoveryOrigin::load(root)?;
        let origin_digest = origin
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| durable::digest(&bytes));
        ensure!(
            state.inputs.origin == origin_digest,
            "refresh source recovery origin differs from saved identity"
        );
        if let Some(origin) = &origin {
            ensure!(
                origin.implementation_commit == state.implementation_commit
                    && origin.child_executable == state.inputs.executable
                    && origin.source_round <= state.round
                    && origin.parent_workflow_id != state.workflow_id,
                "recovery origin does not match source workflow"
            );
            git::output(
                &config.repo,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &origin.reviewed_commit,
                    &state.inputs.head,
                ],
            )
            .context("recovery revision is not an ancestor of source HEAD")?;
        }
        let mut paths = vec![
            root.join("review-state.json"),
            root.join("implementation.json"),
            state.directory(root).join("inputs.json"),
        ];
        if origin.is_some() {
            paths.push(root.join("recovery-origin.json"));
        }
        for role in ROLES {
            paths.push(state.directory(root).join(format!("{}.prompt.txt", role.0)));
            let path = state
                .directory(root)
                .join(format!("{}.report.json", role.0));
            paths.push(path);
        }
        let mut records = Vec::new();
        for path in paths {
            let digest = match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
                Ok(_) => Some(durable::digest(&durable::read(
                    &path,
                    durable::PROMPT_LIMIT,
                )?)),
            };
            records.push((path, digest));
        }
        Ok(Self {
            root: root.to_owned(),
            state,
            records,
            anchor,
            origin,
        })
    }
    fn verify(&self, config: &RunConfig) -> Result<()> {
        if let Some(identity) = &self.state.inputs.snapshot {
            crate::review_inputs::ReviewSnapshot::open(
                &crate::review_inputs::snapshot_directory(&self.root, self.state.round),
                identity,
            )?;
        }
        if let Some(origin) = &self.origin {
            crate::review_inputs::ReviewSnapshot::open(
                &self.root.join("inherited-evidence"),
                &origin.inherited,
            )?;
        }
        for (path, digest) in &self.records {
            let current = match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
                Ok(_) => Some(durable::digest(&durable::read(
                    path,
                    durable::PROMPT_LIMIT,
                )?)),
            };
            ensure!(
                current == *digest,
                "refresh source record changed during import"
            );
        }
        let mut current = ReviewInputs::current(config, self.state.inputs.prompts.clone())?;
        current.executable = self.state.inputs.executable.clone();
        current.snapshot = self.state.inputs.snapshot.clone();
        current.origin = self.state.inputs.origin.clone();
        ensure!(
            identity_matches(&self.state.inputs, &current),
            "refresh source revision or profile changed"
        );
        Ok(())
    }
    fn initialize(self, config: &mut RunConfig, cancel: &dmt::CancellationToken) -> Result<()> {
        let deadline = std::time::Instant::now() + config.session_timeout;
        let active = Some((cancel, deadline));
        use crate::review_inputs::{
            ARTIFACT_LIMIT, ReviewSnapshot, SnapshotBuilder, add_local_evidence,
        };
        self.verify(config)?;
        durable::directory(&config.artifacts)?;
        let mut inherited = SnapshotBuilder::new(
            config.artifacts.join("inherited-evidence"),
            self.state.round,
            self.state.inputs.head.clone(),
        )?;
        if let Some(origin) = &self.origin {
            let snapshot =
                ReviewSnapshot::open(&self.root.join("inherited-evidence"), &origin.inherited)?;
            for entry in &snapshot.manifest().entries {
                crate::review_inputs::check_active(active)?;
                inherited.copy(entry.clone(), &snapshot.bytes(entry)?)?;
            }
        }
        if let Some(identity) = &self.state.inputs.snapshot {
            let snapshot = ReviewSnapshot::open(
                &crate::review_inputs::snapshot_directory(&self.root, self.state.round),
                identity,
            )?;
            for entry in snapshot
                .manifest()
                .entries
                .iter()
                .filter(|e| e.kind == "evidence")
            {
                inherited.copy(entry.clone(), &snapshot.bytes(entry)?)?;
            }
        }
        let sources = add_local_evidence(
            &mut inherited,
            config,
            &self.root,
            &self.state.workflow_id,
            active,
        )?;
        for role in ROLES {
            let path = self
                .state
                .directory(&self.root)
                .join(format!("{}.report.json", role.0));
            if path.try_exists()? {
                inherited.add_origin(
                    &self.state.workflow_id,
                    "evidence",
                    &format!("review-{}-{}.report.json", self.state.round, role.0),
                    "source-review-claim",
                    self.state.round,
                    &durable::read(&path, durable::REPORT_LIMIT)?,
                )?;
            }
        }
        let inherited = inherited.finish()?;
        let origin = RecoveryOrigin {
            version: 1,
            parent_workflow_id: self.state.workflow_id.clone(),
            root_workflow_id: self.origin.as_ref().map_or_else(
                || self.state.workflow_id.clone(),
                |o| o.root_workflow_id.clone(),
            ),
            source_checkpoint: self.records[0]
                .1
                .clone()
                .context("missing source checkpoint digest")?,
            source_round: self.state.round,
            implementation_commit: self.state.implementation_commit.clone(),
            reviewed_commit: self.state.inputs.head.clone(),
            source_executable: self.state.inputs.executable.clone(),
            child_executable: config.executable_digest.clone(),
            inherited: inherited.identity().clone(),
        };
        durable::write_json(
            &config.artifacts.join("recovery-origin.json"),
            &origin,
            false,
        )?;
        durable::barrier("origin")?;
        durable::write_json(
            &config.artifacts.join("implementation.json"),
            &self.anchor,
            false,
        )?;
        durable::bytes(
            &config.artifacts.join("plan.md"),
            config.plan.text.as_bytes(),
            false,
        )?;
        config.recovery_origin = Some(origin);
        let round = refresh_round(self.state.round, config.max_reviews)
            .context("refresh round exceeds budget")?;
        let snapshot = ReviewSnapshot::capture(config, round, cancel)?;
        self.verify(config)?;
        for (path, digest) in sources {
            crate::review_inputs::check_active(active)?;
            ensure!(
                durable::digest(&durable::read(&path, ARTIFACT_LIMIT)?) == digest,
                "source evidence changed during import"
            );
        }
        let eligible = refresh_eligible(
            self.state.phase,
            true,
            true,
            RecoveryOrigin::load(&config.artifacts)?.is_some(),
            snapshot.verify().is_ok(),
        );
        ensure!(eligible, "refresh inputs are not ready");
        let state = ReviewCheckpoint::create(
            config,
            round,
            self.state.implementation_commit.clone(),
            &snapshot,
        )?;
        durable::barrier("ready")?;
        config.resume = Some(state);
        Ok(())
    }
}

pub fn fresh_roles() -> ([u32; 2], [Option<String>; 2], [bool; 2]) {
    ([0, 0], [None, None], [false, false])
}
pub fn can_fix(round: u32, budget: u32) -> bool {
    round > 0 && round < budget
}
