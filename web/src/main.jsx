import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { runListSchema, snapshotSchema } from "../server/schema.js";
import "./style.css";

const NODES = [
  {
    id: "implement",
    label: "Implement",
    number: "01",
    detail: "Build the plan",
  },
  {
    id: "review",
    label: "Review",
    number: "02",
    detail: "Two independent reviewers",
  },
  {
    id: "fix-review",
    label: "Fix review",
    number: "03",
    detail: "Address the findings",
  },
  { id: "done", label: "Done", number: "04", detail: "Approved and complete" },
];
const nodeLabel = (id) =>
  NODES.find((node) => node.id === id)?.label ?? "Waiting for events";
const sessionNode = (name) =>
  name === "implement"
    ? "implement"
    : name.startsWith("fix-review-")
      ? "fix-review"
      : "review";
const sessionLabel = (name) =>
  name === "implement"
    ? "Implementation agent"
    : name.startsWith("fix-review-")
      ? `Fix agent · round ${name.match(/\d+/)?.[0]}`
      : `${name.endsWith("maintainability") ? "Maintainability" : "Correctness"} · round ${name.match(/\d+/)?.[0]}`;
const repoName = (repo) =>
  repo?.replace(/\/$/, "").split("/").at(-1) ?? "Archived run";

async function getJson(url, schema, signal) {
  const response = await fetch(url, {
    signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]),
    cache: "no-store",
  });
  if (!response.ok)
    throw new Error(`Artifact reader returned ${response.status}`);
  return schema.parse(await response.json());
}

function useMonitor(id) {
  const [runs, setRuns] = useState(null);
  const [warnings, setWarnings] = useState([]);
  const [snapshot, setSnapshot] = useState(null);
  const [error, setError] = useState(null);
  const [lastSync, setLastSync] = useState(null);
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    let timer;
    const poll = async () => {
      try {
        const list = await getJson(
          "/api/runs",
          runListSchema,
          controller.signal,
        );
        if (controller.signal.aborted) return;
        setRuns(list.runs);
        setWarnings(list.warnings);
        const detail = id
          ? await getJson(
              `/api/runs/${encodeURIComponent(id)}`,
              snapshotSchema,
              controller.signal,
            )
          : null;
        if (controller.signal.aborted) return;
        if (detail) setSnapshot(detail);
        setError(null);
        setLastSync(Date.now());
      } catch (failure) {
        if (!controller.signal.aborted)
          setError(
            failure instanceof Error ? failure.message : "Connection failed",
          );
      } finally {
        if (!controller.signal.aborted) timer = setTimeout(poll, 1000);
      }
    };
    poll();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, [id]);
  return {
    runs: runs?.map((run) => expireHeartbeat(run, now)) ?? null,
    warnings,
    snapshot: snapshot?.id === id ? expireHeartbeat(snapshot, now) : null,
    error,
    lastSync,
  };
}

function expireHeartbeat(run, now) {
  if (
    run.status !== "running" ||
    (run.heartbeatAt !== null && now - run.heartbeatAt <= 15_000)
  )
    return run;
  return {
    ...run,
    status: "unknown",
    ...("sessions" in run
      ? {
          sessions: run.sessions.map((session) =>
            session.status === "running"
              ? { ...session, status: "unknown" }
              : session,
          ),
        }
      : {}),
  };
}

function Badge({ status }) {
  const label =
    {
      running: "Running",
      completed: "Completed",
      failed: "Failed",
      unknown: "Interrupted / unknown",
    }[status] ?? status;
  return (
    <span className={`badge ${status}`}>
      <span className="status-dot" />
      {label}
    </span>
  );
}

export function nodeStatus(run, id) {
  if (id === "done")
    return run.status === "completed" ? "completed" : "pending";
  const stage = run.stages.filter((stage) => stage.node === id).at(-1);
  if (!stage) return "pending";
  if (stage.outcome !== "started")
    return stage.outcome === "changes" ? "changes" : "completed";
  if (run.currentNode === id && run.status === "failed") return "failed";
  if (run.currentNode === id && run.status === "running") return "running";
  return "unknown";
}

