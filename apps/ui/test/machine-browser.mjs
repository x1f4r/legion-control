// Headless Chromium browser tests for Machine Overview, amber warnings, Stop/Force stop,
// minimal machines with no optional meters, and the "Wake it and continue" flow.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {
  browserTest,
  launchBrowser,
  setTheme,
  startStaticUiServer,
} from './browser-helpers.mjs';

const SHOTS_DIR = process.env.LEGION_MACHINE_SHOTS_DIR || '';

async function captureShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await setTheme(page, 'dark');
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}-dark.png`) });
  await setTheme(page, 'light');
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}-light.png`) });
  await setTheme(page, 'dark');
}

const PLAIN_JARGON_RE = /\b(?:PID|PIDs|SIGTERM|SIGKILL|process|processes)\b/i;

browserTest(
  'Machine Overview: no empty meters on minimal machine, Stop then Force stop, amber warnings, and Wake it and continue',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser();
    const consoleErrors = [];
    const pageErrors = [];

    try {
      // ── 1. Minimal machine reports no optional values -> no empty meters ──────────────────────
      {
        tracker.setStep('minimal machine overview');
        const ctx = await browser.newContext({
          viewport: { width: 412, height: 915 },
          colorScheme: 'dark',
        });
        const page = tracker.attach(await ctx.newPage(), {
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

      await page.goto(`${server.url}/?mock=minimal#/machines/pi`, { waitUntil: 'domcontentloaded' });
      await page.getByText('Nothing much is running.').waitFor({ timeout: 10_000 });
      assert.equal(await page.locator('.gauge').count(), 0, 'Minimal machine must render zero empty meters');
      assert.equal(await page.locator('p.label', { hasText: 'Health' }).count(), 0, 'Health heading omitted when no meters exist');
      assert.equal(await page.locator('.machine-state').count(), 0, 'Phone already shows state in header subtitle');
      assert.equal(await page.locator('dt', { hasText: 'On for' }).count(), 0, 'About must not repeat On for');
      assert.equal(await page.locator('dt', { hasText: 'Status' }).count(), 0, 'About must not repeat Status');
      await captureShot(page, 'phone-minimal-overview');
      await ctx.close();
    }

    // ── 2. Overview busy, Stop -> still_running -> Force stop, plain language, and idle laptop ─
    {
      tracker.setStep('busy overview, stop and force stop');
      const ctx = await browser.newContext({
        viewport: { width: 412, height: 915 },
        colorScheme: 'dark',
      });
      const page = tracker.attach(await ctx.newPage(), {
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      await page.goto(`${server.url}/?mock#/machines/tower`, { waitUntil: 'domcontentloaded' });
      await page.getByText('Using it now').waitFor({ timeout: 10_000 });
      await page.getByText('62% of the processor').waitFor();
      await page.getByText('9.1 GB of graphics memory').waitFor();
      await page.getByText('Temperature').waitFor();
      await page.getByText('Storage').waitFor();
      assert.equal(await page.locator('.machine-state').count(), 0, 'Phone must not duplicate state under header');

      const overviewText = await page.locator('main.main').innerText();
      assert.equal(PLAIN_JARGON_RE.test(overviewText), false, `Plain mode leaked jargon: ${overviewText}`);
      await captureShot(page, 'phone-overview-busy');

      // Tap rustc -> open program sheet -> verify plain details.
      await page.locator('button.row', { hasText: 'rustc' }).click();
      const sheet = page.locator('.sheet');
      await sheet.waitFor();
      await sheet.getByText('rustc × 12').waitFor();
      await sheet.getByText('Running as').waitFor();
      const sheetText = await sheet.innerText();
      assert.equal(PLAIN_JARGON_RE.test(sheetText), false, `Program sheet leaked jargon: ${sheetText}`);
      await captureShot(page, 'phone-program-sheet');
      await sheet.getByRole('button', { name: 'Close' }).click();
      await sheet.waitFor({ state: 'hidden' });

      // Tap stubborn python3 -> Stop -> confirm -> still_running -> Force stop -> removed!
      await page.locator('button.row', { hasText: 'python3' }).click();
      await sheet.waitFor();
      await sheet.getByRole('button', { name: 'Stop', exact: true }).click();
      await sheet.getByText('Stop python3 on tower? Anything unsaved in it is lost.').waitFor();
      await sheet.getByRole('button', { name: 'Stop python3' }).click();

      await sheet.getByText('python3 is still running.').waitFor({ timeout: 5_000 });
      const stillText = await sheet.innerText();
      assert.equal(PLAIN_JARGON_RE.test(stillText), false, `Still-running sheet leaked jargon: ${stillText}`);
      await captureShot(page, 'phone-program-still-running');

      await sheet.getByRole('button', { name: 'Force stop' }).click();
      await sheet.waitFor({ state: 'hidden', timeout: 5_000 });
      await page.waitForFunction(
        () => !document.body.innerText.includes('9.1 GB of graphics memory'),
        null,
        { timeout: 5_000 },
      );

      // Idle laptop with battery (studio).
      await page.goto(`${server.url}/?mock#/machines/studio`, { waitUntil: 'domcontentloaded' });
      await page.getByText('Battery').waitFor({ timeout: 10_000 });
      await page.getByText('Nothing much is running.').waitFor();
      await captureShot(page, 'phone-overview-idle-laptop');

      await ctx.close();
    }

    // ── 3. Amber words on Machines list and amber meters (phone & desktop) ───────────────────
    {
      tracker.setStep('amber warnings phone and desktop');
      const ctx = await browser.newContext({
        viewport: { width: 412, height: 915 },
        colorScheme: 'dark',
      });
      const page = tracker.attach(await ctx.newPage(), {
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      await page.goto(`${server.url}/?mock=amber#/machines`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.getByText('Running hot'), 'Running hot warning');
      await page.getByText('Battery low').waitFor();
      await page.getByText('Storage almost full').waitFor();
      await page.locator('a.machine', { hasText: 'tower' }).getByText('Storage').waitFor();
      await captureShot(page, 'phone-machines-amber');
      await ctx.close();

      // Desktop 1280x800 split view: busy overview (amber), program sheet, still running, idle laptop, minimal, and wake sheet.
      const deskCtx = await browser.newContext({
        viewport: { width: 1280, height: 800 },
        colorScheme: 'dark',
      });
      const deskPage = tracker.attach(await deskCtx.newPage(), {
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      await deskPage.goto(`${server.url}/?mock=amber#/machines/tower`, { waitUntil: 'domcontentloaded' });
      await deskPage.getByText('Running hot').waitFor({ timeout: 10_000 });
      await deskPage.locator('.machine-state', { hasText: 'On for 11 hours · Busy' }).waitFor();
      await deskPage.getByText('62% of the processor').waitFor();
      await captureShot(deskPage, 'desktop-overview-busy');

      await deskPage.locator('button.row', { hasText: 'rustc' }).click();
      const deskSheet = deskPage.locator('.sheet');
      await deskSheet.waitFor();
      await captureShot(deskPage, 'desktop-program-sheet');
      await deskSheet.getByRole('button', { name: 'Close' }).click();
      await deskSheet.waitFor({ state: 'hidden' });

      await deskPage.locator('button.row', { hasText: 'python3' }).click();
      await deskSheet.waitFor();
      await deskSheet.getByRole('button', { name: 'Stop', exact: true }).click();
      await deskSheet.getByRole('button', { name: 'Stop python3' }).click();
      await deskSheet.getByText('python3 is still running.').waitFor({ timeout: 5_000 });
      await captureShot(deskPage, 'desktop-program-still-running');
      await deskSheet.getByRole('button', { name: 'Close' }).click();
      await deskSheet.waitFor({ state: 'hidden' });

      await deskPage.goto(`${server.url}/?mock#/machines/studio`, { waitUntil: 'domcontentloaded' });
      await deskPage.getByText('Battery').waitFor({ timeout: 10_000 });
      await deskPage.getByText('Nothing much is running.').waitFor();
      await captureShot(deskPage, 'desktop-overview-idle-laptop');

      await deskPage.goto(`${server.url}/?mock=minimal#/machines/pi`, { waitUntil: 'domcontentloaded' });
      await deskPage.getByText('Nothing much is running.').waitFor({ timeout: 10_000 });
      await captureShot(deskPage, 'desktop-minimal-overview');

      await deskPage.goto(`${server.url}/?mock=wake-ticks=2#/machines/tower`, { waitUntil: 'domcontentloaded' });
      await deskPage.locator('.machine.off a.machine-link', { hasText: 'laptop' }).waitFor({ timeout: 10_000 });
      await deskPage.locator('.machine.off a.machine-link', { hasText: 'laptop' }).click();
      await deskSheet.waitFor();
      await captureShot(deskPage, 'desktop-wake-prompt');
      await deskSheet.getByRole('button', { name: 'Not now' }).click();
      await deskSheet.waitFor({ state: 'hidden' });

      await deskCtx.close();
    }

    // ── 4. Wake it and continue: success, timeout (fake clock), and leaving the screen ───────
    {
      tracker.setStep('wake it and continue flows');
      const ctx = await browser.newContext({
        viewport: { width: 412, height: 915 },
        colorScheme: 'dark',
      });
      const page = tracker.attach(await ctx.newPage(), {
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      // Inject a deterministic controllable clock for the wake controller before the page loads.
      await page.addInitScript(() => {
        let nowMs = 1_000_000;
        let nextId = 1;
        const intervals = new Map();
        window.__LEGION_TEST_WAKE_CLOCK__ = {
          now: () => nowMs,
          setInterval(fn, ms) {
            const id = nextId++;
            const stepMs = Math.max(1, Number(ms) || 1);
            intervals.set(id, { fn, ms: stepMs, nextAt: nowMs + stepMs });
            return id;
          },
          clearInterval(id) {
            intervals.delete(id);
          },
          async advance(ms) {
            const target = nowMs + ms;
            let iterations = 0;
            while (true) {
              if (++iterations > 1000) {
                throw new Error(`Fake wake clock exceeded 1000 ticks advancing ${ms}ms`);
              }
              let soonest = null;
              for (const item of intervals.values()) {
                if (item.nextAt <= target && (!soonest || item.nextAt < soonest.nextAt)) {
                  soonest = item;
                }
              }
              if (!soonest) break;
              nowMs = soonest.nextAt;
              soonest.nextAt += soonest.ms;
              soonest.fn();
              await Promise.resolve();
              await Promise.resolve();
              await new Promise((r) => setTimeout(r, 5));
            }
            nowMs = target;
          },
        };
      });

      // 4a. Wake succeeds after 2 pings and opens laptop's screen.
      await page.goto(`${server.url}/?mock=wake-ticks=2#/machines`, { waitUntil: 'domcontentloaded' });
      await page.locator('.machine.off', { hasText: 'laptop' }).waitFor({ timeout: 10_000 });
      await page.locator('.machine.off a.machine-link', { hasText: 'laptop' }).click();

      const sheet = page.locator('.sheet');
      await sheet.waitFor();
      await sheet.getByRole('heading', { name: 'laptop is off.' }).waitFor();
      await sheet.getByText('Then: open laptop').waitFor();
      await captureShot(page, 'phone-wake-prompt');

      await sheet.getByRole('button', { name: 'Wake it and continue' }).click();
      await sheet.getByRole('heading', { name: 'Waking laptop…' }).waitFor();
      await sheet.locator('.wake-elapsed', { hasText: '0:00' }).waitFor();
      await sheet.getByRole('button', { name: 'Stop waiting' }).waitFor();
      await sheet.getByText('The wake signal has already been sent.').waitFor();
      await page.evaluate(() => window.__LEGION_TEST_WAKE_CLOCK__.advance(3000));
      await sheet.locator('.wake-elapsed', { hasText: '0:03' }).waitFor();
      await captureShot(page, 'phone-wake-waking');

      // Second 3s tick -> laptop answers -> sheet closes and navigates to laptop Overview!
      await page.evaluate(() => window.__LEGION_TEST_WAKE_CLOCK__.advance(3000));
      await sheet.waitFor({ state: 'hidden', timeout: 5_000 });
      await page.locator('header.top h1', { hasText: 'laptop' }).waitFor({ timeout: 5_000 });

      // 4b. Wake never answers -> 3-minute timeout shows non-redundant body and Try again.
      await page.goto(`${server.url}/?mock=wake-never#/machines`, { waitUntil: 'domcontentloaded' });
      await page.locator('.machine.off a.machine-link', { hasText: 'laptop' }).click();
      await sheet.waitFor();
      await sheet.getByRole('button', { name: 'Wake it and continue' }).click();
      await sheet.getByRole('heading', { name: 'Waking laptop…' }).waitFor();

      await page.evaluate(() => window.__LEGION_TEST_WAKE_CLOCK__.advance(180_000));
      await sheet.getByRole('heading', { name: 'laptop did not answer' }).waitFor({ timeout: 5_000 });
      await sheet
        .getByText('It has been 3 minutes. It may need longer to start, or waking it may not be working.')
        .waitFor({ timeout: 5_000 });
      await sheet.getByRole('button', { name: 'Try again' }).waitFor();
      await captureShot(page, 'phone-wake-timeout');
      await sheet.getByRole('button', { name: 'Not now' }).click();
      await sheet.waitFor({ state: 'hidden' });

      // 4c. Leaving the screen while waiting forgets opening the screen.
      await page.goto(`${server.url}/?mock=wake-ticks=2#/machines`, { waitUntil: 'domcontentloaded' });
      await page.locator('.machine.off a.machine-link', { hasText: 'laptop' }).click();
      await sheet.waitFor();
      await sheet.getByRole('button', { name: 'Wake it and continue' }).click();
      await sheet.getByRole('heading', { name: 'Waking laptop…' }).waitFor();

      // Navigate away to Activity while waiting -> screen navigation is forgotten.
      await page.evaluate(() => { window.location.hash = '#/activity'; });
      await sheet.waitFor({ state: 'hidden', timeout: 5_000 });
      await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
      await page.evaluate(() => window.__LEGION_TEST_WAKE_CLOCK__.advance(9_000));
      // Still on Activity (did NOT jump to laptop)!
      await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();

      // 4d. Explicit Wake button on Machines list enters waking state directly.
      await page.goto(`${server.url}/?mock=wake-ticks=3#/machines`, { waitUntil: 'domcontentloaded' });
      await page.locator('.machine.off', { hasText: 'laptop' }).getByRole('button', { name: 'Wake' }).click();
      await sheet.waitFor();
      await sheet.getByRole('heading', { name: 'Waking laptop…' }).waitFor();
      await sheet.getByRole('button', { name: 'Stop waiting' }).click();
      await sheet.waitFor({ state: 'hidden' });
      await page.locator('.machine.off', { hasText: 'laptop' }).getByText('Wake sent · waiting for it to answer').waitFor();

      await ctx.close();
    }

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
  }
},
);
