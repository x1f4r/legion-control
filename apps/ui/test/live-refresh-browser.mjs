// Headless Chromium tests for live folder refresh and shared touch pull-to-refresh.
// Uses real touch input via CDP Input.dispatchTouchEvent moving across multiple frames.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(TEST_DIR, '..');

const MOCK_TEST_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self' blob: data:; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const MIME_BY_EXT = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.webmanifest': 'application/manifest+json; charset=utf-8',
};

function findBrowser() {
  if (process.env.LEGION_CHROME && fs.existsSync(process.env.LEGION_CHROME)) {
    return process.env.LEGION_CHROME;
  }
  const candidates = [
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error('No Chromium or Chrome binary found; set LEGION_CHROME.');
}

async function startStaticUiServer() {
  const server = http.createServer((req, res) => {
    const urlPath = (req.url || '/').split('?')[0];
    const rel = urlPath === '/' ? 'index.html' : urlPath.replace(/^\/+/, '');
    const full = path.resolve(UI_ROOT, rel);
    if (!full.startsWith(UI_ROOT) || !fs.existsSync(full) || fs.statSync(full).isDirectory()) {
      res.writeHead(404);
      res.end('Not found');
      return;
    }
    const ext = path.extname(full).toLowerCase();
    res.writeHead(200, {
      'Content-Type': MIME_BY_EXT[ext] || 'application/octet-stream',
      'Content-Security-Policy': MOCK_TEST_CSP,
      'Cache-Control': 'no-store',
    });
    fs.createReadStream(full).pipe(res);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}`,
    stop: () => new Promise((resolve) => server.close(resolve)),
  };
}

/**
 * Dispatches a genuine multi-frame touch gesture through Chromium's DevTools Input domain.
 */
async function realTouchSwipe(page, cdp, { startX, startY, endX, endY, steps = 12, stepDelayMs = 16, onMid = null }) {
  await cdp.send('Input.dispatchTouchEvent', {
    type: 'touchStart',
    touchPoints: [{ x: Math.round(startX), y: Math.round(startY), id: 1 }],
  });
  await page.waitForTimeout(stepDelayMs);

  const midStep = Math.floor(steps * 0.75);
  for (let i = 1; i <= steps; i += 1) {
    const frac = i / steps;
    const x = startX + (endX - startX) * frac;
    const y = startY + (endY - startY) * frac;
    await cdp.send('Input.dispatchTouchEvent', {
      type: 'touchMove',
      touchPoints: [{ x: Math.round(x), y: Math.round(y), id: 1 }],
    });
    await page.waitForTimeout(stepDelayMs);
    if (onMid && i === midStep) {
      await onMid();
    }
  }

  await cdp.send('Input.dispatchTouchEvent', {
    type: 'touchEnd',
    touchPoints: [],
  });
}

test('touch pull-to-refresh works across phone screens and respects scroll, early release, horizontal swipe, and edge back', async () => {
  const srv = await startStaticUiServer();
  const browser = await chromium.launch({ executablePath: findBrowser(), headless: true });

  try {
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      isMobile: true,
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
    page.on('console', (msg) => {
      if (msg.type() === 'error') errors.push(`console.error: ${msg.text()}`);
    });

    const cdp = await context.newCDPSession(page);

    // 1. Machines screen: pull down arms past 64px, enters refreshing for >=400ms, then settles.
    await page.goto(`${srv.url}/?mock=1#/machines`, { waitUntil: 'domcontentloaded' });
    await page.locator('.machine').first().waitFor({ state: 'visible', timeout: 10_000 });

    let sawArmed = false;
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 220,
      endX: 195,
      endY: 360,
      steps: 12,
      stepDelayMs: 18,
      async onMid() {
        const state = await page.locator('main.main').getAttribute('data-pull-state');
        if (state === 'armed') sawArmed = true;
      },
    });
    assert.equal(sawArmed, true, 'Machines pull should arm past 64px');
    const stateAfterRelease = await page.locator('main.main').getAttribute('data-pull-state');
    assert.equal(stateAfterRelease, 'refreshing', 'Machines pull should enter refreshing on release');
    await page.waitForTimeout(200);
    assert.equal(
      await page.locator('main.main').getAttribute('data-pull-state'),
      'refreshing',
      'Indicator must stay visible for at least 400ms',
    );
    await page.waitForFunction(
      () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
      null,
      { timeout: 6000 },
    );

    // 2. Nested folder in Files tab: external file appears on pull-to-refresh.
    await page.goto(`${srv.url}/?mock=1#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
    await page.locator('.where').waitFor({ state: 'visible', timeout: 10_000 });
    await page.locator('main .list .row').first().waitFor({ state: 'visible', timeout: 10_000 });

    // Early release (<64px) must cancel without refreshing.
    await page.evaluate(() => {
      window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents', 'early-release-check.txt');
    });
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 240,
      endX: 195,
      endY: 280,
      steps: 8,
      stepDelayMs: 16,
    });
    await page.waitForTimeout(250);
    assert.equal(
      await page.locator('main.main').getAttribute('data-pull-state'),
      'idle',
      'Early release under 64px must cancel back to idle',
    );
    assert.equal(
      await page.locator('text=early-release-check.txt').count(),
      0,
      'Early release must not refresh the folder',
    );

    // Horizontal swipe across middle of list must not trigger pull-to-refresh.
    await realTouchSwipe(page, cdp, {
      startX: 280,
      startY: 300,
      endX: 120,
      endY: 315,
      steps: 10,
      stepDelayMs: 16,
    });
    await page.waitForTimeout(200);
    assert.equal(
      await page.locator('text=early-release-check.txt').count(),
      0,
      'Horizontal swipe must not trigger pull-to-refresh',
    );

    // Full downward pull refreshes the nested folder and reveals the external file.
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 240,
      endX: 195,
      endY: 370,
      steps: 12,
      stepDelayMs: 18,
    });
    await page.locator('text=early-release-check.txt').waitFor({ state: 'visible', timeout: 6000 });
    await page.waitForFunction(
      () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
      null,
      { timeout: 6000 },
    );

    // 3. Left-edge back gesture inside nested folder still steps one folder up!
    await realTouchSwipe(page, cdp, {
      startX: 10,
      startY: 420,
      endX: 170,
      endY: 424,
      steps: 10,
      stepDelayMs: 14,
    });
    await page.waitForTimeout(200);
    const hashAfterEdgeBack = await page.evaluate(() => window.location.hash);
    assert.equal(
      hashAfterEdgeBack,
      '#/files/tower//home/me',
      `Left-edge back swipe inside nested folder should step one folder up (got ${hashAfterEdgeBack})`,
    );

    // 4. Pull while scrolled down scrolls up instead of triggering pull-to-refresh.
    await page.goto(`${srv.url}/?mock=1,large#/files/tower//home/me/Archive5000`, { waitUntil: 'domcontentloaded' });
    await page.locator('main .list .row').first().waitFor({ state: 'visible', timeout: 10_000 });
    await page.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 360;
      window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Archive5000', '000-scrolled-check.txt');
    });
    const beforeScroll = await page.evaluate(() => document.querySelector('main.main').scrollTop);
    assert.ok(beforeScroll >= 300, `expected scrollTop >= 300, got ${beforeScroll}`);

    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 220,
      endX: 195,
      endY: 380,
      steps: 12,
      stepDelayMs: 18,
    });
    await page.waitForTimeout(300);
    const afterScroll = await page.evaluate(() => document.querySelector('main.main').scrollTop);
    assert.ok(afterScroll < beforeScroll, `pull while scrolled down should scroll up (${afterScroll} < ${beforeScroll})`);
    assert.equal(
      await page.locator('text=000-scrolled-check.txt').count(),
      0,
      'Pull while scrolled down must not refresh',
    );

    // 5. Folder under a machine's Files tab.
    await page.goto(`${srv.url}/?mock=1#/machines/tower/files`, { waitUntil: 'domcontentloaded' });
    await page.locator('main .list .row').first().waitFor({ state: 'visible', timeout: 10_000 });
    await page.evaluate(() => {
      window.__LEGION_MOCK__.addExternalFile('tower', '/home/me', 'machine-tab-fresh.txt');
    });
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 240,
      endX: 195,
      endY: 370,
      steps: 12,
      stepDelayMs: 18,
    });
    await page.locator('text=machine-tab-fresh.txt').waitFor({ state: 'visible', timeout: 6000 });
    await page.waitForFunction(
      () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
      null,
      { timeout: 6000 },
    );

    // 6. Search results pull-to-refresh.
    await page.goto(`${srv.url}/?mock=1#/files/@search?q=freshsearch`, { waitUntil: 'domcontentloaded' });
    await page.locator('text=Nothing found').waitFor({ state: 'visible', timeout: 10_000 });
    await page.evaluate(() => {
      window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents', 'freshsearch-note.txt');
    });
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 240,
      endX: 195,
      endY: 370,
      steps: 12,
      stepDelayMs: 18,
    });
    await page.locator('text=freshsearch-note.txt').waitFor({ state: 'visible', timeout: 6000 });
    await page.waitForFunction(
      () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
      null,
      { timeout: 6000 },
    );

    // 7. Activity screen pull-to-refresh.
    await page.goto(`${srv.url}/?mock=1#/activity`, { waitUntil: 'domcontentloaded' });
    await page.locator('main .list .row').first().waitFor({ state: 'visible', timeout: 10_000 });
    await page.evaluate(() => {
      window.__LEGION_MOCK__.addExternalJob('tower', {
        id: 'j-pull-activity-1',
        name: 'Pull activity check',
        state: 'succeeded',
      });
    });
    await realTouchSwipe(page, cdp, {
      startX: 195,
      startY: 240,
      endX: 195,
      endY: 370,
      steps: 12,
      stepDelayMs: 18,
    });
    await page.locator('text=Pull activity check').waitFor({ state: 'visible', timeout: 6000 });

    assert.deepEqual(errors, [], `Unexpected browser errors:\n${errors.join('\n')}`);
    await context.close();
  } finally {
    await browser.close();
    await srv.stop();
  }
});

