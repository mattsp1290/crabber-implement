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

The tested Crabber revision is `a3fdd0ee6eb4f1c05ea973302ebeefd79e829f1a`. It provides typed admission proof-limit errors, model completion metadata, and the tool-extension APIs used by bounded reviews.

Build the binary:

```sh
cargo build --release
```

Sign in with `codex login`. The host reads the existing Codex ChatGPT subscription login from `$CODEX_HOME/auth.json`, or `~/.codex/auth.json` when `CODEX_HOME` is unset. `--codex-auth-file` selects another native login file. The file is read only; the host never copies its refresh token, refreshes the login, or writes to it. An expired access token requires signing in again through Codex.

All model work uses the Codex CLI subscription profile and Responses API. Defaults are `gpt-5.6-terra` with `medium` reasoning effort. There is no API-key provider or fallback.

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
Plan loading scans at most 4096 directory entries, including ignored entries, and selects at most 256 numbered Markdown files. The rendered plan, including document headers and separators, must fit 2,000,000 UTF-8 bytes. The review skill must fit 16 MiB. Bounded regular-file reads reject larger inputs and special files before credentials, provider calls, artifact creation, or branch changes.
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
  --model gpt-5.6-terra \
  --reasoning-effort medium \
  --max-reviews 4 \
  --max-turns 256 \
  --session-timeout 1800 \
  --skills-dir "$HOME/git/dotfiles/.agents/skills"
```

`--skills-dir` must contain `review/SKILL.md`.
The default is `~/git/dotfiles/.agents/skills`.
The program reads the review skill for every invocation. Fix sessions use the repository-owned [autonomous fix-review skill](skills/fix-review-autonomous/SKILL.md), embedded in the binary at build time. An external `fix-review/SKILL.md` is neither required nor loaded.

Autonomous fixes can change configuration, authentication, authorization, permissions, and secrets-handling source code within the plan and tool permissions. Such files do not automatically require manual attention. Genuine missing decisions, permissions, or verification blockers still stop the workflow. Fix agents cannot access live secrets, push, merge, or deploy. The host commits fixes on the working branch and requires both independent reviewers to approve the new revision before completion.

`--base-url` sets the Codex Responses API root without `/responses`.
Its default is `https://chatgpt.com/backend-api/codex`. Only use a trusted endpoint, because it receives the access token.

`CRABBER_IMPLEMENT_CODEX_BASE_URL` and `CRABBER_IMPLEMENT_MODEL` provide environment defaults. The former provider endpoint variable is ignored.

`--max-turns` sets a positive model-turn budget for each agent session. Its default is 256.
`CRABBER_IMPLEMENT_MAX_TURNS` provides the environment default. The flag takes precedence.
A model turn includes one model response and its tool batch. The final answer also consumes a turn.
Provider retries and compaction do not add outer turns.
The turn budget and `--session-timeout` apply independently. Session timeouts must be between 1 and 3600 seconds, matching the native inline-delegation deadline limit.
Both limits cover the entire stage session, including any continuations after an empty model answer.
Reaching either limit stops the workflow and preserves partial edits. The program does not restart the workflow automatically.

If a completed model response has no visible final text, the host can ask it to continue in the same conversation up to two times. Each continuation gets only the remaining turn allowance and shares the original deadline. Incomplete Responses, filtering, provider failures, cancellation, and exhausted budgets stop the stage. An explicit `BLOCKED` report retains the agent's explanation in the error. The host does not configure per-response output caps or escalate them after failure.

The selected reasoning effort applies to ordinary turns and SDK context-compaction calls. Completed response evidence records usage and finish metadata without private reasoning. If the bounded live-event channel lags, the host joins the owned run and reads its stored events and committed final answer before deciding its outcome.

The review budget includes the first review. Exhausting it returns a nonzero exit code and preserves findings.
`NEEDS_DISCUSSION`, malformed reports, agent failures, and fixes marked `needs-manual` also stop the run.
Control-C interrupts the active agents and preserves their artifacts.

## Inspect the result

### Watch a run in the React app

Build the CLI after pulling these changes. Run your plan with the existing command, then start the monitor in another terminal.

```sh
cd ~/git/crabber-implement/web
npm ci
npm run build
ARTIFACTS_DIR="$(git -C /path/to/target/repository rev-parse --absolute-git-dir)/crabber-implement" npm start
```

Open `http://127.0.0.1:4173`. Select a run to see its current workflow node, agent text, tool results, and stage history. Both independent reviewers appear as separate sessions. Selecting a node or session lets you inspect earlier work while execution continues. The app polls once per second.

