// Headless Chromium tests for live folder refresh and shared touch pull-to-refresh.
// Uses real touch input via CDP Input.dispatchTouchEvent moving across multiple frames.

import assert from 'node:assert/strict';
import {
  browserTest,
  launchBrowser,
  startStaticUiServer,
  waitForLayoutSettled,
} from './browser-helpers.mjs';

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

browserTest(
  'touch pull-to-refresh works across phone screens and respects scroll, early release, horizontal swipe, and edge back',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const srv = await startStaticUiServer();
    const browser = await launchBrowser();

    try {
      const context = await browser.newContext({
        viewport: { width: 390, height: 844 },
        hasTouch: true,
        isMobile: true,
      });
      const page = await context.newPage();
      const errors = [];
      tracker.attach(page);
      page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
      page.on('console', (msg) => {
        if (msg.type() === 'error') errors.push(`console.error: ${msg.text()}`);
      });

      const cdp = await context.newCDPSession(page);

      // 1. Machines screen: pull down arms past 64px, enters refreshing for >=400ms, then settles.
      tracker.setStep('machines pull-to-refresh');
      await page.goto(`${srv.url}/?mock=1#/machines`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('.machine').first(), 'machines list visible', { state: 'visible' });

      // Observe every data-pull-state transition directly in the page so CPU stalls between
      // Playwright IPC hops cannot miss the 400ms refreshing window.
      await page.evaluate(() => {
        const main = document.querySelector('main.main');
        const transitions = [{ state: main?.getAttribute('data-pull-state') || '', at: performance.now() }];
        const obs = new MutationObserver(() => {
          const next = main?.getAttribute('data-pull-state') || '';
          if (transitions[transitions.length - 1]?.state !== next) {
            transitions.push({ state: next, at: performance.now() });
          }
        });
        if (main) obs.observe(main, { attributes: true, attributeFilter: ['data-pull-state'] });
        window.__pullStateAudit = { transitions, stop: () => obs.disconnect() };
      });

      await realTouchSwipe(page, cdp, {
        startX: 195,
        startY: 220,
        endX: 195,
        endY: 360,
        steps: 12,
        stepDelayMs: 18,
      });
      await tracker.waitForFn(
        page,
        () => {
          const t = window.__pullStateAudit?.transitions || [];
          const hasRefreshing = t.some((x) => x.state === 'refreshing');
          const cur = document.querySelector('main.main')?.getAttribute('data-pull-state');
          return hasRefreshing && cur === 'idle';
        },
        null,
        'machines pull-to-refresh to enter refreshing and return to idle',
      );
      const transitions = await page.evaluate(() => {
        window.__pullStateAudit?.stop();
        return window.__pullStateAudit?.transitions || [];
      });
      const armedIdx = transitions.findIndex((x) => x.state === 'armed');
      const refreshingIdx = transitions.findIndex((x, idx) => idx > armedIdx && x.state === 'refreshing');
      const idleAfterIdx = transitions.findIndex((x, idx) => idx > refreshingIdx && x.state === 'idle');
      assert.ok(armedIdx >= 0, `Machines pull should arm past 64px; transitions=${JSON.stringify(transitions)}`);
      assert.ok(
        refreshingIdx > armedIdx,
        `Machines pull should enter refreshing on release; transitions=${JSON.stringify(transitions)}`,
      );
      assert.ok(
        idleAfterIdx > refreshingIdx,
        `Machines pull should return to idle after refresh; transitions=${JSON.stringify(transitions)}`,
      );
      const dwellMs = transitions[idleAfterIdx].at - transitions[refreshingIdx].at;
      assert.ok(
        dwellMs >= 380,
        `Indicator must stay in refreshing for at least 400ms (observed ${Math.round(dwellMs)}ms; transitions=${JSON.stringify(transitions)})`,
      );

      // 2. Nested folder in Files tab: external file appears on pull-to-refresh.
      tracker.setStep('files nested folder early-release and pull-to-refresh');
      await page.goto(`${srv.url}/?mock=1#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('.where'), 'Documents breadcrumbs', { state: 'visible' });
      await tracker.waitForLocator(page.locator('main .list .row').first(), 'Documents first row', { state: 'visible' });

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
      await tracker.waitForFn(
        page,
        () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
        null,
        'early release under 64px to cancel back to idle',
      );
      await waitForLayoutSettled(page, 2);
      assert.equal(
        await page.locator('text=early-release-check.txt').count(),
        0,
        'Early release must not refresh the folder',
      );

      // Horizontal swipe across middle of list must not trigger pull-to-refresh.
      tracker.setStep('horizontal swipe must not trigger pull-to-refresh');
      await realTouchSwipe(page, cdp, {
        startX: 280,
        startY: 300,
        endX: 120,
        endY: 315,
        steps: 10,
        stepDelayMs: 16,
      });
      await waitForLayoutSettled(page, 2);
      assert.equal(
        await page.locator('text=early-release-check.txt').count(),
        0,
        'Horizontal swipe must not trigger pull-to-refresh',
      );

      // Full downward pull refreshes the nested folder and reveals the external file.
      tracker.setStep('full downward pull refreshes Documents');
      await realTouchSwipe(page, cdp, {
        startX: 195,
        startY: 240,
        endX: 195,
        endY: 370,
        steps: 12,
        stepDelayMs: 18,
      });
      await tracker.waitForLocator(page.locator('text=early-release-check.txt'), 'early-release-check.txt after pull', {
        state: 'visible',
      });
      await tracker.waitForFn(
        page,
        () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
        null,
        'Documents pull-to-refresh to settle to idle',
      );

      // 3. Left-edge back gesture inside nested folder still steps one folder up!
      tracker.setStep('left-edge back gesture inside nested folder');
      await realTouchSwipe(page, cdp, {
        startX: 10,
        startY: 420,
        endX: 170,
        endY: 424,
        steps: 10,
        stepDelayMs: 14,
      });
      await tracker.waitForFn(
        page,
        () => window.location.hash === '#/files/tower//home/me',
        null,
        'left-edge back swipe hash to step one folder up to /home/me',
      );

      // 4. Pull while scrolled down scrolls up instead of triggering pull-to-refresh.
      tracker.setStep('pull while scrolled down scrolls up without refreshing');
      await page.goto(`${srv.url}/?mock=1,large#/files/tower//home/me/Archive5000`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('main .list .row').first(), 'Archive5000 first row', { state: 'visible' });
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
      await tracker.waitForFn(
        page,
        (prev) => (document.querySelector('main.main')?.scrollTop ?? prev) < prev,
        beforeScroll,
        'pull while scrolled down to decrease scrollTop',
      );
      await waitForLayoutSettled(page, 2);
      assert.equal(
        await page.locator('text=000-scrolled-check.txt').count(),
        0,
        'Pull while scrolled down must not refresh',
      );

      // 5. Folder under a machine's Files tab.
      tracker.setStep('machine Files tab pull-to-refresh');
      await page.goto(`${srv.url}/?mock=1#/machines/tower/files`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('main .list .row').first(), 'machine Files first row', { state: 'visible' });
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
      await tracker.waitForLocator(page.locator('text=machine-tab-fresh.txt'), 'machine-tab-fresh.txt after pull', {
        state: 'visible',
      });
      await tracker.waitForFn(
        page,
        () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
        null,
        'machine Files pull-to-refresh to settle to idle',
      );

      // 6. Search results pull-to-refresh.
      tracker.setStep('search results pull-to-refresh');
      await page.goto(`${srv.url}/?mock=1#/files/@search?q=freshsearch`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('text=Nothing found'), 'empty search state', { state: 'visible' });
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
      await tracker.waitForLocator(page.locator('text=freshsearch-note.txt'), 'freshsearch-note.txt after pull', {
        state: 'visible',
      });
      await tracker.waitForFn(
        page,
        () => document.querySelector('main.main')?.getAttribute('data-pull-state') === 'idle',
        null,
        'search pull-to-refresh to settle to idle',
      );

      // 7. Activity screen pull-to-refresh.
      tracker.setStep('activity pull-to-refresh');
      await page.goto(`${srv.url}/?mock=1#/activity`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('main .list .row').first(), 'activity first row', { state: 'visible' });
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
      await tracker.waitForLocator(page.locator('text=Pull activity check'), 'Pull activity check job after pull', {
        state: 'visible',
      });

      assert.deepEqual(errors, [], `Unexpected browser errors:\n${errors.join('\n')}`);
      await context.close();
    } finally {
      await browser.close();
      await srv.stop();
    }
  },
);

