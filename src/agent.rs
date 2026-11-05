use anyhow::{Context, Result, bail};
use crabber::{
    Agent, AgentConfig, EventKind, PermissionDecision, RuntimeError, Selection, StaticPolicy,
    core::RunStatus,
    extension::Scope,
    providers::{HttpAdapter, HttpResolver, Protocol},
};
use crabber_tools_catalog::prelude::*;
use std::{fs, io::Write, path::Path, sync::Arc, time::Duration};

use crate::config::RunConfig;
#[derive(Clone)]
pub enum SessionMode {
    Mutable,
    Review(crate::review_inputs::ReviewSnapshot),
}

use tracing::Instrument;

const MAX_CONTINUATIONS: usize = 2;
const RESPONSE_SUMMARY_LIMIT: usize = 8;
const CONTINUATION_PROMPT: &str = "Continue the assigned task from the existing session and workspace. Complete any remaining work and return a visible final answer with the result and verification, or begin your answer with BLOCKED and explain the blocker. Do not repeat completed work.";

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ContinuationCause {
    EmptyCompletion,
    OutputLimit,
}

enum AttemptOutcome {
    Complete(String),
    Continue(ContinuationCause),
}

fn next_output_limit(current: u32, ceiling: u32) -> Option<u32> {
    (current < ceiling).then(|| current.saturating_mul(2).min(ceiling))
}

#[derive(serde::Serialize)]
struct ResponseEvidence {
    run_id: crabber::core::RunId,
    observed_turn: usize,
    #[serde(flatten)]
    completion: crabber::core::event_payload::ModelResponseCompleted,
}

#[derive(serde::Serialize)]
struct AttemptEvidence {
    run_id: crabber::core::RunId,
    observed_turns: usize,
    requested_max_output_tokens: u32,
    continuation_cause: Option<ContinuationCause>,
    responses: Vec<ResponseEvidence>,
    response_count: usize,
    responses_truncated: bool,
    status: &'static str,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct StageSession<'a> {
    name: &'a str,
    execution_generation: u64,
    review_round: Option<u32>,
    review_role: Option<&'static str>,
    review_attempt: Option<u32>,
    workflow_run_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<crabber::SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<crabber::core::RunId>,
    max_turns: usize,
    observed_turns: usize,
    continuation_count: usize,
    max_output_tokens: u32,
    max_recovery_output_tokens: u32,
    #[serde(skip)]
    current_output_tokens: u32,
    runs: Vec<AttemptEvidence>,
    status: &'static str,
    error: Option<String>,
    #[serde(skip)]
    store: Arc<crabber::session::MemoryStore>,
    #[serde(skip)]
    deadline: tokio::time::Instant,
    #[serde(skip)]
    config: &'a RunConfig,
    #[serde(skip)]
    metadata: crate::session_artifacts::SessionMetadata,
}

pub async fn session(
    config: &RunConfig,
    name: &str,
    prompt: &str,
    mode: SessionMode,
    cancel: &dmt::CancellationToken,
) -> Result<String> {
    let artifacts = crate::session_artifacts::SessionArtifacts::reserve(&config.artifacts, name)?;
    session_reserved(config, name, prompt, mode, cancel, artifacts).await
}

pub(crate) async fn session_reserved(
    config: &RunConfig,
    name: &str,
    prompt: &str,
    mode: SessionMode,
    cancel: &dmt::CancellationToken,
    mut artifacts: crate::session_artifacts::SessionArtifacts,
) -> Result<String> {
    let span = tracing::info_span!("implement.session", "workflow.run_id" = %config.run_id,
        "session.role" = name, outcome = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty, "error.kind" = tracing::field::Empty);
    let identity = crate::review_checkpoint::session_identity(name);
    let mut stage = StageSession {
        name,
        execution_generation: config.generation,
        review_round: identity.map(|v| v.0),
        review_role: identity.map(|v| crate::review_checkpoint::ROLES[v.1].0),
        review_attempt: identity.map(|v| v.2),
        workflow_run_id: &config.run_id,
        session_id: None,
        run_id: None,
        max_turns: config.max_turns.get(),
        observed_turns: 0,
        continuation_count: 0,
        max_output_tokens: config.output_budget.initial.get(),
        max_recovery_output_tokens: config.output_budget.ceiling.get(),
        current_output_tokens: config.output_budget.initial.get(),
        runs: Vec::new(),
        status: "running",
        error: None,
        store: Arc::new(crabber::session::MemoryStore::new()),
        deadline: tokio::time::Instant::now() + config.session_timeout,
        config,
        metadata: artifacts.metadata,
    };
    stage.persist()?;
    let result = stage
        .execute(
            prompt,
            mode,
            cancel,
            &mut artifacts.transcript,
            &mut artifacts.tools,
            &mut artifacts.responses,
        )
        .instrument(span.clone())
        .await;
    crate::telemetry::finish_span(&span, result.is_ok(), "session_failed");
    stage.status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    stage.error = result.as_ref().err().map(|error| format!("{error:#}"));
    if let Err(error) = &result
        && let Some(attempt) = stage.runs.last_mut()
    {
        attempt.status = "failed";
        attempt.error = Some(format!("{error:#}"));
    }
    match (result, stage.persist()) {
        (Err(error), Err(persist)) => Err(error.context(format!(
            "also failed to persist session evidence: {persist:#}"
        ))),
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
    }
}

impl StageSession<'_> {
    fn persist(&self) -> Result<()> {
        self.metadata.persist(self)
    }

