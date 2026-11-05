# Feature map

- [Plan execution and review](workflow.md) covers implementation, two reviewers, fixes, and completion.
- [Plan input](plan-input.md) covers Markdown files and ordered implementation-plan directories.
- [Failure handling](failures.md) covers invalid reports, exhausted review budgets, invalid inputs, bounded transport retries, and cancellation during backoff.
- [Model turn budget](turn-budget.md) covers long implementations, explicit caps, and session evidence.
- [Live React monitor](monitor.md) covers current nodes, agent output, review loops, archived runs, stale heartbeats, errors, and Docker browser verification.
- [Model completion recovery](completion.md) covers empty final answers, shared budgets, and safe failure diagnostics.

[Codex subagents](codex-subagents.md) covers the subscription route, native delegation, parent linkage, and legacy checkpoint rejection.

- [Datadog telemetry](telemetry.md) covers OTLP traces, correlation, redaction, exporter failures, and local-agent delivery.

- [Review recovery](review-recovery.md) covers durable role reuse, explicit process restart, global rounds, credential policy, explicit fresh child reviews, inherited evidence, and monitor generations.

- [Bounded review inputs](bounded-review-inputs.md) covers final prompt and snapshot limits, omitted evidence retrieval, linked worktrees, input integrity, and typed admission errors.
