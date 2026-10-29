# Plan execution and review

## Sub-features

Implement a plan, checkpoint it, run independent reviewers, apply requested fixes, and review the new revision. Complete only with both approvals.

## How to get to it (user POV)

Run `crabber-implement <plan>` in a clean Git repository with a base commit. The CLI creates a feature branch when started on the base branch.

## Driving it with the CLI harness

Run `cargo test --locked --test cli -- --nocapture` to cover the fix loop through the real executable and tools. Run `python3 scripts/verify_live.py` for GLM-5.2 execution. Require successful exit, expected file contents and actual output, committed changes, two reviewer directories per round, and a manifest matching the reviewed revision. The live helper saves these before removing its repository.

## Gotchas

The live journey may approve on its first review. Use the deterministic fixture to require a requested fix. Readonly reviewers cannot run shell checks. The host supplies implementation summaries and tool settlement evidence for their assessment.

## Autonomous fix policy

Run `cargo test --locked --test cli autonomous -- --nocapture`. Require a config-file finding to be edited through real file tools, committed on the feature branch, and approved by two fresh reviewers. The final review manifest must match the fixed HEAD, and the base branch must remain unchanged. The bundled `skills/fix-review-autonomous/SKILL.md` must be used whether an external interactive fix-review skill is missing or contains conflicting manual-only instructions. A genuine `needs-manual` result still stops before a fix commit or a new review.

Validate the skill with `python3 "$HOME/.codex/skills/.system/skill-creator/scripts/quick_validate.py" skills/fix-review-autonomous` when that authoring tool is available. Run an isolated policy evaluation with synthetic configuration and recorded review artifacts when changing the policy's authorization boundary. Never use the user's target project for this evaluation.
