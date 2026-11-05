---
name: verify-crabber-workflow
description: Exhaustively check normal, resumed and refreshed workflows, preserved global budgets, snapshot readiness and checkpoint transition safety.
---

# Verify workflow, refresh and checkpoint control flow

Run from the repository root with the declared Rust dependencies and Python 3.

```sh
python3 scripts/verify_formal.py --evidence .audit/bounded-review-inputs/formal
```

The checker uses no model requests or credentials. Stop production source writers and live CLI journeys before running mutations. Each mutation rebuilds the CLI test binary.

```sh
python3 scripts/verify_bounded_mutations.py --evidence .audit/bounded-review-inputs/mutations
```

The earlier recovery checks remain available separately.

```sh
python3 scripts/verify_recovery_mutations.py
```

## Property and harm

Completion requires implementation and two matching reviewer reports. A refreshed child must obtain both reports from fresh reviewer attempts. Source reports cannot replace those attempts. Normal, resumed and refreshed invocations retain the same global review budget. The final global review cannot authorize another fix.

Refresh accepts only an incomplete read-only review. Its source identity and records must validate, its origin must be published, and its immutable snapshot must validate before readiness. A failed reader integrity check prevents valid publication. Mutable fix intent and completed workflows cannot become refresh sources. These gates prevent stale approval, replay of unknown mutations and budget resets.

## Covered code and synchronization

`src/formal_verification.rs` compiles the actual graph from `workflow::document_from` with `flows_core::compile` and calls the production occurrence predicate. It uses production `review_checkpoint::{review_round,refresh_round,can_fix,reports_ready,role_start,can_resume,next_phase,identity_matches,refresh_eligible,fresh_roles}`. The graph traversal applies `can_fix` to the same review changes outcome that the host guards. The earlier checker allowed that abstract edge at the final review; the updated traversal now models the host guard directly.

The final prompt bound check calls production `review_inputs::validate_prompt`, which the renderer, checkpoint creation and strict-resume loader use. It checks the exact 256 KiB boundary and one-byte overflow with ordinary, escape-heavy, control-character and multibyte text. A separate actual-constructor check publishes two source reports, runs `RefreshSource::load` and `begin_execution`, and verifies that the child has no cached reports while the source report bytes survive unchanged. These are actual-code assertions alongside the exhaustive control-flow model.

Pre-PR verification started from `8c79ca68227898ebbba7f569a71856551ecfb1bf` and checked the subsequent working-tree changes by source hash. That baseline commit does not contain recovery or bounded-input changes. Rerun the wrapper after checkout or commit to record the current revision and exact covered source hashes. Every successful wrapper run records exact SHA-256 hashes for workflow, checkpoint, snapshot, artifact reader, config, Git capture, durable publication, review rendering and the checker in its `result.json`. A successful run establishes synchronization only for those exact hashes.

The wrapper records the current Rust, Cargo and Python versions. The current implementation was checked with Rust 1.99.1, Cargo 1.99.1 and Python 3.14.5. Rerun after any covered source changes.

## Bounds and model

`crabber-workflow-checker-v3` explores normal entries for each budget 1..100, plus strict-resume and refresh entries for every source round 1..budget. The current graph reaches 1,404,000 states and 1,414,200 terminal cases. Breadth-first reachability retains shortest counterexample paths. A topological check rejects reachable cycles and checks a maximum path length of `2 * budget + 6`.

Graph state tracks implementation, review freshness and global review/fix counts. Tasks settle with a declared label or permanent error. Retries remain disabled. A recovered read-only entry assumes the implementation anchor was validated by the host. Hook nodes are local pass-through actions. The refreshed graph starts at the actual `refresh_round` result. Strict resume from that entry uses the same production global-round mapping.

`crabber-checkpoint-checker-v1` checks all five coordinator phases, matching or mismatching identity, each role's valid or missing report, published or unpublished report data, and approved or rejected verdicts. It calls actual transition guards and checks every supported global round mapping. Identity mutations independently change repository, Git directory, HEAD, branch, base, plan, skill, model, endpoint, turns, output ceiling, recovery ceiling, total budget, executable, both prompt digests, snapshot identity and recovery-origin identity.

`crabber-refresh-checker-v1` enumerates all five source phases and all 32 combinations of identity match, verified source records, published origin, verified snapshot and reader integrity. It also checks all source report pairs and all fresh report pairs. `fresh_roles` must always return zero attempts, absent sessions and false reuse flags. Completion requires two fresh reports and every readiness/integrity guard. Round checks cover budgets 1..100, every eligible source round, zero and over-budget rounds, and later strict-resume occurrences.

Filesystem validity and reader integrity are abstract success/failure inputs to the control-flow model. Concrete CLI and unit tests must establish parsing, no-follow ancestry, digests, encoded response caps, source-race detection and cancellation. The model does not replace those tests.

## Mutation proof and evidence

`verify_bounded_mutations.py` changes actual production source one defect at a time. It resets the refreshed round, accepts stale HEAD, copies source reports into the child while rebinding their input digests, accepts fix intent, bypasses immutable snapshot readiness, and removes the final rendered-prompt ceiling. Each variant must fail its named assertion. Compilation errors and unrelated failures do not count as detection.

The script checks source hashes between variants and restores exact original bytes after each run. Do not edit source concurrently. Its `result.json` records original and mutated hashes, the failing assertion, process exit and restoration result. Per-mutation transcripts retain the counterexamples. The final restored source must pass `verify_formal.py`; that run writes `formal-restored/result.json` beside the mutation directory.

The latest bounded-input run detected all six intended defects, restored both modified production files byte for byte, and passed all five restored checks. `.audit/bounded-review-inputs/mutations/result.json` records each detection. `.audit/bounded-review-inputs/formal-restored/result.json` records the checked source hashes and confirms that they stayed unchanged throughout verification.

The earlier `verify_recovery_mutations.py` checks identity mismatch, a single report counted as both, round reset, fix-intent resume and premature report readiness. Its evidence remains under `.audit/review-recovery/`.

All `.audit/bounded-review-inputs/formal*/` transcripts and result files survive fixture cleanup. The wrapper requires the graph, checkpoint and refresh markers together with `safety=PASS termination=PASS`. An empty or filtered-out test run is a failure.

## Assumptions and exclusions

- Production callers supply parsed identities and validated records to pure readiness predicates. The model proves those predicates and transitions, while source review and integration tests establish call placement.
- Hashes detect corruption and stale data. They do not authenticate against a malicious same-user writer who can rewrite records and hashes.
- Each invocation settles. Arbitrarily many explicit user restarts do not imply unconditional termination. A restart or refresh cannot grant additional review rounds.
- Session turns, output recovery, provider admission, SDK reconciliation and deadline sharing require actual CLI/SDK tests. This model does not prove those mechanisms.
- The reader is abstracted as matching bounded data or failure. The model does not prove byte encoding, filesystem permissions, no-follow behavior, physical fsync durability or cancellation races.
- The host cannot prove model honesty or that reviewers inspected every relevant omitted artifact. Reviewer instructions and runtime capability tests establish the supported behavior.
- Read-only reviewers can restart. Mutable implementation/fix work cannot restart from checkpoints.

Use `/maintain-formal-verification-skill` for a dedicated synchronization pass when covered behavior changes. Do not weaken a failing property to accommodate an implementation defect.
