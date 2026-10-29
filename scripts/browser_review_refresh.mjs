import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFile, writeFile } from 'node:fs/promises';

const require = createRequire(process.env.PLAYWRIGHT_PACKAGE_JSON || '/work/package.json');
const { chromium, expect } = require('@playwright/test');
const evidence = process.env.PLAYWRIGHT_EVIDENCE_DIR || '/evidence';
const exported = process.env.REFRESH_EXPORT_DIR || `${evidence}/cli`;
const base = process.env.PLAYWRIGHT_BASE_URL;
const parent = process.env.LIVE_RUN_ID;
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
const page = await context.newPage();
const errors = [];
page.on('pageerror', error => errors.push(error.message));
const snapshot = async id => (await page.request.get(`${base}/api/runs/${id}`)).json();
try {
  await page.goto(`${base}/?run=${parent}`);
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'failed');
  const failed = await snapshot(parent);
  assert.equal(failed.currentNode, 'review');
  await page.screenshot({ path: `${evidence}/refresh-source-failed.png`, fullPage: true });
  await writeFile(`${evidence}/refresh-continue`, 'source failure observed\n');
  let child;
  await expect.poll(async () => {
    try {
      child = JSON.parse(await readFile(`${exported}/refresh.json`, 'utf8'));
      return typeof child.run_id === 'string';
    } catch { return false; }
  }, { timeout: 60_000 }).toBe(true);
  assert.notEqual(child.run_id, parent);
  await page.goto(`${base}/?run=${child.run_id}`);
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'running', { timeout: 30_000 });
  await expect(page.getByTestId('refresh-lineage')).toContainText('Review round 4.');
  for (const role of ['Correctness', 'Maintainability']) {
    await expect(page.getByRole('button', { name: new RegExp(`${role}.*round 4.*attempt 1`) })).toBeVisible();
  }
  await expect(page.getByText(/Reused/, { exact: false })).toHaveCount(0);
  const running = await snapshot(child.run_id);
  assert.equal(running.parentWorkflowId, parent);
  assert.equal(running.recoveryMode, 'refresh');
  assert.equal(running.reviewRound, 4);
  assert.equal(running.sessions.filter(s => s.reviewRound === 4 && s.status === 'running' && !s.reused).length, 2);
  await page.screenshot({ path: `${evidence}/refresh-child-running.png`, fullPage: true });
  await writeFile(`${evidence}/refresh-release`, 'both fresh reviewers observed\n');
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'completed', { timeout: 60_000 });
  await page.screenshot({ path: `${evidence}/refresh-child-completed.png`, fullPage: true });
  await page.getByTestId('refresh-lineage').getByRole('link', { name: parent }).click();
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'failed');
  assert.equal((await snapshot(parent)).error, failed.error);
  assert.deepEqual(errors, []);
  await writeFile(`${evidence}/refresh-browser.json`, JSON.stringify({ parent, child: child.run_id, globalRound: 4, bothFresh: true, completed: true, sourceFailurePreserved: true, pageErrors: errors }, null, 2));
  await writeFile(`${evidence}/refresh-done`, 'completion and parent failure observed\n');
} finally {
  await context.tracing.stop({ path: `${evidence}/review-refresh-trace.zip` });
  await browser.close();
}
