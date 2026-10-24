import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, symlink, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createReader, deriveState, TAIL_BYTES } from "../server/reader.js";
import { createServer } from "../server/index.js";

const ID = "11111111-1111-4111-8111-111111111111";
const now = 1_800_000_000_000;
const metadata = {
  version: 1,
  run_id: ID,
  repo: "/work/example",
  branch: "feature/live",
  model: "test-model",
  heartbeat_at_ms: now,
};
const event = (node, occurrence, outcome) => ({ node, occurrence, outcome });

async function fixture(t) {
  const root = await mkdtemp(path.join(os.tmpdir(), "crabber-reader-"));
  const run = path.join(root, ID);
  await mkdir(run);
  t.after(() => rm(root, { recursive: true, force: true }));
  const put = (name, value) =>
    writeFile(
      path.join(run, name),
      typeof value === "string" ? value : JSON.stringify(value),
    );
  return {
    root,
    run,
    put,
    reader: await createReader(root, { now: () => now }),
  };
}

test("fresh heartbeat confirms running and latest repeated review occurrence wins", async (t) => {
  const { put, reader } = await fixture(t);
  await put("run.json", metadata);
  await put(
    "stages.jsonl",
    [
      event("implement", 1, "implemented"),
      event("review", 1, "changes"),
      event("fix-review", 1, "fixed"),
      event("review", 2, "started"),
    ]
      .map(JSON.stringify)
      .join("\n") + "\n",
  );
  const run = await reader.snapshot(ID);
  assert.equal(run.status, "running");
  assert.equal(run.currentNode, "review");
  assert.deepEqual(run.stages.at(-1), event("review", 2, "started"));
  assert.equal((await reader.list()).runs[0].id, ID);
});

test("terminal status overrides fresh heartbeat and successful approval alone is not completion", () => {
  const input = { stages: [event("review", 2, "approved")], metadata, now };
  assert.deepEqual(deriveState(input), {
    status: "running",
    currentNode: "review",
  });
  assert.deepEqual(deriveState({ ...input, result: { status: "completed" } }), {
    status: "completed",
    currentNode: "done",
  });
  assert.deepEqual(deriveState({ ...input, result: { status: "failed" } }), {
    status: "failed",
    currentNode: "review",
  });
});

test("stale and legacy runs never claim activity; failed session error survives", async (t) => {
  const { put, reader } = await fixture(t);
  await put(
    "stages.jsonl",
    JSON.stringify(event("implement", 1, "started")) + "\n",
  );
  await put("implement.session.json", {
    name: "implement",
    status: "running",
    observed_turns: 2,
    max_turns: 8,
  });
  assert.equal((await reader.snapshot(ID)).status, "unknown");
  assert.equal((await reader.snapshot(ID)).sessions[0].status, "unknown");
  await put("run.json", { ...metadata, heartbeat_at_ms: now - 15_001 });
  assert.equal((await reader.snapshot(ID)).status, "unknown");
  await put("result.json", { status: "failed", error: "Provider unavailable" });
  await put("implement.session.json", {
    name: "implement",
    status: "failed",
    error: "Provider unavailable",
  });
  const failed = await reader.snapshot(ID);
  assert.equal(failed.status, "failed");
  assert.equal(failed.error, "Provider unavailable");
  assert.equal(failed.sessions[0].error, "Provider unavailable");
});

test("sessions include both reviewers, live tails are bounded and incomplete events are skipped", async (t) => {
  const { put, reader } = await fixture(t);
  await put("run.json", metadata);
  await put(
    "stages.jsonl",
    JSON.stringify(event("review", 1, "started")) + '\n{"node":',
  );
  await put("review-1-correctness.txt", "first output");
  await put(
    "review-1-maintainability.txt",
    "x".repeat(TAIL_BYTES + 100) + "latest",
  );
  await put("review-1-maintainability.tools.txt", "tool latest");
  let run = await reader.snapshot(ID);
  assert.equal(run.sessions.length, 2);
  assert.equal(run.sessions[1].text.truncated, true);
  assert.ok(Buffer.byteLength(run.sessions[1].text.content) <= TAIL_BYTES);
  assert.ok(run.sessions[1].text.content.endsWith("latest"));
  assert.equal(run.sessions[1].tools.content, "tool latest");
  assert.equal(run.stages.length, 1);
  assert.match(run.warnings[0], /invalid event/);
  await put("review-1-correctness.txt", "live output update");
  run = await reader.snapshot(ID);
  assert.equal(run.sessions[0].text.content, "live output update");
});

