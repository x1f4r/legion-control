// Browser tests for Picture-in-Picture video playback and hidden-page idle/resume behaviour
// against a real `legion serve` node and its `state/audit.jsonl`.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
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
