use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use dmt::{
    EngineBuilder, EngineConfig, HandlerError, MemoryStore, NodeContext, NodeHandler, NodeInput,
    NodeKind, NodeOutcome, Outcome, RunStatus,
};
use flows_api::GraphDocument;
use flows_core::{Compiled, compile};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::review::ReviewVerdict;
#[cfg(test)]
use crate::review::parse_report;
use crate::review_checkpoint::{self as checkpoint, CheckpointPhase, ROLES, ReviewCheckpoint};
use crate::{agent, config::RunConfig, git};

const AUTONOMOUS_FIX_REVIEW: &str = include_str!("../skills/fix-review-autonomous/SKILL.md");

struct ReviewedRevision {
    round: u32,
    head: String,
    branch: String,
    directory: PathBuf,
}

struct Workflow {
    config: RunConfig,
    trace: tracing::Span,
    compiled: Compiled,
    reviewed: Mutex<Option<ReviewedRevision>>,
    error: Mutex<Option<String>>,
    writer: Mutex<()>,
    checkpoint: Mutex<Option<ReviewCheckpoint>>,
    first_round: u32,
}

#[derive(Serialize)]
struct RunMetadata {
    version: u32,
    run_id: String,
    repo: PathBuf,
    branch: String,
    model: String,
    heartbeat_at_ms: u128,
    execution_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_workflow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_mode: Option<&'static str>,
    review_round: u32,
}

impl RunMetadata {
    fn heartbeat(&mut self, path: &std::path::Path) -> Result<()> {
        self.heartbeat_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        agent::write_json_atomic(path, self)
    }
}

#[cfg(test)]
pub(crate) fn document(max_reviews: u32, timeout_seconds: u32) -> GraphDocument {
    document_from(max_reviews, timeout_seconds, 1, false)
}

pub(crate) fn document_from(
    max_reviews: u32,
    timeout_seconds: u32,
    first_round: u32,
    resumed: bool,
) -> GraphDocument {
    let task = |id: &str, labels: &[&str], max: u32| {
        json!({
            "id":id,"type":"shell","position":{"x":0,"y":0},
            "retry":{"max_attempts":1,"initial_backoff_seconds":1,"max_backoff_seconds":1},
            "config":{"argv":["crabber-implement-agent",id],"outcome_labels":labels,
              "outcome_from":"stdout_json","on_exhausted":"fail_run","max_occurrences":max,
              "timeout_seconds":timeout_seconds}
        })
    };
    let remaining = max_reviews - first_round + 1;
    let mut nodes = vec![
        task("review", &["approved", "changes"], remaining),
        task("fix-review", &["fixed"], remaining),
        json!({"id":"done","type":"end","position":{"x":0,"y":0},"config":{"status":"completed"}}),
    ];
    let mut edges = vec![
        json!({"from":"review","to":"done","label":"approved"}),
        json!({"from":"review","to":"fix-review","label":"changes"}),
        json!({"from":"fix-review","to":"review","label":"fixed"}),
    ];
    if resumed {
        nodes.insert(0, task("resume-review", &["ready"], 1));
        edges.insert(
            0,
            json!({"from":"resume-review","to":"review","label":"ready"}),
        );
    } else {
        nodes.insert(0, task("implement", &["implemented"], 1));
        edges.insert(
            0,
            json!({"from":"implement","to":"review","label":"implemented"}),
        );
    }
    serde_json::from_value(json!({
        "id":"crabber-implement","name":"Implement and review a plan","inputs":[],
        "nodes":nodes, "edges":edges
    }))
    .expect("static graph document")
}

pub(crate) fn occurrence_allowed(occurrence: u32, max: u32) -> bool {
    occurrence > 0 && occurrence <= max
}