test("invalid IDs, directory symlinks, artifact symlinks and non-regular artifacts cannot expose files", async (t) => {
  const { root, run, reader } = await fixture(t);
  for (const id of ["../secret", "%2e%2e", "implement", `${ID}/..`])
    await assert.rejects(reader.snapshot(id), { status: 400 });
  const other = "22222222-2222-4222-8222-222222222222";
  await symlink(run, path.join(root, other));
  await assert.rejects(reader.snapshot(other), { status: 403 });
  assert.equal((await reader.list()).runs.length, 1);
  await writeFile(path.join(root, "secret"), "hidden secret");
  await symlink(path.join(root, "secret"), path.join(run, "implement.txt"));
  await assert.rejects(reader.snapshot(ID), { status: 403 });
});

test("oversize and malformed metadata provide warnings instead of crashing", async (t) => {
  const { put, reader } = await fixture(t);
  await put("run.json", '{"version":');
  await put("result.json", "x".repeat(TAIL_BYTES + 1));
  const run = await reader.snapshot(ID);
  assert.equal(run.status, "unknown");
  assert.equal(run.warnings.length, 2);
});

test("HTTP endpoint is read only and serves the production app", async (t) => {
  const { put, reader } = await fixture(t);
  await put("run.json", metadata);
  const server = createServer(reader);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}`;
  assert.equal(
    (await fetch(`${base}/api/runs`, { method: "POST" })).status,
    405,
  );
  assert.equal((await fetch(`${base}/api/runs/${ID}`)).status, 200);
  assert.equal((await fetch(`${base}/api/runs/bad`)).status, 400);
  assert.equal((await fetch(`${base}/api/not-here`)).status, 404);
  const app = await fetch(base);
  assert.equal(app.status, 200);
  assert.match(await app.text(), /Crabber/);
  assert.equal(app.headers.get("x-content-type-options"), "nosniff");
});

test("all supported 100-round sessions remain available including the current reviewers", async (t) => {
  const { put, reader } = await fixture(t);
  await put("run.json", metadata);
  await put(
    "stages.jsonl",
    JSON.stringify(event("review", 100, "started")) + "\n",
  );
  await put("implement.session.json", {
    name: "implement",
    status: "completed",
  });
  for (let round = 1; round <= 100; round++) {
    for (const reviewer of ["correctness", "maintainability"]) {
      const name = `review-${round}-${reviewer}`;
      await put(`${name}.session.json`, {
        name,
        status: round === 100 ? "running" : "completed",
      });
    }
    if (round < 100) {
      const name = `fix-review-${round}`;
      await put(`${name}.session.json`, { name, status: "completed" });
    }
  }
  const run = await reader.snapshot(ID);
  assert.equal(run.sessions.length, 300);
  assert.deepEqual(
    run.sessions.slice(-2).map((session) => session.name),
    ["review-100-correctness", "review-100-maintainability"],
  );
  assert.ok(
    run.sessions.slice(-2).every((session) => session.status === "running"),
  );
  assert.equal(run.warnings.length, 0);
});

test("UTF-8 tails omit leading continuation bytes", async (t) => {
  const { put, reader } = await fixture(t);
  await put("implement.txt", "🦀".repeat(20_000) + "abc");
  const run = await reader.snapshot(ID);
  assert.equal(run.sessions[0].text.truncated, true);
  assert.ok(!run.sessions[0].text.content.includes("�"));
  assert.ok(run.sessions[0].text.content.endsWith("abc"));
});
