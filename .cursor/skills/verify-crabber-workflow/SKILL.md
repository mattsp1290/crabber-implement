---
name: verify-crabber-workflow
description: Exhaustively check Codex native delegation, workflow budgets, snapshot readiness and checkpoint transition safety.
---

# Verify native delegation and workflow control

Run from the repository root with the sibling Rust dependencies and Python 3.

```sh
python3 scripts/verify_formal.py --evidence .audit/codex-subagents/formal
```

The checker uses no model requests or credentials. Stop source writers and live CLI journeys before mutations. Each mutation rebuilds the binary, which changes the executable identity used by strict resume.

```sh
python3 scripts/verify_bounded_mutations.py --evidence .audit/codex-subagents/mutations
```

## Properties and harm

Completion requires implementation and two matching reviewer reports. Each missing review role can be claimed once through an exact immutable reservation. Cached roles, mismatched role/task references, duplicate claims and background delegation cannot start children. The local controller derives its tool calls from request history; repeating a request cannot advance an internal script cursor.

Only version 3 Codex native-subagent checkpoints can resume or refresh. Model, reasoning effort, Codex profile, Responses protocol, endpoint and input identity are bound to the checkpoint. Old provider records cannot be imported. A refreshed child obtains both reports from fresh attempts; source reports are evidence only. Global review rounds never reset, and the final global review cannot authorize another fix.

Refresh accepts only an incomplete read-only review. Source identity, source records, published origin and immutable snapshot must validate before readiness. Reader integrity failures prevent report publication. Mutable fix intent and completed workflows cannot become refresh sources. These guards prevent stale approval, replay of unknown mutations and budget resets.

## Covered code and synchronization

`src/formal_verification.rs` compiles the actual graph from `workflow::document_from` and calls production occurrence, round, readiness, identity and phase-transition guards. The native delegation check calls `review_dispatch::Reservations::claim`, the same method used by the mounted extension runner. The actual-loader test in `review_checkpoint/tests.rs` rewrites otherwise valid checkpoint records to versions 1 and 2 and checks both resume and refresh refusal.

The final prompt-bound check calls production `review_inputs::validate_prompt` at the exact 256 KiB boundary and one-byte overflow using ordinary, escaped, control-character and multibyte text. An actual-constructor check publishes two source reports, initializes a refreshed child, and verifies absent cached child reports and unchanged source bytes.

The Codex synchronization baseline is commit `60054fc5b672bc9b82e0f55b1e86df3315b3ce0f`. Verification applies to the subsequent working-tree implementation identified by exact source hashes in `.audit/codex-subagents/formal-restored/result.json`. The baseline commit alone does not contain this implementation. Rerun after checkout or commit to record the current revision and hashes. The wrapper records hashes for dispatch, workflow, checkpoint, snapshot, artifact reader, config, Git capture, durable publication, report rendering and the checker, and requires them to remain unchanged throughout verification.

The checked tools are Rust 1.99.1, Cargo 1.99.1 and Python 3.14.5. Each wrapper run records its actual versions.

## Bounds and model

`crabber-workflow-checker-v3` explores budgets 1..100 and normal entries plus resume/refresh entries for every source round. It reaches 1,404,000 states and 1,414,200 terminal cases. Breadth-first reachability retains shortest counterexamples. Topological checking rejects reachable cycles and bounds paths at `2 * budget + 6`.

`crabber-checkpoint-checker-v1` explores five phases, matching/mismatching identity, each role's valid/missing report, publication readiness and verdict. Identity mutations independently change repository, Git directory, HEAD, branch, base, plan, skill, model, effort, profile, protocol, endpoint, turns, total budget, executable, both prompt digests, snapshot and origin identity. Removed output-cap controls are no longer part of the model.

`crabber-refresh-checker-v1` checks all five source phases and all 32 combinations of identity match, verified records, published origin, verified snapshot and reader integrity. It checks all source/fresh report pairs and the production fresh-role constructor. Round checks cover budgets 1..100, eligible source rounds, zero/over-budget rounds and later resume occurrences.

`crabber-delegation-checker-v1` checks 72 combinations of cached/missing roles, inline/background mode, both valid profiles plus an unknown profile, and both reserved task references plus an unreserved reference. It checks repeat claims and verifies that claiming one role leaves its independent sibling available.

## Mutation evidence

The mutation script introduces eight actual production defects, one at a time. It removes exact task binding, admits old checkpoint protocols, resets refresh rounds, accepts stale HEAD, copies source approval into a child, accepts fix intent, skips snapshot readiness, and removes the final prompt ceiling. Each variant must fail its named assertion. Compilation failures and unrelated test failures do not count as detection.

The script records source/mutated hashes, expected counterexamples and exits. It restores exact original bytes after every variant and then reruns all seven formal checks into `formal-restored/`. Its result files and transcripts survive fixture cleanup. A wrapper run with no matching checks or missing success markers fails.

The older `scripts/verify_recovery_mutations.py` still checks generic checkpoint guards on the current native schema. It does not provide legacy provider compatibility and is not needed for the eight-defect pass above.

## Assumptions and exclusions

- Callers supply parsed identities and validated records to pure guards. Integration tests establish call placement and actual SDK extension ownership.
- The model does not prove the SDK's parallel execution, cancellation, admission, credential transport, tool sandboxing, provider retry behavior or event-stream reconciliation. Those require actual CLI/SDK tests.
- Each invocation settles. Arbitrarily many explicit restarts do not imply unconditional termination. A restart cannot grant additional review rounds.
- Filesystem validity and reader integrity are abstract success/failure inputs. Concrete tests cover byte bounds, no-follow reads, digests and fault injection; the model does not prove physical fsync durability.
- Hashes detect stale or corrupt records, not a malicious same-user writer who can rewrite both records and hashes.
- The host cannot prove model honesty or complete reviewer inspection. Review children have bounded immutable evidence tools and read-only repository tools, without shell or recursive delegation.

Update the model when covered code changes. Use `/maintain-formal-verification-skill` for a dedicated synchronization pass. Never weaken a property to accommodate an implementation defect.
