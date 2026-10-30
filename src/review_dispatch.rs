use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use crabber::providers::{
    DeltaStream, ModelRequest, ProviderBindingError, ProviderError, Resolver, Streamer,
};
use crabber::{Agent, AgentConfig, EventKind, Selection, StreamDelta};
use crabber_extensions::delegate_task::{self, Factory, Limits, Options, Response};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    agent::{self, ParentDelegation, SessionMode},
    config::RunConfig,
    review::ReviewReport,
    review_checkpoint::{ROLES, ReviewCheckpoint},
    review_inputs::ReviewSnapshot,
};

const LOCAL_PROVIDER: &str = "local-review-dispatch";

struct InlineDelegationOnly;
#[async_trait]
impl crabber::extension::Extension for InlineDelegationOnly {
    fn id(&self) -> &'static str {
        "crabber-implement/inline-review-only"
    }
    fn version(&self) -> &'static str {
        "1"
    }
    fn config_hash(&self) -> String {
        crate::durable::digest(delegate_task::TOOL_NAME.as_bytes())
    }
    async fn install(
        &self,
        registrar: &mut crabber::extension::Registrar,
    ) -> Result<(), crabber::ExtensionError> {
        registrar.restrict_tools([delegate_task::TOOL_NAME.into()]);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoleReservation {
    Cached,
    Reserved(String),
    Claimed,
}

pub(crate) struct Reservations([RoleReservation; 2]);
impl Reservations {
    pub(crate) fn new(names: [Option<String>; 2]) -> Self {
        Self(names.map(|name| name.map_or(RoleReservation::Cached, RoleReservation::Reserved)))
    }

    pub(crate) fn claim(&mut self, profile: &str, task: &str, background: bool) -> Option<usize> {
        if background {
            return None;
        }
        let role = ROLES.iter().position(|role| role.0 == profile)?;
        match &self.0[role] {
            RoleReservation::Reserved(name) if name == task => {
                self.0[role] = RoleReservation::Claimed;
                Some(role)
            }
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Dispatch {
    names: [Option<String>; 2],
}

#[async_trait]
impl Resolver for Dispatch {
    fn binding_descriptor(
        &self,
        selection: &Selection,
    ) -> Result<crabber::core::ProviderBindingDescriptor, ProviderBindingError> {
        if selection.provider_id != LOCAL_PROVIDER || selection.model_id != "v1" {
            return Err(ProviderBindingError::Unavailable);
        }
        let identity = serde_json::to_vec(&self.names).expect("role names serialize");
        crabber::core::ProviderBindingDescriptor::new(
            LOCAL_PROVIDER,
            "v1",
            LOCAL_PROVIDER,
            "1",
            crabber::InputFingerprint::new(crate::durable::digest(&identity))
                .expect("digest is valid"),
            Default::default(),
        )
        .map_err(|_| ProviderBindingError::InvalidDescriptor)
    }

    async fn resolve(&self, _: &Selection) -> Result<Arc<dyn Streamer>, ProviderError> {
        Ok(Arc::new(self.clone()))
    }
}

impl Dispatch {
    fn calls(&self) -> Vec<(crabber::core::ToolCallId, serde_json::Value)> {
        self.names
            .iter()
            .enumerate()
            .filter_map(|(role, name)| {
                name.as_ref().map(|name| {
                    let arguments = serde_json::json!({"task":name,"profile":ROLES[role].0});
                    let call_id = format!(
                        "review_{}",
                        crate::durable::digest(arguments.to_string().as_bytes())
                    );
                    (call_id.into(), arguments)
                })
            })
            .collect()
    }

    fn deltas(
        &self,
        parts: &[&crabber::core::ContentBlock],
    ) -> Result<Vec<StreamDelta>, ProviderError> {
        use crabber::core::ContentBlock;
        let invalid = || ProviderError {
            kind: crabber::providers::ProviderErrorKind::Invalid,
            message: "review dispatch history does not match reserved calls".into(),
            retryable: false,
        };
        let calls = self.calls();
        let mut called = vec![false; calls.len()];
        let mut settled = vec![false; calls.len()];
        for part in parts {
            match part {
                ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    let index = calls
                        .iter()
                        .position(|(id, args)| {
                            id == call_id && args == arguments && name == delegate_task::TOOL_NAME
                        })
                        .ok_or_else(invalid)?;
                    if std::mem::replace(&mut called[index], true) {
                        return Err(invalid());
                    }
                }
                ContentBlock::ToolResult { call_id, .. } => {
                    let index = calls
                        .iter()
                        .position(|(id, _)| id == call_id)
                        .ok_or_else(invalid)?;
                    if !called[index] || std::mem::replace(&mut settled[index], true) {
                        return Err(invalid());
                    }
                }
                _ => {}
            }
        }
        let mut output = Vec::new();
        if called.iter().all(|seen| !seen) {
            for (call_id, arguments) in calls {
                output.extend([
                    StreamDelta::ToolCallStart {
                        call_id: call_id.clone(),
                        name: delegate_task::TOOL_NAME.into(),
                    },
                    StreamDelta::ToolCallArgsDelta {
                        call_id: call_id.clone(),
                        text: arguments.to_string(),
                    },
                    StreamDelta::ToolCallDone { call_id },
                ]);
            }
        } else if settled.iter().all(|settled| *settled) {
            output.push(StreamDelta::TextDelta("Review delegation settled.".into()));
        } else {
            return Err(invalid());
        }
        output.push(StreamDelta::Completed);
        Ok(output)
    }
}

#[async_trait]
impl Streamer for Dispatch {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ProviderError> {
        let parts = request
            .messages
            .iter()
            .flat_map(|message| message.parts.iter().map(|part| &part.content))
            .collect::<Vec<_>>();
        Ok(Box::pin(futures_util::stream::iter(self.deltas(&parts)?)))
    }
}

pub async fn run(
    config: Arc<RunConfig>,
    state: ReviewCheckpoint,
    snapshot: ReviewSnapshot,
    names: [Option<String>; 2],
    cached: [Option<ReviewReport>; 2],
    cancel: &dmt::CancellationToken,
) -> Result<[ReviewReport; 2]> {
    for (role, report) in cached.iter().enumerate() {
        if report.is_some() {
            let span = tracing::info_span!("implement.reviewer", "workflow.run_id" = %config.run_id,
                "execution.generation" = config.generation, "review.round" = state.round,
                "review.role" = ROLES[role].0, "review.attempt" = state.attempts[role], outcome = "reused");
            let _entered = span.enter();
        }
    }
    if let [Some(first), Some(second)] = cached {
        return Ok([first, second]);
    }
    let failures = Arc::new(Mutex::new([None::<String>, None]));
    let omitted_bytes = snapshot.render()?.metrics.omitted_bytes;
    let runner_config = config.clone();
    let runner_state = state.clone();
    let runner_failures = failures.clone();
    let reservations = Arc::new(Mutex::new(Reservations::new(names.clone())));
    let parent_trace = tracing::Span::current();
    let runner = Arc::new(move |request: delegate_task::Request| {
        let parent_trace = parent_trace.clone();
        let config = runner_config.clone();
        let state = runner_state.clone();
        let snapshot = snapshot.clone();
        let failures = runner_failures.clone();
        let reservations = reservations.clone();
        Box::pin(async move {
            let role = reservations.lock().unwrap().claim(
                request.profile(),
                request.task(),
                request.background(),
            );
            let Some(role) = role else {
                return Ok(Response::Rejected(
                    "review reservation is unavailable or mismatched".into(),
                ));
            };
            let result = async {
                ensure!(!request.cancellation().is_cancelled(), "review cancelled before admission");
                ensure!(crate::review_checkpoint::role_start(state.phase, true, state.report(&config.artifacts, role)?.is_some()) == Some(true), "review role is not missing");
                let prompt = state.prompt(&config.artifacts, role)?;
                let span = tracing::info_span!(parent: &parent_trace, "implement.reviewer", "workflow.run_id" = %config.run_id,
                    "execution.generation" = config.generation, "review.round" = state.round,
                    "review.role" = ROLES[role].0, "review.attempt" = state.attempts[role],
                    "review.prompt_bytes" = prompt.len(), "review.omitted_bytes" = omitted_bytes, outcome = tracing::field::Empty,
                    "otel.status_code" = tracing::field::Empty, "error.kind" = tracing::field::Empty);
                let parent = ParentDelegation::from_request(&request);
                use tracing::Instrument;
                let text = agent::session(&config, request.task(), &prompt, SessionMode::Review(snapshot.clone(), parent), request.cancellation()).instrument(span.clone()).await;
                crate::telemetry::finish_span(&span, text.is_ok(), "review_failed");
                let text = text?;
                ensure!(!request.cancellation().is_cancelled(), "review cancelled before publication");
                state.publish_role(&config, role, &text, &snapshot)?;
                Ok::<_, anyhow::Error>(())
            }.await;
            match result {
                Ok(()) => Ok(Response::Completed(format!("published {}", ROLES[role].0))),
                Err(error) => {
                    failures.lock().unwrap()[role] = Some(format!("{error:#}"));
                    Ok(Response::Failed(format!(
                        "{} review failed; inspect child session evidence",
                        ROLES[role].0
                    )))
                }
            }
        })
            as std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Response, crabber::ExtensionError>>
                        + Send,
                >,
            >
    });
    let factory = Arc::new(Factory::new(
        Options {
            runner_identity: "crabber-implement-codex-review-v3".into(),
            runner,
            limits: Limits {
                max_task_bytes: 256,
                max_profile_bytes: 32,
                max_result_bytes: 1024,
                max_in_flight: 2,
                max_wait: config.session_timeout,
                shutdown_grace: Duration::from_secs(30),
                max_tracked: 2,
                max_background_duration: Duration::from_secs(1),
                max_wait_ids: 2,
                max_wait_reply_bytes: 2048,
            },
        },
        crabber::FactoryScope::Session,
    )?);
    let mut parent_config = AgentConfig::new(Selection {
        provider_id: LOCAL_PROVIDER.into(),
        model_id: "v1".into(),
    });
    parent_config.directory = config.repo.to_string_lossy().into_owned();
    parent_config.workspace_id = config.run_id.clone();
    parent_config.title = format!("review-{}-dispatch-{}", state.round, config.generation);
    let name = parent_config.title.clone();
    let parent = Agent::builder()
        .behavior_version("crabber-implement.review-dispatch.v1")
        .max_turns(2)
        .execution_mode(crabber::runtime::ExecutionMode::Parallel { max: 2 })
        .provider(Arc::new(Dispatch { names }))
        .config(parent_config)
        .policy(Arc::new(crabber::StaticPolicy::new(
            crabber::PermissionDecision::Allow,
        )))
        .extension_factory(factory.clone())
        .shared_extension(Arc::new(InlineDelegationOnly), crabber::Scope::Global)
        .build()?;
    let mut artifacts =
        crate::session_artifacts::SessionArtifacts::reserve(&config.artifacts, &name)?;
    let mut metadata = serde_json::json!({"name":name,"provider":LOCAL_PROVIDER,"workflow_run_id":config.run_id,"execution_generation":config.generation,"review_round":state.round,"status":"starting"});
    artifacts.metadata.persist(&metadata)?;
    let result = async {
        ensure!(!cancel.is_cancelled(), "review dispatch cancelled");
        let mut run = parent
            .prompt_with_context(
                None,
                "Dispatch the reserved missing review roles.",
                crabber::obs::Telemetry::current_trace_context(),
            )
            .await?;
        metadata["session_id"] = serde_json::to_value(run.session_id())?;
        metadata["run_id"] = serde_json::to_value(run.run_id())?;
        metadata["status"] = "running".into();
        artifacts.metadata.persist(&metadata)?;
        let mut events = run.events();
        let outcome = async {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => anyhow::bail!("review dispatch cancelled"),
                    event = events.recv() => event?,
                };
                let Some(event) = event else { break };
                if matches!(event.kind, EventKind::ToolCallSettled) {
                    writeln!(artifacts.tools, "{}", event.payload)?;
                } else if matches!(event.kind, EventKind::ModelResponseCompleted) {
                    writeln!(artifacts.responses, "{}", event.payload)?;
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if outcome.is_err() {
            run.interrupt();
        }
        let done = run.done().await;
        outcome?;
        ensure!(
            done?.status == crabber::core::RunStatus::Completed,
            "review dispatcher did not complete"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let close = parent.close(Duration::from_secs(30)).await;
    let extensions = parent.close_extensions().await;
    let cleanup = (|| {
        ensure!(
            close.local_drained(),
            "review dispatcher cleanup remains incomplete"
        );
        extensions?;
        Ok::<_, anyhow::Error>(())
    })();
    let result = match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("also failed dispatcher cleanup: {cleanup:#}")))
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), cleanup) => cleanup,
    };
    metadata["status"] = if result.is_ok() {
        "completed"
    } else {
        "failed"
    }
    .into();
    metadata["error"] = result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"))
        .into();
    match (result, artifacts.metadata.persist(&metadata)) {
        (Err(error), Err(persist)) => {
            return Err(error.context(format!(
                "also failed dispatcher metadata publication: {persist:#}"
            )));
        }
        (Err(error), Ok(())) => return Err(error),
        (Ok(()), persist) => persist?,
    }
    let failures = failures.lock().unwrap();
    let reports = [
        state.report(&config.artifacts, 0)?,
        state.report(&config.artifacts, 1)?,
    ];
    Ok([
        reports[0].clone().with_context(|| {
            failures[0]
                .clone()
                .unwrap_or_else(|| "correctness review did not publish".into())
        })?,
        reports[1].clone().with_context(|| {
            failures[1]
                .clone()
                .unwrap_or_else(|| "maintainability review did not publish".into())
        })?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabber::core::ContentBlock;

    #[test]
    fn repeated_requests_dispatch_identical_calls_and_only_complete_after_settlement() {
        let dispatch = Dispatch {
            names: [
                Some("review-1-correctness-attempt-1".into()),
                Some("review-1-maintainability-attempt-1".into()),
            ],
        };
        let first = dispatch.deltas(&[]).unwrap();
        let repeated = dispatch.deltas(&[]).unwrap();
        let call_ids = |deltas: Vec<StreamDelta>| {
            deltas
                .into_iter()
                .filter_map(|delta| match delta {
                    StreamDelta::ToolCallStart { call_id, name } => {
                        assert_eq!(name, delegate_task::TOOL_NAME);
                        Some(call_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(call_ids(first), call_ids(repeated));
        let calls = dispatch.calls();
        let mut history = calls
            .iter()
            .map(|(id, args)| ContentBlock::ToolCall {
                call_id: id.clone(),
                name: delegate_task::TOOL_NAME.into(),
                arguments: args.clone(),
            })
            .collect::<Vec<_>>();
        assert!(
            dispatch
                .deltas(&history.iter().collect::<Vec<_>>())
                .is_err()
        );
        for (id, _) in calls {
            history.push(ContentBlock::ToolResult {
                call_id: id,
                content: vec![],
                is_error: false,
            });
        }
        let completed = dispatch
            .deltas(&history.iter().collect::<Vec<_>>())
            .unwrap();
        assert!(matches!(
            &completed[..],
            [StreamDelta::TextDelta(_), StreamDelta::Completed]
        ));
        history.push(ContentBlock::ToolResult {
            call_id: "unreserved".into(),
            content: vec![],
            is_error: false,
        });
        assert!(
            dispatch
                .deltas(&history.iter().collect::<Vec<_>>())
                .is_err()
        );
    }
}
