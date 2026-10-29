import { constants } from "node:fs";
import { lstat, open, opendir, realpath } from "node:fs/promises";
import path from "node:path";
import {
  metadataSchema,
  resultSchema,
  sessionSchema,
  stageSchema,
  recoverySchema,
} from "./schema.js";

export const TAIL_BYTES = 64 * 1024;
const STAGE_BYTES = 1024 * 1024;
const MAX_RUNS = 500;
const MAX_SESSIONS = 400;
const MAX_ENTRIES = 5000;
const JSON_BYTES = 64 * 1024;
const ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const SESSION =
  /^(implement|review-\d+-(correctness|maintainability)(-attempt-[1-9]\d*)?|fix-review-\d+)$/;

export class ReaderError extends Error {
  constructor(message, status = 500) {
    super(message);
    this.status = status;
  }
}

export function validRunId(value) {
  return ID.test(value);
}

async function directory(root, id) {
  if (!validRunId(id)) throw new ReaderError("Invalid run identifier", 400);
  const location = path.join(root, id);
  let stat;
  try {
    stat = await lstat(location);
  } catch (error) {
    if (error.code === "ENOENT") throw new ReaderError("Run not found", 404);
    throw error;
  }
  if (
    !stat.isDirectory() ||
    stat.isSymbolicLink() ||
    (await realpath(location)) !== location
  ) {
    throw new ReaderError("Unsafe run directory", 403);
  }
  return { location, device: stat.dev, inode: stat.ino };
}

