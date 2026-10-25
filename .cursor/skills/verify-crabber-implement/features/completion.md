# Verify model completion recovery

Use the actual CLI with its local chat-completions fixture. Update the sibling Crabber checkout with the finish-reason and durable completion-event changes before building.

```sh
mkdir -p .audit/completion
cargo test --locked --test cli completion -- --nocapture > .audit/completion/completion.txt 2>&1
cargo test --locked --test cli continuation -- --nocapture > .audit/completion/continuation.txt 2>&1
cargo test --locked --test cli truncated_tools -- --nocapture > .audit/completion/truncated-tools.txt 2>&1
cargo test --locked --test cli length_without -- --nocapture > .audit/completion/length.txt 2>&1
cargo test --locked --test cli explicit_blocker -- --nocapture > .audit/completion/blocker.txt 2>&1
cargo test --locked --test cli failed_stream_text -- --nocapture > .audit/completion/retry.txt 2>&1
```

The fixture replaces the external model endpoint. It does not require a provider key or change a user's project. It drives real Crabber sessions, file tools, the DMT workflow, and local Git commits in disposable repositories.

Require an empty reasoning-only final response to trigger a continuation in the same session and then complete real implementation and review. The incoming conversation must retain the previous task history. Require repeated empty responses to stop after the fixed allowance. Turn and time budgets apply across all continuation runs.

Require a length-truncated response containing tool calls to cause zero tool execution and no host commit. A truncated response without tool calls can continue within the same allowance. Filtering and unrelated provider failures are terminal. An explicit blocker must preserve its explanation.

The failed-stream journey emits partial text, triggers a context-overflow retry, and verifies that the failed text cannot become the accepted final answer. The continuation prompt must remain visible after context compaction. A request-count-only fixture does not prove this history contract.

Session evidence retains the shared session ID, each admitted run ID, continuation count, cumulative observed turns, finish reasons, token usage, and the final status or error. Response summaries are bounded so long sessions remain readable by the monitor. `<stage>.responses.jsonl` retains the full completed-response metadata history. These diagnostics exclude private reasoning. Earlier narration from a tool turn or failed provider attempt must never substitute for the final answer.

The fixture removes its temporary repositories and listeners. The captured logs under `.audit/completion/` survive cleanup. A failing assertion or a filtered command that ran no tests is not a passing journey. Run the complete CLI suite before delivery.