function Workflow({ run, selected, onSelect }) {
  return (
    <section className="workflow panel" aria-labelledby="workflow-title">
      <div className="section-heading">
        <h2 id="workflow-title">Workflow</h2>
        <span className="quiet">Select a node to inspect</span>
      </div>
      <div className="graph">
        {NODES.map((node) => {
          const status = nodeStatus(run, node.id);
          const stage = run.stages
            .filter((stage) => stage.node === node.id)
            .at(-1);
          return (
            <button
              key={node.id}
              type="button"
              data-testid={`node-${node.id}`}
              data-status={status}
              aria-pressed={selected === node.id}
              className={`node node-${node.id} ${status} ${selected === node.id ? "selected" : ""}`}
              onClick={() => onSelect(node.id)}
            >
              <span className="node-top">
                <span className="node-number">{node.number}</span>
                <span className="node-state">
                  {status === "completed"
                    ? "✓"
                    : status === "running"
                      ? "●"
                      : status === "failed"
                        ? "!"
                        : status === "changes"
                          ? "↻"
                          : "○"}
                </span>
              </span>
              <span className="node-title">{node.label}</span>
              <span className="node-description">{node.detail}</span>
              <span className="node-footer">
                {status === "pending"
                  ? "Not reached"
                  : status === "changes"
                    ? "Changes requested"
                    : status === "unknown"
                      ? "Activity unconfirmed"
                      : status.charAt(0).toUpperCase() + status.slice(1)}
                {stage && stage.occurrence > 1 ? ` · #${stage.occurrence}` : ""}
              </span>
            </button>
          );
        })}
      </div>
      <div className="graph-caption">
        <span>
          <i className="legend-dot" /> Implement → Review → Done on approval
        </span>
        <span>Review → Fix review → Review when changes are needed</span>
      </div>
    </section>
  );
}

function SessionDetail({ run, node }) {
  const [sessionName, setSessionName] = useState(null);
  const [tab, setTab] = useState("text");
  const sessions = run.sessions.filter(
    (session) => sessionNode(session.name) === node,
  );
  const selected =
    sessions.find((session) => session.name === sessionName) ?? sessions.at(-1);
  return (
    <section className="session-panel panel" aria-labelledby="session-title">
      <div className="section-heading">
        <div>
          <span className="eyebrow">INSPECTING NODE</span>
          <h2 id="session-title" data-testid="inspected-node">
            {nodeLabel(node)}
          </h2>
        </div>
        <span className="quiet">
          {sessions.length} {sessions.length === 1 ? "session" : "sessions"}
        </span>
      </div>
      {sessions.length > 0 ? (
        <>
          <div
            className="session-picker"
            role="group"
            aria-label="Agent sessions"
          >
            {sessions.map((session) => (
              <button
                type="button"
                key={session.name}
                aria-pressed={session.name === selected?.name}
                onClick={() => setSessionName(session.name)}
              >
                <span className={`status-dot ${session.status}`} />
                {sessionLabel(session.name)}
              </button>
            ))}
          </div>
          {selected && (
            <>
              <div className="session-meta">
                <code>{selected.name}</code>
                <span>
                  {selected.observedTurns !== null
                    ? `${selected.observedTurns} / ${selected.maxTurns ?? "?"} turns`
                    : "Turn count unavailable"}
                </span>
                <Badge status={selected.status} />
              </div>
              {selected.error && (
                <div className="inline-error" role="status">
                  {selected.error}
                </div>
              )}
              <div
                className="output-tabs"
                role="tablist"
                aria-label="Session output"
              >
                <button
                  id="text-tab"
                  role="tab"
                  aria-controls="session-output"
                  aria-selected={tab === "text"}
                  onClick={() => setTab("text")}
                >
                  Agent text
                </button>
                <button
                  id="tools-tab"
                  role="tab"
                  aria-controls="session-output"
                  aria-selected={tab === "tools"}
                  onClick={() => setTab("tools")}
                >
                  Tool results
                </button>
                <span className="quiet">Latest 64 KiB per stream</span>
              </div>
              {selected[tab].truncated && (
                <div className="truncation">
                  Earlier output omitted. Showing the latest 64 KiB.
                </div>
              )}
              <pre
                className="output"
                id="session-output"
                role="tabpanel"
                aria-labelledby={`${tab}-tab`}
                data-testid="session-output"
              >
                {selected[tab].content ||
                  (selected.status === "running"
                    ? "Waiting for agent output…"
                    : "No output recorded for this stream.")}
              </pre>
            </>
          )}
        </>
      ) : (
        <div className="empty-output">
          <span className="empty-symbol">{node === "done" ? "✓" : "⌁"}</span>
          <h3>
            {node === "done" ? "Workflow terminal" : "No sessions recorded"}
          </h3>
          <p>
            {node === "done"
              ? "The workflow finishes after both reviewers approve. Agent output stays available in earlier nodes."
              : "Agent text and tool activity will appear here when this node starts."}
          </p>
        </div>
      )}
    </section>
  );
}

