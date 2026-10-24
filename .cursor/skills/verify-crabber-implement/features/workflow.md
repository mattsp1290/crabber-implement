# Plan execution and review

## Sub-features

Implement a plan, checkpoint it, run independent reviewers, apply requested fixes, and review the new revision. Complete only with both approvals.

## How to get to it (user POV)

Run `crabber-implement <plan>` in a clean Git repository with a base commit. The CLI creates a feature branch when started on the base branch.

## Driving it with the CLI harness

Run `cargo test --locked --test cli -- --nocapture` to cover the fix loop through the real executable and tools. Run `python3 scripts/verify_live.py` for GLM-5.2 execution. Require successful exit, expected file contents and actual output, committed changes, two reviewer directories per round, and a manifest matching the reviewed revision. The live helper saves these before removing its repository.

## Gotchas

The live journey may approve on its first review. Use the deterministic fixture to require a requested fix. Readonly reviewers cannot run shell checks. The host supplies implementation summaries and tool settlement evidence for their assessment.
