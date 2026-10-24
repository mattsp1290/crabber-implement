use anyhow::{Context, Result, bail};
use crabber::{
    Agent, AgentConfig, EventKind, PermissionDecision, Selection, StaticPolicy,
    core::RunStatus,
    extension::Scope,
    providers::{HttpAdapter, HttpResolver, Protocol},
};
use crabber_tools_catalog::prelude::*;
use std::{fs, path::Path, sync::Arc, time::Duration};

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
        .memory()
        .provider(Arc::new(resolver))
        .config(agent_config)
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .shared_extension(Arc::new(tools), Scope::Global)
        .build()?;
    let mut run = agent
        .prompt(None, prompt)
        .await
        .context("start Crabber session")?;
    let mut events = run.events();
    let mut text = String::new();
    let mut transcript = String::new();
    let mut tool_log = String::new();
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
                            text.clear();
                        } else if event.kind == EventKind::TextDelta {
                            let delta = event.payload["text"].as_str().unwrap_or_default();
                            text.push_str(delta);
                            transcript.push_str(delta);
                        } else if event.kind == EventKind::ToolCallSettled {
                            tool_log.push_str(&serde_json::to_string(&event.payload)?);
                            tool_log.push('\n');
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
    write_text(&config.artifacts.join(format!("{name}.txt")), &transcript)?;
    write_text(
        &config.artifacts.join(format!("{name}.tools.txt")),
        &tool_log,
    )?;
    outcome?;
    let result = result?;
    close?;
    if result.status != RunStatus::Completed {
        bail!(
            "Crabber session {name} ended with {:?}; see {}",
            result.status,
            config.artifacts.display()
        );
    }
    Ok(text)
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}
