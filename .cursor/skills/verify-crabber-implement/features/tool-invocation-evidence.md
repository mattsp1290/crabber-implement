# Verify prepared tool invocation evidence

Run this journey from the host checkout. It uses the local scripted provider in `tests/cli.rs` and needs no credentials.

```sh
mkdir -p .audit/invocation-evidence
CRABBER_VERIFY_ARTIFACTS="$PWD/.audit/invocation-evidence" cargo test --locked --test cli cli_streams_live_artifacts_before_completion -- --exact --nocapture
cargo test --locked --bin crabber-implement lagged_live_observation_has_committed_answer_and_durable_completion -- --nocapture
python3 -m unittest scripts/test_analyze_run.py
```

Inspect the exported `live` and `completed` directories after the test exits. The test drops its scratch repository and provider listener. The exported artifacts must remain readable.

Read each `implement.tools.txt` row as JSON. Require `observation.session_id`, `observation.run_id`, `observation.event_kind`, and `call_id`. Pair a pending row with its settled row by session, run, and call ID. Require a real settled row while the live session is running. Confirm that ordinary pending `file_write` rows include the prepared path and content in `arguments`.

Keep every SDK payload field at its original top level. Removing `observation` from a settled row must reproduce the durable SDK payload. Retain shell result content and nonzero exit codes. A pending row establishes preparation only. It does not establish execution or success. Historical and frontend pending rows can omit `arguments`. `arguments_unavailable: "event_not_storable"` means the complete pending event could not store the optional arguments.

The host unit journey forces live observation lag after a pending call, reconstructs both event kinds, rebuilds twice without duplicates, and continues with a new run in the same session. Require identical tool-log bytes after repeated reconstruction and unchanged prior-attempt bytes.

For SDK input and persistence checks, run from the sibling `crabber` checkout.

```sh
cargo test -p crabber-runtime --features sqlite pending_arguments
cargo test -p crabber-runtime --features sqlite frontend_replay
```

These tests compare final prepared input with executor input after pipeline and extension transformations. They discover the real complete-record JSON-depth boundary, preserve accepted execution at that boundary, exercise NUL sanitation and escaped large text, and reopen SQLite to read enriched and unavailable events. Frontend replay retains its historical pending shape.

Keep exported evidence until review completes. The formal workflow guard model is unchanged by this behavior.