    fn check_active(&self, cancel: &dmt::CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            bail!("session cancelled");
        }
        if tokio::time::Instant::now() >= self.deadline {
            bail!(
                "session exceeded {} seconds",
                self.config.session_timeout.as_secs()
            );
        }
        Ok(())
    }

    fn turn_limit(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "Crabber session {} reached its turn limit of {} after {} observed model turns; use --max-turns to set a larger per-session budget. Partial edits are preserved; see {}",
            self.name,
            self.max_turns,
            self.observed_turns,
            self.config.artifacts.display()
        )
    }

    async fn execute(
        &mut self,
        prompt: &str,
        mode: SessionMode,
        cancel: &dmt::CancellationToken,
        transcript: &mut fs::File,
        tool_log: &mut fs::File,
        response_log: &mut fs::File,
    ) -> Result<String> {
        let mut next_prompt = prompt.to_owned();
        loop {
            self.check_active(cancel)?;
            let remaining = self.max_turns.saturating_sub(self.observed_turns);
            if remaining == 0 {
                return Err(self.turn_limit());
            }
            let agent = build_agent(
                self.config,
                self.name,
                mode.clone(),
                remaining,
                self.current_output_tokens,
                self.store.clone(),
            )?;
            let attempt_span = tracing::info_span!("implement.attempt", "workflow.run_id" = %self.workflow_run_id,
                "continuation.index" = self.continuation_count as u64,
                "output.requested_cap" = self.current_output_tokens,
                "session.id" = tracing::field::Empty, "run.id" = tracing::field::Empty,
                "continuation.reason" = tracing::field::Empty, outcome = tracing::field::Empty, "otel.status_code" = tracing::field::Empty,
                "error.kind" = tracing::field::Empty);
            let result = self
                .attempt(
                    &agent,
                    &next_prompt,
                    cancel,
                    transcript,
                    tool_log,
                    response_log,
                )
                .instrument(attempt_span.clone())
                .await;
            if let Ok(AttemptOutcome::Continue(cause)) = &result {
                attempt_span.record("outcome", "continuation_required");
                attempt_span.record(
                    "continuation.reason",
                    match cause {
                        ContinuationCause::EmptyCompletion => "empty_completion",
                        ContinuationCause::OutputLimit => "output_limit",
                    },
                );
            } else {
                crate::telemetry::finish_span(&attempt_span, result.is_ok(), "attempt_failed");
            }
            let close = agent.close_extensions().await;
            let continuation = result?;
            close?;
            self.check_active(cancel)?;
            let cause = match continuation {
                AttemptOutcome::Complete(text) => return Ok(text),
                AttemptOutcome::Continue(cause) => cause,
            };
            if self.observed_turns >= self.max_turns {
                return Err(self.turn_limit());
            }
            match cause {
                ContinuationCause::OutputLimit => {
                    let larger = next_output_limit(
                        self.current_output_tokens,
                        self.max_recovery_output_tokens,
                    );
                    let reason = if larger.is_none() {
                        Some("output ceiling")
                    } else if self.continuation_count == MAX_CONTINUATIONS {
                        Some("continuation allowance")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        bail!(
                            "Crabber session {} exhausted output-limit recovery ({reason}); last requested per-response cap {}, configured ceiling {}, continuations {}/{}, observed turns {}/{}. Configure --max-output-tokens and --max-recovery-output-tokens for the provider. Requested caps may be clamped by the provider. Partial edits are preserved; see {}",
                            self.name,
                            self.current_output_tokens,
                            self.max_recovery_output_tokens,
                            self.continuation_count,
                            MAX_CONTINUATIONS,
                            self.observed_turns,
                            self.max_turns,
                            self.config.artifacts.display()
                        );
                    }
                    self.current_output_tokens = larger.unwrap();
                    next_prompt = format!(
                        "The previous response reached its per-response output allowance. The new requested cap is {} tokens. {} Take a small concrete next tool action or return a factual final answer. Do not reproduce private reasoning.",
                        self.current_output_tokens, CONTINUATION_PROMPT
                    );
                }
                ContinuationCause::EmptyCompletion => {
                    if self.continuation_count == MAX_CONTINUATIONS {
                        bail!(
                            "Crabber session {} returned no final answer after {} continuations (empty completion); partial edits are preserved; see {}",
                            self.name,
                            self.continuation_count,
                            self.config.artifacts.display()
                        );
                    }
                    next_prompt = CONTINUATION_PROMPT.to_owned();
                }
            }
            self.continuation_count += 1;
            self.persist()?;
        }
    }

    async fn attempt(
        &mut self,
        agent: &Agent,
        prompt: &str,
        cancel: &dmt::CancellationToken,
        transcript: &mut fs::File,
        tool_log: &mut fs::File,
        response_log: &mut fs::File,
    ) -> Result<AttemptOutcome> {
        self.check_active(cancel)?;
        let admission = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("session cancelled")),
            _ = tokio::time::sleep_until(self.deadline) => Err(anyhow::anyhow!("session exceeded {} seconds", self.config.session_timeout.as_secs())),
            run = agent.prompt_with_context(self.session_id.clone(), prompt, crabber::obs::Telemetry::current_trace_context()) => Ok(run),
        };
        let mut run = match admission {
            Ok(Ok(run)) => run,
            Ok(Err(error)) => {
                let error = anyhow::Error::new(error).context("start Crabber session");
                let cleanup = agent.close(Duration::from_secs(30)).await;
                return Err(if cleanup.local_drained() {
                    error
                } else {
                    error.context("admission cleanup remains incomplete")
                });
            }
            Err(error) => {
                let cleanup = agent.close(Duration::from_secs(30)).await;
                if !cleanup.local_drained() {
                    return Err(error.context("admission cleanup remains incomplete"));
                }
                return Err(error);
            }
        };
        tracing::Span::current().record("session.id", run.session_id().to_string());
        tracing::Span::current().record("run.id", run.run_id().to_string());
        self.session_id = Some(run.session_id().clone());
        self.run_id = Some(run.run_id().clone());
        self.runs.push(AttemptEvidence {
            run_id: run.run_id().clone(),
            observed_turns: 0,
            requested_max_output_tokens: self.current_output_tokens,
            continuation_cause: None,
            responses: Vec::new(),
            response_count: 0,
            responses_truncated: false,
            status: "running",
            error: None,
        });
        let outcome = async {
            self.persist()?;
            let mut events = run.events();
            let mut text = String::new();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("session cancelled"),
                    _ = tokio::time::sleep_until(self.deadline) => bail!("session exceeded {} seconds", self.config.session_timeout.as_secs()),
                    event = events.recv() => event?,
                };
                let Some(event) = event else { break };
                match event.kind {
                    EventKind::TurnStarted => {
                        self.observed_turns += 1;
                        self.runs.last_mut().unwrap().observed_turns += 1;
                        text.clear();
                        self.persist()?;
                    }
                    EventKind::MessageStarted => text.clear(),
                    EventKind::TextDelta => {
                        let delta = event.payload["text"].as_str().unwrap_or_default();
                        text.push_str(delta);
                        transcript.write_all(delta.as_bytes()).context("write session transcript")?;
                    }
                    EventKind::ToolCallSettled => writeln!(tool_log, "{}", event.payload).context("write session tool log")?,
                    EventKind::ModelResponseCompleted => {
                        let attempt = self.runs.last_mut().unwrap();
                        let response = ResponseEvidence { run_id: attempt.run_id.clone(), observed_turn: self.observed_turns, completion: serde_json::from_value(event.payload.clone()).context("decode model completion evidence")? };
                        writeln!(response_log, "{}", serde_json::to_string(&response)?).context("write model response evidence")?;
                        attempt.response_count += 1;
                        attempt.responses.push(response);
                        if attempt.responses.len() > RESPONSE_SUMMARY_LIMIT {
                            attempt.responses.remove(0);
                            attempt.responses_truncated = true;
                        }
                        self.persist()?;
                    }
                    _ => {}
                }
            }
            Ok(text)
        }.await;
        if outcome.is_err() {
            run.interrupt();
        }
        let control = run.control();
        let done_future = run.done();
        tokio::pin!(done_future);
        let done = tokio::select! {
            biased;
            _ = cancel.cancelled() => { control.interrupt(); let _ = done_future.await; Err(anyhow::anyhow!("session cancelled")) },
            _ = tokio::time::sleep_until(self.deadline) => { control.interrupt(); let _ = done_future.await; Err(anyhow::anyhow!("session exceeded {} seconds", self.config.session_timeout.as_secs())) },
            result = &mut done_future => Ok(result),
        };
        let runtime_error = done
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().err())
            .map(ToString::to_string);
        let result = (|| {
            let text = outcome?;
            let done = done?;
            match done {
                Err(RuntimeError::OutputLimit { tool_call_count: 0 })
                    if self
                        .runs
                        .last()
                        .and_then(|attempt| attempt.responses.last())
                        .is_some_and(|response| {
                            response.observed_turn == self.observed_turns
                                && response.completion.finish_reason
                                    == Some(crabber::core::ModelFinishReason::Length)
                                && response.completion.tool_call_count == 0
                                && response.completion.stream_completed
                        }) =>
                {
                    Ok(AttemptOutcome::Continue(ContinuationCause::OutputLimit))
                }
                Err(RuntimeError::TurnLimit) => Err(self.turn_limit()),
                Err(error) => Err(error.into()),
                Ok(result) => {
                    if result.status != RunStatus::Completed {
                        bail!(
                            "Crabber session {} ended with {:?}; see {}",
                            self.name,
                            result.status,
                            self.config.artifacts.display()
                        );
                    }
                    let response = self
                        .runs
                        .last()
                        .and_then(|attempt| attempt.responses.last())
                        .filter(|response| {
                            response.observed_turn == self.observed_turns
                                && response.completion.stream_completed
                        })
                        .context("completed Crabber run has no final model response evidence")?;
                    Ok(
                        if response.completion.has_visible_text && !text.trim().is_empty() {
                            AttemptOutcome::Complete(text)
                        } else {
                            AttemptOutcome::Continue(ContinuationCause::EmptyCompletion)
                        },
                    )
                }
            }
        })();
        let attempt = self.runs.last_mut().unwrap();
        attempt.status = match &result {
            Ok(AttemptOutcome::Complete(_)) => "completed",
            Ok(AttemptOutcome::Continue(cause)) => {
                attempt.continuation_cause = Some(*cause);
                "continuation_required"
            }
            Err(_) => "failed",
        };
        attempt.error = result
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"))
            .or(runtime_error);
        self.persist()?;
        result
    }
}

