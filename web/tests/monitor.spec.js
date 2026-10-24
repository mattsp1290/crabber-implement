import { test, expect } from "@playwright/test";
import { mkdir, writeFile, appendFile, rm, rename } from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";

const root = process.env.TEST_ARTIFACTS_DIR;
let id;
let run;
let heartbeat;
const put = (name, value) =>
  writeFile(
    path.join(run, name),
    typeof value === "string" ? value : JSON.stringify(value),
  );
const stage = (node, occurrence, outcome) =>
  appendFile(
    path.join(run, "stages.jsonl"),
    JSON.stringify({ node, occurrence, outcome }) + "\n",
  );
const metadata = () => ({
  version: 1,
  run_id: id,
  repo: "/workspace/signal-lab",
  branch: "feat/live-monitor",
  model: "crabber-test",
  heartbeat_at_ms: Date.now(),
});
const session = (name, status = "running") =>
  put(`${name}.session.json`, {
    name,
    status,
    workflow_run_id: id,
    observed_turns: 1,
    max_turns: 12,
    error: null,
  });

test.beforeEach(async () => {
  id = randomUUID();
  run = path.join(root, id);
  await mkdir(run, { recursive: true });
  await put("run.json", metadata());
  await put("stages.jsonl", "");
  heartbeat = setInterval(() => {
    put("run.json", metadata()).catch(() => {});
  }, 2000);
});
test.afterEach(async () => {
  clearInterval(heartbeat);
  await rm(run, { recursive: true, force: true });
});

test("live disk writes flow through implementation, two reviewers, fix loop and terminal result", async ({
  page,
}, testInfo) => {
  await stage("implement", 1, "started");
  await session("implement");
  await put("implement.txt", "Reading the supplied plan.");
  await page.goto(`/?run=${id}`);
  await expect(page.getByTestId("current-node")).toHaveText("Implement");
  await expect(page.getByTestId("node-implement")).toHaveAttribute(
    "data-status",
    "running",
  );
  await expect(page.getByTestId("session-output")).toContainText(
    "Reading the supplied plan.",
  );
  await appendFile(
    path.join(run, "implement.txt"),
    '\nLive implementation output <script>alert("unsafe")</script>.',
  );
  await expect(page.getByTestId("session-output")).toContainText(
    "Live implementation output <script>",
  );
  await expect(
    page.locator("script").filter({ hasText: 'alert("unsafe")' }),
  ).toHaveCount(0);
  await session("implement", "completed");
  await stage("implement", 1, "implemented");
  await stage("review", 1, "started");
  await session("review-1-correctness");
  await session("review-1-maintainability");
  await put("review-1-correctness.txt", "Correctness findings arrived.");
  await put(
    "review-1-maintainability.txt",
    "Maintainability findings arrived.",
  );
  await expect(page.getByTestId("current-node")).toHaveText("Review");
  await expect(
    page.getByRole("button", { name: "Correctness · round 1" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Maintainability · round 1" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Correctness · round 1" }).click();
  await expect(page.getByTestId("session-output")).toContainText(
    "Correctness findings",
  );
  await page.getByTestId("node-implement").click();
  await expect(page.getByTestId("inspected-node")).toHaveText("Implement");
  await expect(page.getByTestId("current-node")).toHaveText("Review");
  await stage("review", 1, "changes");
  await stage("fix-review", 1, "started");
  await session("fix-review-1");
  await put("fix-review-1.txt", "Fixing findings.");
  await expect(page.getByTestId("current-node")).toHaveText("Fix review");
  await expect(page.getByTestId("inspected-node")).toHaveText("Implement");
  await stage("fix-review", 1, "fixed");
  await stage("review", 2, "started");
  await session("review-2-correctness");
  await session("review-2-maintainability");
  await put("review-2-correctness.txt", "Second round is clean.");
  await expect(page.getByTestId("node-review")).toHaveAttribute(
    "data-status",
    "running",
  );
  await page.getByTestId("node-review").click();
  await expect(
    page.getByRole("button", { name: "Correctness · round 2" }),
  ).toBeVisible();
  await put("review-2-maintainability.txt", "Maintainability review is clean.");
  for (const name of [
    "review-1-correctness",
    "review-1-maintainability",
    "fix-review-1",
    "review-2-correctness",
    "review-2-maintainability",
  ])
    await session(name, "completed");
  await stage("review", 2, "approved");
  await put("result.json", { status: "completed", run_id: "dmt-run" });
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "completed",
  );
  await expect(page.getByTestId("current-node")).toHaveText("Done");
  await expect(page.getByTestId("node-done")).toHaveAttribute(
    "data-status",
    "completed",
  );
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({
    path: testInfo.outputPath("completed-desktop.png"),
    fullPage: true,
  });
});

