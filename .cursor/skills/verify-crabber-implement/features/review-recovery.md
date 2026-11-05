# Verify review recovery

Run from the repository root. Use only local fixture providers; no paid model call is needed.

```sh
mkdir -p .audit/review-recovery
cargo test --locked --test cli review_recovery -- --test-threads=2 > .audit/review-recovery/recovery.txt 2>&1
cargo test --locked review_checkpoint::tests > .audit/review-recovery/checkpoints.txt 2>&1
python3 scripts/verify_formal.py --evidence .audit/review-recovery/formal
python3 scripts/verify_web.py --evidence .audit/review-recovery/web
```

The CLI tests exercise actual Git, DMT, Crabber sessions and local HTTP failures. Require reuse without another completed-role request, preserved session/report bytes, a fresh pair of reviewers after a fix, later-round fix identities, no budget reset, killed-process recovery, stale-input refusal and fix-intent refusal. Cancel a resumed reviewer, then require a later read-only attempt to acquire the released lock and preserve the completed role. Exhausting the final global round must fail without starting a fix. Cached approvals finish without credentials; cached changes refuse before intent when credentials are absent. Restoring credentials permits the ordinary fix flow.

Checkpoint unit tests cover bounded/no-follow reads, report corruption, exclusive session ownership, and injected directory/write/sync/publication errors. A subprocess test kills the renderer at a deterministic barrier after its first durable rendered file, then proves clean-tree recovery from the paired reports. The formal check explores the actual normal/resumed graph and production transition guards. It does not establish physical disk durability.

The Docker journey adds a failed review and a second CLI process. Playwright observes correctness marked Reused while maintainability attempt 2 is running, then final completion. Inspect `review-failed.png`, `review-resumed.png`, `review-completed.png`, `review-recovery-trace.zip` and `review-browser.json`. Exported checkpoints and historical execution artifacts under `review-cli/` must survive cleanup.

If Docker itself is unavailable, record its failure and run the same review journey against the native production server:

```sh
python3 scripts/verify_review_browser.py
```

This fallback needs local Playwright browsers installed (`cd web && npx playwright install chromium`). Its result explicitly identifies the native runtime; it does not establish Docker coverage. Evidence defaults to `.audit/review-recovery/browser-local/`. Both helpers release their fixture holds and remove only their own listeners/processes. Require the saved screenshots, result JSON, and copied checkpoints after temporary Git repositories are gone.

Do not rebuild or mutate sources while a recovery journey runs: input identity intentionally includes the executable hash. Historical pre-checkpoint runs cannot be imported or resumed.

## Refresh after an executable change

Strict resume keeps the saved executable, prompt, snapshot, and recovery-origin identities. `--refresh-review` imports an eligible version 1 or version 2 source into a fresh child. Both roles start at attempt 1, with no reused approval. The child retains the original implementation anchor and global review budget.

```sh
cargo test --locked --test cli refresh_ -- --test-threads=2
python3 scripts/verify_web.py --evidence .audit/bounded-review-inputs/web
```

Require a final-round source to start both fresh reviewers without implementation or fix calls. A change request at the maximum round must fail without fixing. An earlier-round child can fix, review again, and resume under its own identity. A refresh of that child must retain deduplicated inherited evidence and the same original implementation anchor, even after both parent artifact directories are unavailable.

For the exact legacy failure journey, retain a compatible old executable before rebuilding and pass its path through `CRABBER_VERIFY_LEGACY_BINARY`. The fixture first uses that executable to create a version 1 round-4 admission failure after three fixes, then refreshes with the current binary. It never edits the original incident repository.

The Docker refresh journey observes the failed parent, both child reviewers running at global round 4 and attempt 1, then child completion. Inspect the refresh screenshots, browser result, and copied source and child artifact directories. The parent failure and all source bytes remain unchanged. Child `recovery-origin.json`, `implementation.json`, and `inherited-evidence/` survive cleanup. Publication fault tests and process-kill tests must never treat an incomplete snapshot as a ready checkpoint. Run formal source mutations only after every recovery process has exited.