`ARTIFACTS_DIR` is the directory containing run UUID directories, rather than one individual run. The CLI creates it when a run starts. The monitor reads this directory without modifying it. It can restart independently of the CLI and display completed runs after the CLI exits. Runs without a recent heartbeat show an unknown status until a terminal result exists. Older binary versions provide archived output but cannot stream live text.

To run the production app in Docker, build from `web` and mount the artifacts read only.

```sh
docker build -t crabber-monitor web
docker run --rm -p 127.0.0.1:4173:4173 \
  -v "$(git -C /path/to/target/repository rev-parse --absolute-git-dir)/crabber-implement:/artifacts:ro" \
  crabber-monitor
```

The app displays local agent transcripts and tool output. Keep its listener on localhost. The Docker image contains the monitor. The executor continues to run on the host with its existing Rust dependencies and repository access.

### Read saved artifacts

The terminal prints each stage and the artifact directory.
Artifacts live under the target repository's Git directory at `crabber-implement/<run-id>/`.
They include the plan, graph document, session text, tool result logs, stage events, and final status.
`run.json` records repository, branch, model, and a process heartbeat. Session text and settled tool results are appended as events arrive. Session JSON is written atomically at session start and completion. `result.json` is the terminal workflow outcome.
Each session also writes `<stage>.session.json` on success or failure after the session starts.
It records the workflow run ID, Crabber session and run IDs, configured turn budget, observed model-turn count, status, and error.
These records contain no provider credentials.
They also record individual continuation runs, cumulative turn counts, finish reasons, and token usage. Session JSON keeps a bounded tail of response summaries. `<stage>.responses.jsonl` retains every completed-response record. Private model reasoning is not copied into these diagnostics. A provider request ending successfully does not by itself mean that implementation completed.
### Datadog tracing

Retryable provider failures receive at most five attempts per model turn, with waits of 1, 2, 4, and 8 seconds. The session deadline and cancellation still interrupt these waits. Failures after visible text or tool-call data are not retried. Transport errors report fixed `phase`, `cause`, and I/O categories without raw error text or URLs. These categories also appear on provider trace spans as `transport.phase`, `transport.cause`, and `transport.io_kind`. A missing I/O cause is reported as `unknown`; it does not establish a provider outage.