pub(crate) fn path_deny_policy() -> Result<PathDenyPolicy> {
    Ok(PathDenyPolicy::new(
        [
            ".git",
            ".git/**",
            "**/.git/**",
            ".env",
            ".env.*",
            "**/.env",
            "**/.env.*",
            "*.pem",
            "**/*.pem",
            ".netrc",
            "**/id_rsa",
            "**/id_ed25519",
        ]
        .map(str::to_owned),
    )?)
}

fn build_agent(
    config: &RunConfig,
    name: &str,
    mode: SessionMode,
    max_turns: usize,
    max_output_tokens: u32,
    store: Arc<crabber::session::MemoryStore>,
) -> Result<Agent> {
    let resolver = HttpResolver::new().with_adapter(
        HttpAdapter::opencode_go(Protocol::ChatCompletions).with_base_url(&config.base_url),
    );
    build_agent_with_resolver(
        config,
        name,
        mode,
        max_turns,
        max_output_tokens,
        store,
        (
            Selection {
                provider_id: "opencode-go".into(),
                model_id: config.model.clone(),
            },
            Arc::new(resolver),
        ),
    )
}

fn build_agent_with_resolver(
    config: &RunConfig,
    name: &str,
    mode: SessionMode,
    max_turns: usize,
    max_output_tokens: u32,
    store: Arc<crabber::session::MemoryStore>,
    provider: (Selection, Arc<dyn crabber::providers::Resolver>),
) -> Result<Agent> {
    let readonly = matches!(&mode, SessionMode::Review(_));
    let mut enabled = vec![
        ToolId::FileRead,
        ToolId::FileList,
        ToolId::Search,
        ToolId::Glob,
    ];
    if !readonly {
        enabled.extend([
            ToolId::FileWrite,
            ToolId::FileEdit,
            ToolId::MultiEdit,
            ToolId::FileDelete,
            ToolId::FileMove,
            ToolId::ApplyPatch,
            ToolId::Shell,
        ]);
    }
    let tools = StandardTools::new(
        WorkspaceRoot::open(&config.repo)?,
        Policies {
            deny: path_deny_policy()?,
            enabled: EnabledSet::only(enabled),
            restrict_to_enabled: !readonly,
            shell: ShellPolicy {
                shell_binary: "/bin/sh".into(),
                startup_mode: StartupMode::NonLogin,
                env: EnvPolicy::minimal_allowlist(),
                run_as: None,
                output_cap_bytes: 65_536,
            },
            search: SearchPolicy::resolve_rg_from_path(EnvPolicy::minimal_allowlist())?,
            url_fetch: None,
            tracker: None,
            user_interact: None,
            limits: Limits {
                max_in_flight: 4,
                max_blocking_wait: Duration::from_secs(2),
            },
        },
    )?;
    let mut agent_config = AgentConfig::new(provider.0);
    agent_config.directory = config.repo.to_string_lossy().into_owned();
    agent_config.workspace_id = config.run_id.clone();
    agent_config.title = name.into();
    agent_config.max_output_tokens = Some(max_output_tokens);
    agent_config.system_prompt = Some(format!(
        "You are a coding agent working in {}. Follow applicable AGENTS.md instructions. Complete the assigned task using the available tools. Never push, deploy, contact others, change branches, commit, or modify Git metadata. The host owns commits. Do not read credentials. Shell commands run locally with the user's access and are not sandboxed. Report uncertainty and failures honestly. Stop rather than claiming success when blocked.",
        config.repo.display()
    ));
    let standard_names = tools
        .definitions()
        .iter()
        .map(|definition| definition.info.name.clone())
        .collect();
    let mut builder = Agent::builder()
        .behavior_version("crabber-implement.v1")
        .max_turns(max_turns)
        .store(store)
        .provider(provider.1)
        .config(agent_config)
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .shared_extension(Arc::new(tools), Scope::Global);
    if let SessionMode::Review(snapshot) = mode {
        builder = builder.shared_extension(
            Arc::new(crate::review_artifacts::ReviewArtifacts {
                snapshot,
                standard_names,
            }),
            Scope::Global,
        );
    }
    Ok(builder.build()?)
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::next_output_limit;

    #[test]
    fn output_limit_grows_clamps_and_never_overflows() {
        assert_eq!(next_output_limit(16384, 65536), Some(32768));
        assert_eq!(next_output_limit(32768, 65536), Some(65536));
        assert_eq!(next_output_limit(40000, 50000), Some(50000));
        assert_eq!(next_output_limit(65536, 65536), None);
        assert_eq!(next_output_limit(u32::MAX - 1, u32::MAX), Some(u32::MAX));
        assert_eq!(next_output_limit(u32::MAX, u32::MAX), None);
    }
    #[tokio::test]
    async fn artifact_collision_never_replaces_session_metadata() {
        for suffix in ["session.json", "txt", "tools.txt", "responses.jsonl"] {
            let (_temp, config) = crate::review_checkpoint::tests::fixture();
            let name = "review-1-correctness-attempt-1";
            let path = config.artifacts.join(format!("{name}.{suffix}"));
            std::fs::write(&path, b"original outcome").unwrap();
            let result = super::session(
                &config,
                name,
                "never execute",
                super::SessionMode::Mutable,
                &dmt::CancellationToken::new(),
            )
            .await;
            assert!(format!("{:#}", result.unwrap_err()).contains("reserve"));
            assert_eq!(std::fs::read(&path).unwrap(), b"original outcome");
        }
    }
}