pub async fn run(config: RunConfig) -> Result<()> {
    let timeout = u32::try_from(
        config
            .session_timeout
            .as_secs()
            .saturating_mul(2)
            .saturating_add(60),
    )
    .context("session timeout is too large")?;
    let first_round = config.resume.as_ref().map_or(1, |s| s.round);
    let document = document_from(
        config.max_reviews,
        timeout,
        first_round,
        config.resume.is_some(),
    );
    let compiled = compile(&document, &BTreeMap::new())
        .map_err(|errors| anyhow::anyhow!("workflow compile failed: {errors:?}"))?;
    fs::write(
        config.artifacts.join("graph.json"),
        serde_json::to_vec_pretty(&document)?,
    )?;
    println!("artifacts {}", config.artifacts.display());
    let branch = git::output(&config.repo, &["branch", "--show-current"])?;
    println!("branch {branch}");
    let metadata_path = config.artifacts.join("run.json");
    let mut metadata = RunMetadata {
        version: 2,
        run_id: config.run_id.clone(),
        repo: config.repo.clone(),
        branch,
        model: config.model.clone(),
        heartbeat_at_ms: 0,
        execution_generation: config.generation,
        parent_workflow_id: config
            .recovery_origin
            .as_ref()
            .map(|o| o.parent_workflow_id.clone()),
        recovery_mode: config.recovery_origin.as_ref().map(|_| "refresh"),
        review_round: first_round,
    };
    metadata.heartbeat(&metadata_path)?;
    let graph = compiled.graph.clone();
    let checkpoint = config.resume.clone();
    let handler = Arc::new(Workflow {
        trace: tracing::Span::current(),
        config,
        compiled,
        reviewed: Mutex::new(None),
        error: Mutex::new(None),
        writer: Mutex::new(()),
        checkpoint: Mutex::new(checkpoint),
        first_round,
    });
    let mut builder = EngineBuilder::default()
        .store(Arc::new(MemoryStore::new()))
        .graph(graph.clone())
        .config(EngineConfig {
            workers: 1,
            claim_limit: 1,
            poll_interval: Duration::from_millis(20),
            lease: Duration::from_secs(120),
            heartbeat_every: Duration::from_secs(10),
            ..EngineConfig::default()
        });
    for (id, node) in graph.nodes() {
        if !matches!(node.kind, NodeKind::Wait { .. } | NodeKind::End { .. }) {
            builder = builder.handler(id.clone(), handler.clone());
        }
    }
    let engine = builder.build()?.start().await?;
    let completion = async {
        let run_id = engine.start_run(graph.id(), json!({})).await?;
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let status = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    engine.cancel(&run_id, "operator interrupt").await?;
                    break RunStatus::Cancelled;
                }
                _ = heartbeat.tick() => metadata.heartbeat(&metadata_path)?,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    let run = engine.run(&run_id).await?.context("workflow run disappeared")?;
                    if run.status.is_terminal() { break run.status; }
                }
            }
        };
        Ok::<_, anyhow::Error>((run_id, status))
    }
    .await;
    let shutdown = engine.shutdown(Duration::from_secs(30)).await;
    let (run_id, status) = completion?;
    shutdown?;
    tracing::Span::current().record("workflow.status", status.to_string());
    if status == RunStatus::Completed
        && let Some(state) = handler.checkpoint.lock().await.as_mut()
    {
        state.inputs.verify(&handler.config)?;
        state.phase = checkpoint::next_phase(
            state.phase,
            checkpoint::PhaseEvent::Complete(
                [
                    state.report(&handler.config.artifacts, 0)?.is_some(),
                    state.report(&handler.config.artifacts, 1)?.is_some(),
                ],
                true,
            ),
        )
        .context("completion requires fresh matching reports")?;
        state.persist(&handler.config.artifacts)?;
    }
    agent::write_json_atomic(
        &handler.config.artifacts.join("result.json"),
        &json!({"run_id":run_id,"status":status,"error":*handler.error.lock().await,"execution_generation":handler.config.generation}),
    )?;
    ensure!(
        status == RunStatus::Completed,
        "workflow {status}: {}. Artifacts: {}",
        handler
            .error
            .lock()
            .await
            .as_deref()
            .unwrap_or("see result.json"),
        handler.config.artifacts.display()
    );
    println!(
        "done {}",
        git::output(&handler.config.repo, &["rev-parse", "HEAD"])?
    );
    Ok(())
}

