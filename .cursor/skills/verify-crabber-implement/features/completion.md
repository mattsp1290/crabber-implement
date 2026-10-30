# Verify model completion recovery

Use the actual CLI with its local Codex Responses fixture and synthetic native credentials.

```sh
mkdir -p .audit/completion
cargo test --locked --test cli completion -- --nocapture > .audit/completion/completion.txt 2>&1
cargo test --locked --test cli continuation -- --nocapture > .audit/completion/continuation.txt 2>&1
cargo test --locked --test cli explicit_blocker -- --nocapture > .audit/completion/blocker.txt 2>&1
cargo test --locked --test cli failed_stream_text -- --nocapture > .audit/completion/retry.txt 2>&1
```

The fixture replaces the external model endpoint. It does not require a provider key or change a user's project. It drives real Crabber sessions, file tools, the DMT workflow, and local Git commits in disposable repositories.

Require an empty reasoning-only final response to trigger a continuation in the same session and then complete real implementation and review. The incoming conversation must retain the previous task history. Require repeated empty responses to stop after the fixed allowance. Turn and time budgets apply across all continuation runs.

Incomplete Responses and provider failures are terminal. An explicit blocker must preserve its explanation. The host does not escalate output caps.

The failed-stream journey emits partial text, triggers a context-overflow retry, and verifies that the failed text cannot become the accepted final answer. The continuation prompt must remain visible after context compaction. A request-count-only fixture does not prove this history contract.

Session evidence retains the shared session ID, each admitted run ID, continuation count, cumulative observed turns, finish reasons, token usage, and the final status or error. Response summaries are bounded so long sessions remain readable by the monitor. `<stage>.responses.jsonl` retains the full completed-response metadata history. These diagnostics exclude private reasoning. Earlier narration from a tool turn or failed provider attempt must never substitute for the final answer.

The fixture removes its temporary repositories and listeners. The captured logs under `.audit/completion/` survive cleanup. A failing assertion or a filtered command that ran no tests is not a passing journey. Run the complete CLI suite before delivery.

The old ChatCompletions length/cap and transport-backoff cases were removed with that provider. The Codex codec reports incomplete Responses as failures and does not expose the finish cause needed to justify output escalation. These removed cases are not claimed as Codex coverage.
