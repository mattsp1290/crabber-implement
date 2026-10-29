import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { writeFile } from 'node:fs/promises';
const require = createRequire(process.env.PLAYWRIGHT_PACKAGE_JSON || '/work/package.json');
const { chromium, expect } = require('@playwright/test');
const evidence = process.env.PLAYWRIGHT_EVIDENCE_DIR || '/evidence';
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
const page = await context.newPage();
const errors = [];
page.on('pageerror', error => errors.push(error.message));
const snapshot = async () => (await page.request.get(`${process.env.PLAYWRIGHT_BASE_URL}/api/runs/${process.env.LIVE_RUN_ID}`)).json();
try {
  await page.goto(process.env.PLAYWRIGHT_BASE_URL);
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'failed');
  await expect(page.getByTestId('current-node')).toHaveAttribute('data-node', 'review');
  const failed = await snapshot();
  assert.equal(failed.sessions.find(s => s.reviewRole === 'correctness').status, 'completed');
  assert.equal(failed.sessions.find(s => s.reviewRole === 'maintainability').status, 'failed');
  await page.screenshot({ path: `${evidence}/review-failed.png`, fullPage: true });
  await writeFile(`${evidence}/review-continue`, 'resume only the missing reviewer\n');
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'running', { timeout: 30_000 });
  await expect(page.getByRole('button', { name: /Correctness · round 1 · attempt 1 · Reused/ })).toBeVisible();
  await expect.poll(async () => (await snapshot()).sessions.some(s => s.reviewRole === 'maintainability' && s.reviewAttempt === 2 && s.status === 'running'), { timeout: 30_000 }).toBe(true);
  const resumed = await snapshot();
  assert.equal(resumed.error, null);
  assert.equal(resumed.currentNode, 'review');
  await page.screenshot({ path: `${evidence}/review-resumed.png`, fullPage: true });
  await writeFile(`${evidence}/review-release`, 'browser observed cached and restarted roles\n');
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'completed', { timeout: 45_000 });
  await page.screenshot({ path: `${evidence}/review-completed.png`, fullPage: true });
  assert.deepEqual(errors, []);
  await writeFile(`${evidence}/review-browser.json`, JSON.stringify({ failed: failed.status, resumed: resumed.status, completed: true, pageErrors: errors }, null, 2));
  await writeFile(`${evidence}/review-done`, 'browser observed completion\n');
} finally {
  await context.tracing.stop({ path: `${evidence}/review-recovery-trace.zip` });
  await browser.close();
}
