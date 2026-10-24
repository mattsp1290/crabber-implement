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

```sh
python3 -c 'import json; from pathlib import Path; p=Path(".audit/web"); assert json.loads((p/"result.json").read_text())["status"] == "passed"; assert (p/"cli/completed").is_dir()'
```

For manual inspection, use the launch command in the repository README. `ARTIFACTS_DIR` must name the parent directory of run UUID directories. The monitor uses one-second polling, displays bounded text/tool tails, and marks missing or stale process heartbeats as unknown. The terminal `result.json` takes precedence over the heartbeat.
