#!/usr/bin/env node
// Playwright CDP driver for apps/legion-app/e2e-android.sh.
// Drives the real Android WebView over `adb forward` + Chrome DevTools Protocol.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const { chromium } = require(path.resolve(HERE, '../ui/test/node_modules/playwright-core'));

const APP_ID = process.env.E2E_APP_ID || 'com.x1f4r.legion.debug';
const ACTIVITY = `${APP_ID}/com.x1f4r.legion.MainActivity`;

const SERIAL = requiredEnv('E2E_ADB_SERIAL');
const SHOTS_DIR = requiredEnv('E2E_SHOTS_DIR');
const LEGION_BIN = requiredEnv('E2E_LEGION_BIN');
const NODE_HOME = requiredEnv('E2E_NODE_HOME');
const SSHD_PORT = requiredEnv('E2E_SSHD_PORT');
const SSHD_PID = Number(requiredEnv('E2E_SSHD_PID'));
const SSHD_LOG = process.env.E2E_SSHD_LOG || '';
const MODE = process.env.E2E_MODE || 'phone';
let activePage = null;

function requiredEnv(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`Missing required environment variable: ${name}`);
  }
  return value;
}

function log(msg) {
  process.stdout.write(`[e2e:${MODE}] ${msg}\n`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function adb(args, opts = {}) {
  return execFileSync('adb', ['-s', SERIAL, ...args], {
    encoding: opts.encoding ?? 'utf8',
    maxBuffer: 32 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function legionNode(args) {
  return execFileSync(LEGION_BIN, args, {
    encoding: 'utf8',
    env: { ...process.env, LEGION_HOME: NODE_HOME },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function createInvite() {
  const out = legionNode([
    'pair',
    '--as',
    'phone',
    '--host',
    '10.0.2.2',
    '--port',
    String(SSHD_PORT),
    '--json',
  ]);
  const parsed = JSON.parse(out);
  assert.ok(
    typeof parsed.invite === 'string' && parsed.invite.startsWith('legion-invite-'),
    `Unexpected invite payload: ${out}`,
  );
  return parsed.invite;
}

function findFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      const port = typeof addr === 'object' && addr ? addr.port : 0;
      server.close((err) => {
        if (err) reject(err);
        else resolve(port);
      });
    });
  });
}

async function waitForWebViewSocket(timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    let pid = '';
    try {
      pid = adb(['shell', 'pidof', APP_ID]).trim().split(/\s+/)[0] || '';
    } catch {
      pid = '';
    }
    if (pid) {
      const unixSockets = adb(['shell', 'cat', '/proc/net/unix']);
      const target = `webview_devtools_remote_${pid}`;
      if (unixSockets.includes(target)) {
        return { pid, socket: target };
      }
    }
    await sleep(300);
  }
  throw new Error(`Timed out waiting for WebView DevTools socket for ${APP_ID}`);
}

async function waitForCdpEndpoint(port, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  const url = `http://127.0.0.1:${port}/json`;
  let lastBody = '';
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url);
      if (res.ok) {
        lastBody = await res.text();
        const targets = JSON.parse(lastBody);
        if (
          Array.isArray(targets) &&
          targets.some(
            (t) =>
              t &&
              t.type === 'page' &&
              typeof t.url === 'string' &&
              (t.url.includes('tauri.localhost') || t.url.startsWith('http://localhost')),
          )
        ) {
          return;
        }
      }
    } catch {
      // Retry until forwarded socket accepts connections.
    }
    await sleep(250);
  }
  throw new Error(`Timed out waiting for Legion page target at ${url}; last=${lastBody}`);
}

