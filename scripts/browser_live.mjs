import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { access, writeFile } from 'node:fs/promises';
const require = createRequire('/work/package.json');
const { chromium, expect } = require('@playwright/test');
const browser = await chromium.launch();
const context = await browser.newContext({
  baseURL: process.env.PLAYWRIGHT_BASE_URL,
  viewport: { width: 1440, height: 1000 },
});
await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
const page = await context.newPage();
const errors = [];
page.on('pageerror', error => errors.push(error.message));
try {
  await page.goto(process.env.PLAYWRIGHT_BASE_URL);
  await expect(page.getByTestId('current-node')).toHaveAttribute('data-node', 'implement');
  await expect(page.getByTestId('session-output')).toContainText('Implemented live');
  const snapshot = await page.request.get(`/api/runs/${process.env.LIVE_RUN_ID}`);
  assert.equal(snapshot.status(), 200);
  const live = await snapshot.json();
  assert.equal(live.status, 'running');
  assert.equal(live.sessions.find(session => session.name === 'implement').status, 'running');
  assert.ok(live.sessions.find(session => session.name === 'implement').tools.content.length > 0);
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({ path: '/evidence/live-cli.png', fullPage: true });
  await writeFile('/evidence/restart-request', 'restart monitor during live execution\n');
  await expect.poll(async () => {
    try { await access('/evidence/restarted'); return true; }
    catch { return false; }
  }, { timeout: 30_000 }).toBe(true);
  await page.reload();
  await expect(page.getByTestId('run-status')).toHaveAttribute('data-status', 'running');
  await expect(page.getByTestId('session-output')).toContainText('Implemented live');
  await writeFile('/evidence/release', 'browser observed live output\n');
  await expect(page.getByTestId('current-node')).toHaveAttribute('data-node', 'done', { timeout: 45_000 });
  await page.getByTestId('node-implement').click();
  await expect(page.getByTestId('session-output')).toContainText('Implemented live artifacts and verified hello.txt.');
  await expect.poll(async () => {
    const response = await page.request.get(`/api/runs/${process.env.LIVE_RUN_ID}`);
    return (await response.json()).status;
  }).toBe('completed');
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({ path: '/evidence/completed-cli.png', fullPage: true });
  await page.reload();
  await expect(page.getByTestId('current-node')).toHaveAttribute('data-node', 'done');
  assert.deepEqual(errors, []);
  await writeFile('/evidence/live-browser.json', JSON.stringify({
    runId: process.env.LIVE_RUN_ID,
    liveStatus: live.status,
    liveText: live.sessions.find(session => session.name === 'implement').text.content,
    pageErrors: errors,
    completed: true,
  }, null, 2));
  await writeFile('/evidence/finished', 'browser observed completed output\n');
} finally {
  await context.tracing.stop({ path: '/evidence/live-cli-trace.zip' });
  await browser.close();
}
