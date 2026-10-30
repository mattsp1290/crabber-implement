---
name: verify-crabber-implement
description: Build and drive the crabber-implement CLI through plan execution, review, fixes, and failure handling. Use after changing its user-facing behavior.
---

# Verify crabber-implement

## Launch

Run from the repository root with sibling checkouts `../crabber` and `../dmt-flows` present.

```sh
cargo build --locked
./target/debug/crabber-implement --help
```

The executable prints `Usage: crabber-implement [OPTIONS] <PLAN>`. Help lists model `gpt-5.6-terra`, reasoning effort `medium`, `--codex-auth-file`, and a session timeout bounded at 3600 seconds. Each invocation exits when its workflow finishes. The React monitor is separate. Git and ripgrep 14 or later are required. Live requests use the existing Codex ChatGPT subscription through native `auth.json`; no API key or OpenCode provider is supported.

## Doctor

```sh
./target/debug/crabber-implement --version
rg --version
```

Expect `crabber-implement 0.1.0` and ripgrep 14 or later. Before an authorized live journey, sign in through `codex login` and confirm its login file exists without printing its contents. An expired access token requires refreshing the login through Codex. The host only reads access-token/account/expiry metadata and never writes or clears the file.

## Drive

Use the real CLI with a local protocol fixture for deterministic coverage. The fixture replaces only the external model endpoint. The DMT engine, Crabber runtime, tool calls and Git operations remain real.

```sh
mkdir -p .audit/verification
cargo test --locked --test cli -- --nocapture > .audit/verification/cli.txt 2>&1
```

For an authorized real-provider check, use the existing Codex login and run the live helper. Never print or copy token values.

```sh
python3 scripts/verify_live.py --evidence .audit/live
```

This helper creates a disposable Git repository and a plan. It runs the actual executable with `gpt-5.6-terra` with medium reasoning, then independently runs the requested Python program and compares its output. It copies workflow and review artifacts before deleting its scratch repository. A failed assertion means the journey failed. Read the captured stderr and result rather than assuming a model summary proves success.

For a turn-limit regression, run the focused [model turn budget journey](features/turn-budget.md). It exercises more than 64 model requests through the actual executable and verifies explicit-budget failure. It uses no live provider credential.

For premature completion or retry changes, run the [model completion journey](features/completion.md). It checks bounded empty-completion continuations, shared budgets, preserved post-compaction prompts, and final-answer isolation from failed attempts.

For the React monitor, run the [Docker and Playwright journey](features/monitor.md). It launches the production monitor in Docker, drives real artifact updates through a browser, and observes the actual CLI before and after a paused model response. No provider credential is needed.

## Evidence

Deterministic evidence is `.audit/verification/cli.txt`. Live evidence is `.audit/live/result.json`, `stdout.txt`, `stderr.txt`, `program.txt`, `hello.py`, `git-log.txt`, `git-status.txt`, and the copied review and workflow directories. Require a successful CLI exit, the expected program output, review manifests tied to the resulting revision, and stage records. Each stage also records a `.session.json` with session and run IDs, configured turn budget, observed turns, and outcome, including failed runs. A model's implementation summary alone is insufficient.

The live helper uses the default external skill checkout under `~/git/dotfiles/.agents/skills`. Its exact skill text can change results. The fixture uses isolated skill files to keep protocol tests deterministic. Shell tools execute on the host. The CLI's acquire and release graph hooks do not create a sandbox.

## Cleanup

The fixture and live helper remove only their own temporary repositories and listeners. Evidence under `.audit/` survives cleanup. Confirm it after running.

```sh
python3 -c 'from pathlib import Path; assert Path(".audit/live/result.json").is_file()'
```

Each live invocation costs provider usage. Do not rerun the live journey solely because documentation changed. Update this skill and its feature map when behavior changes. Use `/maintain-verification-skill` for a dedicated maintenance pass.

Fix sessions use the bundled autonomous policy. Verify sensitive-source auto-fixes and genuine blockers through `features/workflow.md`; the external skills directory needs only `review/SKILL.md`.

Datadog APM traces default to the loopback agent receiver. Follow `features/telemetry.md` for credential-free wire assertions and an explicit local-agent delivery check. Use `--no-telemetry` for manual runs that must not export.

For checkpoint, resume, reviewer identity or rendering changes, run [review recovery](features/review-recovery.md). Preserve the same binary throughout each two-process journey; do not run source mutation checks concurrently with browser or CLI recovery tests.

For final reviewer prompts, snapshots, artifact tools, or input caps, run [bounded review inputs](features/bounded-review-inputs.md). For an upgraded executable, use the explicit refresh journey in [review recovery](features/review-recovery.md). Normal invocation, strict resume, and refresh have separate acceptance checks. Never run source mutations while a browser or strict-resume journey depends on an unchanged executable.

For the Codex route and native review ownership, run [Codex subagents](features/codex-subagents.md). The local coordinator calls the native extension through a real parent Agent; each child records its parent session/run/call IDs. Strict resume and refresh accept only checkpoint version 3. All older provider records are rejected before model calls.