async function connectWebView() {
  const { pid, socket } = await waitForWebViewSocket();
  const port = await findFreePort();
  adb(['forward', `tcp:${port}`, `localabstract:${socket}`]);

  try {
    await waitForCdpEndpoint(port);
    const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
    let page = null;
    const deadline = Date.now() + 20000;
    while (Date.now() < deadline) {
      const matches = [];
      for (const ctx of browser.contexts()) {
        for (const candidate of ctx.pages()) {
          const url = candidate.url();
          if (url.includes('tauri.localhost') || url.startsWith('http://localhost')) {
            matches.push(candidate);
          }
        }
      }
      for (const candidate of matches) {
        const visible = await candidate
          .evaluate(() => {
            const root = document.querySelector('#root > *');
            if (!root) return false;
            const rect = root.getBoundingClientRect();
            return rect.width > 0 && rect.height > 0 && document.visibilityState === 'visible';
          })
          .catch(() => false);
        if (visible) {
          page = candidate;
          break;
        }
      }
      if (page) break;
      await sleep(250);
    }
    if (!page) {
      await browser.close();
      throw new Error(`Connected to CDP (pid ${pid}) but found no Legion WebView page`);
    }
    await page.waitForSelector('#root > *', { timeout: 20000 });
    activePage = page;
    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        log(`console.error: ${msg.text()}`);
      }
    });
    page.on('pageerror', (err) => {
      log(`pageerror: ${err.message}`);
    });
    return {
      pid,
      port,
      browser,
      page,
      async close() {
        try {
          await browser.close();
        } catch {
          // Ignore close errors if app was force-stopped.
        }
        try {
          adb(['forward', '--remove', `tcp:${port}`]);
        } catch {
          // Ignore if forward already removed.
        }
      },
    };
  } catch (err) {
    try {
      adb(['forward', '--remove', `tcp:${port}`]);
    } catch {
      // Ignore cleanup errors.
    }
    throw err;
  }
}

async function takeShot(page, name) {
  try {
    await page.evaluate(() => {
      if (document.activeElement && typeof document.activeElement.blur === 'function') {
        document.activeElement.blur();
      }
    });
  } catch {
    // Best-effort blur.
  }
  await sleep(450);
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  const outPath = path.join(SHOTS_DIR, name);
  const png = adb(['exec-out', 'screencap', '-p'], { encoding: 'buffer' });
  assert.ok(png.length > 1000, `Screenshot ${name} is unexpectedly small (${png.length} bytes)`);
  fs.writeFileSync(outPath, png);
  log(`Saved screenshot ${name} (${png.length} bytes)`);
}

function findThrowawayGatePids() {
  const pids = [];
  for (const entry of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    const pid = Number(entry);
    if (pid === process.pid || pid === SSHD_PID) continue;
    try {
      const cmdline = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8');
      if (!cmdline.includes(LEGION_BIN) || !cmdline.includes('gate')) continue;
      const environ = fs.readFileSync(`/proc/${pid}/environ`, 'utf8');
      if (environ.includes(`LEGION_HOME=${NODE_HOME}\0`)) {
        pids.push(pid);
      }
    } catch {
      // Process exited or belongs to another user.
    }
  }
  return pids;
}

function findSshdChildPids() {
  const pids = [];
  for (const entry of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    const pid = Number(entry);
    if (pid === SSHD_PID) continue;
    try {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
      const closeParen = stat.lastIndexOf(')');
      if (closeParen < 0) continue;
      const fields = stat.slice(closeParen + 2).split(' ');
      const ppid = Number(fields[1]);
      if (ppid === SSHD_PID) {
        pids.push(pid);
      }
    } catch {
      // Process exited or unreadable.
    }
  }
  return pids;
}

async function killNodeSessions({ requireExisting = false } = {}) {
  const gatePids = findThrowawayGatePids();
  const sshdChildren = findSshdChildPids();
  const targets = [...new Set([...gatePids, ...sshdChildren])];
  if (requireExisting) {
    assert.ok(
      targets.length > 0,
      `Expected an active node-side session process for ${NODE_HOME}, found none`,
    );
  }
  for (const pid of targets) {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      // Already exited.
    }
  }
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (findThrowawayGatePids().length === 0 && findSshdChildPids().length === 0) {
      break;
    }
    await sleep(100);
  }
  log(`Killed ${targets.length} node-side session process(es): ${targets.join(', ') || 'none'}`);
  return targets.length;
}

async function pairViaUi(page, invite, deviceName) {
  const nameInput = page.locator('label.field:has-text("Name this device") input');
  await nameInput.waitFor({ state: 'visible', timeout: 15000 });
  await nameInput.fill(deviceName);

  const inviteTextarea = page.locator('textarea[placeholder*="invite"]');
  if (!(await inviteTextarea.isVisible().catch(() => false))) {
    await page.getByRole('button', { name: 'Paste invite' }).click();
    await inviteTextarea.waitFor({ state: 'visible', timeout: 10000 });
  }
  await inviteTextarea.fill(invite);
  await inviteTextarea.blur();
  await sleep(300);
  const pairBtn = page.getByRole('button', { name: 'Pair device' });
  await pairBtn.scrollIntoViewIfNeeded();
  await pairBtn.click();
}

