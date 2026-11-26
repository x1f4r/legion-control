// Browser tests for Picture-in-Picture video playback and hidden-page idle/resume behaviour
// against a real `legion serve` node and its `state/audit.jsonl`.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import {
  browserTest,
  launchBrowser,
  LEGION_BIN,
  startLegionServer,
  startStaticUiServer,
  waitForLayoutSettled,
  waitForMediaReady,
} from './browser-helpers.mjs';

const PIP_SHOTS_DIR = process.env.LEGION_PIP_SHOTS_DIR || '';

function makeWorkRoot() {
  const base =
    process.env.LEGION_TEST_CACHE ||
    (process.env.RUNNER_TEMP
      ? path.join(process.env.RUNNER_TEMP, 'legion-ui-test')
      : path.join(os.homedir(), '.cache', 'legion-ui-test'));
  fs.mkdirSync(base, { recursive: true });
  return fs.mkdtempSync(path.join(base, 'pip-run-'));
}

function runLegion(args, legionHome, userHome) {
  const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  return execFileSync(LEGION_BIN, args, { env, encoding: 'utf8' });
}

function readAuditEntries(auditPath) {
  if (!fs.existsSync(auditPath)) return [];
  const raw = fs.readFileSync(auditPath, 'utf8').trim();
  if (!raw) return [];
  return raw
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return { raw: line };
      }
    });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function setupTwoNodeAuditFleet(root) {
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const piHome = path.join(root, 'pi-legion');
  const piUser = path.join(root, 'pi-user');
  const docsDir = path.join(towerUser, 'docs');

  for (const dir of [
    path.join(towerHome, 'config'),
    path.join(towerHome, 'state'),
    docsDir,
    path.join(piHome, 'config'),
    path.join(piHome, 'state'),
    path.join(piUser, 'Downloads'),
    path.join(piUser, 'inbox'),
  ]) {
    fs.mkdirSync(dir, { recursive: true });
  }

  // 6 MiB payload transferred over a throttled stdio pipe so the transfer stays visibly in-flight
  // for ~2.5 s, long enough to hide the page mid-transfer and watch progress finish while hidden.
  const payload = Buffer.alloc(6 * 1024 * 1024, 0x41);
  fs.writeFileSync(path.join(docsDir, 'transfer-sample.bin'), payload);

  const policy = [
    'default = "owner"',
    '',
    '[principals.ui]',
    'role = "operator"',
    '',
    '[roles.operator]',
    'allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy", "effect:power"]',
    'deny = ["effect:admin"]',
    '',
  ].join('\n');

  fs.writeFileSync(
    path.join(towerHome, 'config', 'node.toml'),
    'name = "tower"\nsystem = "linux"\nbusy = "none"\n',
  );
  fs.writeFileSync(path.join(towerHome, 'config', 'policy.toml'), policy);

  fs.writeFileSync(
    path.join(piHome, 'config', 'node.toml'),
    'name = "pi"\nsystem = "linux"\nbusy = "none"\n',
  );
  fs.writeFileSync(path.join(piHome, 'config', 'policy.toml'), policy);

  // Throttled stdio bridge for pi: supports both `rpc` (full speed) and `stream` (throttled ~20 ms
  // per chunk) so fs.transfer stays visibly in-flight while the browser hides the tab mid-transfer.
  const throttledHelperPath = path.join(root, 'throttled-pi-helper.mjs');
  fs.writeFileSync(
    throttledHelperPath,
    [
      "import { spawn } from 'node:child_process';",
      "import { Transform } from 'node:stream';",
      "const [, , bin, legionHome, userHome, subcommand = 'rpc'] = process.argv;",
      'const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };',
      'delete env.XDG_CONFIG_HOME;',
      'delete env.XDG_STATE_HOME;',
      "if (subcommand !== 'stream') {",
      "  const child = spawn(bin, [subcommand], { env, stdio: 'inherit' });",
      "  child.on('exit', (code, signal) => { if (signal) process.kill(process.pid, signal); process.exit(code ?? 1); });",
      '} else {',
      "  const child = spawn(bin, ['stream'], { env, stdio: ['pipe', 'pipe', 'inherit'] });",
      '  const throttle = new Transform({',
      '    transform(chunk, _enc, cb) {',
      '      if (chunk.length >= 16384) {',
      '        setTimeout(() => cb(null, chunk), 25);',
      '      } else {',
      '        cb(null, chunk);',
      '      }',
      '    },',
      '  });',
      '  process.stdin.pipe(throttle).pipe(child.stdin);',
      '  child.stdout.pipe(process.stdout);',
      "  child.on('exit', (code, signal) => { if (signal) process.kill(process.pid, signal); process.exit(code ?? 1); });",
      '}',
      '',
    ].join('\n'),
  );

  const helperCmd = [process.execPath, throttledHelperPath, LEGION_BIN, piHome, piUser, 'rpc'];
  fs.writeFileSync(
    path.join(towerHome, 'config', 'fleet.toml'),
    `[machines.tower]\nssh = "tower.example"\n\n[machines.pi]\ncommand = ${JSON.stringify(helperCmd)}\n`,
  );

  const tokenOut = JSON.parse(
    runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser),
  );
  assert.ok(tokenOut.token && tokenOut.token.startsWith('lgn_'));

  runLegion(
    ['fav.add', 'machine=pi', `path=${path.join(piUser, 'inbox')}`, 'kind=dir', 'pinned=true', '--json'],
    towerHome,
    towerUser,
  );

  return {
    towerHome,
    towerUser,
    piHome,
    piUser,
    auditPath: path.join(towerHome, 'state', 'audit.jsonl'),
    token: tokenOut.token,
  };
}

