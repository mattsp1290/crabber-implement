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