#[async_trait]
impl NodeHandler for Workflow {
    async fn run(&self, ctx: NodeContext, _input: NodeInput) -> Result<NodeOutcome, HandlerError> {
        let _writer = self.writer.lock().await;
        let span = tracing::info_span!(parent: &self.trace, "implement.stage",
            "workflow.run_id" = %self.config.run_id, "stage.name" = %ctx.node_id,
            "stage.occurrence" = tracing::field::Empty, outcome = tracing::field::Empty,
            "otel.status_code" = tracing::field::Empty, "error.kind" = tracing::field::Empty);
        let result = self.execute(&ctx).instrument(span.clone()).await;
        crate::telemetry::finish_span(&span, result.is_ok(), "stage_failed");
        match result {
            Ok(label) => Ok(NodeOutcome::Done(Outcome::done(label))),
            Err(error) => {
                let message = format!("{error:#}");
                eprintln!("{} failed: {}", ctx.node_id, message);
                *self.error.lock().await = Some(message.clone());
                Err(HandlerError::Permanent(message))
            }
        }
    }
}

impl Workflow {
    async fn execute(&self, ctx: &NodeContext) -> Result<&'static str> {
        ensure!(
            ctx.attempt == 1,
            "refusing to replay a side-effecting stage after a lease reclaim"
        );
        ensure!(!ctx.cancel.is_cancelled(), "stage cancelled");
        let id = ctx.node_id.as_str();
        if id == "borb_acquire" {
            return Ok("ok");
        }
        if id == "resume-review" {
            let guard = self.checkpoint.lock().await;
            let state = guard
                .as_ref()
                .context("resume requires review checkpoint")?;
            ensure!(
                checkpoint::can_resume(state.phase),
                "review boundary cannot resume"
            );
            state.inputs.verify(&self.config)?;
            return Ok("ready");
        }
        if id == "release:done" {
            let reviewed = self.reviewed.lock().await;
            let reviewed = reviewed.as_ref().context("completion requires a review")?;
            ensure!(
                git::output(&self.config.repo, &["rev-parse", "HEAD"])? == reviewed.head,
                "HEAD changed after approval"
            );
            ensure!(
                git::output(&self.config.repo, &["branch", "--show-current"])? == reviewed.branch,
                "branch changed after approval"
            );
            git::require_clean(&self.config.repo)?;
            return Ok("ok");
        }
        let occurrence = ctx
            .step_key
            .as_str()
            .rsplit('/')
            .next()
            .context("missing step occurrence")?
            .parse::<u32>()?
            .checked_add(1)
            .context("step occurrence overflow")?;
        tracing::Span::current().record("stage.occurrence", occurrence);
        let node = self
            .compiled
            .nodes
            .get(id)
            .context("unknown workflow node")?;
        ensure!(
            node.max_occurrences
                .is_none_or(|max| occurrence_allowed(occurrence, max)),
            "review budget exhausted; no approval obtained"
        );
        let occurrence = if matches!(id, "review" | "fix-review") {
            checkpoint::review_round(self.first_round, occurrence, self.config.max_reviews)
                .context("review budget exhausted")?
        } else {
            occurrence
        };
        println!("{id} {occurrence}");
        self.audit(id, occurrence, "started")?;
        let label = match id {
            "implement" => {
                let prompt = format!(
                    "Implement the entire supplied plan. Read AGENTS.md and all relevant code first. Run the plan's verification commands and fix failures. Do not commit; the host commits your changes. End with a factual summary of changed behavior and verification results. If blocked, say BLOCKED and explain why.\n\n{}",
                    self.config.plan.text
                );
                let before = self.revision()?;
                let text = agent::session(
                    &self.config,
                    "implement",
                    &prompt,
                    agent::SessionMode::Mutable,
                    &ctx.cancel,
                )
                .await?;
                ensure!(
                    self.revision()? == before,
                    "implementation changed branch or HEAD; refusing host commit"
                );
                ensure!(
                    !text
                        .trim_start()
                        .strip_prefix("BLOCKED")
                        .is_some_and(|tail| tail.is_empty()
                            || tail.starts_with(
                                |character: char| character.is_whitespace() || character == ':'
                            )),
                    "implementation reported a blocker: {}",
                    text.trim()
                );
                ensure!(!ctx.cancel.is_cancelled(), "stage cancelled before commit");
                git::commit_changes(&self.config.repo, "feat: implement supplied plan")?;
                ensure!(
                    !git::output(
                        &self.config.repo,
                        &[
                            "diff",
                            &format!("{}...HEAD", self.config.base),
                            "--name-only"
                        ]
                    )?
                    .is_empty(),
                    "implementation produced no committed changes against the base"
                );
                crate::durable::write_json(
                    &self.config.artifacts.join("implementation.json"),
                    &json!({"commit":git::output(&self.config.repo, &["rev-parse","HEAD"])?, "plan":crate::durable::digest(self.config.plan.text.as_bytes())}),
                    false,
                )?;
                "implemented"
            }
            "review" => self.review(occurrence, &ctx.cancel).await?,
            "fix-review" => {
                let reviewed = self.reviewed.lock().await;
                let reviewed = reviewed
                    .as_ref()
                    .context("fix stage has no selected review")?;
                ensure!(
                    git::output(&self.config.repo, &["rev-parse", "HEAD"])? == reviewed.head,
                    "review is stale; HEAD changed"
                );
                ensure!(
                    git::output(&self.config.repo, &["branch", "--show-current"])?
                        == reviewed.branch,
                    "review belongs to another branch"
                );
                let prompt = format!(
                    "Execute the following fix-review skill (fix-review-autonomous) with arguments --auto --review-dir {}. Use that exact review directory and this bundled autonomous policy. Both reviewer reports must be considered. The host has verified the manifest's branch and HEAD. Apply valid code fixes autonomously, including configuration and security-sensitive source files, within the plan and tool permissions. Report genuine blockers using needs-manual. Do not commit, push, merge, or deploy. The host will commit fixes and require fresh independent review. End with the skill's exact auto-summary block.\n\n{}\n\nOriginal plan\n{}",
                    reviewed.directory.display(),
                    AUTONOMOUS_FIX_REVIEW,
                    self.config.plan.text
                );
                ensure!(
                    reviewed.round == occurrence,
                    "fix round differs from reviewed round"
                );
                crate::config::require_credentials()?;
                let name = format!("fix-review-{}", reviewed.round);
                let artifacts = crate::session_artifacts::SessionArtifacts::reserve(
                    &self.config.artifacts,
                    &name,
                )?;
                {
                    let mut guard = self.checkpoint.lock().await;
                    let state = guard.as_mut().context("missing review checkpoint")?;
                    state.inputs.verify(&self.config)?;
                    ensure!(
                        state.phase == CheckpointPhase::ReviewReportsReady,
                        "fix requires two complete matching reports"
                    );
                    state.phase = checkpoint::next_phase(
                        state.phase,
                        checkpoint::PhaseEvent::Fix([
                            state.report(&self.config.artifacts, 0)?.is_some(),
                            state.report(&self.config.artifacts, 1)?.is_some(),
                        ]),
                    )
                    .context("fix requires matching reports")?;
                    state.persist(&self.config.artifacts)?;
                }
                let text = agent::session_reserved(
                    &self.config,
                    &name,
                    &prompt,
                    agent::SessionMode::Mutable,
                    &ctx.cancel,
                    artifacts,
                )
                .await?;
                ensure!(
                    git::output(&self.config.repo, &["rev-parse", "HEAD"])? == reviewed.head
                        && git::output(&self.config.repo, &["branch", "--show-current"])?
                            == reviewed.branch,
                    "fix-review changed branch or HEAD; refusing host commit"
                );
                ensure!(
                    manual_count(&text)? == 0,
                    "fix-review requires manual attention; see its transcript"
                );
                ensure!(!ctx.cancel.is_cancelled(), "stage cancelled before commit");
                git::commit_changes(&self.config.repo, "fix: address plan review findings")?;
                if let Some(state) = self.checkpoint.lock().await.as_mut() {
                    state.phase =
                        checkpoint::next_phase(state.phase, checkpoint::PhaseEvent::Commit)
                            .context("fix commit without intent")?;
                    state.persist(&self.config.artifacts)?;
                }
                "fixed"
            }
            _ => bail!("unknown stage {id}"),
        };
        ensure!(!ctx.cancel.is_cancelled(), "stage cancelled");
        self.audit(id, occurrence, label)?;
        Ok(label)
    }

    fn revision(&self) -> Result<(String, String)> {
        Ok((
            git::output(&self.config.repo, &["branch", "--show-current"])?,
            git::output(&self.config.repo, &["rev-parse", "HEAD"])?,
        ))
    }

    fn audit(&self, node: &str, occurrence: u32, outcome: &str) -> Result<()> {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.config.artifacts.join("stages.jsonl"))?,
            "{}",
            json!({"node":node,"occurrence":occurrence,"outcome":outcome})
        )?;
        Ok(())
    }

    async fn review(&self, round: u32, cancel: &dmt::CancellationToken) -> Result<&'static str> {
        git::require_clean(&self.config.repo)?;
        let branch = git::output(&self.config.repo, &["branch", "--show-current"])?;
        ensure!(
            branch != self.config.base,
            "review requires a feature branch"
        );
        let head = git::output(&self.config.repo, &["rev-parse", "HEAD"])?;

        let existing = self.checkpoint.lock().await.clone();
        let mut state = if let Some(state) = existing.filter(|state| state.round == round) {
            state.inputs.verify(&self.config)?;
            state
        } else {
            let anchor: serde_json::Value = crate::durable::json(
                &self.config.artifacts.join("implementation.json"),
                crate::durable::METADATA_LIMIT,
            )?;
            if let Some(previous) = self.checkpoint.lock().await.as_ref() {
                ensure!(
                    previous.phase == CheckpointPhase::FixCommitted
                        && previous.round.checked_add(1) == Some(round),
                    "new review requires a committed fix"
                );
            }
            ReviewCheckpoint::create(
                &self.config,
                round,
                anchor["commit"]
                    .as_str()
                    .context("missing implementation anchor")?
                    .to_owned(),
                &crate::review_inputs::ReviewSnapshot::capture(&self.config, round, cancel)?,
            )?
        };
        let snapshot = crate::review_inputs::ReviewSnapshot::open(
            &crate::review_inputs::snapshot_directory(&self.config.artifacts, round),
            state
                .inputs
                .snapshot
                .as_ref()
                .context("missing review snapshot identity")?,
        )?;
        let metrics = snapshot.render()?.metrics;
        let cached = [
            state.report(&self.config.artifacts, 0)?,
            state.report(&self.config.artifacts, 1)?,
        ];
        let mut names = [None, None];
        for i in 0..2 {
            state.reused[i] = cached[i].is_some();
            if cached[i].is_none() {
                names[i] = Some(state.reserve_role(&self.config.artifacts, i)?);
            } else {
                println!("reusing review {round} {}", ROLES[i].0);
            }
        }
        state.persist(&self.config.artifacts)?;
        *self.checkpoint.lock().await = Some(state.clone());
        let run_role = |i: usize| {
            let state = &state;
            let snapshot = &snapshot;
            let cached = cached[i].clone();
            let name = names[i].clone();
            async move {
                let span = tracing::info_span!("implement.reviewer", "workflow.run_id" = %self.config.run_id,
                    "execution.generation" = self.config.generation, "review.round" = round, "review.role" = ROLES[i].0,
                    "review.attempt" = state.attempts[i], "review.prompt_bytes" = tracing::field::Empty, "review.omitted_bytes" = tracing::field::Empty, outcome = if cached.is_some() { "reused" } else { "started" },
                    "otel.status_code" = tracing::field::Empty, "error.kind" = tracing::field::Empty);
                let reused = cached.is_some();
                let result = async {
                    let start = checkpoint::role_start(state.phase, true, cached.is_some())
                        .context("reviewer cannot start in this checkpoint state")?;
                    if !start {
                        return cached.context("cached reviewer report missing");
                    }
                    let prompt = state.prompt(&self.config.artifacts, i)?;
                    tracing::Span::current().record("review.prompt_bytes", prompt.len() as u64);
                    tracing::Span::current()
                        .record("review.omitted_bytes", metrics.omitted_bytes as u64);
                    let text = agent::session(
                        &self.config,
                        name.as_deref().unwrap(),
                        &prompt,
                        agent::SessionMode::Review(snapshot.clone()),
                        cancel,
                    )
                    .await?;
                    state.publish_role(&self.config, i, &text, snapshot)
                }
                .instrument(span.clone())
                .await;
                if !reused {
                    crate::telemetry::finish_span(&span, result.is_ok(), "review_failed");
                }
                result
            }
        };
        let (first, second) = tokio::join!(run_role(0), run_role(1));
        let reports = [first?, second?];
        ensure!(
            checkpoint::reports_ready([
                state.report(&self.config.artifacts, 0)?.is_some(),
                state.report(&self.config.artifacts, 1)?.is_some()
            ]),
            "both reviewer checkpoints are required"
        );
        state.inputs.verify(&self.config)?;
        state.phase = checkpoint::next_phase(
            state.phase,
            checkpoint::PhaseEvent::Reports([
                state.report(&self.config.artifacts, 0)?.is_some(),
                state.report(&self.config.artifacts, 1)?.is_some(),
            ]),
        )
        .context("review reports are incomplete")?;
        state.persist(&self.config.artifacts)?;
        let stats = git::output(
            &self.config.repo,
            &["diff", &format!("{}...HEAD", self.config.base), "--stat"],
        )?;
        let directory = crate::review::publish_review(&self.config, &mut state, &reports, &stats)?;
        *self.checkpoint.lock().await = Some(state);
        println!("reviews {}", directory.display());
        ensure!(
            !reports
                .iter()
                .any(|report| report.verdict == ReviewVerdict::NeedsDiscussion),
            "review requires discussion; inspect {}",
            directory.display()
        );
        let approved = reports
            .iter()
            .all(|report| report.verdict == ReviewVerdict::Approve && report.blocking_items() == 0);
        *self.reviewed.lock().await = Some(ReviewedRevision {
            head,
            branch,
            directory,
            round,
        });
        if approved {
            Ok("approved")
        } else {
            ensure!(
                checkpoint::can_fix(round, self.config.max_reviews),
                "review budget exhausted; latest findings require changes"
            );
            Ok("changes")
        }
    }
}