function Timeline({ run }) {
  return (
    <aside className="timeline panel" aria-labelledby="timeline-title">
      <div className="section-heading">
        <h2 id="timeline-title">Event trail</h2>
        <span className="quiet">{run.stages.length}</span>
      </div>
      <ol>
        {[...run.stages].reverse().map((stage, index) => (
          <li
            key={`${run.stages.length - index}-${stage.node}-${stage.occurrence}`}
          >
            <span className={`event-dot ${stage.outcome}`} />
            <div>
              <strong>
                {nodeLabel(stage.node)}
                <span className="event-round">#{stage.occurrence}</span>
              </strong>
              <span className="event-outcome">
                {
                  {
                    started: "Started",
                    implemented: "Implementation committed",
                    changes: "Changes requested",
                    approved: "Both reviewers approved",
                    fixed: "Fixes committed",
                  }[stage.outcome]
                }
              </span>
            </div>
          </li>
        ))}
      </ol>
      {run.stages.length === 0 && (
        <p className="quiet no-events">Waiting for the first workflow event.</p>
      )}
      <div className="timeline-note">
        Events are read from disk in recorded order.
      </div>
    </aside>
  );
}

function App() {
  const [runId, setRunId] = useState(() =>
    new URLSearchParams(location.search).get("run"),
  );
  const [inspectedNode, setInspectedNode] = useState(null);
  const [filter, setFilter] = useState("");
  const { runs, warnings, snapshot: run, error, lastSync } = useMonitor(runId);
  useEffect(() => {
    if (!runId && runs?.length) selectRun(runs[0].id);
  }, [runs, runId]);
  function selectRun(id) {
    setRunId(id);
    setInspectedNode(null);
    const url = new URL(location.href);
    url.searchParams.set("run", id);
    history.replaceState(null, "", url);
  }
  const visibleRuns =
    runs?.filter((item) =>
      `${item.repo ?? ""} ${item.branch ?? ""} ${item.id}`
        .toLowerCase()
        .includes(filter.toLowerCase()),
    ) ?? [];
  const selectedNode = inspectedNode ?? run?.currentNode ?? "implement";
  return (
    <div className="app-shell">
      <a className="skip-link" href="#main">
        Skip to run details
      </a>
      <aside className="sidebar">
        <a href="/" className="brand" aria-label="Crabber monitor home">
          <span className="brand-icon">c</span>
          <span>
            crabber<span className="brand-sub">WORKFLOW MONITOR</span>
          </span>
        </a>
        <div className="sidebar-heading">
          <h2>Runs</h2>
          <span className="count">{runs?.length ?? "—"}</span>
        </div>
        <label className="search">
          <span>⌕</span>
          <input
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="Find a run…"
            aria-label="Find a run"
          />
        </label>
        <nav className="run-list" aria-label="Workflow runs">
          {visibleRuns.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`run-card ${item.id === runId ? "selected" : ""}`}
              aria-current={item.id === runId ? "true" : undefined}
              onClick={() => selectRun(item.id)}
            >
              <span className="run-card-top">
                <strong>{repoName(item.repo)}</strong>
                <span className={`status-dot ${item.status}`} />
              </span>
              <span className="run-branch">
                ⑂ {item.branch ?? "Branch unavailable"}
              </span>
              <span className="run-card-bottom">
                <code>{item.id.slice(0, 8)}</code>
                <span>{item.status}</span>
              </span>
            </button>
          ))}
        </nav>
        {runs?.length === 0 && (
          <p className="sidebar-empty">No run artifacts yet.</p>
        )}
        {runs && runs.length > 0 && visibleRuns.length === 0 && (
          <p className="sidebar-empty">No matching runs.</p>
        )}
        <div className="sidebar-footer">
          <span className={`status-dot ${error ? "failed" : "running"}`} />
          {error ? "Reader disconnected" : "Artifact reader connected"}
          <span className="read-only">READ ONLY</span>
        </div>
      </aside>
      <main id="main">
        <header className="topbar">
          <span>
            WORKSPACE <span className="breadcrumb">/</span> Run monitor
          </span>
          <span className="poll-label">
            <span className={`status-dot ${error ? "failed" : "running"}`} />
            {error ? "Reconnecting" : "Polling every second"}
          </span>
        </header>
        {warnings.map((warning) => (
          <div className="connection-error" key={warning}>
            {warning}
          </div>
        ))}
        {error && (
          <div className="connection-error" role="alert">
            <strong>Connection interrupted.</strong> {error}. Retrying
            automatically. {lastSync && "Showing the last successful snapshot."}
          </div>
        )}
        {runs === null && !run && (
          <div className="full-empty">
            <span className="empty-symbol">⌁</span>
            <h1>
              {error
                ? "Artifact reader unavailable"
                : "Loading your workspace…"}
            </h1>
            <p>
              {error
                ? "The monitor will reconnect automatically."
                : "Reading workflow runs from local artifacts."}
            </p>
          </div>
        )}
        {runs?.length === 0 && (
          <div className="full-empty">
            <span className="empty-symbol">⌁</span>
            <span className="eyebrow">READY WHEN YOU ARE</span>
            <h1>No workflow runs yet</h1>
            <p>
              Start crabber-implement with your plan. This monitor will pick up
              its artifacts automatically.
            </p>
            <code>crabber-implement --help</code>
          </div>
        )}
        {runs && runs.length > 0 && !run && (
          <div className="full-empty">
            <h1>{error ? "Run unavailable" : "Loading run…"}</h1>
            <p>
              {error
                ? "Select another run or wait for the reader to reconnect."
                : "Gathering sessions and workflow events."}
            </p>
          </div>
        )}
        {run && (
          <div className="main-content">
            <div className="run-heading">
              <div>
                <span className="eyebrow">RUN OVERVIEW</span>
                <h1>
                  {repoName(run.repo)} <span className="heading-slash">/</span>{" "}
                  <span className="branch-name">
                    {run.branch ?? "Archived run"}
                  </span>
                </h1>
                <div className="run-identifiers">
                  <code>{run.id}</code>
                  <span>{run.model ?? "Model unavailable"}</span>
                </div>
              </div>
              <Badge status={run.status} />
            </div>
            <section
              className="current-strip"
              aria-label="Current workflow state"
            >
              <div>
                <span className="eyebrow">CURRENT NODE</span>
                <strong
                  data-testid="current-node"
                  data-node={run.currentNode ?? ""}
                >
                  {nodeLabel(run.currentNode)}
                </strong>
              </div>
              <p data-testid="run-status" data-status={run.status}>
                {run.status === "running"
                  ? "Heartbeat active. Following live artifact updates."
                  : run.status === "completed"
                    ? "Workflow finished. Both reviewers approved."
                    : run.status === "failed"
                      ? "Workflow stopped with a failure. Inspect the recorded output."
                      : "No recent heartbeat. Activity cannot be confirmed."}
              </p>
              <span className="current-mark">
                {run.status === "running"
                  ? "↗"
                  : run.status === "completed"
                    ? "✓"
                    : "·"}
              </span>
            </section>
            {run.error && (
              <div className="inline-error" role="alert">
                {run.error}
              </div>
            )}
            {run.warnings.map((warning) => (
              <div className="warning" key={warning}>
                {warning}
              </div>
            ))}
            <Workflow
              run={run}
              selected={selectedNode}
              onSelect={setInspectedNode}
            />
            <div className="detail-grid">
              <SessionDetail
                key={`${run.id}-${selectedNode}`}
                run={run}
                node={selectedNode}
              />
              <Timeline run={run} />
            </div>
            {run.plan && (
              <details className="plan panel">
                <summary>
                  Supplied plan {run.plan.truncated ? "· truncated" : ""}
                </summary>
                <pre>{run.plan.content}</pre>
              </details>
            )}
            <footer className="main-footer">
              <span>LOCAL ARTIFACTS · NO AGENT CONTROL</span>
              <span>
                {lastSync
                  ? `Synced ${new Date(lastSync).toLocaleTimeString()}`
                  : "Connecting"}
              </span>
            </footer>
          </div>
        )}
      </main>
    </div>
  );
}

createRoot(document.getElementById("root")).render(<App />);
