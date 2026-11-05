# Failure handling

## Sub-features

Reject malformed review output, missing reports, contradictory approvals, genuine manual fix blockers, exhausted review budgets, and exhausted model turn budgets. Reject dirty repositories and missing plans before model execution.

An empty model answer can trigger at most two continuations in the same conversation. Reject exhausted continuation allowances, provider filtering, and truncated tool calls with specific diagnostics. Preserve explicit blocker explanations. See [model completion recovery](completion.md) for the focused fixture journeys.

## How to get to it (user POV)

Invoke the same CLI with `--max-reviews` to bound the loop. Errors print to stderr and return a nonzero exit code. Completed checkpoints and reports remain available for inspection. Turn-limit errors identify the configured cap and the `--max-turns` setting.

## Driving it with the CLI harness

Run `cargo test --locked --test cli -- --nocapture` and `cargo test --locked missing_and_contradictory_review_results_fail_closed`. Require each failure case to exit unsuccessfully and never print `done`. Inspect `.audit/verification/cli.txt` for the recorded assertions.

## Gotchas

Cancellation and provider failure may leave partial product edits. The CLI preserves work for inspection and does not promise crash resume. The repository lock prevents concurrent CLI writers, but does not prevent another editor or Git command from changing files.

## Transient provider transport failures

```sh
CRABBER_VERIFY_ARTIFACTS="$PWD/.audit/transport-recovery/snapshots" cargo test --locked --test cli transport_ -- --nocapture
```

`transport_outage_recovers_without_replaying_tools_or_starting_a_continuation` allows an implementation tool to write its file, then drops four HTTP requests before sending response headers. Require recovery on attempt five after the 1, 2, 4, and 8 second waits, identical request bodies for those five attempts, two implementation turns with no continuation, and a completed reviewed workflow. Its OTLP receiver requires four failed provider spans with fixed transport phase/cause categories and no URL, plan, or credential sentinels. The fixture uses no external model or credentials.

`session_deadline_interrupts_transport_backoff` keeps dropping requests. Require the two-second session deadline to interrupt backoff within six seconds, report the timeout, and leave no product edits. The SDK also tests retry exhaustion, cancellation during a wait, and truncated streams after observable output.

The five-attempt retry policy lives in `../crabber/crates/crabber-runtime/src/orchestrator/model_support.rs`. Transport classification lives in `../crabber/crates/crabber-providers/src/real/errors.rs`. The workflow graph and its separate formal proof are unchanged; these tests cover retries within a work action, which that graph proof abstracts away.

The recovered run's session, result, and telemetry artifacts are copied under `.audit/transport-recovery/snapshots/transport-recovered/` before the temporary repository is removed. Keep the command transcript alongside those files. Cleanup stops fixture listeners and removes the temporary repository; the copied evidence remains.

## Bounded input and refresh failures

The [bounded review input journey](bounded-review-inputs.md) checks storage overflow, credential-denied changed paths, corrupt snapshots, and typed SDK admission proof limits. These failures cannot become automatic approval. An admission failure preserves its primary typed error even when cleanup is incomplete.

Run `cargo test --locked --test cli refresh_ -- --test-threads=2` for source identity, phase, version, corruption, symlink, final-round budget, and child cancellation checks. Run `cargo test --locked --bin crabber-implement killed_refresh_initialization -- --nocapture` for deterministic subprocess kills after origin publication, during snapshot construction, after readiness, and after one role report. Only complete matching snapshots can become ready. The source bytes remain unchanged, and subsequent strict resume uses the child's own identity.

With SDK revision `a3fdd0ee6eb4f1c05ea973302ebeefd79e829f1a`, the two transport-retry tests above reproduce failures recorded before the bounded-input implementation. Keep them visible in full-suite output. They do not count as passing coverage, and the bounded-review work does not change their expectations.