The CLI exports metadata-only APM traces to the local Datadog agent at `http://127.0.0.1:4318/v1/traces` by default. No application ingestion key is needed; the agent forwards data using its own configuration. [Enable OTLP ingestion in the agent](https://docs.datadoghq.com/opentelemetry/setup/otlp_ingest_in_the_agent/) and keep its HTTP receiver bound to loopback for a host-local deployment. If `otlp_config.traces.enabled` is explicitly false, set it to true and restart the agent. A listening OTLP logs endpoint alone does not establish trace ingestion.

The service defaults to `crabber-implement`. `DD_SERVICE` or `OTEL_SERVICE_NAME` overrides it; `DD_ENV` and `DD_VERSION` provide environment/version tags. The version defaults to the executable's package version. Standard `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT` settings override the collector endpoint. The SDK supports HTTP/protobuf and HTTP/JSON. Protocol and timeout overrides apply with the default endpoint too. Standard `OTEL_TRACES_SAMPLER` settings remain effective. Export request/shutdown timeout is capped at three seconds. The local receiver does not require `OTEL_EXPORTER_OTLP_HEADERS`; do not put credentials into endpoint URLs.

Each workflow has a root trace. Stage, session, continuation-attempt, model-call, and tool-call spans include their relevant identifiers, outcomes, timings, and model token counts. `workflow.run_id` tags all trace resources so background SDK lifecycle spans can be found even when they are outside the main execution tree. The workflow identifier is the artifact directory name. `telemetry.json` records the root trace ID and local exporter status on startup and shutdown. `flushed` means the SDK completed shutdown without a transport error. `delivery` remains `unverified`: sampling may suppress all spans, and the exporter does not surface collector partial-rejection counts. This state does not prove any spans were sent, accepted, indexed, or retained. Verify delivery with a Datadog query.

Prompts, plan text, paths, branch names, tool arguments/results, provider response bodies, raw error messages, and private reasoning are excluded from exports. Detailed diagnostics remain in the local artifacts. This integration exports APM traces through the agent; it does not enable the SDK's separate agentless metrics, logs, or LLM Observability intake, and it does not load `DD_API_KEY`.

Use `--no-telemetry`, `CRABBER_IMPLEMENT_NO_TELEMETRY=true`, or `OTEL_SDK_DISABLED=true` to disable export. Invalid telemetry configuration or collector failure prints a redacted warning and records a finite error category in `telemetry.json`. The workflow keeps its original success/failure result. Exporter shutdown is awaited on workflow success, failure, and cancellation before the process exits.

Pup authentication is independent of export configuration. After ingestion, query US3 APM with `service:crabber-implement @workflow.run_id:<artifact-directory-name>`. Ordinary CLI fixture tests disable telemetry. The explicit `telemetry_local_agent_smoke` test submits a synthetic failure through the local agent without running the user's plan.

Review reports live at `reviews/<run-id>-<round>/` in the target repository.
Each round has a `review-v1` manifest and two reviewer directories with the five files required by the review skill.
The host passes the exact selected review directory to fix-review and verifies its branch and commit before applying fixes.
Generated review directories are ignored through the repository's local Git exclude file.

A successful run exits with code zero and prints `done <commit>`.
Inspect the commits and review reports before publishing them.

## Resume an interrupted review

New runs save each validated reviewer report as soon as that reviewer finishes. If review fails or the process stops before fix work begins, rerun the same command with `--resume-review <workflow-id>`:

```sh
~/git/crabber-implement/target/debug/crabber-implement \
  ".agents/plans/fiber-watermill-chat/" \
  --session-timeout 3600 --max-turns 3000 \
  --resume-review <workflow-id>
```

Use the UUID printed in the artifact path. Keep the original plan, model, endpoint and budget options. Keep the same executable: rebuilding changes its fingerprint and makes the old checkpoint ineligible. HEAD, branch, base, plan and review skill must still match, and the worktree must be clean. A changed input causes refusal before provider calls.

Resume starts fresh read-only sessions for unfinished reviewers and reuses validated reports. It preserves earlier transcripts and terminal results under `executions/<generation>/`, then follows the ordinary fix and fresh-review flow. Global review limits still apply. Cached approvals can complete without provider credentials; pending reviewers and fixes require a valid native Codex login.

Resume is refused after fix intent has been recorded, because the host cannot safely replay an interrupted mutation. Implementation and fix sessions cannot be resumed. Runs created before review checkpoint support remain viewable but cannot be resumed.

The monitor shows each reviewer's round, attempt and reused status. Resumed launches keep the workflow ID and create a new trace with the execution generation and predecessor trace ID. Transport spans additionally expose finite `transport.protocol_code` and `transport.chain_status` categories; they never include source error messages.

## Start a fresh review after upgrading

Use `--refresh-review <workflow-id>` when the saved review used another executable. Supply the original plan, repository, base, model, reasoning effort, endpoint, turn limit, and maximum review count.

```sh
crabber-implement /path/to/original-plan \
  --repo /path/to/repository \
  --max-reviews 4 \
  --refresh-review <source-workflow-id>
```

Resume and refresh accept only version 3 Codex native-subagent checkpoints at an incomplete review boundary. Old OpenCode/version 1/version 2 records are rejected; start a new run. It rejects fix intent, committed-fix boundaries, completed workflows, unsupported versions, and legacy transcripts without checkpoints. The source worktree must be clean, its HEAD unchanged, and its original implementation anchor valid. `--refresh-review` and `--resume-review` are mutually exclusive. The wall-clock session timeout remains an execution control outside saved input identity.

Refresh creates a new workflow UUID with generation 1 and starts both reviewers in fresh read-only sessions. Credentials are required even when the source has paired reports. The source reports become evidence, never approval for the child. The child retains the original implementation commit and copies inherited evidence into its own immutable storage. Later reviews and another refresh use that local inventory without reading ancestor workflow directories.

The global review round and maximum review count do not reset. A refresh of round 4 with `--max-reviews 4` can approve or fail, but cannot run a fix or review 5. Initialization does not switch branches, create product commits, replay implementation, or rerun a fix. A failed initialization can leave an incomplete child for inspection. Repeating the command creates another UUID and preserves every source artifact. Strict resume can later continue the child under the child's own executable and snapshot identity.

The monitor links the child to its parent and displays the preserved round and fresh reviewer attempts. The source failure remains a separate run. `recovery-origin.json` records private provenance. Run and telemetry metadata expose only the parent UUID and finite recovery mode.

## Review input limits

Each final reviewer prompt is at most 256 KiB of UTF-8 bytes, including role instructions and references. Raw excerpts share a 64 KiB allowance across the plan, review skill, diff, and execution evidence. Omitted content remains accessible through `review_artifact_catalog` and `review_artifact_read`. Reviewers must inspect relevant omitted evidence and report uncertainty when they cannot inspect it.

Each snapshot contains the plan, review skill, pinned Git diff, committed changed files, commit list, and execution evidence. Binary files retain their bytes. Deleted files and committed symlinks retain their classification, and symlink targets are never opened. Non-UTF-8 Git paths use lossless hexadecimal labels. The host checks credential-denied changed paths before reading Git bodies. Original logs remain unchanged.

Snapshot limits are 16 MiB per artifact, 128 MiB in total, 4096 entries, and 1 MiB for serialized manifest metadata. Overflow fails explicitly before review sessions launch. Artifact requests accept immutable IDs and bounded byte ranges, never paths. Encoded responses stay below 32 KiB and include continuation offsets. Directory handles, no-follow opens, and digest checks prevent path substitution. An integrity failure prevents report publication even if a reviewer ignores the tool error.

`review-checkpoints/<round>/input-metrics.json` records final prompt bytes, snapshot bytes, excerpt bytes, and omitted bytes. Numeric prompt and omission counts also appear on reviewer spans. Model-authored summaries remain claims. Unstructured tool output is never automatically labeled a passing verification result.

An SDK admission proof limit remains a typed failure with finite diagnostic metadata. The host closes the local attempt and preserves the primary error. It does not raise proof budgets, retry the failed admission automatically, or treat local task drainage as store reconciliation.

Configuration size ceilings do not guarantee SDK admission for every stage. Implementation still receives the complete plan. A near-2 MB plan can exhaust its admission proof allowance before review begins. Review snapshots reference large plan and skill content through the bounded artifact reader.

## Execution model and limits

`flows-core` from dmt-flows compiles the workflow document. The matching pinned DMT engine executes it.
A local `NodeHandler` maps the work nodes to Crabber sessions.
The document uses the compiler's supported `shell` node type as its task envelope.
Its descriptive `argv` values are not launched as subprocesses.
The injected acquire and release nodes do not create a sandbox. The release node verifies that approval still matches the repository.

Implement and fix-review agents have repository file tools and a local shell.
They run with your user's access. Shell access is not a security sandbox.
The shell uses a minimal environment without provider credentials.
Review agents have file reading, listing, globbing, search, and bounded immutable-artifact tools.
The host supplies a checked snapshot of the diff, changed files, commit history, plan, and implementation evidence. The artifact capability works in linked worktrees whose Git directory is outside the workspace tool root.
A deterministic local parent Agent emits exactly one `delegate_task` call for each missing role. Its session-scoped native extension owns both review lifecycles with a concurrency limit of two. The parent makes no external model request and exposes only inline delegation. Each call carries a small immutable reservation; the trusted runner loads the bounded prompt and starts an independent read-only Codex child session. Review children cannot delegate again.

The runner validates and durably publishes its report before returning a small receipt. Parent session, run, and tool-call IDs are recorded in each child's `parent_delegation` metadata. The monitor shows the selected child's subagent identity. A failed role does not erase its completed sibling. Cancellation interrupts the parent and children, and the host waits for cleanup before returning. The host writes their structured results to the skill's report format.
Reviewers assess the supplied verification evidence. They cannot independently run shell checks.

One process holds an advisory repository lock. The engine runs one work node at a time.
Mutation nodes have no automatic retry and refuse lease replay.
Workflow state uses DMT's in-memory store. Explicit review-only recovery reconstructs a bounded graph from durable checkpoints; other stages have no crash resume.
A failed run can leave task edits or commits for inspection. Commit or stash remaining edits before starting a new run.

## Verify changes

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Verify the React app and its reader with Node tests, then drive the production Docker app with Playwright.

```sh
npm --prefix web ci
npm --prefix web test
npm --prefix web run build
python3 scripts/verify_web.py
```

The Docker journey needs Docker, Node, Cargo, and the sibling Rust checkouts. It uses a local model protocol fixture and synthetic native credentials. Playwright checks artifact updates and the real CLI while an implementation session is paused, then verifies completion. Screenshots, browser traces, CLI output, and exported artifacts survive container and fixture cleanup under `.audit/web/`. The browser image is pinned to the installed Playwright version.

`tests/cli.rs` drives the actual binary against a local Codex Responses fixture.
It checks file mutation, local commits, the review-fix-review cycle, malformed review rejection, input failures, model-turn limits beyond 64 requests, and bounded continuation after empty completed answers.
The fixture replaces the external model boundary. It uses the real Crabber tools and DMT engine.

The project-local verification skills document the live model journey and the bounded workflow check.
