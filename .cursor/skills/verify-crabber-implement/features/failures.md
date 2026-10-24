# Failure handling

## Sub-features

Reject malformed review output, missing reports, contradictory approvals, manual fix requirements, and exhausted review budgets. Reject dirty repositories and missing plans before model execution.

## How to get to it (user POV)

Invoke the same CLI with `--max-reviews` to bound the loop. Errors print to stderr and return a nonzero exit code. Completed checkpoints and reports remain available for inspection.

## Driving it with the CLI harness

Run `cargo test --locked --test cli -- --nocapture` and `cargo test --locked missing_and_contradictory_review_results_fail_closed`. Require each failure case to exit unsuccessfully and never print `done`. Inspect `.audit/verification/cli.txt` for the recorded assertions.

## Gotchas

Cancellation and provider failure may leave partial product edits. The CLI preserves work for inspection and does not promise crash resume. The repository lock prevents concurrent CLI writers, but does not prevent another editor or Git command from changing files.
