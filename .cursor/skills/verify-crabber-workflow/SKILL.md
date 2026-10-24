---
name: verify-crabber-workflow
description: Exhaustively check crabber-implement workflow completion and review-budget safety after graph, outcome-label, or occurrence-guard changes.
---

# Verify workflow control flow

Run from the repository root.

```sh
python3 scripts/verify_formal.py
```

The command runs `formal_workflow_reachability` against the actual graph returned by `workflow::document` and compiled by `flows_core::compile`. It needs the Rust dependencies already declared by this project and Python 3. It makes no model requests and needs no credential.

## Property and harm

Every completed workflow has implemented the plan and received a fresh approval from review. Every possible sequence of work outcomes terminates within its review budget. A broken edge or occurrence guard could otherwise report success after requested changes or run agents without a bound.

## Coverage record

The checker is `crabber-workflow-checker-v1` in `src/formal_verification.rs`. The covered product code is `src/workflow.rs`, specifically `document` and `occurrence_allowed`. Read the handler's `execute` and `review` methods when changing either function to confirm their outcome labels still mean what the checker models.

The synchronization commit is `3b07798647ecc4ce4028140e2ce7f851deb6edfb`. Its covered source bytes match the hashes from the passing `.audit/formal/result.json`. That record also captures tool versions. Move this commit forward only after confirming the checker still describes the covered implementation and rerunning the command.

## Tool and scope

The tool is an explicit finite-state reachability checker written in Rust. It explores the compiled graph itself, rather than a separately transcribed graph. Breadth-first exploration retains the shortest trace to each abstract state. A topological check rejects reachable cycles and computes the longest path to each state. The initial verified toolchain is rustc 1.99.1, Cargo 1.99.1 and Python 3.14.5; each run records its actual versions.

The checker covers every integer review budget from 1 through 100, matching the CLI's supported range. For each budget it explores all declared successful outcome labels and a permanent-error outcome at each task. State records the current node, successful implementation, review freshness, and the review and fix counts. It uses the runtime's occurrence predicate before starting a modeled task. It checks a path bound of `2 * budget + 6` states.

This is a bounded control-flow proof under these assumptions:

- Work tasks settle with one declared label or a permanent error. Their internal tool execution is one abstract action.
- `implemented` means the runtime has accepted the implementation checkpoint. `approved` means the host has accepted both reviewer reports. Report honesty and plan completeness are outside this model.
- The runtime calls `occurrence_allowed` before each work session. The checker tests that predicate, while its placement in the handler remains a source-review obligation.
- Work-task retries stay disabled. The checker rejects a compiled work task whose maximum attempts differs from one. It does not model lease loss, process crashes, cancellation races or concurrent external Git writes.
- Injected acquire and release hooks are local pass-through operations. Their retry policies are outside the model because they perform no repository mutation.
- The model allows a final-round `changes` outcome to reach a fix before a later occurrence fails. The runtime rejects that outcome earlier. This adds paths to the model and does not remove an actual success path.

This check does not prove the agent implements a plan correctly, that review Markdown agrees with its structured verdict, or that shell commands remain inside the repository.

## Evidence and failure handling

`.audit/formal/transcript.txt` contains the checker output. `.audit/formal/result.json` contains source hashes, compiler versions, bounds and exit status. A successful run contains `safety=PASS termination=PASS`; the wrapper fails if no check actually ran. Cleanup does not remove either file.

A failed assertion prints its budget and shortest path. For example, routing fix-review's `fixed` label to done reaches completion before a fresh review approves the fixes. Inspect the referenced actual graph edge before deciding whether a failure is a product defect or a stale model. New stages or labels intentionally fail as unmodeled until their semantics are added.

## Mutation proof

Before changing `src/workflow.rs` for a mutation proof, stop other source writers and save the exact file bytes outside the repository. Change the `fix-review` edge with label `fixed` from target `review` to target `done`. Run the command with `--evidence .audit/formal-mutation-edge` and require a nonzero exit with `completed without fresh approval`. Restore the original bytes and rerun the normal command.

For a separate budget check, change `occurrence_allowed` to always return true. Require a nonzero exit with `session budget exceeded`, restore the exact bytes and rerun. Never retain either mutation. Preserve the failed-run transcripts with the passing-run evidence.

Use `/maintain-formal-verification-skill` for a dedicated synchronization pass after covered code changes.
