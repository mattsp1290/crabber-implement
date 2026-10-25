# Model turn budget

## Sub-features

Configure how many model responses each agent session can consume. `--max-turns` defaults to 256 and `CRABBER_IMPLEMENT_MAX_TURNS` sets its environment default. The wall-clock session timeout and the workflow review budget are separate limits.

## How to get to it (user POV)

Run `crabber-implement <plan> --max-turns 256` in the target repository. One response and its tool batch count as one turn, even when the model requests several tools. The final answer also requires a turn.

## Driving it with the CLI harness

```sh
mkdir -p .audit/turn-limit
cargo test --locked --test cli turn_budget -- --nocapture > .audit/turn-limit/cli-after.txt 2>&1
```

Require the default-budget journey to make more than 64 model requests and complete implementation and review. Require the explicit small-budget journey to fail without starting review or printing `done`. Check its `implement.session.json` for session and run IDs, the configured budget, observed turn count, and failed status. Partial edits must remain available after failure.

## Gotchas

The CLI does not configure a Datadog exporter. If authenticated `pup` searches find no matching run, inspect the local session records and tool transcripts.

The CLI uses the sibling Crabber checkout's `AgentBuilder::max_turns` API. Update that checkout with the corresponding facade change before building. Increasing `--session-timeout` does not increase the turn budget. Increasing `--max-reviews` does not increase it either. A larger turn budget does not resume an already failed in-memory session. Commit or stash partial edits before starting a new invocation.

Empty-answer continuations keep the same session history but admit a new model run with only the remaining turn allowance. The stage's cumulative count and original deadline do not reset. The separate continuation limit allows at most two additional prompts. Verify this with `continuation_does_not_reset_the_turn_budget` in the CLI fixture.
