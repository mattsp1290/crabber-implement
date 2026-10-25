use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use dmt::{
    EngineBuilder, EngineConfig, HandlerError, MemoryStore, NodeContext, NodeHandler, NodeInput,
    NodeKind, NodeOutcome, Outcome, RunStatus,
};
use flows_api::GraphDocument;
use flows_core::{Compiled, compile};
use serde::{Deserialize, Serialize};
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

use crate::{agent, config::RunConfig, git};

const REPORTS: [&str; 5] = [
    "00-overview.md",
    "01-critical-and-important.md",
    "02-suggestions.md",
    "03-positive-notes.md",
    "04-action-items.md",
];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewVerdict {
    Approve,
    RequestChanges,
    NeedsDiscussion,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewReport {
    verdict: ReviewVerdict,
    summary: String,
    findings: Vec<Finding>,
    positive_notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
enum Severity {
    Critical,
    Important,
    Suggestion,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    severity: Severity,
    path: String,
    line: u32,
    description: String,
    suggested_fix: String,
}

impl ReviewReport {
    fn blocking_items(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity != Severity::Suggestion)
            .count()
    }
}

struct ReviewedRevision {
    head: String,
    branch: String,
    directory: PathBuf,
}

struct Workflow {
    config: RunConfig,
    compiled: Compiled,
    reviewed: Mutex<Option<ReviewedRevision>>,
    error: Mutex<Option<String>>,
    writer: Mutex<()>,
}

#[derive(Serialize)]
struct RunMetadata {
    version: u32,
    run_id: String,
    repo: PathBuf,
    branch: String,
    model: String,
    heartbeat_at_ms: u128,
}

impl RunMetadata {
    fn heartbeat(&mut self, path: &std::path::Path) -> Result<()> {
        self.heartbeat_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        agent::write_json_atomic(path, self)
    }
}

pub(crate) fn document(max_reviews: u32, timeout_seconds: u32) -> GraphDocument {
    let task = |id: &str, labels: &[&str], max: u32| {
        json!({
            "id":id,"type":"shell","position":{"x":0,"y":0},
            "retry":{"max_attempts":1,"initial_backoff_seconds":1,"max_backoff_seconds":1},
            "config":{"argv":["crabber-implement-agent",id],"outcome_labels":labels,
              "outcome_from":"stdout_json","on_exhausted":"fail_run","max_occurrences":max,
              "timeout_seconds":timeout_seconds}
        })
    };
    serde_json::from_value(json!({
        "id":"crabber-implement","name":"Implement and review a plan","inputs":[],
        "nodes":[task("implement", &["implemented"], 1),
          task("review", &["approved","changes"], max_reviews),
          task("fix-review", &["fixed"], max_reviews),
          {"id":"done","type":"end","position":{"x":0,"y":0},"config":{"status":"completed"}}],
        "edges":[{"from":"implement","to":"review","label":"implemented"},
          {"from":"review","to":"done","label":"approved"},
          {"from":"review","to":"fix-review","label":"changes"},
          {"from":"fix-review","to":"review","label":"fixed"}]
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
    let document = document(config.max_reviews, timeout);
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
        version: 1,
        run_id: config.run_id.clone(),
        repo: config.repo.clone(),
        branch,
        model: config.model.clone(),
        heartbeat_at_ms: 0,
    };
    metadata.heartbeat(&metadata_path)?;
    let graph = compiled.graph.clone();
    let handler = Arc::new(Workflow {
        config,
        compiled,
        reviewed: Mutex::new(None),
        error: Mutex::new(None),
        writer: Mutex::new(()),
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
    let workflow = async {
        let engine = builder.build()?.start().await?;
        let run_id = engine.start_run(graph.id(), json!({})).await?;
        let status = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    engine.cancel(&run_id, "operator interrupt").await?;
                    break RunStatus::Cancelled;
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    let run = engine.run(&run_id).await?.context("workflow run disappeared")?;
                    if run.status.is_terminal() { break run.status; }
                }
            }
        };
        engine.shutdown(Duration::from_secs(30)).await?;
        agent::write_json_atomic(
            &handler.config.artifacts.join("result.json"),
            &json!({"run_id":run_id,"status":status,"error":*handler.error.lock().await}),
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
    };
    tokio::pin!(workflow);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    loop {
        tokio::select! {
            biased;
            result = &mut workflow => return result,
            _ = heartbeat.tick() => metadata.heartbeat(&metadata_path)?,
        }
    }
}

#[async_trait]
impl NodeHandler for Workflow {
    async fn run(&self, ctx: NodeContext, _input: NodeInput) -> Result<NodeOutcome, HandlerError> {
        let _writer = self.writer.lock().await;
        let result = self.execute(&ctx).await;
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
        println!("{id} {occurrence}");
        self.audit(id, occurrence, "started")?;
        let label = match id {
            "implement" => {
                let prompt = format!(
                    "Implement the entire supplied plan. Read AGENTS.md and all relevant code first. Run the plan's verification commands and fix failures. Do not commit; the host commits your changes. End with a factual summary of changed behavior and verification results. If blocked, say BLOCKED and explain why.\n\n{}",
                    self.config.plan.text
                );
                let before = self.revision()?;
                let text =
                    agent::session(&self.config, "implement", &prompt, false, &ctx.cancel).await?;
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
                    "Execute the following fix-review skill with arguments --auto --review-dir {}. Use that exact review directory. Both reviewer reports must be considered. The host has verified the manifest's branch and HEAD. Preserve its needs-manual guard. Do not commit. End with the skill's exact auto-summary block.\n\n{}\n\nOriginal plan\n{}",
                    reviewed.directory.display(),
                    self.config.fix_skill,
                    self.config.plan.text
                );
                let text = agent::session(
                    &self.config,
                    &format!("fix-review-{occurrence}"),
                    &prompt,
                    false,
                    &ctx.cancel,
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
        let directory = self
            .config
            .repo
            .join("reviews")
            .join(format!("{}-{round}", self.config.run_id));
        ensure!(!directory.exists(), "review directory already exists");
        let diff = git::output(
            &self.config.repo,
            &["diff", &format!("{}...HEAD", self.config.base)],
        )?;
        ensure!(!diff.is_empty(), "review has no changes against base");
        let names = git::raw_output(
            &self.config.repo,
            &[
                "diff",
                &format!("{}...HEAD", self.config.base),
                "--name-only",
                "-z",
            ],
        )?;
        let mut context = format!(
            "Branch {branch}\nBase {}\nHEAD {head}\nDiff\n{diff}\n",
            self.config.base
        );
        for name in names.split('\0').filter(|name| !name.is_empty()) {
            let tree = git::output(&self.config.repo, &["ls-tree", "HEAD", "--", name])?;
            if !tree.is_empty() {
                match git::raw_output(&self.config.repo, &["show", &format!("HEAD:{name}")]) {
                    Ok(body) => context.push_str(&format!("\nCommitted file {name}\n{body}\n")),
                    Err(_) => context.push_str(&format!("\nBinary file {name}\n")),
                }
            }
            ensure!(context.len() <= 8_000_000, "review context exceeds 8 MB");
        }
        for entry in fs::read_dir(&self.config.artifacts)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy();
            if (name.starts_with("implement") || name.starts_with("fix-review"))
                && name.ends_with(".txt")
            {
                context.push_str(&format!(
                    "\nExecution evidence {}\n{}",
                    name,
                    fs::read_to_string(&path)?
                ));
            }
        }
        ensure!(
            context.len() <= 8_000_000,
            "review context exceeds 8 MB including verification evidence"
        );
        context.push_str(&format!(
            "\nCommits\n{}\nOriginal plan\n{}",
            git::output(
                &self.config.repo,
                &["log", &format!("{}..HEAD", self.config.base), "--oneline"]
            )?,
            self.config.plan.text
        ));
        let reviewers = [
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
        let prompt = |index: usize| {
            format!(
                "Perform your assigned independent review using the review skill below. The host handles prerequisites, Git context, reviewer concurrency, directory creation, manifest, and writing your five files. You have readonly tools. Do not invoke other agents or write reports yourself. Your assigned display name is {}, slug {}, role {}.\n\nReturn ONLY a JSON object with keys verdict, summary, findings, positive_notes. verdict is APPROVE, REQUEST_CHANGES, or NEEDS_DISCUSSION. summary is a factual paragraph. findings is an array of objects with exactly severity, path, line, description, suggested_fix. severity is Critical, Important, or Suggestion. path is a repository-relative path, line is a positive integer, description explains the problem, and suggested_fix includes a concrete fix or code snippet. positive_notes is an array of strings referencing specific code. Return empty arrays when there are no findings or positive notes. The host derives blocking counts and renders the five skill files from this single typed report. Never approve an unimplemented requirement or failed verification.\n\nSkill\n{}\n\nReview context\n{}",
                reviewers[index].1,
                reviewers[index].0,
                reviewers[index].2,
                self.config.review_skill,
                context
            )
        };
        let first_prompt = prompt(0);
        let second_prompt = prompt(1);
        let first_name = format!("review-{round}-correctness");
        let second_name = format!("review-{round}-maintainability");
        let (first, second) = tokio::join!(
            agent::session(&self.config, &first_name, &first_prompt, true, cancel),
            agent::session(&self.config, &second_name, &second_prompt, true, cancel)
        );
        let reports = [parse_report(&first?)?, parse_report(&second?)?];
        ensure!(
            git::output(&self.config.repo, &["rev-parse", "HEAD"])? == head,
            "HEAD changed during review"
        );
        git::require_clean(&self.config.repo)?;
        let exclude = PathBuf::from(git::output(
            &self.config.repo,
            &["rev-parse", "--git-path", "info/exclude"],
        )?);
        let exclude = if exclude.is_absolute() {
            exclude
        } else {
            self.config.repo.join(exclude)
        };
        if let Some(parent) = exclude.parent() {
            fs::create_dir_all(parent)?;
        }
        writeln!(
            OpenOptions::new().append(true).create(true).open(exclude)?,
            "\n/reviews/{}-{round}/",
            self.config.run_id
        )?;
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version":"review-v1","branch":branch,"base_ref":self.config.base,"head_sha":head,"pass":round,
                "reviewers":reviewers.iter().map(|(slug,display_name,role)| json!({"slug":slug,"display_name":display_name,"role":role})).collect::<Vec<_>>()
            }))?,
        )?;
        let stats = git::output(
            &self.config.repo,
            &["diff", &format!("{}...HEAD", self.config.base), "--stat"],
        )?;
        for (reviewer, report) in reviewers.iter().zip(&reports) {
            let slug = reviewer.0;
            let folder = directory.join(slug);
            fs::create_dir(&folder)?;
            for (filename, body) in REPORTS
                .into_iter()
                .zip(render_report(report, &branch, &head, reviewer, &stats))
            {
                fs::write(folder.join(filename), body)?;
            }
        }
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
        });
        if approved {
            Ok("approved")
        } else {
            ensure!(
                round < self.config.max_reviews,
                "review budget exhausted; latest findings require changes"
            );
            Ok("changes")
        }
    }
}

