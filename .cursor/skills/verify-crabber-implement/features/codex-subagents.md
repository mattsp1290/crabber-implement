# Codex subscription and native subagent reviews

Run the CLI fixtures against the real binary. They supply synthetic native auth files and a loopback Responses endpoint, so no subscription usage is consumed.

```sh
mkdir -p .audit/codex-subagents
cargo test --locked --test cli -- --nocapture > .audit/codex-subagents/cli.txt 2>&1
cargo test --locked --bin crabber-implement -- --nocapture > .audit/codex-subagents/unit.txt 2>&1
```

Require raw requests to carry the configured model and reasoning effort, the Codex account/originator headers, `store: false`, and Responses input. Defaults are `gpt-5.6-terra` and medium. Context compaction must preserve the configured effort. The host reads `--codex-auth-file` or the existing Codex login path without writing, clearing, or copying refresh credentials.

Normal and refreshed reviews run through the session-scoped native `delegate_task` extension. The local parent emits stable calls for exactly the missing reserved roles, exposes no background task tools, and makes no external model request. Each role owns a separate read-only child with no delegation tools. Require two parent tool receipts and matching parent session/run/call IDs on the child session metadata. Reports must exist before successful receipts. Resuming a partial review reuses the completed role and dispatches only its missing sibling.

The unit checks exercise deterministic repeated dispatch, rejection of mismatched and duplicate role reservations, cancellation admission, credential-denied paths, and committed-answer recovery after live-event lag. The formal skill checks the actual role claim guards across cached/missing states, valid/invalid profiles and references, background/inline mode, and duplicate claims. Its mutation pass must detect a removed reservation check.

Checkpoint schema 3 binds the Codex profile, Responses protocol, model, effort, endpoint and input identity. Version 1 and version 2 records are refused for both resume and refresh. The model and effort can be selected explicitly, but all work uses the native Codex route.

After the fixture's disposable repository is gone, inspect the captured command logs and any copied workflow/checkpoint evidence. Never rebuild or mutate production files during a two-process resume journey because the executable digest is part of checkpoint identity.

## Model-side tool batching

Require every real host Responses request to set `parallel_tool_calls: true`. This allows the model to return multiple tool calls in one response. The runtime executes those calls sequentially.

```sh
cargo test --locked --test cli responses::batched_tool_calls_replay_both_results_and_preserve_errors_in_call_order -- --nocapture
```

The fixture interleaves arguments and completes calls in reverse stream order. Require the successful write and denied path escape to replay in original call order. Inspect exact call IDs, argument payloads, write output, and the `path_escape` error. This deterministic CLI journey uses the actual executable and a loopback Responses endpoint.

## Mutable-session test timeout guidance

Inspect `src/agent.rs` where `build_agent_with_resolver` creates the system prompt. Require finite framework-level test timeout guidance only when `readonly` is false. The guidance asks for a deadline shorter than the outer shell timeout and sized for the suite and repeat count. It treats timeouts as failures, preserves diagnostics, and asks the agent to diagnose before retrying without skipping tests or blindly increasing deadlines.

This is prompt guidance. The host does not enforce a framework timeout or prove the model follows it. Read-only reviewers retain their existing prompt. Real-workflow evidence must independently verify preserved assertions, completed tests, both distinct final reviewers, and the shared acceptance checks. A candidate that completes where its baseline times out provides within-budget completion evidence, not a successful-pair speedup.