async function waitForTowerWithRealNumbers(page, timeoutMs = 30000) {
  await page.locator('.start-heading').waitFor({ state: 'detached', timeout: timeoutMs }).catch(() => {});
  const towerCard = page.locator('a.machine', { hasText: 'tower' }).first();
  await towerCard.waitFor({ state: 'visible', timeout: timeoutMs });
  // Wait until live telemetry arrives for tower (labelled Processor / Memory / Disk stats).
  const stats = towerCard.locator('.stats');
  await stats.waitFor({ state: 'visible', timeout: timeoutMs });
  const text = await stats.innerText();
  assert.match(text, /Processor[\s\S]*\d+%/, `Unexpected stats text: ${text}`);
  assert.match(text, /Memory[\s\S]*\d+%/, `Unexpected stats text: ${text}`);
  assert.match(text, /Disk[\s\S]*\d+%/, `Unexpected stats text: ${text}`);
  return towerCard;
}

async function runPhoneSuite() {
  let conn = await connectWebView();
  let page = conn.page;

  // 1. First start shows the pairing screen.
  log('Step 1: verifying first-start pairing screen');
  await page
    .getByRole('heading', { name: /Pair this device/i })
    .waitFor({ state: 'visible', timeout: 20000 });
  await page.getByRole('button', { name: 'Scan the code' }).waitFor({ state: 'visible' });
  await page.getByRole('button', { name: 'Paste invite' }).waitFor({ state: 'visible' });
  await takeShot(page, '01-pairing.png');

  // 2. Paste invite + device name pairs; Machines lists tower with labelled numbers.
  log('Step 2: pairing via pasted invite');
  const invite1 = createInvite();
  await pairViaUi(page, invite1, 'pixel-e2e');
  const towerCard = await waitForTowerWithRealNumbers(page);
  assert.equal(await page.locator('nav.nav a').count(), 3, 'Expected 3 root bottom nav items');
  await takeShot(page, '02-paired-machines.png');

  // 3. Machine -> More -> Command runs `echo hello-from-phone` with docked bottom prompt.
  log('Step 3: running echo hello-from-phone in Command');
  await towerCard.click();
  assert.equal(await page.locator('nav.nav.parts a').count(), 4, 'Expected 4 machine part items');
  await page.locator('nav.nav.parts a', { hasText: 'More' }).click();
  await page.locator('a.row', { hasText: 'Command' }).click();
  const cmdInput = page.locator('.dock input[aria-label="Command"]');
  await cmdInput.waitFor({ state: 'visible', timeout: 10000 });
  await cmdInput.fill('echo hello-from-phone');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.locator('pre.out', { hasText: 'hello-from-phone' }).waitFor({
    state: 'visible',
    timeout: 20000,
  });
  await page.waitForFunction(
    () => (document.body.innerText || '').includes('Finished'),
    null,
    { timeout: 15000 },
  );
  await takeShot(page, '03-command-output.png');

  // 4. Files part lists a directory.
  log('Step 4: verifying Files part directory listing');
  await page.locator('nav.nav.parts a', { hasText: 'Files' }).click();
  await page.locator('.row', { hasText: 'hello.txt' }).waitFor({
    state: 'visible',
    timeout: 15000,
  });
  await page.locator('.row', { hasText: 'subdir/' }).waitFor({
    state: 'visible',
    timeout: 10000,
  });
  await takeShot(page, '04-files.png');

  // 5. Android system Back steps back in WebView history; then return to Machines -> Settings -> Technical details -> Actions -> All tools -> meta.whoami.
  log('Step 5: verifying Android system Back, enabling Technical details in Settings, and running meta.whoami');
  adb(['shell', 'input', 'keyevent', '4']);
  await page.locator('.dock input[aria-label="Command"]').waitFor({ state: 'visible', timeout: 10000 });
  await page.locator('header.top button[aria-label="Back"]').click();
  await page.locator('header.top button[aria-label="Back"]').click();
  await page.locator('header.top a[aria-label="Settings"]').waitFor({ state: 'visible', timeout: 10000 });
  await page.locator('header.top a[aria-label="Settings"]').click();
  await page.locator('header.top h1', { hasText: 'Settings' }).waitFor({ state: 'visible', timeout: 10000 });
  await takeShot(page, '05a-settings.png');
  await page.getByLabel(/Technical details/i).check();
  await page.locator('header.top button[aria-label="Back"]').click();
  await page.locator('nav.nav a', { hasText: 'Actions' }).click();
  await page.locator('.tile[href="#/actions/restart"]').waitFor({ state: 'visible', timeout: 10000 });
  await takeShot(page, '05b-actions.png');
  await page.locator('a.row', { hasText: 'All tools' }).click();
  const filterInput = page.locator('input[type="search"]').first();
  if (await filterInput.isVisible().catch(() => false)) {
    await filterInput.fill('meta.whoami');
  }
  await page.locator('.row', { hasText: 'meta.whoami' }).first().click();
  await page.getByRole('button', { name: /^Run$/ }).click();
  const resultKv = page.locator('dl.kv').first();
  await resultKv.waitFor({ state: 'visible', timeout: 15000 });
  const whoamiText = await resultKv.innerText();
  assert.match(whoamiText, /principal\s+phone/);
  assert.match(whoamiText, /role\s+operator/);
  const doorMatch = whoamiText.match(/\bdoor\s+(\S+)/);
  assert.ok(doorMatch, `Expected door line in meta.whoami result:\n${whoamiText}`);
  const reportedDoor = doorMatch[1];
  assert.ok(
    reportedDoor === 'ssh' || reportedDoor === 'rpc',
    `Expected door ssh or rpc, got ${reportedDoor}`,
  );
  await takeShot(page, '05-tools-whoami.png');

  // 6. Power part asks first and only the dry run is performed.
  log('Step 6: checking power tool confirmation and dry run');
  await page.evaluate(() => {
    window.location.hash = '#/machines/tower/power';
  });
  const sleepRow = page.locator('.row', { hasText: /Sleep tower/i }).first();
  await sleepRow.waitFor({ state: 'visible', timeout: 15000 });
  await sleepRow.getByRole('button', { name: 'Sleep' }).click();
  const dialog = page.locator('[role="dialog"]');
  await dialog.waitFor({ state: 'visible', timeout: 10000 });
  const dryBtn = dialog.getByRole('button', { name: /Try it \(dry run\)/i });
  await dryBtn.waitFor({ state: 'visible', timeout: 10000 });
  await dryBtn.click();
  await dialog.locator('pre.out').waitFor({ state: 'visible', timeout: 15000 });
  const dryText = await dialog.locator('pre.out').innerText();
  assert.ok(dryText.trim().length > 0, 'Expected non-empty dry run output');
  await takeShot(page, '06-power-dryrun.png');
  await dialog.getByRole('button', { name: 'Cancel' }).click();
  await dialog.waitFor({ state: 'detached', timeout: 10000 });

  // 7. Force-stop and relaunch: still paired, no pairing prompt.
  log('Step 7: force-stopping and relaunching the app');
  await conn.close();
  adb(['shell', 'am', 'force-stop', APP_ID]);
  await sleep(1000);
  adb(['shell', 'am', 'start', '-W', '-n', ACTIVITY]);
  conn = await connectWebView();
  page = conn.page;
  const relaunchedTower = await waitForTowerWithRealNumbers(page);
  assert.equal(
    await page.locator('.start-heading').count(),
    0,
    'Pairing screen should not appear after force-stop and relaunch',
  );
  await takeShot(page, '07-after-relaunch.png');

  // 8. Kill the node-side session process, call again: reconnects transparently.
  log('Step 8: killing node-side session process and verifying transparent reconnect');
  await relaunchedTower.click();
  await page.locator('nav.nav.parts a', { hasText: 'More' }).click();
  await page.locator('a.row', { hasText: 'Command' }).click();
  const runInput = page.locator('.dock input[aria-label="Command"]');
  await runInput.waitFor({ state: 'visible', timeout: 10000 });
  await runInput.fill('echo before-kill');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.locator('pre.out', { hasText: 'before-kill' }).waitFor({
    state: 'visible',
    timeout: 15000,
  });

  await killNodeSessions({ requireExisting: true });

  await runInput.fill('echo after-reconnect');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.locator('pre.out', { hasText: 'after-reconnect' }).waitFor({
    state: 'visible',
    timeout: 20000,
  });
  await takeShot(page, '08-after-reconnect.png');

  // 9. Revoke the device on the node; next call shows calm error with way back to pairing.
  log('Step 9: revoking device on node and checking calm error + recovery');
  const removeOut = legionNode(['device.remove', 'name=pixel-e2e']);
  log(`device.remove output: ${removeOut.trim().replace(/\s+/g, ' ')}`);
  await killNodeSessions({ requireExisting: false });

  await runInput.fill('echo after-revoke');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  const pairAgainBtn = page.getByRole('button', { name: 'Pair again' });
  await pairAgainBtn.waitFor({ state: 'visible', timeout: 20000 });
  const noticeText = await page.locator('.notice').first().innerText();
  assert.ok(noticeText.length > 0, 'Expected calm error notice after revocation');
  await takeShot(page, '09-revoked-error.png');

  // 10. Pair again with a fresh invite and verify calls work again.
  log('Step 10: pairing again with a fresh invite');
  await pairAgainBtn.click();
  await page
    .getByRole('heading', { name: /Pair this device/i })
    .waitFor({ state: 'visible', timeout: 15000 });
  const invite2 = createInvite();
  await pairViaUi(page, invite2, 'pixel-e2e-2');
  const towerAfterRepair = await waitForTowerWithRealNumbers(page);
  await towerAfterRepair.click();
  await page.locator('nav.nav.parts a', { hasText: 'More' }).click();
  await page.locator('a.row', { hasText: 'Command' }).click();
  await page.locator('.dock input[aria-label="Command"]').fill('echo repaired-ok');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.locator('pre.out', { hasText: 'repaired-ok' }).waitFor({
    state: 'visible',
    timeout: 20000,
  });
  await takeShot(page, '10-repaired.png');

  await conn.close();
  log('All 10 phone e2e assertions passed.');
}