fn parse_report(text: &str) -> Result<ReviewReport> {
    let text = text.trim();
    let text = text.strip_suffix("```").unwrap_or(text).trim_end();
    let report = text
        .char_indices()
        .filter(|(_, character)| *character == '{')
        .find_map(|(offset, _)| serde_json::from_str::<ReviewReport>(&text[offset..]).ok())
        .context("review must return its structured JSON report")?;
    ensure!(
        !report.summary.trim().is_empty(),
        "review summary cannot be empty"
    );
    for finding in &report.findings {
        let path = std::path::Path::new(&finding.path);
        ensure!(
            !finding.path.is_empty()
                && !path.is_absolute()
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
            "finding path must be repository-relative"
        );
        ensure!(
            finding.line > 0
                && !finding.description.trim().is_empty()
                && !finding.suggested_fix.trim().is_empty(),
            "finding needs a line, description, and suggested fix"
        );
    }
    ensure!(
        report.verdict != ReviewVerdict::Approve || report.blocking_items() == 0,
        "review approval contradicts blocking findings"
    );
    Ok(report)
}

fn render_report(
    report: &ReviewReport,
    branch: &str,
    head: &str,
    reviewer: &(&str, &str, &str),
    stats: &str,
) -> [String; 5] {
    let verdict = serde_json::to_value(report.verdict)
        .expect("enum serializes")
        .as_str()
        .unwrap()
        .to_owned();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let overview = format!(
        "# Review by {}\n\nBranch: {branch}\nHEAD: {head}\nDate: Unix timestamp {timestamp}\nReviewer slug: {}\nRole: {}\n\n{}\n\nOverall verdict: {verdict}\n\n```text\n{stats}\n```\n",
        reviewer.1, reviewer.0, reviewer.2, report.summary
    );
    let details = |suggestions: bool| {
        let mut body = String::new();
        for finding in report
            .findings
            .iter()
            .filter(|finding| (finding.severity == Severity::Suggestion) == suggestions)
        {
            body.push_str(&format!(
                "## {:?} at {}:{}\n\n{}\n\nSuggested fix\n\n{}\n\n",
                finding.severity,
                finding.path,
                finding.line,
                finding.description,
                finding.suggested_fix
            ));
        }
        if body.is_empty() {
            body.push_str("No issues found.\n");
        }
        body
    };
    let positives = if report.positive_notes.is_empty() {
        "No specific positive notes recorded.\n".into()
    } else {
        report
            .positive_notes
            .iter()
            .map(|note| format!("- {note}\n"))
            .collect()
    };
    let mut actions = String::from("# Action items\n\n");
    for severity in [
        Severity::Critical,
        Severity::Important,
        Severity::Suggestion,
    ] {
        actions.push_str(&format!("## {severity:?}\n\n"));
        for finding in report
            .findings
            .iter()
            .filter(|finding| finding.severity == severity)
        {
            actions.push_str(&format!(
                "- [ ] [{}:{}] {} Suggested fix: {}\n",
                finding.path,
                finding.line,
                finding.description.replace('\n', " "),
                finding.suggested_fix.replace('\n', " ")
            ));
        }
        actions.push('\n');
    }
    [overview, details(false), details(true), positives, actions]
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
