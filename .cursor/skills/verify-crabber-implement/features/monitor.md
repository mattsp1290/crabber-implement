# Verify the live React monitor

Run from the repository root. Docker must be running. Cargo needs the sibling Crabber and dmt-flows checkouts used by the CLI build.

```sh
npm --prefix web ci
npm --prefix web test
npm --prefix web run build
python3 scripts/verify_web.py
```

The helper builds the production Docker image and pulls the matching pinned Playwright browser image. It starts the monitor with a read-only artifact mount. Browser tests update fixture artifacts through a separate writable mount and check node changes, session selection, review rounds, mobile layout, and error recovery.

The helper then starts `cli_streams_live_artifacts_before_completion` against the real CLI and a local model protocol fixture. The fixture pauses an implementation response while its text, settled tool results, running session record, and heartbeat are available. Playwright reads those actual files through the Docker server before releasing the response. It checks completion while the CLI test still preserves its temporary repository.

The CLI remains on the host. Docker runs the production monitor and Playwright browser. The fixture replaces the external provider boundary. No external provider key is used.

Evidence stays under `.audit/web/`. Require a passing `result.json`, CLI test output in `cli.txt`, live and completed screenshots, and the exported live/completed artifact directories under `cli/`. Browser failures save traces and screenshots under `results/`. A screenshot of a completed fixture alone does not prove live output.

The helper stops only its named monitor container, releases fixture holds, and waits for fixture cleanup even after failure. Evidence is outside disposable containers and temporary Git repositories. Confirm it survives cleanup.

The refresh journey starts with a failed source at global round four. It observes a distinct child with two fresh reviewer attempts, releases their fixture responses, waits for paired approval, and follows the lineage link back to the preserved source failure. Require `refresh-browser.json`, the source/running/completed screenshots, `refresh-cli.txt`, and copied source and child artifacts under `refresh-cli/`. The synthetic browser test also checks that version 3 checkpoints retain fresh attempt labels.

If the default Docker daemon has storage errors, run the helper with `DOCKER_HOST` pointing to a task-owned healthy daemon. Do not restart shared workloads.

```sh
python3 -c 'import json; from pathlib import Path; p=Path(".audit/web"); assert json.loads((p/"result.json").read_text())["status"] == "passed"; assert (p/"cli/completed").is_dir()'
```

For manual inspection, use the launch command in the repository README. `ARTIFACTS_DIR` must name the parent directory of run UUID directories. The monitor uses one-second polling, displays bounded text/tool tails, and marks missing or stale process heartbeats as unknown. The terminal `result.json` takes precedence only when it matches the current execution generation. A prior failure is ignored during a resumed launch. The review recovery journey also verifies reused reviewer labels and attempt numbers.

The same Docker driver exercises explicit refresh through `cli_review_refresh_browser_journey`. It displays the failed source and its linked child, both fresh reviewers at global round 4 and attempt 1, then child completion while the source remains failed. Parent workflow metadata must be a UUID and recovery mode must be `refresh`; incomplete or malformed lineage is rejected. Inspect the `refresh-browser.json` result and refresh screenshots under the selected evidence directory. The driver copies source and child artifacts before deleting its disposable repository.

The native version 3 reader exposes bounded `parent_delegation` IDs. Selecting a review child shows its Crabber subagent badge and parent session, run, and call identifiers. Existing archived records remain inspectable, but the CLI cannot recover old provider checkpoints.