browserTest(
  'Picture in picture: top-bar button gating, enter/leave preserves position and play state, close exits PiP, music omits button',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      // 1. Unsupported browser (document.pictureInPictureEnabled === false): PiP button is ABSENT.
      {
        tracker.setStep('unsupported browser omits PiP button');
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
        await ctx.addInitScript(() => {
          Object.defineProperty(document, 'pictureInPictureEnabled', {
            configurable: true,
            get: () => false,
          });
        });
        const page = tracker.attach(await ctx.newPage(), {
          label: 'pip-unsupported',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock=browser-fs#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await tracker.waitForLocator(page.locator('.player-video-wrap video[src]'), 'video loaded (unsupported PiP)');
        await waitForMediaReady(page.locator('.player-video-wrap video[src]'), 'video metadata (unsupported PiP)', {
          tracker,
          minReadyState: 1,
        });

        assert.equal(
          await page.locator('.viewer-top.player-top .player-pip-btn').count(),
          0,
          'PiP button must be absent (never disabled) when document.pictureInPictureEnabled is false',
        );
        await page.locator('.viewer-top.player-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'video viewer closed', { state: 'detached' });

        await ctx.close();
      }

      // 2. Supported browser: PiP button sits right next to More, enters/leaves preserving position
      //    and play state, updates MediaSession, closes PiP on viewer close, and is absent on Music.
      {
        tracker.setStep('supported browser PiP lifecycle');
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
        await ctx.addInitScript(() => {
          let pipEl = null;
          window.__pipAudit = { requestCalls: 0, exitCalls: 0 };
          Object.defineProperty(document, 'pictureInPictureEnabled', {
            configurable: true,
            get: () => true,
          });
          Object.defineProperty(document, 'pictureInPictureElement', {
            configurable: true,
            get: () => pipEl,
          });
          HTMLVideoElement.prototype.requestPictureInPicture = function requestPictureInPicture() {
            window.__pipAudit.requestCalls += 1;
            pipEl = this;
            const win = { width: 480, height: 270 };
            this.dispatchEvent(new Event('enterpictureinpicture'));
            return Promise.resolve(win);
          };
          document.exitPictureInPicture = function exitPictureInPicture() {
            window.__pipAudit.exitCalls += 1;
            const prev = pipEl;
            pipEl = null;
            if (prev) {
              prev.dispatchEvent(new Event('leavepictureinpicture'));
            }
            return Promise.resolve();
          };
        });
        const page = tracker.attach(await ctx.newPage(), {
          label: 'pip-supported',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock=browser-fs#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await tracker.waitForLocator(page.locator('.player-video-wrap video[src]'), 'video loaded (supported PiP)');
        await waitForMediaReady(page.locator('.player-video-wrap video[src]'), 'video metadata (supported PiP)', {
          tracker,
          minReadyState: 1,
        });

        const pipBtn = page.locator('.viewer-top.player-top .player-pip-btn');
        await tracker.waitForLocator(pipBtn, 'video top-bar PiP button');
        await waitForLayoutSettled(page, 2);

        // Verify PiP button sits directly next to More with zero overlap and never disabled.
        const topBarCheck = await page.evaluate(() => {
          const top = document.querySelector('.viewer-top.player-top');
          const pip = top?.querySelector('.player-pip-btn');
          const more = top?.querySelector('button[aria-label="More options"]');
          if (!pip || !more) return null;
          const pipRect = pip.getBoundingClientRect();
          const moreRect = more.getBoundingClientRect();
          return {
            disabled: pip.disabled,
            ariaLabel: pip.getAttribute('aria-label'),
            ariaPressed: pip.getAttribute('aria-pressed'),
            directlyBeforeMore: pip.nextElementSibling === more,
            pipRight: pipRect.right,
            moreLeft: moreRect.left,
            pipWidth: pipRect.width,
            pipHeight: pipRect.height,
          };
        });
        assert.ok(topBarCheck, 'expected both PiP and More buttons in video top bar');
        assert.equal(topBarCheck.disabled, false);
        assert.equal(topBarCheck.ariaLabel, 'Picture in picture');
        assert.equal(topBarCheck.ariaPressed, 'false');
        assert.equal(topBarCheck.directlyBeforeMore, true, 'PiP button must sit directly next to More');
        assert.ok(
          topBarCheck.pipRight <= topBarCheck.moreLeft + 1,
          `PiP button overlapped More: ${topBarCheck.pipRight} > ${topBarCheck.moreLeft}`,
        );

        if (PIP_SHOTS_DIR) {
          fs.mkdirSync(PIP_SHOTS_DIR, { recursive: true });
          await page.screenshot({ path: path.join(PIP_SHOTS_DIR, 'browser-video-pip-button.png') });
        }

        // Start playback and seek to a known position before entering PiP.
        await page.evaluate(async () => {
          const v = document.querySelector('.player-video-wrap video');
          v.muted = true;
          v.currentTime = Math.min(0.6, Math.max(0.25, (v.duration || 1) * 0.35));
          await v.play();
        });
        await tracker.waitForFn(
          page,
          () => {
            const v = document.querySelector('.player-video-wrap video');
            return Boolean(v && !v.paused && v.currentTime >= 0.25);
          },
          null,
          'video playing at seeked position before PiP',
        );
        const beforePip = await page.evaluate(() => {
          const v = document.querySelector('.player-video-wrap video');
          return {
            time: v.currentTime,
            paused: v.paused,
            msState: navigator.mediaSession?.playbackState || null,
            msTitle: navigator.mediaSession?.metadata?.title || null,
          };
        });
        assert.equal(beforePip.paused, false);
        assert.equal(beforePip.msTitle, 'Arrival.mp4');

        // Enter PiP via the top-bar button -> preserves position and play state.
        await pipBtn.click();
        await tracker.waitForLocator(page.locator('.viewer.pip-active .player-video-wrap.pip-active'), 'entered web PiP');
        const duringPip = await page.evaluate(() => {
          const v = document.querySelector('.player-video-wrap video');
          return {
            inPip: document.pictureInPictureElement === v,
            hasWrapClass: document.querySelector('.player-video-wrap')?.classList.contains('pip-active'),
            hasViewerClass: document.querySelector('.viewer')?.classList.contains('pip-active'),
            time: v.currentTime,
            paused: v.paused,
            audit: window.__pipAudit,
          };
        });
        assert.equal(duringPip.inPip, true);
        assert.equal(duringPip.hasWrapClass, true);
        assert.equal(duringPip.hasViewerClass, true);
        assert.equal(duringPip.paused, false, 'entering PiP must keep video playing');
        assert.ok(
          duringPip.time >= beforePip.time - 0.1,
          `entering PiP lost position: ${duringPip.time} < ${beforePip.time}`,
        );
        assert.equal(duringPip.audit.requestCalls, 1);

        // Leave PiP -> preserves position and play state, restores overlay.
        await page.evaluate(() => document.exitPictureInPicture());
        await tracker.waitForFn(
          page,
          () => !document.pictureInPictureElement,
          null,
          'left web PiP',
        );
        const afterLeave = await page.evaluate(() => {
          const v = document.querySelector('.player-video-wrap video');
          const overlay = document.querySelector('.player-overlay');
          return {
            time: v.currentTime,
            paused: v.paused,
            overlayVisible: Boolean(overlay && !overlay.classList.contains('hidden')),
            hasWrapClass: document.querySelector('.player-video-wrap')?.classList.contains('pip-active'),
          };
        });
        assert.equal(afterLeave.paused, false, 'leaving PiP must keep video playing');
        assert.ok(
          afterLeave.time >= duringPip.time - 0.1,
          `leaving PiP lost position: ${afterLeave.time} < ${duringPip.time}`,
        );
        assert.equal(afterLeave.overlayVisible, true, 'leaving PiP must show the overlay');
        assert.equal(afterLeave.hasWrapClass, false);

        // Re-enter PiP and close viewer -> closing exits PiP cleanly.
        await pipBtn.click();
        await tracker.waitForFn(
          page,
          () => Boolean(document.pictureInPictureElement),
          null,
          're-entered web PiP before close',
        );
        await page.evaluate(() => {
          // Close via Escape / viewer close handler while in PiP.
          document.querySelector('.viewer-top.player-top button[aria-label="Close"]')?.click();
        });
        await tracker.waitForLocator(page.locator('.viewer'), 'video viewer closed from PiP', { state: 'detached' });
        const finalAudit = await page.evaluate(() => ({
          inPip: Boolean(document.pictureInPictureElement),
          audit: window.__pipAudit,
        }));
        assert.equal(finalAudit.inPip, false, 'closing video viewer must exit active PiP');
        assert.equal(finalAudit.audit.exitCalls, 2);

        // Music player NEVER shows a PiP button even when pictureInPictureEnabled is true.
        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).click();
        await tracker.waitForLocator(page.locator('.music-screen'), 'music screen opened');
        assert.equal(
          await page.locator('.player-pip-btn').count(),
          0,
          'Music player must never show a Picture-in-Picture button',
        );

        await ctx.close();
      }

      assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
      assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);

browserTest(
  'Nothing runs when nobody is looking: hidden tab makes zero node calls, active transfer keeps reporting, return does one refresh',
  { timeout: 120_000 },
  async (_t, tracker) => {
    assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
    const root = makeWorkRoot();
    const fleet = setupTwoNodeAuditFleet(root);
    const server = await startLegionServer(fleet.towerHome, fleet.towerUser);
    tracker.setServerStderr(server.getStderr);
    const browser = await launchBrowser();
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const ctx = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      await ctx.addInitScript(() => {
        let visState = 'visible';
        Object.defineProperty(document, 'visibilityState', {
          configurable: true,
          get: () => visState,
        });
        Object.defineProperty(document, 'hidden', {
          configurable: true,
          get: () => visState !== 'visible',
        });
        window.__setTestVisibility = (next, { dispatchFocus = false } = {}) => {
          visState = next;
          document.dispatchEvent(new Event('visibilitychange'));
          if (dispatchFocus && next === 'visible') {
            window.dispatchEvent(new Event('focus'));
          }
        };
      });
      const page = tracker.attach(await ctx.newPage(), {
        label: 'hidden-audit',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      // 1. Connect to real legion serve and open Machine Overview (5 s live sys.info poller).
      tracker.setStep('connect and open Machine Overview');
      await page.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, {
        waitUntil: 'domcontentloaded',
      });
      await tracker.waitForLocator(page.locator('.split-left .split-label-fact', { hasText: /^2$/ }), 'fleet loaded');
      await page.goto(`${server.url}/#/machines/tower`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.getByText('Health'), 'tower Overview visible');

      // Wait for at least one periodic 5 s Overview poll while visible to prove polling is active.
      const countAfterMount = readAuditEntries(fleet.auditPath).length;
      await tracker.waitForCondition(
        'live Overview sys.info poll while visible',
        () => {
          const added = readAuditEntries(fleet.auditPath).slice(countAfterMount);
          return added.some((e) => e.tool === 'sys.info');
        },
        { timeoutMs: 10_000, intervalMs: 200, page },
      );

      // 2. Hide the page and confirm ZERO calls hit state/audit.jsonl for longer than the 5 s poll.
      tracker.setStep('hide tab and verify zero node calls in state/audit.jsonl');
      await page.evaluate(() => window.__setTestVisibility('hidden'));
      // Allow any already-dispatched in-flight request to finish writing its audit line.
      await sleep(500);
      const countWhileHiddenStart = readAuditEntries(fleet.auditPath).length;
      await sleep(6_500);
      const addedWhileHidden = readAuditEntries(fleet.auditPath).slice(countWhileHiddenStart);
      assert.equal(
        addedWhileHidden.length,
        0,
        `Hidden page must make 0 node calls, saw ${addedWhileHidden.length}: ${JSON.stringify(addedWhileHidden.map((e) => e.tool))}`,
      );

      // 3. Return to visible (dispatching both visibilitychange and focus, as browsers do) and
      //    verify ONE immediate refresh rather than a duplicate burst.
      tracker.setStep('return to visible and verify single deduplicated refresh');
      await page.evaluate(() => window.__setTestVisibility('visible', { dispatchFocus: true }));
      await tracker.waitForCondition(
        'single refresh on return to visible',
        () => {
          const added = readAuditEntries(fleet.auditPath).slice(countWhileHiddenStart);
          return added.some((e) => e.tool === 'sys.info');
        },
        { timeoutMs: 10_000, intervalMs: 100, page },
      );
      // Wait past any focus-duplication window (1.5 s) but well under the next 5 s overview tick.
      await sleep(1_500);
      const addedOnReturn = readAuditEntries(fleet.auditPath).slice(countWhileHiddenStart);
      const returnedFleetList = addedOnReturn.filter((e) => e.tool === 'fleet.list');
      const returnedFleetPing = addedOnReturn.filter((e) => e.tool === 'fleet.ping');
      const returnedSysProcs = addedOnReturn.filter((e) => e.tool === 'sys.procs');
      const returnedSysInfo = addedOnReturn.filter((e) => e.tool === 'sys.info');
      assert.equal(
        returnedFleetList.length,
        1,
        `Expected exactly 1 fleet.list on return (no visibilitychange+focus burst), got ${returnedFleetList.length}: ${JSON.stringify(addedOnReturn.map((e) => e.tool))}`,
      );
      assert.equal(
        returnedFleetPing.length,
        1,
        `Expected exactly 1 fleet.ping on return (no duplicate burst), got ${returnedFleetPing.length}`,
      );
      assert.equal(
        returnedSysProcs.length,
        1,
        `Expected exactly 1 overview sys.procs poll on return, got ${returnedSysProcs.length}`,
      );
      assert.ok(
        returnedSysInfo.length >= 1 && returnedSysInfo.length <= 3,
        `Expected single fleet fan-out (2 nodes) + 1 overview sys.info on return, got ${returnedSysInfo.length}: ${JSON.stringify(addedOnReturn.map((e) => e.tool))}`,
      );

      // 4. Active transfer exception: start a real transfer to pi, hide the tab mid-transfer, and
      //    verify Activity keeps polling & updating progress while hidden, then stops once done.
      tracker.setStep('verify active transfer keeps reporting while tab is hidden');
      await page.goto(`${server.url}/#/files/tower/${encodeURIComponent(path.join(fleet.towerUser, 'docs'))}`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.list a.row', { hasText: 'transfer-sample.bin' }).click();
      await tracker.waitForLocator(page.locator('.viewer'), 'transfer-sample.bin viewer');
      await page.locator('.viewer-bar button', { hasText: 'Send to' }).click();
      const sendSheet = page.locator('.sheet');
      await tracker.waitForLocator(sendSheet, 'Send to sheet');
      await sendSheet.getByRole('button', { name: 'Copy to pi' }).click();
      await tracker.waitForLocator(sendSheet, 'Send to sheet closed', { state: 'hidden' });
      await page.locator('.viewer-top button[aria-label="Close"]').click();
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer closed after Send to', { state: 'detached' });

      await page.locator('nav.nav a', { hasText: 'Activity' }).click();
      const transferRow = page.locator('main.main .list a.row', { hasText: /transfer-sample\.bin/ }).first();
      await tracker.waitForLocator(transferRow, 'transfer row in Activity');
      const initialRowText = await transferRow.innerText();
      assert.ok(
        !/\b(?:Stopped|Failed)\b/.test(initialRowText),
        `Transfer failed immediately before hiding tab: ${JSON.stringify(initialRowText)}`,
      );

      // Hide the page immediately while the throttled transfer is running.
      await page.evaluate(() => window.__setTestVisibility('hidden'));
      const countTransferHiddenStart = readAuditEntries(fleet.auditPath).length;

      // While hidden, Activity must continue polling until the transfer reports Finished.
      let lastRowText = initialRowText;
      try {
        await tracker.waitForCondition(
          'active transfer completes and updates UI while tab is hidden',
          async () => {
            lastRowText = await transferRow.innerText();
            if (/\b(?:Stopped|Failed)\b/.test(lastRowText)) {
              throw new Error(`Transfer job failed while hidden: ${JSON.stringify(lastRowText)}`);
            }
            return /\bFinished\b/.test(lastRowText);
          },
          { timeoutMs: 20_000, intervalMs: 200, page },
        );
      } catch (err) {
        const recentAudit = readAuditEntries(fleet.auditPath).slice(countTransferHiddenStart);
        let jobsDump = '';
        let logsDump = '';
        try {
          jobsDump = runLegion(['job.list', '--json'], fleet.towerHome, fleet.towerUser);
          const firstId = JSON.parse(jobsDump).jobs?.[0]?.id;
          if (firstId) {
            logsDump = runLegion(['job.logs', `id=${firstId}`, '--json'], fleet.towerHome, fleet.towerUser);
          }
        } catch {}
        throw new Error(
          `${err.message}\n  initialRowText=${JSON.stringify(initialRowText)}\n  lastRowText=${JSON.stringify(lastRowText)}\n  auditDuringHidden=${JSON.stringify(recentAudit.map((e) => e.tool))}\n  jobs=${jobsDump}\n  logs=${logsDump}`,
        );
      }
      const addedDuringHiddenTransfer = readAuditEntries(fleet.auditPath).slice(countTransferHiddenStart);
      assert.ok(
        addedDuringHiddenTransfer.some((e) => e.tool === 'job.list' || e.tool === 'job.status'),
        `Active transfer should keep polling while tab is hidden; audit saw: ${JSON.stringify(addedDuringHiddenTransfer.map((e) => e.tool))}`,
      );

      // Once the transfer has finished while hidden, polling must stop completely (0 new calls).
      await sleep(500);
      const countAfterTransferDone = readAuditEntries(fleet.auditPath).length;
      await sleep(2_200);
      const addedAfterDoneWhileHidden = readAuditEntries(fleet.auditPath).slice(countAfterTransferDone);
      assert.equal(
        addedAfterDoneWhileHidden.length,
        0,
        `Polling must stop once transfer finishes while hidden, saw: ${JSON.stringify(addedAfterDoneWhileHidden.map((e) => e.tool))}`,
      );

      await ctx.close();
      assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
      assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
    } finally {
      await browser.close();
      await server.stop();
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
);

const VIRTUAL_PNG_CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let c = i;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[i] = c >>> 0;
  }
  return table;
})();

function makeTinyPng(width, height, r, g, b) {
  function crc32(buf) {
    let c = 0xffffffff;
    for (let i = 0; i < buf.length; i += 1) {
      c = VIRTUAL_PNG_CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
    }
    return (c ^ 0xffffffff) >>> 0;
  }
  function chunk(type, data) {
    const typeBuf = Buffer.from(type, 'ascii');
    const lenBuf = Buffer.alloc(4);
    lenBuf.writeUInt32BE(data.length, 0);
    const crcBuf = Buffer.alloc(4);
    crcBuf.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
    return Buffer.concat([lenBuf, typeBuf, data, crcBuf]);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 2;
  const stride = 1 + width * 3;
  const raw = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y += 1) {
    const row = y * stride;
    raw[row] = 0;
    for (let x = 0; x < width; x += 1) {
      const px = row + 1 + x * 3;
      raw[px] = r;
      raw[px + 1] = g;
      raw[px + 2] = b;
    }
  }
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function makeTinyWav(durationSecs = 2) {
  const sampleRate = 8000;
  const byteRate = sampleRate * 2;
  const dataLen = byteRate * durationSecs;
  const fmt = Buffer.alloc(24);
  fmt.write('fmt ', 0, 'ascii');
  fmt.writeUInt32LE(16, 4);
  fmt.writeUInt16LE(1, 8);
  fmt.writeUInt16LE(1, 10);
  fmt.writeUInt32LE(sampleRate, 12);
  fmt.writeUInt32LE(byteRate, 16);
  fmt.writeUInt16LE(2, 20);
  fmt.writeUInt16LE(16, 22);
  const dataHdr = Buffer.alloc(8);
  dataHdr.write('data', 0, 'ascii');
  dataHdr.writeUInt32LE(dataLen, 4);
  const body = Buffer.concat([Buffer.from('WAVE', 'ascii'), fmt, dataHdr, Buffer.alloc(dataLen)]);
  const riff = Buffer.alloc(8);
  riff.write('RIFF', 0, 'ascii');
  riff.writeUInt32LE(body.length, 4);
  return Buffer.concat([riff, body]);
}

function setupVirtualMediaNode(root) {
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const galleryDir = path.join(towerUser, 'VirtualGallery');

  for (const dir of [
    path.join(towerHome, 'config'),
    path.join(towerHome, 'state'),
    galleryDir,
  ]) {
    fs.mkdirSync(dir, { recursive: true });
  }

  const pngBytes = makeTinyPng(16, 16, 68, 132, 224);
  // 500 entries (> VIRTUAL_MIN_ITEMS=60 and > PAGE_SIZE=200): odd entries are PNG images and
  // even entries are text files (<50% media so folder opens in List view), every name wraps to
  // 2 lines on phone, and entry 500 is a playable WAV song at the very bottom of the list.
  for (let i = 1; i <= 499; i += 1) {
    const id = String(i).padStart(3, '0');
    if (i % 2 === 1) {
      fs.writeFileSync(
        path.join(galleryDir, `item_${id}_Quarterly_Production_Archive_Snapshot_Detail.png`),
        pngBytes,
      );
    } else {
      fs.writeFileSync(
        path.join(galleryDir, `item_${id}_Quarterly_Production_Archive_Snapshot_Detail.txt`),
        `Entry ${id}\n`,
      );
    }
  }
  fs.writeFileSync(path.join(galleryDir, 'z_500_final_album_song.wav'), makeTinyWav(2));

  const policy = [
    'default = "owner"',
    '',
    '[principals.ui]',
    'role = "operator"',
    '',
    '[roles.operator]',
    'allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy", "effect:power"]',
    'deny = ["effect:admin"]',
    '',
  ].join('\n');

  fs.writeFileSync(
    path.join(towerHome, 'config', 'node.toml'),
    'name = "tower"\nsystem = "linux"\nbusy = "none"\n',
  );
  fs.writeFileSync(path.join(towerHome, 'config', 'policy.toml'), policy);
  fs.writeFileSync(
    path.join(towerHome, 'config', 'fleet.toml'),
    '[machines.tower]\nssh = "tower.example"\n',
  );

  const tokenOut = JSON.parse(
    runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser),
  );
  assert.ok(tokenOut.token && tokenOut.token.startsWith('lgn_'));

  return { towerHome, towerUser, galleryDir, token: tokenOut.token };
}

browserTest(
  'Virtual list with hidden/visible transitions and small player bar: thumbnails, scroll, loadNextPage, long names, and bottom inset',
  { timeout: 150_000 },
  async (_t, tracker) => {
    assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
    const root = makeWorkRoot();
    const env = setupVirtualMediaNode(root);
    const server = await startLegionServer(env.towerHome, env.towerUser);
    tracker.setServerStderr(server.getStderr);
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    const visibilityInitScript = () => {
      let visState = 'visible';
      Object.defineProperty(document, 'visibilityState', {
        configurable: true,
        get: () => visState,
      });
      Object.defineProperty(document, 'hidden', {
        configurable: true,
        get: () => visState !== 'visible',
      });
      const realDateNow = Date.now.bind(Date);
      let clockOffsetMs = 0;
      Date.now = () => realDateNow() + clockOffsetMs;
      window.__advanceTestClock = (ms) => {
        clockOffsetMs += ms;
      };
      window.__setTestVisibility = (next, { dispatchFocus = false } = {}) => {
        visState = next;
        document.dispatchEvent(new Event('visibilitychange'));
        if (dispatchFocus && next === 'visible') {
          window.dispatchEvent(new Event('focus'));
        }
      };
    };

    try {
      // ── 1. Phone (390x844): bullets (a), (b), (c), (d), and (e) on main.main virtual list ──
      const phoneCtx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
      await phoneCtx.addInitScript(visibilityInitScript);
      const page = tracker.attach(await phoneCtx.newPage(), {
        label: 'virtual-hidden-phone',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      const rpcCalls = [];
      page.on('request', (req) => {
        if (req.method() !== 'POST' || !req.url().includes('/v1/call')) return;
        try {
          const body = JSON.parse(req.postData() || '{}');
          if (body?.tool) rpcCalls.push({ tool: body.tool, args: body.args || {} });
        } catch {
          // Ignore malformed payloads.
        }
      });

      const galleryUrl = `${server.url}/#/files/tower/${encodeURIComponent(env.galleryDir)}`;
      tracker.setStep('connect and open 500-entry virtualized gallery on phone');
      await page.goto(`${server.url}/#token=${encodeURIComponent(env.token)}`, {
        waitUntil: 'domcontentloaded',
      });
      await tracker.waitForLocator(page.locator('nav.nav a', { hasText: 'Files' }), 'Files nav link');
      await page.goto(galleryUrl, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(
        page.locator('.list.vlist a.row[aria-label^="item_001_"]'),
        'virtualized gallery first row',
      );
      // Wait for initial visible thumbnails and initial name-fit pass to finish.
      await tracker.waitForFn(
        page,
        () => {
          const firstImg = document.querySelector('.list.vlist a.row[aria-label^="item_001_"] .thumb img[src^="data:"]');
          const firstSlot = document.querySelector('.list.vlist a.row[aria-label^="item_001_"] .fn-slot');
          return Boolean(firstImg && firstSlot?.getAttribute('data-fn-ready') === '1');
        },
        null,
        'initial row 1 thumbnail and fitted name ready',
      );
      await waitForLayoutSettled(page, 2);

      // ── Bullets (a) & (d): mount/unmount while hidden -> zero queue leaks, zero hidden work,
      //    single name-fit batch on return for in-view rows only, and only in-view thumbnails load.
      tracker.setStep('bullets (a) & (d): unmount paused rows and mount rows while hidden');
      await page.evaluate(() => window.__setTestVisibility('hidden'));
      await sleep(150);
      const rpcCountAtHide1 = rpcCalls.length;
      const nameStatsBeforeHide1 = await page.evaluate(() => ({ ...(window.__LEGION_NAME_FIT_STATS__ || {}) }));

      // First jump while hidden: mount rows around ~45 (inside initial 120 renderLimit).
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = 3000;
      });
      await waitForLayoutSettled(page, 3);
      const midHiddenPaths = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        return [...document.querySelectorAll('.list.vlist .vlist-item')]
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.bottom >= sr.top && r.top <= sr.bottom;
          })
          .map((el) => el.dataset.vlistKey)
          .filter((p) => p && p.endsWith('.png'));
      });
      assert.ok(midHiddenPaths.length >= 3, `Expected visible PNG rows at first hidden scroll, got ${midHiddenPaths.length}`);

      // Second jump while STILL hidden: unmount those mid-range rows and mount rows near ~95.
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = 6200;
      });
      await waitForLayoutSettled(page, 3);
      await sleep(250);

      const hiddenWindowInfo = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        const mounted = [...document.querySelectorAll('.list.vlist .vlist-item')];
        const visiblePngs = mounted
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.bottom >= sr.top && r.top <= sr.bottom;
          })
          .map((el) => el.dataset.vlistKey)
          .filter((p) => p && p.endsWith('.png'));
        const mountedSet = new Set(mounted.map((el) => el.dataset.vlistKey));
        return {
          mountedCount: mounted.length,
          mountedKeys: [...mountedSet],
          visiblePngs,
          stats: { ...(window.__LEGION_NAME_FIT_STATS__ || {}) },
        };
      });
      const unmountedWhilePaused = midHiddenPaths.filter((p) => !hiddenWindowInfo.mountedKeys.includes(p));
      assert.ok(
        unmountedWhilePaused.length >= 2,
        `Expected earlier hidden rows to unmount after second jump: mid=${midHiddenPaths.length}, unmounted=${unmountedWhilePaused.length}`,
      );
      assert.ok(hiddenWindowInfo.visiblePngs.length >= 3, 'Expected newly mounted visible PNG rows at second jump');

      // Assert ZERO thumb calls and ZERO name-fit batches occurred while hidden.
      const callsWhileHidden1 = rpcCalls.slice(rpcCountAtHide1);
      assert.equal(
        callsWhileHidden1.filter((c) => c.tool === 'fs.thumb').length,
        0,
        `Must make 0 fs.thumb calls while hidden, saw: ${JSON.stringify(callsWhileHidden1)}`,
      );
      assert.equal(
        hiddenWindowInfo.stats.totalBatches,
        nameStatsBeforeHide1.totalBatches,
        'FileNameList must perform 0 measurement batches while hidden',
      );
      assert.equal(
        hiddenWindowInfo.stats.totalMeasuredRows,
        nameStatsBeforeHide1.totalMeasuredRows,
        'FileNameList must measure 0 rows while hidden',
      );

      // Return to visible: verify ONE name-fit batch for in-view rows only, and thumbnails load for
      // mounted in-view rows without requesting any row that unmounted while paused.
      const rpcCountBeforeReturn1 = rpcCalls.length;
      await page.evaluate(() => window.__setTestVisibility('visible'));
      try {
        await tracker.waitForFn(
          page,
          (targetPaths) => {
            const scroller = document.querySelector('main.main');
            const sr = scroller.getBoundingClientRect();
            const slots = [...document.querySelectorAll('.list.vlist .vlist-item')]
              .filter((el) => {
                const r = el.getBoundingClientRect();
                return r.bottom > sr.top && r.top < sr.bottom;
              })
              .map((el) => el.querySelector('.fn-slot'));
            if (slots.length === 0 || !slots.every((s) => s?.getAttribute('data-fn-ready') === '1')) {
              return false;
            }
            return targetPaths.every((p) => {
              const item = document.querySelector(`.list.vlist .vlist-item[data-vlist-key="${CSS.escape(p)}"]`);
              return Boolean(item?.querySelector('.thumb img[src^="data:"]'));
            });
          },
          hiddenWindowInfo.visiblePngs,
          'in-view names fitted and mounted-while-hidden thumbnails loaded after return',
          { timeoutMs: 8_000 },
        );
      } catch (err) {
        const diag = await page.evaluate((targetPaths) => {
          const scroller = document.querySelector('main.main');
          const sr = scroller.getBoundingClientRect();
          const vis = [...document.querySelectorAll('.list.vlist .vlist-item')]
            .filter((el) => {
              const r = el.getBoundingClientRect();
              return r.bottom > sr.top && r.top < sr.bottom;
            })
            .map((el) => ({
              key: el.dataset.vlistKey,
              ready: el.querySelector('.fn-slot')?.getAttribute('data-fn-ready'),
              hasImg: Boolean(el.querySelector('.thumb img[src^="data:"]')),
            }));
          const targets = targetPaths.map((p) => {
            const item = document.querySelector(`.list.vlist .vlist-item[data-vlist-key="${CSS.escape(p)}"]`);
            return {
              key: p,
              mounted: Boolean(item),
              hasImg: Boolean(item?.querySelector('.thumb img[src^="data:"]')),
              top: item ? Math.round(item.getBoundingClientRect().top) : null,
            };
          });
          return { scrollTop: scroller.scrollTop, srTop: sr.top, srBottom: sr.bottom, vis, targets, stats: window.__LEGION_NAME_FIT_STATS__ };
        }, hiddenWindowInfo.visiblePngs);
        throw new Error(`${err.message}\ndiag=${JSON.stringify(diag)}\nrpcSinceReturn=${JSON.stringify(rpcCalls.slice(rpcCountBeforeReturn1))}`);
      }
      await sleep(250);

      const afterReturn1 = await page.evaluate(() => ({
        mountedCount: document.querySelectorAll('.list.vlist .vlist-item').length,
        stats: { ...(window.__LEGION_NAME_FIT_STATS__ || {}) },
      }));
      assert.equal(
        afterReturn1.stats.totalBatches - nameStatsBeforeHide1.totalBatches,
        1,
        `Long file names must fit ONCE on return from hidden (batches ${nameStatsBeforeHide1.totalBatches} -> ${afterReturn1.stats.totalBatches})`,
      );
      assert.ok(
        afterReturn1.stats.lastBatchRows > 0 && afterReturn1.stats.lastBatchRows <= afterReturn1.mountedCount,
        `Return fit must measure only mounted in-view rows (lastBatchRows=${afterReturn1.stats.lastBatchRows}, mounted=${afterReturn1.mountedCount})`,
      );

      const thumbPathsOnReturn1 = rpcCalls
        .slice(rpcCountBeforeReturn1)
        .filter((c) => c.tool === 'fs.thumb')
        .flatMap((c) => c.args?.paths || []);
      for (const p of hiddenWindowInfo.visiblePngs) {
        assert.ok(
          thumbPathsOnReturn1.includes(p),
          `Row mounted while hidden must load thumbnail on return: missing ${p}`,
        );
      }
      for (const p of unmountedWhilePaused) {
        assert.ok(
          !thumbPathsOnReturn1.includes(p),
          `Unmounted row must not leak a thumbnail queue entry on return: unexpectedly fetched ${p}`,
        );
      }

      // ── Bullet (c): loadNextPage stays quiet while hidden and fires at the end of the virtual
      //    list after a hidden/visible cycle.
      tracker.setStep('bullet (c): loadNextPage gated while hidden and fires on return');
      // First advance incremental renderLimit from 120 -> 200 while visible (still only page 1).
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = Math.max(0, scroller.scrollHeight - scroller.clientHeight - 40);
      });
      await tracker.waitForFn(
        page,
        () => {
          const pager = document.querySelector('.folder-pager');
          return Boolean(pager && /200 of 500/.test(pager.textContent || ''));
        },
        null,
        'renderLimit expanded to 200 of 500 before hidden pagination test',
      );
      await waitForLayoutSettled(page, 2);

      // Now hide the page and scroll all the way to the very end of the 200-row virtual list.
      await page.evaluate(() => window.__setTestVisibility('hidden'));
      await sleep(150);
      const rpcCountBeforeHiddenBottom = rpcCalls.length;
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = scroller.scrollHeight - scroller.clientHeight;
      });
      await waitForLayoutSettled(page, 3);
      await sleep(600);

      const listCallsWhileHiddenAtEnd = rpcCalls
        .slice(rpcCountBeforeHiddenBottom)
        .filter((c) => c.tool === 'fs.list');
      assert.equal(
        listCallsWhileHiddenAtEnd.length,
        0,
        `loadNextPage must not fire while hidden, saw fs.list calls: ${JSON.stringify(listCallsWhileHiddenAtEnd)}`,
      );
      assert.ok(
        await page.locator('.folder-pager').isVisible(),
        'Pager must remain at 200 of 500 while hidden',
      );

      // Return to visible without scrolling again or clicking Show more -> loadNextPage fires!
      await page.evaluate(() => window.__setTestVisibility('visible'));
      await tracker.waitForLocator(page.locator('.folder-pager'), 'pager gone after automatic loadNextPage on return', {
        state: 'detached',
      });
      const nextPageCalls = rpcCalls
        .slice(rpcCountBeforeHiddenBottom)
        .filter((c) => c.tool === 'fs.list' && Number(c.args?.offset) === 200);
      assert.equal(
        nextPageCalls.length,
        1,
        `Expected 1 offset=200 loadNextPage call on return from hidden, got ${nextPageCalls.length}`,
      );

      // ── Bullet (b): return from hidden while scrolled far down (>10s stale) refreshes ONCE,
      //    keeps scroll position, and loads thumbnails of the rows now in view, not rows that
      //    scrolled out while hidden.
      tracker.setStep('bullet (b): stale return scrolled far down refreshes once, keeps scroll, loads in-view thumbs only');
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = Math.round(scroller.scrollHeight * 0.68);
      });
      await waitForLayoutSettled(page, 3);
      const beforeHideScrolledState = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        const visiblePngs = [...document.querySelectorAll('.list.vlist .vlist-item')]
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.bottom >= sr.top && r.top <= sr.bottom;
          })
          .map((el) => el.dataset.vlistKey)
          .filter((p) => p && p.endsWith('.png'));
        return { scrollTop: scroller.scrollTop, visiblePngs };
      });
      assert.ok(beforeHideScrolledState.scrollTop > 18_000, `Expected far-down scrollTop, got ${beforeHideScrolledState.scrollTop}`);

      // Hide, advance clock past 10s staleness window, and scroll further down so an uncached
      // batch mounts and the previous batch unmounts while hidden.
      await page.evaluate(() => {
        window.__setTestVisibility('hidden');
        window.__advanceTestClock(15_000);
      });
      await sleep(150);
      const rpcCountBeforeHiddenScrollB = rpcCalls.length;

      // Scroll first to ~76% (uncached rows mount while paused), then to ~88% (those rows unmount
      // while paused and a fresh uncached region around row ~440 mounts in view).
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = Math.round(scroller.scrollHeight * 0.76);
      });
      await waitForLayoutSettled(page, 3);
      const scrolledOutWhileHiddenPngs = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        return [...document.querySelectorAll('.list.vlist .vlist-item')]
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.bottom >= sr.top && r.top <= sr.bottom;
          })
          .map((el) => el.dataset.vlistKey)
          .filter((p) => p && p.endsWith('.png'));
      });

      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = Math.round(scroller.scrollHeight * 0.88);
      });
      await waitForLayoutSettled(page, 3);
      await sleep(250);

      const farDownBeforeReturn = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        const mounted = [...document.querySelectorAll('.list.vlist .vlist-item')];
        const mountedKeys = new Set(mounted.map((el) => el.dataset.vlistKey));
        const visiblePngs = mounted
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.bottom >= sr.top && r.top <= sr.bottom;
          })
          .map((el) => el.dataset.vlistKey)
          .filter((p) => p && p.endsWith('.png'));
        const anchor = mounted.find((el) => {
          const r = el.getBoundingClientRect();
          return r.top >= sr.top + 60 && r.bottom <= sr.bottom - 60;
        });
        return {
          scrollTop: scroller.scrollTop,
          mountedKeys: [...mountedKeys],
          visiblePngs,
          anchorKey: anchor?.dataset?.vlistKey || null,
        };
      });
      const scrolledOutDisjoint = scrolledOutWhileHiddenPngs.filter(
        (p) => !farDownBeforeReturn.mountedKeys.includes(p),
      );
      assert.ok(scrolledOutDisjoint.length >= 2, 'Expected intermediate hidden rows to scroll out before return');
      assert.ok(farDownBeforeReturn.visiblePngs.length >= 3, 'Expected uncached visible PNGs at ~88% scroll');
      assert.equal(
        rpcCalls.slice(rpcCountBeforeHiddenScrollB).filter((c) => c.tool === 'fs.list' || c.tool === 'fs.thumb').length,
        0,
        'Zero fs.list or fs.thumb calls may run while hidden',
      );

      // Return to visible with both visibilitychange and focus -> refreshes ONCE, keeps scroll
      // position, and loads ONLY the thumbnails now in view.
      const rpcCountBeforeStaleReturn = rpcCalls.length;
      await page.evaluate(() => window.__setTestVisibility('visible', { dispatchFocus: true }));
      await tracker.waitForCondition(
        'stale far-down folder refreshes once on return',
        () => rpcCalls.slice(rpcCountBeforeStaleReturn).some((c) => c.tool === 'fs.list'),
        { timeoutMs: 10_000, intervalMs: 50, page },
      );
      await tracker.waitForFn(
        page,
        (targetPaths) =>
          targetPaths.every((p) => {
            const item = document.querySelector(`.list.vlist .vlist-item[data-vlist-key="${CSS.escape(p)}"]`);
            return Boolean(item?.querySelector('.thumb img[src^="data:"]'));
          }),
        farDownBeforeReturn.visiblePngs,
        'far-down in-view thumbnails loaded after stale return',
      );
      await sleep(300);

      const callsAfterStaleReturn = rpcCalls.slice(rpcCountBeforeStaleReturn);
      const staleListCalls = callsAfterStaleReturn.filter((c) => c.tool === 'fs.list');
      assert.equal(
        staleListCalls.length,
        1,
        `Returning from hidden scrolled far down must refresh ONCE (no focus duplicate, no extra page), got ${staleListCalls.length}: ${JSON.stringify(staleListCalls)}`,
      );
      const staleThumbPaths = callsAfterStaleReturn
        .filter((c) => c.tool === 'fs.thumb')
        .flatMap((c) => c.args?.paths || []);
      for (const p of farDownBeforeReturn.visiblePngs) {
        assert.ok(
          staleThumbPaths.includes(p),
          `In-view row at ~88% scroll must load thumbnail on return: missing ${p}`,
        );
      }
      for (const p of scrolledOutDisjoint) {
        assert.ok(
          !staleThumbPaths.includes(p),
          `Row scrolled out while hidden must NOT load thumbnail on return: unexpectedly fetched ${p}`,
        );
      }
      assert.ok(
        !staleThumbPaths.some((p) => /item_00[1-9]_|item_01[0-9]_|item_02[0-4]_/.test(p)),
        `Top rows 001..024 must NOT be fetched when scrolled far down: ${JSON.stringify(staleThumbPaths)}`,
      );

      const farDownAfterReturn = await page.evaluate((anchorKey) => {
        const scroller = document.querySelector('main.main');
        const sr = scroller.getBoundingClientRect();
        const anchorEl = anchorKey
          ? document.querySelector(`.list.vlist .vlist-item[data-vlist-key="${CSS.escape(anchorKey)}"]`)
          : null;
        const ar = anchorEl ? anchorEl.getBoundingClientRect() : null;
        return {
          scrollTop: scroller.scrollTop,
          scrollHeight: scroller.scrollHeight,
          hasPager: Boolean(document.querySelector('.folder-pager')),
          anchorStillVisible: Boolean(ar && ar.bottom >= sr.top && ar.top <= sr.bottom),
        };
      }, farDownBeforeReturn.anchorKey);
      assert.equal(farDownAfterReturn.hasPager, false, 'Quiet refresh must keep all 500 entries loaded');
      assert.ok(
        farDownAfterReturn.anchorStillVisible &&
          Math.abs(farDownAfterReturn.scrollTop - farDownBeforeReturn.scrollTop) <= 180,
        `Scroll position must be preserved across stale return: before=${farDownBeforeReturn.scrollTop} after=${JSON.stringify(farDownAfterReturn)}`,
      );

      // ── Bullet (e) phone: small player bar height feeds virtual list bottom inset (last row
      //    reachable above the bar with zero overlap and zero blank gap).
      tracker.setStep('bullet (e) phone: small player bar bottom inset with virtual list');
      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = scroller.scrollHeight - scroller.clientHeight;
      });
      await tracker.waitForLocator(
        page.locator('.list.vlist a.row[aria-label="z_500_final_album_song.wav"]'),
        'last row z_500_final_album_song.wav on phone',
      );
      await page.locator('.list.vlist a.row[aria-label="z_500_final_album_song.wav"]').click();
      await tracker.waitForLocator(page.locator('.music-screen'), 'music screen from virtual list last row');
      await tracker.waitForLocator(page.locator('audio[data-legion-music-audio][src]'), 'song audio[src]', {
        state: 'attached',
      });
      await waitForMediaReady(page.locator('audio[data-legion-music-audio][src]'), 'song metadata ready', {
        tracker,
        minReadyState: 1,
      });
      await page.locator('.music-play-btn').click();
      await page.locator('.music-top button[aria-label="Close"]').click();
      await tracker.waitForLocator(page.locator('.viewer'), 'music screen closed on phone', { state: 'detached' });
      await tracker.waitForLocator(page.locator('.mini-player'), 'small player bar shown over virtual list on phone');
      await waitForLayoutSettled(page, 2);

      await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        scroller.scrollTop = scroller.scrollHeight - scroller.clientHeight;
      });
      await waitForLayoutSettled(page, 2);

      const phoneInset = await page.evaluate(() => {
        const scroller = document.querySelector('main.main');
        const mini = document.querySelector('.mini-player');
        const dock = document.querySelector('.dock');
        const lastRow = document.querySelector('.list.vlist a.row[aria-label="z_500_final_album_song.wav"]');
        if (!scroller || !mini || !lastRow) return null;
        const sr = scroller.getBoundingClientRect();
        const mr = mini.getBoundingClientRect();
        const dr = dock ? dock.getBoundingClientRect() : null;
        const lr = lastRow.getBoundingClientRect();
        return {
          miniHeight: Math.round(mr.height),
          padBottom: parseFloat(getComputedStyle(scroller).paddingBottom),
          lastRowTop: Math.round(lr.top),
          lastRowBottom: Math.round(lr.bottom),
          scrollerTop: Math.round(sr.top),
          scrollerBottom: Math.round(sr.bottom),
          dockTop: dr ? Math.round(dr.top) : null,
          dockBottom: dr ? Math.round(dr.bottom) : null,
          miniTop: Math.round(mr.top),
          clearanceInScroller: Math.round(sr.bottom - lr.bottom),
        };
      });
      assert.ok(phoneInset, 'Expected phone virtual list + mini player geometry');
      assert.ok(Math.abs(phoneInset.miniHeight - 56) <= 1, `Mini player height must be 56px, got ${phoneInset.miniHeight}`);
      assert.ok(phoneInset.padBottom >= 68, `Phone virtual scroller bottom padding must include 56px mini bar, got ${phoneInset.padBottom}`);
      assert.ok(
        phoneInset.lastRowTop >= phoneInset.scrollerTop - 1 &&
          phoneInset.lastRowBottom <= (phoneInset.dockTop ?? phoneInset.miniTop) + 1 &&
          phoneInset.clearanceInScroller >= 66,
        `Phone virtual list last row must sit cleanly above dock and mini player: ${JSON.stringify(phoneInset)}`,
      );
      if (phoneInset.dockBottom != null) {
        assert.ok(
          phoneInset.dockBottom <= phoneInset.miniTop + 1,
          `Phone dock overlapped mini player: ${JSON.stringify(phoneInset)}`,
        );
      }

      await phoneCtx.close();

      // ── 2. Desktop (1100x720): bullet (e) on split-right virtual list with small player bar ──
      tracker.setStep('bullet (e) desktop: small player bar bottom inset with split-view virtual list');
      const deskCtx = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const desk = tracker.attach(await deskCtx.newPage(), {
        label: 'virtual-mini-desktop',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });
      await desk.goto(`${server.url}/#token=${encodeURIComponent(env.token)}`, {
        waitUntil: 'domcontentloaded',
      });
      await tracker.waitForLocator(desk.locator('nav.nav a', { hasText: 'Files' }), 'desktop Files nav');
      await desk.goto(galleryUrl, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(
        desk.locator('.split-right .list.vlist a.row[aria-label^="item_001_"]'),
        'desktop virtualized gallery first row',
      );
      // Scroll to load all 500 entries and reach the final song row.
      for (let i = 0; i < 6; i += 1) {
        const done = await desk.evaluate(() => {
          const scroller = document.querySelector('.split-right .split-body');
          if (scroller) scroller.scrollTop = scroller.scrollHeight - scroller.clientHeight;
          return !document.querySelector('.folder-pager') &&
            Boolean(document.querySelector('.split-right .list.vlist a.row[aria-label="z_500_final_album_song.wav"]'));
        });
        if (done) break;
        await waitForLayoutSettled(desk, 2);
        await sleep(150);
      }
      await tracker.waitForLocator(
        desk.locator('.split-right .list.vlist a.row[aria-label="z_500_final_album_song.wav"]'),
        'desktop last row z_500_final_album_song.wav',
      );
      await desk.locator('.split-right .list.vlist a.row[aria-label="z_500_final_album_song.wav"]').click();
      await tracker.waitForLocator(desk.locator('.music-screen'), 'desktop music screen');
      await desk.locator('.music-top button[aria-label="Close"]').click();
      await tracker.waitForLocator(desk.locator('.viewer'), 'desktop music viewer closed', { state: 'detached' });
      await tracker.waitForLocator(desk.locator('.mini-player'), 'desktop small player bar over virtual list');
      await waitForLayoutSettled(desk, 2);

      await desk.evaluate(() => {
        const scroller = document.querySelector('.split-right .split-body');
        scroller.scrollTop = scroller.scrollHeight - scroller.clientHeight;
      });
      await waitForLayoutSettled(desk, 2);

      const deskInset = await desk.evaluate(() => {
        const scroller = document.querySelector('.split-right .split-body');
        const mini = document.querySelector('.mini-player');
        const lastRow = document.querySelector('.split-right .list.vlist a.row[aria-label="z_500_final_album_song.wav"]');
        if (!scroller || !mini || !lastRow) return null;
        const sr = scroller.getBoundingClientRect();
        const mr = mini.getBoundingClientRect();
        const lr = lastRow.getBoundingClientRect();
        return {
          miniHeight: Math.round(mr.height),
          padBottom: parseFloat(getComputedStyle(scroller).paddingBottom),
          lastRowTop: Math.round(lr.top),
          lastRowBottom: Math.round(lr.bottom),
          scrollerTop: Math.round(sr.top),
          scrollerBottom: Math.round(sr.bottom),
          miniTop: Math.round(mr.top),
          clearanceInScroller: Math.round(sr.bottom - lr.bottom),
        };
      });
      assert.ok(deskInset, 'Expected desktop virtual list + mini player geometry');
      assert.ok(Math.abs(deskInset.miniHeight - 56) <= 1, `Desktop mini player height must be 56px, got ${deskInset.miniHeight}`);
      assert.ok(deskInset.padBottom >= 80, `Desktop split-body bottom padding must include 56px mini bar, got ${deskInset.padBottom}`);
      assert.ok(
        deskInset.lastRowTop >= deskInset.scrollerTop - 1 &&
          deskInset.lastRowBottom <= deskInset.miniTop + 1 &&
          deskInset.clearanceInScroller >= 80,
        `Desktop virtual list last row must sit cleanly above mini player: ${JSON.stringify(deskInset)}`,
      );

      await deskCtx.close();
      assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
      assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
    } finally {
      await browser.close();
      await server.stop();
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
);
