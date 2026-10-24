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

pub async fn session(
    config: &RunConfig,
    name: &str,
    prompt: &str,
    readonly: bool,
    cancel: &dmt::CancellationToken,
) -> Result<String> {
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
        .max_turns(config.max_turns.get())
        .memory()
        .provider(Arc::new(resolver))
        .config(agent_config)
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .shared_extension(Arc::new(tools), Scope::Global)
        .build()?;
    let mut transcript = fs::File::create(config.artifacts.join(format!("{name}.txt")))?;
    let mut tool_log = fs::File::create(config.artifacts.join(format!("{name}.tools.txt")))?;
    let mut run = agent
        .prompt(None, prompt)
        .await
        .context("start Crabber session")?;
    let session_id = run.session_id().clone();
    let run_id = run.run_id().clone();
    let evidence_path = config.artifacts.join(format!("{name}.session.json"));
    let mut evidence = serde_json::json!({
        "name": name,
        "workflow_run_id": config.run_id,
        "session_id": session_id,
        "run_id": run_id,
        "max_turns": config.max_turns,
        "observed_turns": 0,
        "status": "running",
        "error": null,
    });
    write_json_atomic(&evidence_path, &evidence)?;
    let mut events = run.events();
    let mut observed_turns = 0usize;
    let mut text = String::new();
    let deadline = tokio::time::sleep(config.session_timeout);
    tokio::pin!(deadline);
    let outcome = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => { run.interrupt(); break Err(anyhow::anyhow!("session cancelled")); }
            _ = &mut deadline => { run.interrupt(); break Err(anyhow::anyhow!("session exceeded {} seconds", config.session_timeout.as_secs())); }
            event = events.recv() => {
                match event {
                    Ok(Some(event)) => {
                        if event.kind == EventKind::TurnStarted {
                            observed_turns += 1;
                            evidence["observed_turns"] = observed_turns.into();
                            if let Err(error) = write_json_atomic(&evidence_path, &evidence) {
                                run.interrupt();
                                break Err(error);
                            }
                            text.clear();
                        } else if event.kind == EventKind::TextDelta {
                            let delta = event.payload["text"].as_str().unwrap_or_default();
                            text.push_str(delta);
                            if let Err(error) = transcript.write_all(delta.as_bytes()) {
                                run.interrupt();
                                break Err(anyhow::anyhow!(error).context("write session transcript"));
                            }
                        } else if event.kind == EventKind::ToolCallSettled
                            && let Err(error) = writeln!(tool_log, "{}", event.payload) {
                            run.interrupt();
                            break Err(anyhow::anyhow!(error).context("write session tool log"));
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => { run.interrupt(); break Err(anyhow::anyhow!(error)); }
                }
            }
        }
    };
    drop(events);
    let result = run.done().await;
    let close = agent.close_extensions().await;
    let completion = outcome.and_then(|()| {
        let result = result.map_err(|error| {
            if matches!(error, RuntimeError::TurnLimit) {
                anyhow::anyhow!(
                    "Crabber session {name} reached its turn limit of {} after {observed_turns} observed model turns; use --max-turns to set a larger per-session budget. Partial edits are preserved; see {}",
                    config.max_turns,
                    config.artifacts.display()
                )
            } else {
                anyhow::anyhow!(error)
            }
        })?;
        close?;
        if result.status != RunStatus::Completed {
            bail!(
                "Crabber session {name} ended with {:?}; see {}",
                result.status,
                config.artifacts.display()
            );
        }
        Ok(())
    });
    evidence["observed_turns"] = observed_turns.into();
    evidence["status"] = if completion.is_ok() {
        "completed"
    } else {
        "failed"
    }
    .into();
    evidence["error"] = completion
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"))
        .into();
    write_json_atomic(&evidence_path, &evidence)?;
    completion?;
    Ok(text)
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}
