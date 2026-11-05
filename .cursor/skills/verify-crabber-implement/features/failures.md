# Failure handling

## Sub-features

Reject malformed review output, missing reports, contradictory approvals, genuine manual fix blockers, exhausted review budgets, and exhausted model turn budgets. Reject dirty repositories and missing plans before model execution.

An empty model answer can trigger at most two continuations in the same conversation. Reject exhausted continuation allowances, incomplete Responses and provider failures. Preserve explicit blocker explanations. See [model completion recovery](completion.md) for the focused fixture journeys.

## How to get to it (user POV)

Invoke the same CLI with `--max-reviews` to bound the loop. Errors print to stderr and return a nonzero exit code. Completed checkpoints and reports remain available for inspection. Turn-limit errors identify the configured cap and the `--max-turns` setting.

## Driving it with the CLI harness

Run `cargo test --locked --test cli -- --nocapture` and `cargo test --locked missing_and_contradictory_review_results_fail_closed`. Require each failure case to exit unsuccessfully and never print `done`. Inspect `.audit/verification/cli.txt` for the recorded assertions.

## Gotchas

Cancellation and provider failure may leave partial product edits. The CLI preserves work for inspection and does not promise crash resume. The repository lock prevents concurrent CLI writers, but does not prevent another editor or Git command from changing files.

## Bounded input and refresh failures

The [bounded review input journey](bounded-review-inputs.md) checks storage overflow, credential-denied changed paths, corrupt snapshots, and typed SDK admission proof limits. These failures cannot become automatic approval. An admission failure preserves its primary typed error even when cleanup is incomplete.

Run `cargo test --locked --test cli refresh_ -- --test-threads=2` for source identity, phase, version, corruption, symlink, final-round budget, and child cancellation checks. Run `cargo test --locked --bin crabber-implement killed_refresh_initialization -- --nocapture` for deterministic subprocess kills after origin publication, during snapshot construction, after readiness, and after one role report. Only complete matching snapshots can become ready. The source bytes remain unchanged, and subsequent strict resume uses the child's own identity.