test("mobile keeps node selection, live tools, truncation and failure accessible", async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await stage("implement", 1, "started");
  await session("implement");
  await put("implement.txt", "x".repeat(70_000) + "\nLatest retained text.");
  await put(
    "implement.tools.txt",
    "read_file src/main.rs\nTool result arrived.",
  );
  await page.goto(`/?run=${id}`);
  await expect(
    page.getByText("Earlier output omitted. Showing the latest 64 KiB."),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Tool results" }).click();
  await expect(page.getByTestId("session-output")).toContainText(
    "Tool result arrived.",
  );
  await appendFile(
    path.join(run, "implement.tools.txt"),
    "\nLive tool append.",
  );
  await expect(page.getByTestId("session-output")).toContainText(
    "Live tool append.",
  );
  await put("result.json", { status: "failed", error: "Session timed out" });
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "failed",
  );
  await expect(page.getByRole("alert")).toContainText("Session timed out");
  await expect(page.getByTestId("node-implement")).toHaveAttribute(
    "data-status",
    "failed",
  );
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({
    path: testInfo.outputPath("mobile-failure.png"),
    fullPage: true,
  });
});

test("stale heartbeat and archived runs report unknown until a terminal result exists", async ({
  page,
}) => {
  clearInterval(heartbeat);
  await put("run.json", {
    ...metadata(),
    heartbeat_at_ms: Date.now() - 20_000,
  });
  await stage("review", 1, "started");
  await session("review-1-correctness");
  await page.goto(`/?run=${id}`);
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "unknown",
  );
  await expect(page.getByTestId("node-review")).toHaveAttribute(
    "data-status",
    "unknown",
  );
  await rm(path.join(run, "run.json"));
  await put("result.json", { status: "completed" });
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "completed",
  );
  await expect(page.getByRole("heading", { level: 1 })).toContainText(
    "Archived run",
  );
});

test("storage errors retain the last snapshot and recover through real artifact reads", async ({
  page,
}) => {
  await stage("implement", 1, "started");
  await session("implement");
  await put("implement.txt", "Snapshot survives the outage.");
  await page.goto(`/?run=${id}`);
  await expect(page.getByTestId("session-output")).toContainText(
    "Snapshot survives the outage.",
  );
  clearInterval(heartbeat);
  const hidden = `${run}.temporarily-unavailable`;
  await rename(run, hidden);
  try {
    await expect(page.getByRole("alert")).toContainText(
      "Connection interrupted.",
    );
    await expect(page.getByTestId("session-output")).toContainText(
      "Snapshot survives the outage.",
    );
    await page.clock.install({ time: new Date(Date.now() + 20_000) });
    await expect(page.getByTestId("run-status")).toHaveAttribute(
      "data-status",
      "unknown",
    );
    await page.clock.setFixedTime(new Date());
  } finally {
    await rename(hidden, run);
  }
  await put("run.json", metadata());
  await expect(page.getByRole("alert")).toHaveCount(0);
  await appendFile(path.join(run, "implement.txt"), "\nRecovered output.");
  await expect(page.getByTestId("session-output")).toContainText(
    "Recovered output.",
  );
});

test("run selector is searchable and keyboard operable", async ({ page }) => {
  await stage("implement", 1, "started");
  await session("implement");
  const other = randomUUID();
  const otherDir = path.join(root, other);
  await mkdir(otherDir);
  try {
    await writeFile(
      path.join(otherDir, "run.json"),
      JSON.stringify({
        ...metadata(),
        run_id: other,
        repo: "/workspace/keyboard-project",
        branch: "feat/keyboard",
      }),
    );
    await writeFile(
      path.join(otherDir, "result.json"),
      JSON.stringify({ status: "completed" }),
    );
    await page.goto(`/?run=${id}`);
    await page
      .getByRole("textbox", { name: "Find a run" })
      .fill("keyboard-project");
    const button = page
      .getByRole("navigation", { name: "Workflow runs" })
      .getByRole("button");
    await expect(button).toHaveCount(1);
    await button.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("heading", { level: 1 })).toContainText(
      "keyboard-project",
    );
    await expect(page.getByTestId("run-status")).toHaveAttribute(
      "data-status",
      "completed",
    );
  } finally {
    await rm(otherDir, { recursive: true, force: true });
  }
});

test("an unavailable run URL still exposes valid runs for recovery", async ({
  page,
}) => {
  await stage("implement", 1, "started");
  await session("implement");
  await put("implement.txt", "Recovered through the run selector.");
  await page.goto("/?run=not-a-valid-run");
  await expect(page.getByRole("alert")).toContainText(
    "Connection interrupted.",
  );
  const selector = page
    .getByRole("navigation", { name: "Workflow runs" })
    .getByRole("button")
    .filter({ hasText: id.slice(0, 8) });
  await selector.click();
  await expect(page.getByTestId("session-output")).toContainText(
    "Recovered through the run selector.",
  );
  await expect(page.getByRole("alert")).toHaveCount(0);
});