fn manual_count(text: &str) -> Result<u32> {
    let marker = text
        .rsplit("<!-- auto-summary ")
        .next()
        .context("missing fix summary")?;
    ensure!(marker != text, "fix-review omitted auto-summary");
    let fields = marker
        .split_once("-->")
        .context("unterminated fix summary")?
        .0
        .split_whitespace()
        .collect::<Vec<_>>();
    ensure!(
        fields.len() == 3,
        "fix summary needs fixed, skipped, and needs-manual counts"
    );
    let mut counts = Vec::new();
    for (field, name) in fields.iter().zip(["fixed:", "skipped:", "needs-manual:"]) {
        counts.push(
            field
                .strip_prefix(name)
                .context("invalid fix summary field")?
                .parse::<u32>()
                .context("invalid fix summary count")?,
        );
    }
    Ok(counts[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_compiles_with_bounded_review_cycle() {
        let graph = compile(&document(4, 3600), &BTreeMap::new()).unwrap();
        assert!(graph.nodes.contains_key("implement"));
        assert_eq!(graph.nodes["review"].max_occurrences, Some(4));
    }
    #[test]
    fn review_accepts_final_structured_report_after_model_narration() {
        let report = json!({"verdict":"APPROVE","summary":"Verified the plan.","positive_notes":[],"findings":[]});
        for response in [
            format!("I checked the committed file.\n```json\n{report}\n```"),
            format!("I checked the committed file.{report}"),
        ] {
            assert_eq!(
                parse_report(&response).unwrap().verdict,
                ReviewVerdict::Approve
            );
        }
        assert!(parse_report(&format!("{report} trailing incomplete response")).is_err());
    }

    #[test]
    fn missing_and_contradictory_review_results_fail_closed() {
        assert!(parse_report("done").is_err());
        let report = json!({"verdict":"APPROVE","summary":"Review complete.","positive_notes":[],"findings":[{"severity":"Important","path":"hello.txt","line":1,"description":"Broken greeting.","suggested_fix":"Fix the greeting."}]});
        assert!(parse_report(&report.to_string()).is_err());
        assert_eq!(
            manual_count("<!-- auto-summary fixed:2 skipped:0 needs-manual:1 -->").unwrap(),
            1
        );
        assert!(manual_count("fixed everything").is_err());
    }
}
