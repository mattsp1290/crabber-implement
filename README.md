# crabber-implement

Run a Markdown implementation plan through an implement, review, and fix-review workflow.
The `done` node is reachable only after both independent reviewers approve the current commit.

## Build and run

Keep these checkouts beside each other:

```text
~/git/crabber-implement
~/git/crabber
~/git/dmt-flows
```

The Crabber checkout must include the public `AgentBuilder::max_turns` API.

Build the binary:

```sh
cargo build --release
```

Export `OPENCODE_GO_API_KEY` in your shell. The program reads the environment and does not source shell profiles.
The default provider is OpenCode Go, using `glm-5.2` and its chat-completions API.

Run from the repository you want to change:

```sh
~/git/crabber-implement/target/release/crabber-implement .agents/plans/my-plan
```

You can also select a target explicitly:

```sh
./target/release/crabber-implement /path/to/plan.md --repo /path/to/repository
```

The plan can be one `.md` file or a directory with `00-overview.md` and numerically prefixed Markdown documents.
Directory documents are read in filename order. A document marked `Status: Blocked` stops the run.
Commit or stash existing changes before starting, including an untracked plan inside the target repository.

The program creates an `implement/<run-id>` branch when started on the base branch.
On an existing feature branch, it continues on that branch.
The base defaults to `main`, or `main_branch` in the target's `.ralph` file.
Implementation and fixes become local commits. The program does not push them.

## Configure a run

```sh
crabber-implement /path/to/plan \
  --repo /path/to/repository \
  --base main \
  --model glm-5.2 \
  --max-reviews 4 \
  --max-turns 256 \
  --session-timeout 1800 \
  --skills-dir "$HOME/git/dotfiles/.agents/skills"
```

`--skills-dir` must contain `review/SKILL.md` and `fix-review/SKILL.md`.
The default is `~/git/dotfiles/.agents/skills`.
The program reads both files for every invocation and supplies their instructions to the corresponding agents.

`--base-url` sets an OpenAI-compatible API root without `/chat/completions`.
Its default is `https://opencode.ai/zen/go/v1`.
`CRABBER_IMPLEMENT_BASE_URL` and `CRABBER_IMPLEMENT_MODEL` provide environment defaults.

`--max-turns` sets a positive model-turn budget for each agent session. Its default is 256.
`CRABBER_IMPLEMENT_MAX_TURNS` provides the environment default. The flag takes precedence.
A model turn includes one model response and its tool batch. The final answer also consumes a turn.
Provider retries and compaction do not add outer turns.
The turn budget and `--session-timeout` apply independently.
Reaching either limit stops the workflow and preserves partial edits. The program does not restart the task automatically.

The review budget includes the first review. Exhausting it returns a nonzero exit code and preserves findings.
`NEEDS_DISCUSSION`, malformed reports, agent failures, and fixes marked `needs-manual` also stop the run.
Control-C interrupts the active agents and preserves their artifacts.

## Inspect the result

The terminal prints each stage and the artifact directory.
Artifacts live under the target repository's Git directory at `crabber-implement/<run-id>/`.
They include the plan, graph document, session text, tool result logs, stage events, and final status.
Each session also writes `<stage>.session.json` on success or failure after the session starts.
It records the workflow run ID, Crabber session and run IDs, configured turn budget, observed model-turn count, status, and error.
These records contain no provider credentials.
The CLI writes diagnostics locally and does not configure a Datadog exporter.

Review reports live at `reviews/<run-id>-<round>/` in the target repository.
Each round has a `review-v1` manifest and two reviewer directories with the five files required by the review skill.
The host passes the exact selected review directory to fix-review and verifies its branch and commit before applying fixes.
Generated review directories are ignored through the repository's local Git exclude file.

A successful run exits with code zero and prints `done <commit>`.
Inspect the commits and review reports before publishing them.

## Execution model and limits

`flows-core` from dmt-flows compiles the workflow document. The matching pinned DMT engine executes it.
A local `NodeHandler` maps the work nodes to Crabber sessions.
The document uses the compiler's supported `shell` node type as its task envelope.
Its descriptive `argv` values are not launched as subprocesses.
The injected acquire and release nodes do not create a sandbox. The release node verifies that approval still matches the repository.

Implement and fix-review agents have repository file tools and a local shell.
They run with your user's access. Shell access is not a security sandbox.
The shell uses a minimal environment without the provider key.
Review agents have only file reading, listing, globbing, and search tools.
The host supplies the diff, changed files, commit history, plan, and implementation evidence.
Each reviewer receives a separate Crabber session. The host writes their structured results to the skill's report format.
Reviewers assess the supplied verification evidence. They cannot independently run shell checks.

One process holds an advisory repository lock. The engine runs one work node at a time.
Mutation nodes have no automatic retry and refuse lease replay.
Workflow state uses DMT's in-memory store. Artifacts survive a crash, but automatic resume is not implemented.
A failed run can leave task edits or commits for inspection. Commit or stash remaining edits before starting a new run.

## Verify changes

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`tests/cli.rs` drives the actual binary against a local chat-completions fixture.
It checks file mutation, local commits, the review-fix-review cycle, malformed review rejection, input failures, and model-turn limits beyond 64 requests.
The fixture replaces the external model boundary. It uses the real Crabber tools and DMT engine.

The project-local verification skills document the live model journey and the bounded workflow check.