test('live folder refresh updates open folders on upload, background copy, rename, delete, mkdir, off-screen cache, and >10s resume', async () => {
  const srv = await startStaticUiServer();
  const browser = await chromium.launch({ executablePath: findBrowser(), headless: true });

  try {
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      isMobile: true,
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
    page.on('console', (msg) => {
      if (msg.type() === 'error') errors.push(`console.error: ${msg.text()}`);
    });

    // 1. Upload into an open scrollable folder: appears live without navigating and keeps scroll position.
    await page.goto(`${srv.url}/?mock=1#/files/tower//home/me`, { waitUntil: 'domcontentloaded' });
    await page.locator('main .list .row', { hasText: 'Documents' }).waitFor({ state: 'visible', timeout: 10_000 });

    // Seed 24 files before opening Documents so the folder is taller than the viewport while fitting on page 1.
    await page.evaluate(() => {
      for (let i = 1; i <= 24; i += 1) {
        const num = String(i).padStart(2, '0');
        window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents', `doc-${num}.txt`);
      }
    });
    await page.locator('main .list .row', { hasText: 'Documents' }).click();
    await page.locator('main .list .row .title', { hasText: 'doc-24.txt' }).waitFor({
      state: 'attached',
      timeout: 5000,
    });
    // Let initial post-navigation scroll restore settle before scrolling down.
    await page.waitForTimeout(80);
    await page.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 240;
    });
    await page.waitForTimeout(50);
    const scrollBeforeUpload = await page.evaluate(() => document.querySelector('main.main').scrollTop);
    assert.ok(scrollBeforeUpload >= 200, `expected scrollTop >= 200, got ${scrollBeforeUpload}`);

    // Open More sheet -> Add from this phone.
    await page.getByRole('button', { name: 'More', exact: true }).click();
    await page.locator('.sheet').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('.sheet .row', { hasText: 'Add from this' }).click();
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });

    // Upload completes asynchronously in mock pump; file appears in Documents automatically.
    await page.locator('.row .title', { hasText: /^Upload-\d+\.jpg$/ }).first().waitFor({
      state: 'attached',
      timeout: 10_000,
    });
    // No spinner replaced the list, and scroll position was preserved!
    const scrollAfterUpload = await page.evaluate(() => document.querySelector('main.main').scrollTop);
    assert.ok(
      Math.abs(scrollAfterUpload - scrollBeforeUpload) <= 4,
      `scrollTop jumped during upload live refresh: before=${scrollBeforeUpload}, after=${scrollAfterUpload}`,
    );

    // 2. New folder, rename, and delete update the open folder immediately without navigating away.
    // Scroll back to top of Documents so rows are in view.
    await page.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 0;
    });

    // New folder
    await page.getByRole('button', { name: 'More', exact: true }).click();
    await page.locator('.sheet .row', { hasText: 'New folder' }).click();
    await page.locator('.sheet input[type="text"]').fill('LiveCreated');
    await page.locator('.sheet button[type="submit"]').click();
    await page.locator('main .list .row .title', { hasText: 'LiveCreated' }).waitFor({
      state: 'visible',
      timeout: 5000,
    });

    // Rename a file from the viewer, then close viewer -> renamed file is already shown in the folder!
    const firstFileRow = page.locator('main .list .row', { hasText: 'long-log.txt' }).first();
    await firstFileRow.waitFor({ state: 'visible', timeout: 5000 });
    await firstFileRow.click();
    await page.locator('.viewer').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('.viewer').getByRole('button', { name: 'More' }).click();
    await page.locator('.sheet .row', { hasText: 'Rename' }).click();
    await page.locator('.sheet input[type="text"]').fill('long-log-renamed.txt');
    await page.locator('.sheet button[type="submit"]').click();
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden', timeout: 5000 });
    await page.locator('main .list .row .title', { hasText: 'long-log-renamed.txt' }).waitFor({
      state: 'visible',
      timeout: 5000,
    });
    assert.equal(await page.locator('main .list .row .title', { hasText: 'long-log.txt' }).count(), 0);

    // Delete via selection mode -> disappears immediately!
    await page.getByRole('button', { name: 'More', exact: true }).click();
    await page.locator('.sheet .row', { hasText: 'Select' }).click();
    await page.locator('main .list .row', { hasText: 'long-log-renamed.txt' }).click();
    await page.locator('.dock button', { hasText: 'Delete' }).click();
    await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
    await page.locator('main .list .row .title', { hasText: 'long-log-renamed.txt' }).waitFor({
      state: 'detached',
      timeout: 5000,
    });

    // 3. Background copy (Send to another machine) into an open destination folder finishing later,
    // even after the toast is dismissed!
    await page.locator('main .list .row', { hasText: 'README.md' }).first().click();
    await page.locator('.viewer').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('.viewer').getByRole('button', { name: 'Send to' }).click();
    await page.locator('.sheet').waitFor({ state: 'visible', timeout: 5000 });
    // Choose `pi` (reachable other machine) and confirm Copy here.
    await page.locator('.sheet .row', { hasText: 'pi' }).first().click();
    await page.locator('.sheet button.primary').click();
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });

    // Immediately navigate to the destination folder on `pi` (/home/me/Films, pi's pinned default)
    // before the background transfer finishes.
    await page.evaluate(() => {
      window.location.hash = '#/files/pi//home/me/Films';
    });
    await page.locator('.where').waitFor({ state: 'visible', timeout: 5000 });

    // Dismiss the toast right away to verify background transfer tracking does NOT depend on toast!
    const okBtn = page.locator('.toast-banner button[aria-label="Dismiss"]');
    if (await okBtn.isVisible().catch(() => false)) {
      await okBtn.click();
    }

    // Wait for the background transfer to finish and automatically appear in pi's Films folder.
    await page.locator('main .list .row .title', { hasText: 'README.md' }).waitFor({
      state: 'visible',
      timeout: 12_000,
    });

    // 4. Change to a cached off-screen folder appears automatically when navigating back to it.
    await page.evaluate(() => {
      window.location.hash = '#/files/tower//home/me/Documents';
    });
    await page.locator('main .list .row .title', { hasText: 'LiveCreated' }).waitFor({
      state: 'visible',
      timeout: 5000,
    });
    // Navigate into LiveCreated (now cached empty).
    await page.locator('main .list .row', { hasText: 'LiveCreated' }).click();
    await page.locator('text=This folder is empty.').waitFor({ state: 'visible', timeout: 5000 });
    // Go back up to Documents -> Send README.md into LiveCreated (same-machine copy) -> open LiveCreated again!
    await page.locator('.where a', { hasText: 'Documents' }).click();
    await page.locator('main .list .row', { hasText: 'README.md' }).waitFor({ state: 'visible', timeout: 5000 });

    await page.getByRole('button', { name: 'More', exact: true }).click();
    await page.locator('.sheet .row', { hasText: 'Select' }).click();
    await page.locator('main .list .row', { hasText: 'README.md' }).click();
    await page.locator('.dock button', { hasText: 'Send to' }).click();
    await page.locator('.sheet').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('.sheet button[role="radio"]', { hasText: 'tower' }).click();
    await page.locator('.sheet button', { hasText: 'Change' }).click();
    // Step up from tower's default pinned folder (/home/me/Videos/Holiday) to /home/me, then into Documents/LiveCreated.
    while ((await page.locator('.sheet .row', { hasText: 'Documents' }).count()) === 0) {
      await page.locator('.sheet .row', { hasText: 'Up a folder' }).click();
      await page.waitForTimeout(80);
    }
    await page.locator('.sheet .row', { hasText: 'Documents' }).first().click();
    await page.locator('.sheet .row', { hasText: 'LiveCreated' }).first().click();
    await page.locator('.sheet button.primary', { hasText: 'Use LiveCreated' }).click();
    await page.locator('.sheet button.primary', { hasText: /Copy/ }).click();
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });

    // Navigate back into cached off-screen LiveCreated -> must show README.md automatically!
    await page.locator('main .list .row', { hasText: 'LiveCreated' }).click();
    await page.locator('main .list .row .title', { hasText: 'README.md' }).waitFor({
      state: 'visible',
      timeout: 5000,
    });
    // Let step 4's coalesced refetch settle before testing step 5's external-change visibility rule.
    await page.waitForTimeout(200);

    // 5. Returning to foreground after >10s refetches once for external changes (and not before 10s).
    await page.evaluate(() => {
      window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents/LiveCreated', 'from-keyboard.txt');
      // First trigger focus/visibility within <10s: must NOT refetch yet.
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.waitForTimeout(250);
    assert.equal(
      await page.locator('main .list .row .title', { hasText: 'from-keyboard.txt' }).count(),
      0,
      'Must not refetch on visibilitychange when data is fresher than 10 seconds',
    );

    // Advance Date.now by 11 seconds and fire visibilitychange -> must refetch once and show the external file.
    await page.evaluate(() => {
      const realNow = Date.now.bind(Date);
      Date.now = () => realNow() + 11_000;
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.locator('main .list .row .title', { hasText: 'from-keyboard.txt' }).waitFor({
      state: 'visible',
      timeout: 5000,
    });

    assert.deepEqual(errors, [], `Unexpected browser errors:\n${errors.join('\n')}`);
    await context.close();
  } finally {
    await browser.close();
    await srv.stop();
  }
});