async function readFile(run, name, limit, tail = false) {
  const location = path.join(run.location, name);
  let handle;
  try {
    handle = await open(
      location,
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
    const stat = await handle.stat();
    const parent = await lstat(run.location);
    if (
      !stat.isFile() ||
      parent.isSymbolicLink() ||
      parent.dev !== run.device ||
      parent.ino !== run.inode
    ) {
      throw new ReaderError("Unsafe artifact file", 403);
    }
    if (!tail && stat.size > limit)
      throw new ReaderError(`${name} exceeds the metadata size limit`);
    const length = Math.min(stat.size, limit);
    const buffer = Buffer.alloc(length);
    const { bytesRead } = await handle.read(
      buffer,
      0,
      length,
      tail ? stat.size - length : 0,
    );
    const after = await lstat(run.location);
    if (after.dev !== run.device || after.ino !== run.inode)
      throw new ReaderError("Run changed while reading", 409);
    let start = 0;
    if (tail && stat.size > length)
      while (start < bytesRead && (buffer[start] & 0xc0) === 0x80) start++;
    return {
      content: buffer.subarray(start, bytesRead).toString("utf8"),
      truncated: stat.size > length,
    };
  } catch (error) {
    if (error.code === "ENOENT") return null;
    if (error.code === "ELOOP")
      throw new ReaderError("Symlink artifacts are not readable", 403);
    throw error;
  } finally {
    await handle?.close();
  }
}

async function json(run, name, schema, warnings, limit = JSON_BYTES) {
  try {
    const file = await readFile(run, name, limit);
    if (!file) return null;
    return schema.parse(JSON.parse(file.content));
  } catch (error) {
    if (error instanceof ReaderError && error.status === 403) throw error;
    warnings.push(`Cannot read ${name}. It may be incomplete.`);
    return null;
  }
}

async function stagesFor(run, warnings) {
  const file = await readFile(run, "stages.jsonl", STAGE_BYTES, true);
  if (!file) return [];
  if (file.truncated)
    warnings.push("Event history is truncated to the latest 1 MiB.");
  const lines = file.content.split("\n");
  if (file.truncated) lines.shift();
  const stages = [];
  for (const line of lines) {
    if (!line.trim()) continue;
    try {
      stages.push(stageSchema.parse(JSON.parse(line)));
    } catch {
      warnings.push("An incomplete or invalid event was skipped.");
    }
  }
  return stages;
}

export function deriveState({ stages, metadata, result, now }) {
  if (result && (result.execution_generation ?? 1) !== (metadata?.execution_generation ?? 1)) result = null;
  const last = stages.at(-1);
  const heartbeat = metadata?.heartbeat_at_ms;
  const fresh =
    heartbeat !== undefined &&
    now - heartbeat <= 15_000 &&
    heartbeat <= now + 5_000;
  const status = result
    ? result.status === "completed"
      ? "completed"
      : "failed"
    : fresh
      ? "running"
      : "unknown";
  const currentNode = status === "completed" ? "done" : (last?.node ?? null);
  return { status, currentNode };
}

async function readRun(root, id, full, now) {
  const run = await directory(root, id);
  const warnings = [];
  const [metadata, rawResult, stages] = await Promise.all([
    json(run, "run.json", metadataSchema, warnings),
    json(run, "result.json", resultSchema, warnings),
    stagesFor(run, warnings),
  ]);
  const result = rawResult && (rawResult.execution_generation ?? 1) === (metadata?.execution_generation ?? 1) ? rawResult : null;
  const stat = await lstat(run.location);
  const state = deriveState({ metadata, result, stages, now });
  const summary = {
    id,
    repo: metadata?.repo ?? null,
    branch: metadata?.branch ?? null,
    model: metadata?.model ?? null,
    ...state,
    updatedAt: metadata?.heartbeat_at_ms ?? stat.mtimeMs,
    heartbeatAt: metadata?.heartbeat_at_ms ?? null,
    resultStatus: result?.status ?? null,
    error: result?.error ?? null,
    parentWorkflowId: metadata?.parent_workflow_id ?? null,
    recoveryMode: metadata?.recovery_mode ?? null,
    reviewRound: metadata?.review_round ?? null,
  };
  if (!full) return summary;
  const recovery = await json(run, "review-state.json", recoverySchema, warnings, 1024 * 1024);
  const activeRecovery = recovery?.workflow_id === id && recovery.generation === metadata?.execution_generation ? recovery : null;
  const activeNames = activeRecovery ? ["correctness","maintainability"].map((role,i) => activeRecovery.attempts[i] > 0 ? `review-${activeRecovery.round}-${role}-attempt-${activeRecovery.attempts[i]}` : null) : [];
  const names = new Set(activeNames.filter(Boolean));
  let scanned = 0;
  for await (const entry of await opendir(run.location)) {
    const base = entry.name.replace(
      /(\.session\.json|\.tools\.txt|\.txt)$/,
      "",
    );
    if (SESSION.test(base) && base !== entry.name) names.add(base);
    if (++scanned >= MAX_ENTRIES || names.size >= MAX_SESSIONS) {
      warnings.push(
        `Session enumeration limited to ${MAX_SESSIONS} sessions and ${MAX_ENTRIES} directory entries.`,
      );
      break;
    }
  }
  const sessions = await Promise.all(
    [...names].sort(sessionOrder).map(async (name) => {
      const [session, text, tools] = await Promise.all([
        json(run, `${name}.session.json`, sessionSchema, warnings),
        readFile(run, `${name}.txt`, TAIL_BYTES, true),
        readFile(run, `${name}.tools.txt`, TAIL_BYTES, true),
      ]);
      return {
        name,
        status:
          session?.status === "running" && (state.status !== "running" || (session.execution_generation ?? 1) !== (metadata?.execution_generation ?? 1))
            ? "unknown"
            : (session?.status ?? "unknown"),
        observedTurns: session?.observed_turns ?? null,
        reviewRound: session?.review_round ?? null,
        reviewRole: session?.review_role ?? null,
        reviewAttempt: session?.review_attempt ?? null,
        reused: !!activeRecovery && activeNames.some((n,i) => n === name && activeRecovery.reused[i]),
        maxTurns: session?.max_turns ?? null,
        text: text ?? { content: "", truncated: false },
        tools: tools ?? { content: "", truncated: false },
        error: session?.error ?? null,
      };
    }),
  );
  const plan = await readFile(run, "plan.md", TAIL_BYTES, true);
  return {
    ...summary,
    stages,
    sessions,
    plan,
    warnings: [...new Set(warnings)],
  };
}

function sessionOrder(a, b) {
  const key = (name) =>
    name === "implement"
      ? 0
      : Number(name.match(/\d+/)?.[0] ?? 0) * 10 +
        (name.startsWith("fix") ? 5 : 0);
  return key(a) - key(b) || a.localeCompare(b);
}

export async function createReader(artifactsDir, { now = Date.now } = {}) {
  const root = await realpath(artifactsDir);
  if (!(await lstat(root)).isDirectory())
    throw new ReaderError("Artifacts path is not a directory");
  return {
    async list() {
      const runs = [];
      const warnings = [];
      let scanned = 0;
      for await (const entry of await opendir(root)) {
        if (++scanned > MAX_ENTRIES || runs.length >= MAX_RUNS) {
          warnings.push(
            `Run enumeration limited to ${MAX_RUNS} runs and ${MAX_ENTRIES} directory entries.`,
          );
          break;
        }
        if (!entry.isDirectory() || !validRunId(entry.name)) continue;
        try {
          runs.push(await readRun(root, entry.name, false, now()));
        } catch (error) {
          if (!(error instanceof ReaderError)) throw error;
        }
      }
      return {
        runs: runs.sort(
          (a, b) => b.updatedAt - a.updatedAt || a.id.localeCompare(b.id),
        ),
        warnings,
      };
    },
    snapshot(id) {
      return readRun(root, id, true, now());
    },
  };
}