async function runTabletSuite() {
  const conn = await connectWebView();
  const page = conn.page;

  log('Step 11: pairing on tablet AVD for wide paired screenshot');
  await page
    .getByRole('heading', { name: /Pair this device/i })
    .waitFor({ state: 'visible', timeout: 20000 });
  const invite = createInvite();
  await pairViaUi(page, invite, 'tablet-e2e');
  await waitForTowerWithRealNumbers(page);
  await page.waitForFunction(
    () => {
      const text = document.body.innerText || '';
      return /health/i.test(text) && text.includes('Processor') && text.includes('Memory') && text.includes('tower');
    },
    null,
    { timeout: 20000 },
  );
  await takeShot(page, '11-tablet-paired.png');

  await conn.close();
  log('Tablet wide paired assertion passed.');
}

async function main() {
  if (MODE === 'phone') {
    await runPhoneSuite();
  } else if (MODE === 'tablet') {
    await runTabletSuite();
  } else {
    throw new Error(`Unknown E2E_MODE: ${MODE}`);
  }
}

main().catch(async (err) => {
  console.error(`[e2e:${MODE}] FAILED:`, err);
  if (activePage) {
    try {
      const url = activePage.url();
      const text = await activePage.locator('body').innerText();
      console.error(`[e2e:${MODE}] Page URL: ${url}`);
      console.error(`[e2e:${MODE}] Page text:\n${text}`);
      await takeShot(activePage, `99-failure-${MODE}.png`);
      const afterText = await activePage.locator('body').innerText().catch(() => '');
      console.error(`[e2e:${MODE}] Page text after screenshot:\n${afterText}`);
      const logcat = adb(['logcat', '-d', '-t', '40']).trim();
      console.error(`[e2e:${MODE}] logcat tail:\n${logcat}`);
    } catch (diagErr) {
      console.error(`[e2e:${MODE}] Could not capture failure diagnostics:`, diagErr.message);
    }
  }
  if (SSHD_LOG && fs.existsSync(SSHD_LOG)) {
    const sshdTail = fs.readFileSync(SSHD_LOG, 'utf8').split('\n').slice(-40).join('\n');
    console.error(`[e2e:${MODE}] sshd.log tail:\n${sshdTail}`);
  }
  process.exit(1);
});