browserTest(
  'live folder refresh updates open folders on upload, background copy, rename, delete, mkdir, off-screen cache, and >10s resume',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const srv = await startStaticUiServer();
    const browser = await launchBrowser();

    try {
      const context = await browser.newContext({
        viewport: { width: 390, height: 844 },
        hasTouch: true,
        isMobile: true,
      });
      const page = await context.newPage();
      const errors = [];
      tracker.attach(page);
      page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
      page.on('console', (msg) => {
        if (msg.type() === 'error') errors.push(`console.error: ${msg.text()}`);
      });

      // 1. Upload into an open scrollable folder: appears live without navigating and keeps scroll position.
      tracker.setStep('upload into open scrollable folder');
      await page.goto(`${srv.url}/?mock=1#/files/tower//home/me`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('main .list .row', { hasText: 'Documents' }), 'Documents row', {
        state: 'visible',
      });

      // Seed 24 files before opening Documents so the folder is taller than the viewport while fitting on page 1.
      await page.evaluate(() => {
        for (let i = 1; i <= 24; i += 1) {
          const num = String(i).padStart(2, '0');
          window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents', `doc-${num}.txt`);
        }
      });
      await page.locator('main .list .row', { hasText: 'Documents' }).click();
      await tracker.waitForLocator(page.locator('main .list .row .title', { hasText: 'doc-24.txt' }), 'doc-24.txt row', {
        state: 'attached',
      });
      // Wait for post-navigation layout/scroll restore to finish before scrolling down.
      await tracker.waitForFn(
        page,
        () => {
          const m = document.querySelector('main.main');
          return Boolean(m && m.scrollHeight > m.clientHeight + 240);
        },
        null,
        'Documents list to become scrollable',
      );
      await waitForLayoutSettled(page, 2);
      await page.evaluate(() => {
        const m = document.querySelector('main.main');
        if (m) m.scrollTop = 240;
      });
      await waitForLayoutSettled(page, 2);
      const scrollBeforeUpload = await page.evaluate(() => document.querySelector('main.main').scrollTop);
      assert.ok(scrollBeforeUpload >= 200, `expected scrollTop >= 200, got ${scrollBeforeUpload}`);

      // Open More sheet -> Add from this phone.
      await page.getByRole('button', { name: 'More', exact: true }).click();
      await tracker.waitForLocator(page.locator('.sheet'), 'More sheet visible', { state: 'visible' });
      await page.locator('.sheet .row', { hasText: 'Add from this' }).click();
      await tracker.waitForLocator(page.locator('.sheet'), 'More sheet hidden', { state: 'hidden' });

      // Upload completes asynchronously in mock pump; file appears in Documents automatically.
      await tracker.waitForLocator(
        page.locator('.row .title', { hasText: /^Upload-\d+\.jpg$/ }).first(),
        'uploaded file row in Documents',
        { state: 'attached' },
      );
      // No spinner replaced the list, and scroll position was preserved!
      const scrollAfterUpload = await page.evaluate(() => document.querySelector('main.main').scrollTop);
      assert.ok(
        Math.abs(scrollAfterUpload - scrollBeforeUpload) <= 4,
        `scrollTop jumped during upload live refresh: before=${scrollBeforeUpload}, after=${scrollAfterUpload}`,
      );

      // 2. New folder, rename, and delete update the open folder immediately without navigating away.
      tracker.setStep('mkdir, rename, and delete live updates');
      await page.evaluate(() => {
        const m = document.querySelector('main.main');
        if (m) m.scrollTop = 0;
      });

      // New folder
      await page.getByRole('button', { name: 'More', exact: true }).click();
      await page.locator('.sheet .row', { hasText: 'New folder' }).click();
      await page.locator('.sheet input[type="text"]').fill('LiveCreated');
      await page.locator('.sheet button[type="submit"]').click();
      await tracker.waitForLocator(page.locator('main .list .row .title', { hasText: 'LiveCreated' }), 'LiveCreated folder', {
        state: 'visible',
      });

      // Rename a file from the viewer, then close viewer -> renamed file is already shown in the folder!
      const firstFileRow = page.locator('main .list .row', { hasText: 'long-log.txt' }).first();
      await tracker.waitForLocator(firstFileRow, 'long-log.txt row', { state: 'visible' });
      await firstFileRow.click();
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer for long-log.txt', { state: 'visible' });
      await page.locator('.viewer').getByRole('button', { name: 'More options', exact: true }).click();
      await page.locator('.sheet .row', { hasText: 'Rename' }).click();
      await page.locator('.sheet input[type="text"]').fill('long-log-renamed.txt');
      await page.locator('.sheet button[type="submit"]').click();
      await tracker.waitForLocator(page.locator('.sheet'), 'Rename sheet hidden', { state: 'hidden' });
      await page.locator('.viewer-top button[aria-label="Close"]').click();
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer hidden after rename', { state: 'hidden' });
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'long-log-renamed.txt' }),
        'long-log-renamed.txt row',
        { state: 'visible' },
      );
      assert.equal(await page.locator('main .list .row .title', { hasText: 'long-log.txt' }).count(), 0);

      // Delete via selection mode -> disappears immediately!
      await page.getByRole('button', { name: 'More', exact: true }).click();
      await page.locator('.sheet .row', { hasText: 'Select' }).click();
      await page.locator('main .list .row', { hasText: 'long-log-renamed.txt' }).click();
      await page.locator('.dock button', { hasText: 'Delete' }).click();
      await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'long-log-renamed.txt' }),
        'long-log-renamed.txt detached after delete',
        { state: 'detached' },
      );

      // 3. Background copy (Send to another machine) into an open destination folder finishing later,
      // even after the toast is dismissed!
      tracker.setStep('background copy to pi appears live');
      await page.locator('main .list .row', { hasText: 'README.md' }).first().click();
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer for README.md', { state: 'visible' });
      await page.locator('.viewer').getByRole('button', { name: 'Send to' }).click();
      await tracker.waitForLocator(page.locator('.sheet'), 'Send to sheet', { state: 'visible' });
      await page.locator('.sheet .row', { hasText: 'pi' }).first().click();
      await page.locator('.sheet button.primary').click();
      await tracker.waitForLocator(page.locator('.sheet'), 'Send to sheet hidden', { state: 'hidden' });

      // Immediately navigate to the destination folder on `pi` (/home/me/Films, pi's pinned default)
      // before the background transfer finishes.
      await page.evaluate(() => {
        window.location.hash = '#/files/pi//home/me/Films';
      });
      await tracker.waitForLocator(page.locator('.where'), 'pi Films breadcrumbs', { state: 'visible' });

      // Dismiss the toast right away to verify background transfer tracking does NOT depend on toast!
      const okBtn = page.locator('.toast-banner button[aria-label="Dismiss"]');
      if (await okBtn.isVisible().catch(() => false)) {
        await okBtn.click();
      }

      // Wait for the background transfer to finish and automatically appear in pi's Films folder.
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'README.md' }),
        'README.md in pi Films after background copy',
        { state: 'visible' },
      );

      // 4. Change to a cached off-screen folder appears automatically when navigating back to it.
      tracker.setStep('off-screen cached folder updates on same-machine copy');
      await page.evaluate(() => {
        window.location.hash = '#/files/tower//home/me/Documents';
      });
      await tracker.waitForLocator(page.locator('main .list .row .title', { hasText: 'LiveCreated' }), 'LiveCreated in Documents', {
        state: 'visible',
      });
      // Navigate into LiveCreated (now cached empty).
      await page.locator('main .list .row', { hasText: 'LiveCreated' }).click();
      await tracker.waitForLocator(page.locator('text=This folder is empty.'), 'empty LiveCreated state', { state: 'visible' });
      // Go back up to Documents -> Send README.md into LiveCreated (same-machine copy) -> open LiveCreated again!
      await page.locator('.where a', { hasText: 'Documents' }).click();
      await tracker.waitForLocator(page.locator('main .list .row', { hasText: 'README.md' }), 'README.md back in Documents', {
        state: 'visible',
      });

      await page.getByRole('button', { name: 'More', exact: true }).click();
      await page.locator('.sheet .row', { hasText: 'Select' }).click();
      await page.locator('main .list .row', { hasText: 'README.md' }).click();
      await page.locator('.dock button', { hasText: 'Send to' }).click();
      await tracker.waitForLocator(page.locator('.sheet'), 'Send to sheet for LiveCreated', { state: 'visible' });
      await page.locator('.sheet button[role="radio"]', { hasText: 'tower' }).click();
      await page.locator('.sheet button', { hasText: 'Change' }).click();
      // Step up from tower's default pinned folder to Home (/home/me), stopping as soon as the
      // Browse header reaches Home so we never overshoot while fs.list is in flight.
      const browseLabel = page.locator('.sheet p.label', { hasText: /^Browse / });
      await tracker.waitForLocator(browseLabel, 'destination picker browse label', { state: 'visible' });
      for (let step = 0; step < 8; step += 1) {
        const prevLabel = ((await browseLabel.textContent()) || '').trim();
        if (prevLabel === 'Browse Home') break;
        await page.locator('.sheet .row', { hasText: 'Up a folder' }).click();
        await tracker.waitForFn(
          page,
          (prev) => {
            const lbl = [...document.querySelectorAll('.sheet p.label')].find((el) =>
              el.textContent?.startsWith('Browse '),
            );
            return Boolean(lbl && lbl.textContent?.trim() !== prev);
          },
          prevLabel,
          `folder picker to step up from "${prevLabel}"`,
        );
      }
      const browseList = page.locator('.sheet p.label:has-text("Browse ") + .list');
      await tracker.waitForLocator(browseList.locator('.row', { hasText: 'Documents' }).first(), 'Documents row in picker', {
        state: 'visible',
      });
      await browseList.locator('.row', { hasText: 'Documents' }).first().click();
      await tracker.waitForLocator(browseList.locator('.row', { hasText: 'LiveCreated' }).first(), 'LiveCreated row in picker', {
        state: 'visible',
      });
      await browseList.locator('.row', { hasText: 'LiveCreated' }).first().click();
      await tracker.waitForLocator(page.locator('.sheet button.primary', { hasText: 'Use LiveCreated' }), 'Use LiveCreated button', {
        state: 'visible',
      });
      await page.locator('.sheet button.primary', { hasText: 'Use LiveCreated' }).click();
      await page.locator('.sheet button.primary', { hasText: /Copy/ }).click();
      await tracker.waitForLocator(page.locator('.sheet'), 'Copy sheet hidden', { state: 'hidden' });

      // Navigate back into cached off-screen LiveCreated -> must show README.md automatically!
      await page.locator('main .list .row', { hasText: 'LiveCreated' }).click();
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'README.md' }),
        'README.md in LiveCreated after off-screen copy',
        { state: 'visible' },
      );
      await waitForLayoutSettled(page, 2);

      // 5. Returning to foreground after >10s refetches once for external changes (and not before 10s).
      tracker.setStep('foreground resume <10s skip and >10s refetch');
      await page.evaluate(() => {
        window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents/LiveCreated', 'from-keyboard.txt');
        // First trigger focus/visibility within <10s: must NOT refetch yet.
        document.dispatchEvent(new Event('visibilitychange'));
      });
      await waitForLayoutSettled(page, 3);
      assert.equal(
        await page.locator('main .list .row .title', { hasText: 'from-keyboard.txt' }).count(),
        0,
        'Must not refetch on visibilitychange when data is fresher than 10 seconds',
      );

      // 5b. An upload transitioning running -> cancelling -> cancelled must announce nothing
      // (so the unannounced external file stays absent), whereas a finished upload DOES announce.
      await page.evaluate(() => {
        window.__LEGION_MOCK__.simulateUploadTransition(
          'tower',
          '/home/me/Documents/LiveCreated',
          'aborted-upload.txt',
          ['running', 'cancelling', 'cancelled'],
        );
      });
      await waitForLayoutSettled(page, 3);
      assert.equal(
        await page.locator('main .list .row .title', { hasText: 'from-keyboard.txt' }).count(),
        0,
        'Cancelling/cancelled upload must not trigger a folder refetch',
      );
      assert.equal(
        await page.locator('main .list .row .title', { hasText: 'aborted-upload.txt' }).count(),
        0,
        'Cancelled upload must not appear in the folder',
      );

      // Now simulate a finished upload into the same open folder -> announces and refreshes immediately.
      await page.evaluate(() => {
        window.__LEGION_MOCK__.simulateUploadTransition(
          'tower',
          '/home/me/Documents/LiveCreated',
          'completed-upload.txt',
          ['running', 'done'],
        );
      });
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'completed-upload.txt' }),
        'completed-upload.txt after finished upload',
        { state: 'visible' },
      );
      await waitForLayoutSettled(page, 2);

      // Advance Date.now by 11 seconds with a second external file and fire visibilitychange -> must refetch once.
      await page.evaluate(() => {
        window.__LEGION_MOCK__.addExternalFile('tower', '/home/me/Documents/LiveCreated', 'from-keyboard-later.txt');
        const realNow = Date.now.bind(Date);
        Date.now = () => realNow() + 11_000;
        document.dispatchEvent(new Event('visibilitychange'));
      });
      await tracker.waitForLocator(
        page.locator('main .list .row .title', { hasText: 'from-keyboard-later.txt' }),
        'from-keyboard-later.txt after >10s visibilitychange',
        { state: 'visible' },
      );

      assert.deepEqual(errors, [], `Unexpected browser errors:\n${errors.join('\n')}`);
      await context.close();
    } finally {
      await browser.close();
      await srv.stop();
    }
  },
);
