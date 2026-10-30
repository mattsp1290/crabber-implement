use anyhow::{Context, Result, bail};
use crabber::{
    Agent, AgentConfig, EventKind, PermissionDecision, RuntimeError, Selection, StaticPolicy,
    core::RunStatus, extension::Scope,
};
use crabber_tools_catalog::prelude::*;
use std::{
    fs,
    io::{Seek, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use crate::config::RunConfig;
#[derive(Clone)]
pub enum SessionMode {
    Mutable,
    Review(crate::review_inputs::ReviewSnapshot, ParentDelegation),
}

use tracing::Instrument;

const MAX_CONTINUATIONS: usize = 2;
const RESPONSE_SUMMARY_LIMIT: usize = 8;
const CONTINUATION_PROMPT: &str = "Continue the assigned task from the existing session and workspace. Complete any remaining work and return a visible final answer with the result and verification, or begin your answer with BLOCKED and explain the blocker. Do not repeat completed work.";

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ContinuationCause {
    EmptyCompletion,
}

enum AttemptOutcome {
    Complete(String),
    Continue(ContinuationCause),
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
    continuation_cause: Option<ContinuationCause>,
    responses: Vec<ResponseEvidence>,
    response_count: usize,
    responses_truncated: bool,
    status: &'static str,
    error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ParentDelegation {
    session_id: crabber::SessionId,
    run_id: crabber::core::RunId,
    call_id: crabber::core::ToolCallId,
}

impl ParentDelegation {
    pub(crate) fn from_request(request: &crabber_extensions::delegate_task::Request) -> Self {
        Self {
            session_id: request.session_id().clone(),
            run_id: request.run_id().clone(),
            call_id: request.call_id().clone(),
        }
    }
}

#[derive(serde::Serialize)]
struct StageSession<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_delegation: Option<ParentDelegation>,
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
        parent_delegation: match &mode {
            SessionMode::Review(_, parent) => Some(parent.clone()),
            SessionMode::Mutable => None,
        },
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
                self.store.clone(),
            )?;
            let attempt_span = tracing::info_span!("implement.attempt", "workflow.run_id" = %self.workflow_run_id,
                "continuation.index" = self.continuation_count as u64,
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

    fn observe_event(
        &mut self,
        event: &crabber::EventRecord,
        text: &mut String,
        transcript: &mut fs::File,
        tool_log: &mut fs::File,
        response_log: &mut fs::File,
    ) -> Result<()> {
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
                transcript
                    .write_all(delta.as_bytes())
                    .context("write session transcript")?;
            }
            EventKind::ToolCallSettled => {
                writeln!(tool_log, "{}", event.payload).context("write session tool log")?
            }
            EventKind::ModelResponseCompleted => {
                let attempt = self.runs.last_mut().unwrap();
                let response = ResponseEvidence {
                    run_id: attempt.run_id.clone(),
                    observed_turn: self.observed_turns,
                    completion: serde_json::from_value(event.payload.clone())
                        .context("decode model completion evidence")?,
                };
                writeln!(response_log, "{}", serde_json::to_string(&response)?)
                    .context("write model response evidence")?;
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
        Ok(())
    }

    async fn reconcile_observation(
        &mut self,
        transcript: &mut fs::File,
        tool_log: &mut fs::File,
        response_log: &mut fs::File,
        tool_offset: u64,
        response_offset: u64,
    ) -> Result<String> {
        use crabber::session::Store;
        let session_id = self
            .session_id
            .clone()
            .context("missing admitted session")?;
        let run_id = self.run_id.clone().context("missing admitted run")?;
        let attempt = self.runs.last_mut().context("missing attempt")?;
        self.observed_turns -= attempt.observed_turns;
        attempt.observed_turns = 0;
        attempt.responses.clear();
        attempt.response_count = 0;
        attempt.responses_truncated = false;
        tool_log.set_len(tool_offset)?;
        tool_log.seek(std::io::SeekFrom::Start(tool_offset))?;
        response_log.set_len(response_offset)?;
        response_log.seek(std::io::SeekFrom::Start(response_offset))?;
        let mut last_cursor = None;
        let mut ignored_text = String::new();
        loop {
            let events = self
                .store
                .list_events(&session_id, last_cursor, 256)
                .await?;
            if events.is_empty() {
                break;
            }
            for event in events {
                last_cursor = event.cursor;
                if event.run_id == run_id {
                    self.observe_event(
                        &event,
                        &mut ignored_text,
                        transcript,
                        tool_log,
                        response_log,
                    )?;
                }
            }
        }
        let text = committed_answer(&self.store, &session_id, &run_id).await?;
        writeln!(
            transcript,
            "\n[Live observation lagged; recovered committed final answer]\n{text}"
        )?;
        Ok::<_, anyhow::Error>(text)
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
            continuation_cause: None,
            responses: Vec::new(),
            response_count: 0,
            responses_truncated: false,
            status: "running",
            error: None,
        });
        let tool_offset = tool_log.stream_position()?;
        let response_offset = response_log.stream_position()?;
        let mut observation_lagged = false;
        let mut outcome = async {
            self.persist()?;
            let mut events = run.events();
            let mut text = String::new();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("session cancelled"),
                    _ = tokio::time::sleep_until(self.deadline) => bail!("session exceeded {} seconds", self.config.session_timeout.as_secs()),
                    event = events.recv() => match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { observation_lagged = true; break; }
                        Err(error) => return Err(error.into()),
                    },
                };
                let Some(event) = event else { break };
                self.observe_event(&event, &mut text, transcript, tool_log, response_log)?;

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
        if observation_lagged {
            let recovered = self
                .reconcile_observation(
                    transcript,
                    tool_log,
                    response_log,
                    tool_offset,
                    response_offset,
                )
                .await;
            if outcome.is_ok() {
                outcome = recovered;
            }
        }
        let runtime_error = done
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().err())
            .map(ToString::to_string);
        let result = (|| {
            let text = outcome?;
            let done = done?;
            match done {
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

async fn committed_answer(
    store: &crabber::session::MemoryStore,
    session: &crabber::SessionId,
    run: &crabber::core::RunId,
) -> Result<String> {
    use crabber::session::Store;
    Ok(store
        .list_messages(session, None)
        .await?
        .into_iter()
        .rev()
        .find(|message| {
            message.run_id.as_ref() == Some(run) && message.role == crabber::core::Role::Assistant
        })
        .map(|message| {
            message
                .parts
                .into_iter()
                .filter_map(|part| match part.content {
                    crabber::core::ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default())
}

pub(crate) fn path_deny_policy(config: &RunConfig) -> Result<PathDenyPolicy> {
    let mut patterns = vec![
        ".codex",
        ".codex/**",
        "**/.codex/**",
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
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let auth = config
        .codex_auth_file
        .canonicalize()
        .or_else(|_| std::path::absolute(&config.codex_auth_file))?;
    if let Ok(relative) = auth.strip_prefix(&config.repo) {
        let relative = relative
            .to_str()
            .context("Codex login path inside repository must be UTF-8")?;
        patterns.push(
            relative
                .chars()
                .map(|c| if "*?[]!{}\\".contains(c) { '?' } else { c })
                .collect(),
        );
    }
    Ok(PathDenyPolicy::new(patterns)?)
}

fn build_agent(
    config: &RunConfig,
    name: &str,
    mode: SessionMode,
    max_turns: usize,
    store: Arc<crabber::session::MemoryStore>,
) -> Result<Agent> {
    let resolver = crate::codex_provider::CodexProvider::new(config);
    build_agent_with_resolver(
        config,
        name,
        mode,
        max_turns,
        store,
        (
            Selection {
                provider_id: "codex".into(),
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
    store: Arc<crabber::session::MemoryStore>,
    provider: (Selection, Arc<dyn crabber::providers::Resolver>),
) -> Result<Agent> {
    let readonly = matches!(&mode, SessionMode::Review(..));
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
            deny: path_deny_policy(config)?,
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
    agent_config.max_output_tokens = None;
    agent_config.model_controls.reasoning_effort = Some(config.reasoning_effort);
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
        .behavior_version("crabber-implement.codex.v3")
        .max_turns(max_turns)
        .store(store)
        .provider(provider.1)
        .config(agent_config)
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .shared_extension(Arc::new(tools), Scope::Global);
    if let SessionMode::Review(snapshot, _) = mode {
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
    #[test]
    fn credential_paths_are_denied_for_tools_and_snapshots() {
        let (_temp, mut config) = crate::review_checkpoint::tests::fixture();
        config.codex_auth_file = config.repo.join("tokens[private].json");
        std::fs::write(&config.codex_auth_file, "synthetic").unwrap();
        let deny = super::path_deny_policy(&config).unwrap();
        for path in [
            "tokens[private].json",
            ".codex/auth.json",
            "nested/.codex/auth.json",
        ] {
            assert!(
                deny.matches(std::path::Path::new(path)),
                "credential path visible: {path}"
            );
        }
        assert!(!deny.matches(std::path::Path::new("src/main.rs")));
    }

    #[tokio::test]
    async fn lagged_live_observation_has_committed_answer_and_durable_completion() {
        use crabber::session::Store;
        let store = std::sync::Arc::new(crabber::session::MemoryStore::new());
        let mut deltas = (0..600)
            .map(|_| crabber::StreamDelta::TextDelta("x".into()))
            .collect::<Vec<_>>();
        deltas.push(crabber::StreamDelta::Completed);
        let agent = crabber::Agent::builder()
            .behavior_version("lag-regression.v1")
            .store(store.clone())
            .provider(std::sync::Arc::new(crabber::FakeProvider::scripted(vec![
                deltas,
            ])))
            .config(crabber::AgentConfig::new(crabber::Selection {
                provider_id: "fake".into(),
                model_id: "fixture".into(),
            }))
            .build()
            .unwrap();
        let mut run = agent.prompt(None, "emit final text").await.unwrap();
        let session = run.session_id().clone();
        let run_id = run.run_id().clone();
        let mut events = run.events();
        run.done().await.unwrap();
        assert!(matches!(
            events.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
        ));
        assert_eq!(
            super::committed_answer(&store, &session, &run_id)
                .await
                .unwrap(),
            "x".repeat(600)
        );
        let durable = store.list_events(&session, None, 256).await.unwrap();
        assert_eq!(
            durable
                .iter()
                .filter(|event| event.kind == crabber::EventKind::TurnStarted)
                .count(),
            1
        );
        assert_eq!(
            durable
                .iter()
                .filter(|event| event.kind == crabber::EventKind::ModelResponseCompleted)
                .count(),
            1
        );
        use std::io::{Seek, Write};
        let (_temp, config) = crate::review_checkpoint::tests::fixture();
        let mut artifacts =
            crate::session_artifacts::SessionArtifacts::reserve(&config.artifacts, "lagged")
                .unwrap();
        artifacts.tools.write_all(b"prior tool\n").unwrap();
        artifacts.responses.write_all(b"prior response\n").unwrap();
        let tool_offset = artifacts.tools.stream_position().unwrap();
        let response_offset = artifacts.responses.stream_position().unwrap();
        artifacts
            .tools
            .write_all(b"partial current tool\n")
            .unwrap();
        let attempt = |id, observed| super::AttemptEvidence {
            run_id: id,
            observed_turns: observed,
            continuation_cause: None,
            responses: vec![],
            response_count: 0,
            responses_truncated: false,
            status: "running",
            error: None,
        };
        let mut stage = super::StageSession {
            parent_delegation: None,
            name: "lagged",
            execution_generation: 1,
            review_round: None,
            review_role: None,
            review_attempt: None,
            workflow_run_id: &config.run_id,
            session_id: Some(session),
            run_id: Some(run_id.clone()),
            max_turns: 256,
            observed_turns: 7,
            continuation_count: 1,
            runs: vec![attempt(crabber::core::RunId::new(), 7), attempt(run_id, 0)],
            status: "running",
            error: None,
            store,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            config: &config,
            metadata: artifacts.metadata,
        };
        let mut text = String::new();
        for event in &durable {
            stage
                .observe_event(
                    event,
                    &mut text,
                    &mut artifacts.transcript,
                    &mut artifacts.tools,
                    &mut artifacts.responses,
                )
                .unwrap();
        }
        assert_eq!(stage.observed_turns, 8);
        for _ in 0..2 {
            assert_eq!(
                stage
                    .reconcile_observation(
                        &mut artifacts.transcript,
                        &mut artifacts.tools,
                        &mut artifacts.responses,
                        tool_offset,
                        response_offset
                    )
                    .await
                    .unwrap(),
                "x".repeat(600)
            );
            assert_eq!(stage.observed_turns, 8);
            assert_eq!(stage.runs[0].observed_turns, 7);
            assert_eq!(stage.runs[1].observed_turns, 1);
            assert_eq!(stage.runs[1].response_count, 1);
            assert_eq!(
                std::fs::read_to_string(config.artifacts.join("lagged.responses.jsonl"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
            assert_eq!(
                std::fs::read_to_string(config.artifacts.join("lagged.tools.txt")).unwrap(),
                "prior tool\n"
            );
        }
        assert!(
            agent
                .close(std::time::Duration::from_secs(1))
                .await
                .local_drained()
        );
        agent.close_extensions().await.unwrap();
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
    fn test_parent() -> super::ParentDelegation {
        super::ParentDelegation {
            session_id: crabber::SessionId::new(),
            run_id: crabber::core::RunId::new(),
            call_id: crabber::core::ToolCallId::new(),
        }
    }

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
                SessionMode::Review(snapshot, test_parent()),
                2,
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
            SessionMode::Review(snapshot, test_parent()),
            2,
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
