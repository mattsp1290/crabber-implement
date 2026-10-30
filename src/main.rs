mod agent;
mod codex_auth;
mod codex_provider;
mod config;
mod durable;
#[cfg(test)]
mod formal_verification;
mod git;
mod review;
mod review_artifacts;
mod review_checkpoint;
mod review_dispatch;
mod review_inputs;
mod session_artifacts;
mod telemetry;
mod workflow;

use anyhow::Result;
use clap::Parser;
use tracing::Instrument;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = config::Cli::parse();
    let disabled = cli.no_telemetry
        || std::env::var("OTEL_SDK_DISABLED").is_ok_and(|value| value == "true" || value == "1");
    let mut config = config::RunConfig::load(cli)?;
    let cancellation = dmt::CancellationToken::new();
    let startup_cancel = cancellation.clone();
    let mut startup = tokio::task::spawn_blocking(move || {
        review_checkpoint::begin_execution_cancellable(&mut config, &startup_cancel)?;
        Ok::<_, anyhow::Error>(config)
    });
    let config = tokio::select! {
        result = &mut startup => result??,
        _ = tokio::signal::ctrl_c() => {
            cancellation.cancel();
            startup.await??;
            anyhow::bail!("review initialization cancelled");
        }
    };
    let artifacts = config.artifacts.clone();
    let mut telemetry = telemetry::RunTelemetry::start(
        disabled,
        &config.run_id,
        config.generation,
        config.recovery_origin.as_ref(),
    );
    let span = tracing::info_span!("implement.workflow", "workflow.run_id" = %config.run_id,
        "workflow.parent_run_id" = config.recovery_origin.as_ref().map(|o| o.parent_workflow_id.as_str()).unwrap_or(""), "recovery.mode" = if config.recovery_origin.is_some() { "refresh" } else { "" }, "model.id" = %config.model, "execution.generation" = config.generation, "predecessor.trace_id" = config.predecessor_trace.as_deref().unwrap_or(""), "workflow.status" = tracing::field::Empty,
        outcome = tracing::field::Empty, "otel.status_code" = tracing::field::Empty,
        "error.kind" = tracing::field::Empty);
    telemetry.record_start(&artifacts, &span);
    let result = workflow::run(config).instrument(span.clone()).await;
    telemetry::finish_span(&span, result.is_ok(), "workflow_failed");
    drop(span);
    telemetry.finish(&artifacts).await;
    result
}