#[cfg(test)]
mod review_admission_tests {
    use super::*;
    #[tokio::test]
    async fn configured_review_tools_admit_worst_case_final_prompt() {
        for atom in ["\\", "\"", "\n", "🙂"] {
            let (_temp, config) = crate::review_checkpoint::tests::fixture();
            let snapshot = crate::review_checkpoint::tests::snapshot(&config, 1);
            let provider = crabber::FakeProvider::scripted(vec![vec![
                crabber::StreamDelta::TextDelta("complete".into()),
                crabber::StreamDelta::Completed,
            ]]);
            let agent = build_agent_with_resolver(
                &config,
                "review-1-correctness-attempt-1",
                SessionMode::Review(snapshot),
                2,
                16384,
                Arc::new(crabber::session::MemoryStore::new()),
                (
                    Selection {
                        provider_id: "fake".into(),
                        model_id: "scripted".into(),
                    },
                    Arc::new(provider.clone()),
                ),
            )
            .unwrap();
            let prompt = atom.repeat(crate::review_inputs::PROMPT_LIMIT / atom.len());
            let run = agent.prompt(None, &prompt).await.unwrap();
            run.done().await.unwrap();
            let requests = provider.requests();
            let mut tools = requests[0]
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>();
            tools.sort();
            assert_eq!(
                tools,
                [
                    "file_list",
                    "file_read",
                    "glob",
                    "review_artifact_catalog",
                    "review_artifact_read",
                    "search"
                ]
            );
            assert!(agent.close(Duration::from_secs(1)).await.local_drained());
            agent.close_extensions().await.unwrap();
        }
    }
    #[tokio::test]
    async fn review_combined_allowlist_rejects_shell() {
        let (_temp, config) = crate::review_checkpoint::tests::fixture();
        let snapshot = crate::review_checkpoint::tests::snapshot(&config, 1);
        let call = crabber::core::ToolCallId::new();
        let provider = crabber::FakeProvider::scripted(vec![
            vec![
                crabber::StreamDelta::ToolCallStart {
                    call_id: call.clone(),
                    name: "shell".into(),
                },
                crabber::StreamDelta::ToolCallArgsDelta {
                    call_id: call.clone(),
                    text: "{\"command\":\"touch escaped\"}".into(),
                },
                crabber::StreamDelta::ToolCallDone { call_id: call },
                crabber::StreamDelta::Completed,
            ],
            vec![
                crabber::StreamDelta::TextDelta("rejected".into()),
                crabber::StreamDelta::Completed,
            ],
        ]);
        let agent = build_agent_with_resolver(
            &config,
            "review",
            SessionMode::Review(snapshot),
            2,
            16384,
            Arc::new(crabber::session::MemoryStore::new()),
            (
                Selection {
                    provider_id: "fake".into(),
                    model_id: "scripted".into(),
                },
                Arc::new(provider.clone()),
            ),
        )
        .unwrap();
        let run = agent.prompt(None, "attempt shell").await.unwrap();
        let _ = run.done().await;
        assert!(!config.repo.join("escaped").exists());
        assert!(agent.close(Duration::from_secs(1)).await.local_drained());
        agent.close_extensions().await.unwrap();
    }
}
