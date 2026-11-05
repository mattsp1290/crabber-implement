# Datadog APM telemetry

## Observable behavior

The executor exports metadata-only OTLP traces to a local agent by default. `telemetry.json` contains its root trace ID and finite local exporter status. Workflow IDs correlate stage/session/attempt spans and all SDK trace resources. Model/tool timing and token attributes are exported without plan text, prompts, arguments/results, raw errors, paths, or reasoning.

## Credential-free wire verification

```sh
cargo test --locked --test cli telemetry_ -- --nocapture
```

Require decoded HTTP/protobuf requests on `/v1/traces`, service/version resource tags, main execution trace parentage across spawned DMT handlers and concurrent reviewers, and distinct continuation attempts. Secret sentinels must be absent from every exported payload. A duplicate-tool-start failure and cancellation must export closed workflow spans before exit. Disabled telemetry must send nothing. Invalid configuration, unavailable collectors, and hanging collectors must preserve application outcome and terminate within the tested bound. Most fixture journeys explicitly disable export. The transport recovery journey in [failure handling](failures.md) also checks exported `transport.phase`, `transport.cause`, and `transport.io_kind` categories against a local OTLP receiver.

The receiver assertions distinguish background SDK lifecycle spans from the main execution tree; all resources still carry `workflow.run_id`. A successful SDK shutdown is recorded as `flushed` with `delivery: unverified`. Sampling can suppress all requests; a collector can reject spans in a successful HTTP response. The SDK does not surface those partial-rejection counts, so backend retrieval is the delivery gate.

## Local agent delivery

Confirm the local agent's HTTP trace receiver is enabled, not just listening for logs. For the normal default endpoint, `otlp_config.receiver.protocols.http.endpoint` is `127.0.0.1:4318` and `otlp_config.traces.enabled` is true. Never print the agent's API key or full configuration. Restarting the system service may require the operator's macOS password.

```sh
mkdir -p .audit/datadog/live
CRABBER_VERIFY_ARTIFACTS="$PWD/.audit/datadog/live" cargo test --locked --test cli telemetry_local_agent_smoke -- --ignored --nocapture
```

This submits one synthetic duplicate-tool-call failure via a local fake model provider. Require exporter status `flushed`, retain the copied artifacts after scratch cleanup, and find its printed workflow ID in US3 Datadog APM using Pup. Read-only Pup searches use `DD_TOKEN_STORAGE=file` and `DD_SITE=us3.datadoghq.com`; do not print tokens. Keep capture time bounds and query limits small. Distinguish local receipt from backend retrieval. Do not run the user's project or model plan for this check.

## Cleanup and evidence

Stop only test-owned listeners/processes. Keep decoded assertions, exporter state, source revisions, API query summaries, and trace IDs under `.audit/datadog/`. Do not copy credentials into evidence. If the agent cannot be restarted or backend access fails, preserve the working code and exact blocker instead of claiming end-to-end delivery.

For bounded reviews, reviewer spans export numeric `review.prompt_bytes` and `review.omitted_bytes`. Refresh workflows expose `workflow.parent_run_id` and recovery mode `refresh`; `telemetry.json` also records the parent UUID. Snapshot contents and arbitrary artifact paths stay private. Run `cargo test --locked --test cli admission -- --nocapture` to verify actual SDK proof-limit metadata through the CLI and scrubbed OTLP. Keep ordinary invalid-input failures distinct from typed proof exhaustion. Local task drainage does not prove unresolved preparation state was reconciled.
