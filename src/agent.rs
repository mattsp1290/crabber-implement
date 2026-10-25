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

const MAX_CONTINUATIONS: usize = 2;
const RESPONSE_SUMMARY_LIMIT: usize = 8;
const CONTINUATION_PROMPT: &str = "Continue the assigned task from the existing session and workspace. Complete any remaining work and return a visible final answer with the result and verification, or begin your answer with BLOCKED and explain the blocker. Do not repeat completed work.";

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
    responses: Vec<ResponseEvidence>,
    response_count: usize,
    responses_truncated: bool,
    status: &'static str,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct StageSession<'a> {
    name: &'a str,
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
}

pub async fn session(
    config: &RunConfig,
    name: &str,
    prompt: &str,
    readonly: bool,
    cancel: &dmt::CancellationToken,
) -> Result<String> {
    let mut stage = StageSession {
        name,
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
    };
    stage.persist()?;
    let result = stage.execute(prompt, readonly, cancel).await;
    stage.status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    stage.error = result.as_ref().err().map(|error| format!("{error:#}"));
    stage.persist()?;
    result
}

impl StageSession<'_> {
    fn persist(&self) -> Result<()> {
        write_json_atomic(
            &self
                .config
                .artifacts
                .join(format!("{}.session.json", self.name)),
            self,
        )
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
        readonly: bool,
        cancel: &dmt::CancellationToken,
    ) -> Result<String> {
        let mut transcript =
            fs::File::create(self.config.artifacts.join(format!("{}.txt", self.name)))?;
        let mut tool_log = fs::File::create(
            self.config
                .artifacts
                .join(format!("{}.tools.txt", self.name)),
        )?;
        let mut response_log = fs::File::create(
            self.config
                .artifacts
                .join(format!("{}.responses.jsonl", self.name)),
        )?;
        loop {
            self.check_active(cancel)?;
            let remaining = self.max_turns.saturating_sub(self.observed_turns);
            if remaining == 0 {
                return Err(self.turn_limit());
            }
            let agent = build_agent(
                self.config,
                self.name,
                readonly,
                remaining,
                self.store.clone(),
            )?;
            let result = self
                .attempt(
                    &agent,
                    if self.continuation_count == 0 {
                        prompt
                    } else {
                        CONTINUATION_PROMPT
                    },
                    cancel,
                    &mut transcript,
                    &mut tool_log,
                    &mut response_log,
                )
                .await;
            let close = agent.close_extensions().await;
            let continuation = result?;
            close?;
            self.check_active(cancel)?;
            if let Some(text) = continuation {
                return Ok(text);
            }
            if self.observed_turns >= self.max_turns {
                return Err(self.turn_limit());
            }
            if self.continuation_count == MAX_CONTINUATIONS {
                bail!(
                    "Crabber session {} returned no final answer after {} continuations; partial edits are preserved; see {}",
                    self.name,
                    self.continuation_count,
                    self.config.artifacts.display()
                );
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
    ) -> Result<Option<String>> {
        self.check_active(cancel)?;
        let admission = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("session cancelled")),
            _ = tokio::time::sleep_until(self.deadline) => Err(anyhow::anyhow!("session exceeded {} seconds", self.config.session_timeout.as_secs())),
            run = agent.prompt(self.session_id.clone(), prompt) => Ok(run),
        };
        let mut run = match admission {
            Ok(run) => run.context("start Crabber session")?,
            Err(error) => {
                let cleanup = agent.close(Duration::from_secs(30)).await;
                if !cleanup.local_drained() {
                    return Err(error.context("admission cleanup remains incomplete"));
                }
                return Err(error);
            }
        };
        self.session_id = Some(run.session_id().clone());
        self.run_id = Some(run.run_id().clone());
        self.runs.push(AttemptEvidence {
            run_id: run.run_id().clone(),
            observed_turns: 0,
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
                    Ok(None)
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
                        (response.completion.has_visible_text && !text.trim().is_empty())
                            .then_some(text),
                    )
                }
            }
        })();
        let attempt = self.runs.last_mut().unwrap();
        attempt.status = match &result {
            Ok(Some(_)) => "completed",
            Ok(None) => "continuation_required",
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

fn build_agent(
    config: &RunConfig,
    name: &str,
    readonly: bool,
    max_turns: usize,
    store: Arc<crabber::session::MemoryStore>,
) -> Result<Agent> {
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
            deny: PathDenyPolicy::new(
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
            )?,
            enabled: EnabledSet::only(enabled),
            restrict_to_enabled: true,
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
    let resolver = HttpResolver::new().with_adapter(
        HttpAdapter::opencode_go(Protocol::ChatCompletions).with_base_url(&config.base_url),
    );
    let mut agent_config = AgentConfig::new(Selection {
        provider_id: "opencode-go".into(),
        model_id: config.model.clone(),
    });
    agent_config.directory = config.repo.to_string_lossy().into_owned();
    agent_config.workspace_id = config.run_id.clone();
    agent_config.title = name.into();
    agent_config.max_output_tokens = Some(16_384);
    agent_config.system_prompt = Some(format!(
        "You are a coding agent working in {}. Follow applicable AGENTS.md instructions. Complete the assigned task using the available tools. Never push, deploy, contact others, change branches, commit, or modify Git metadata. The host owns commits. Do not read credentials. Shell commands run locally with the user's access and are not sandboxed. Report uncertainty and failures honestly. Stop rather than claiming success when blocked.",
        config.repo.display()
    ));
    let agent = Agent::builder()
        .behavior_version("crabber-implement.v1")
        .max_turns(max_turns)
        .store(store)
        .provider(Arc::new(resolver))
        .config(agent_config)
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .shared_extension(Arc::new(tools), Scope::Global)
        .build()?;
    Ok(agent)
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}
