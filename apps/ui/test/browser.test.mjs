// End-to-end browser test for the Legion v2 web interface served by a real `legion serve`.
// Drives headless Chromium/Chrome at 390x844 (phone) and 1100x720 (wide) against a real two-node
// fleet reached through a local `command = [...]` pipe, verifying zero console/CSP errors.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';
import { chromium } from 'playwright-core';
import './files-browser.mjs';
import './live-refresh-browser.mjs';
import './machine-browser.mjs';
import './player-browser.mjs';
import './split-view-browser.mjs';
import './ui-craft-browser.mjs';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(TEST_DIR, '..', '..', '..');
const LEGION_BIN =
  process.env.LEGION_BIN ||
  path.join(REPO_ROOT, 'target', 'release', process.platform === 'win32' ? 'legion.exe' : 'legion');
const SHOTS_DIR = process.env.LEGION_UI_SHOTS_DIR || '';

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

function makeWorkRoot() {
  const base =
    process.env.LEGION_TEST_CACHE ||
    (process.env.RUNNER_TEMP
      ? path.join(process.env.RUNNER_TEMP, 'legion-ui-test')
      : path.join(os.homedir(), '.cache', 'legion-ui-test'));
  fs.mkdirSync(base, { recursive: true });
  return fs.mkdtempSync(path.join(base, 'run-'));
}

function runLegion(args, legionHome, userHome) {
  const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  return execFileSync(LEGION_BIN, args, { env, encoding: 'utf8' });
}

async function startServer(legionHome, userHome, options = {}) {
  const { listen = '127.0.0.1:0', envExtra = {} } = options;
  const env = {
    ...process.env,
    ...envExtra,
    LEGION_HOME: legionHome,
    HOME: userHome,
    USERPROFILE: userHome,
  };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  const child = spawn(LEGION_BIN, ['serve', '--listen', listen], {
    env,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`legion serve did not announce its port. stderr:\n${stderr}`));
    }, 15_000);
    const rl = readline.createInterface({ input: child.stderr });
    rl.on('line', (line) => {
      stderr += `${line}\n`;
      const prefix = 'legion serve: listening on ';
      if (line.startsWith(prefix)) {
        clearTimeout(timer);
        resolve(line.slice(prefix.length).trim());
      }
    });
    child.once('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`legion serve exited early (${code}). stderr:\n${stderr}`));
    });
  });

  async function stop() {
    if (child.exitCode !== null) return;
    child.kill('SIGTERM');
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        child.kill('SIGKILL');
        resolve();
      }, 8000);
      child.once('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
  }

  return { url, stop, getStderr: () => stderr };
}

function setupFleet(root) {
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const piHome = path.join(root, 'pi-legion');
  const piUser = path.join(root, 'pi-user');
  const sampleDir = path.join(towerUser, 'docs');

  for (const dir of [
    path.join(towerHome, 'config'),
    path.join(towerHome, 'state'),
    sampleDir,
    path.join(piHome, 'config'),
    path.join(piHome, 'state'),
    piUser,
  ]) {
    fs.mkdirSync(dir, { recursive: true });
  }

  fs.writeFileSync(
    path.join(sampleDir, 'hello.txt'),
    'Hello from Legion real server preview.\nSecond line of text.\n',
  );

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

  const probeCmd = [process.execPath, '-e', "console.log('1239887 worker'); process.exit(0)"];
  fs.writeFileSync(
    path.join(piHome, 'config', 'node.toml'),
    [
      'name = "pi"',
      'system = "linux"',
      '',
      '[[busy.probes]]',
      'name = "agents"',
      `run = ${JSON.stringify(probeCmd)}`,
      '',
    ].join('\n'),
  );
  fs.writeFileSync(path.join(piHome, 'config', 'policy.toml'), policy);

  const helperCmd = [process.execPath, path.join(TEST_DIR, 'rpc-helper.mjs'), LEGION_BIN, piHome, piUser];
  fs.writeFileSync(
    path.join(towerHome, 'config', 'fleet.toml'),
    `[machines.tower]\nssh = "tower.example"\n\n[machines.pi]\ncommand = ${JSON.stringify(helperCmd)}\n`,
  );

  const tokenOut = JSON.parse(
    runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser),
  );
  assert.ok(tokenOut.token && tokenOut.token.startsWith('lgn_'));

  return { towerHome, towerUser, piHome, piUser, token: tokenOut.token };
}

async function setTheme(page, theme) {
  await page.evaluate((t) => {
    const raw = localStorage.getItem('legion.settings.v1');
    const obj = raw ? JSON.parse(raw) : {};
    obj.theme = t;
    localStorage.setItem('legion.settings.v1', JSON.stringify(obj));
    document.documentElement.dataset.theme = t;
  }, theme);
  await page.waitForTimeout(80);
}

async function dispatchSwipe(page, selector, { startX, startY, endX, endY }) {
  await page.evaluate(
    ({ sel, sx, sy, ex, ey }) => {
      const target = document.querySelector(sel);
      if (!target) throw new Error(`Swipe target not found: ${sel}`);
      const opts = (x, y) => ({
        bubbles: true,
        cancelable: true,
        pointerId: 1,
        pointerType: 'touch',
        button: 0,
        clientX: x,
        clientY: y,
      });
      target.dispatchEvent(new PointerEvent('pointerdown', opts(sx, sy)));
      target.dispatchEvent(new PointerEvent('pointermove', opts((sx + ex) / 2, (sy + ey) / 2)));
      target.dispatchEvent(new PointerEvent('pointerup', opts(ex, ey)));
    },
    { sel: selector, sx: startX, sy: startY, ex: endX, ey: endY },
  );
  await page.waitForTimeout(120);
}

const CATALOG_TOOL_RE =
  /\b(?:power|fs|fav|sync|exec|job|svc|proj|fleet|sys|meta|device|token|config|net|env)\.[a-z_]+\b/g;

async function assertNoToolNames(page, label) {
  const text = await page.locator('body').innerText();
  const matches = [...new Set(text.match(CATALOG_TOOL_RE) || [])];
  assert.deepEqual(matches, [], `Tool name(s) leaked in plain mode (${label}): ${matches.join(', ')}\n---\n${text}`);
}

async function maybeShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}.png`) });
}

test('real legion serve v2 UI end-to-end on phone (390x844) and desktop (1100x720)', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const fleet = setupFleet(root);
  const server = await startServer(fleet.towerHome, fleet.towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    // ── Phone viewport (390x844) ───────────────────────────────────────────────────────────────
    const phoneCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
    });
    const page = await phoneCtx.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    await page.goto(server.url, { waitUntil: 'domcontentloaded' });
    await page.getByRole('heading', { name: /Connect this browser to tower/ }).waitFor();
    assert.equal(await page.locator('.wordmark').count(), 0, 'No wordmark in header');
    await maybeShot(page, 'real-phone-dark-start');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-start');
    await setTheme(page, 'dark');

    // Enter token and connect.
    await page.getByText('What a token allows').waitFor();
    await page.getByLabel('Token', { exact: true }).fill(fleet.token);
    await page.getByRole('button', { name: 'Connect' }).click();

    // Machines appear with labelled Processor / Memory / Storage, no wordmark/role chip/refresh button.
    await page.getByText('2 are on').waitFor({ timeout: 15_000 });
    assert.equal(await page.locator('.wordmark').count(), 0);
    assert.equal(await page.locator('header.top .chip').count(), 0);
    assert.equal(await page.locator('button[aria-label="Refresh"]').count(), 0);
    await page.locator('header.top a[aria-label="Settings"]').waitFor();
    assert.equal(await page.locator('nav.nav a').count(), 4, 'Root bottom bar has 4 places');

    const towerCard = page.locator('a.machine', { hasText: 'tower' });
    await towerCard.waitFor();
    await towerCard.getByText('Processor').waitFor();
    await towerCard.getByText('Memory').waitFor();
    await towerCard.getByText('Storage').waitFor();

    const piCard = page.locator('a.machine', { hasText: 'pi' });
    await piCard.waitFor();
    await piCard.getByText(/Busy · agents/).waitFor({ timeout: 5_000 });
    const piCardText = await piCard.innerText();
    assert.ok(!piCardText.includes('[object Object]'), `raw object in pi card: ${piCardText}`);
    assert.ok(!piCardText.includes('1239887'), `raw probe detail leaked into card: ${piCardText}`);
    await maybeShot(page, 'real-phone-dark-machines');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-machines');
    await setTheme(page, 'dark');

    // Pull-to-refresh gesture on Machines list works without error.
    await dispatchSwipe(page, 'main.main', { startX: 195, startY: 160, endX: 195, endY: 280 });
    await page.getByText('2 are on').waitFor({ timeout: 10_000 });

    // Horizontal swipe across root places: Machines -> Files -> Activity -> Actions -> Activity -> Files -> Machines.
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Actions' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 100, startY: 360, endX: 300, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 100, startY: 360, endX: 300, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 100, startY: 360, endX: 300, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Machines' }).waitFor();

    // Open busy machine (pi) -> bottom nav becomes 4 machine parts (Overview, Files, Power, More).
    await piCard.click();
    assert.equal(await page.locator('nav.nav.parts a').count(), 4);
    await page.getByText('agents: busy: 1239887 worker').waitFor({ timeout: 5_000 });
    await assertNoToolNames(page, 'real phone pi overview');
    await page.locator('nav.nav.parts a', { hasText: 'Power' }).click();
    await page.getByText('Loading power state…').waitFor({ state: 'hidden' });
    await page.getByText('agents: busy: 1239887 worker').waitFor({ timeout: 5_000 });
    await assertNoToolNames(page, 'real phone pi power');

    // Dry-run refusal on busy pi in plain mode: neutral notice, plain sentence, collapsed "More", no commands.
    await page.getByRole('button', { name: 'Sleep', exact: true }).click();
    const piDialog = page.getByRole('dialog');
    await piDialog.waitFor();
    await piDialog.getByRole('button', { name: 'Try it (dry run)' }).click();
    await piDialog.locator('.notice.neutral', { hasText: 'It would refuse right now:' }).waitFor({ timeout: 10_000 });
    assert.equal(await piDialog.locator('.notice.bad').count(), 0, 'Dry-run refusal must use neutral notice, not red error');
    assert.equal(await piDialog.locator('pre.out').count(), 0, 'Plain dry-run refusal must not show raw commands');
    const moreDetails = piDialog.locator('details.more');
    await moreDetails.waitFor();
    assert.equal(await moreDetails.getAttribute('open'), null, 'Dry-run refusal details should start collapsed');
    await assertNoToolNames(page, 'real phone pi dry-run refusal sheet');
    await maybeShot(page, 'real-phone-dark-power-refusal-plain');
    await piDialog.getByRole('button', { name: 'Cancel' }).click();
    await piDialog.waitFor({ state: 'hidden' });

    // Back to Machines and open tower.
    await page.locator('header.top button[aria-label="Back"]').click();
    await page.locator('a.machine', { hasText: 'tower' }).click();
    await page.getByText('Health').waitFor();
    assert.equal(
      await page.locator('.older-note').count(),
      0,
      'Current real node must not show an older-Legion note on Machine Overview',
    );
    await assertNoToolNames(page, 'real phone tower overview');
    await maybeShot(page, 'real-phone-dark-machine-overview');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-machine-overview');
    await setTheme(page, 'dark');

    // Horizontal swipe across machine parts: Overview -> Files; opens same Files browser at tower.
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 362 });
    await page.locator('nav.nav.parts a[aria-current="page"]', { hasText: 'Files' }).waitFor();
    await page.locator('.list a.row', { hasText: 'docs' }).waitFor();
    assert.equal(
      await page.locator('.older-note').count(),
      0,
      'Current real node must not show an older-Legion note at Files root',
    );
    await page.locator('.list a.row', { hasText: 'docs' }).click();
    await page.locator('.list a.row', { hasText: 'hello.txt' }).click();
    await page.locator('.viewer pre.viewer-text').getByText('Hello from Legion real server preview.').waitFor();
    await assertNoToolNames(page, 'real phone tower files');
    await maybeShot(page, 'real-phone-dark-machine-files');
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // Real node omits move capability on fs.transfer: Send to another machine offers only Copy.
    await page.locator('.list a.row', { hasText: 'hello.txt' }).click();
    await page.locator('.viewer').waitFor();
    await page.locator('.viewer-bar button', { hasText: 'Send to' }).click();
    const realSendSheet = page.locator('.sheet');
    await realSendSheet.waitFor();
    await page.waitForFunction(
      () => document.activeElement?.classList?.contains('sheet'),
      null,
      { timeout: 3000 },
    );
    await realSendSheet.getByRole('button', { name: 'Copy to pi' }).waitFor();
    assert.equal(
      await realSendSheet.getByRole('button', { name: 'Move', exact: true }).count(),
      0,
      'Real node without move capability must offer only Copy when sending to another machine',
    );
    await page.keyboard.press('Escape');
    await realSendSheet.waitFor({ state: 'hidden' });
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.goto(`${server.url}/#/machines/tower/power`, { waitUntil: 'domcontentloaded' });
    await page.getByText('Loading power state…').waitFor({ state: 'hidden' });
    await page.getByRole('button', { name: 'Sleep', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Restart', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Shut down', exact: true }).waitFor();
    await assertNoToolNames(page, 'real phone tower power');
    await maybeShot(page, 'real-phone-dark-machine-power');

    await page.getByRole('button', { name: 'Sleep', exact: true }).click();
    const towerPlainDialog = page.getByRole('dialog');
    await towerPlainDialog.waitFor();
    await towerPlainDialog.getByRole('button', { name: 'Try it (dry run)' }).click();
    await towerPlainDialog
      .locator('.notice.neutral', { hasText: /tower would go to sleep\.|It would refuse right now:/ })
      .waitFor({ timeout: 10_000 });
    assert.equal(await towerPlainDialog.locator('.notice.bad').count(), 0, 'Plain dry run must not use red error style');
    assert.equal(await towerPlainDialog.locator('pre.out').count(), 0, 'Plain dry run must not show raw commands');
    assert.ok(!(await towerPlainDialog.innerText()).includes('Dry run commands'));
    await assertNoToolNames(page, 'real phone tower plain dry-run sheet');
    await maybeShot(page, 'real-phone-dark-power-sheet-plain');
    await towerPlainDialog.getByRole('button', { name: 'Cancel' }).click();
    await towerPlainDialog.waitFor({ state: 'hidden' });

    // More -> Command (docked at bottom with "Keep running after I leave" + "Name it").
    await page.locator('nav.nav.parts a', { hasText: 'More' }).click();
    await assertNoToolNames(page, 'real phone tower more');
    await page.locator('a.row', { hasText: 'Command' }).click();
    await page.locator('.dock input[aria-label="Command"]').waitFor();
    await page.locator('.dock input[aria-label="Command"]').fill('echo hello-stream-one; sleep 0.15; echo hello-stream-two');
    await page.locator('.dock button[type="submit"]').click();
    await page.locator('pre.out').getByText('Finished').waitFor({ timeout: 10_000 });
    const runOut = await page.locator('pre.out').innerText();
    assert.match(runOut, /\nhello-stream-one\n/);
    assert.match(runOut, /\nhello-stream-two/);
    assert.match(runOut, /Finished/);
    await maybeShot(page, 'real-phone-dark-machine-command');

    // Start a named background job using "Keep running after I leave" + "Name it".
    await page.getByLabel(/Keep running after I leave/i).check();
    await page.getByLabel('Name for background task').fill('Stream check');
    await page.locator('.dock input[aria-label="Command"]').fill('echo job-live-line; sleep 30');
    await page.locator('.dock button[type="submit"]').click();
    await page.getByRole('button', { name: 'Follow output' }).click();
    await page.getByText('Stream check').first().waitFor({ timeout: 10_000 });
    // In plain mode, "Show what it printed" starts collapsed and expands on click.
    const outputDisclosure = page.locator('details.more', { hasText: 'Show what it printed' });
    await outputDisclosure.waitFor();
    assert.equal(await outputDisclosure.getAttribute('open'), null);
    await outputDisclosure.locator('summary').click();
    await page.locator('pre.out').getByText('job-live-line').waitFor({ timeout: 10_000 });
    await maybeShot(page, 'real-phone-dark-activity-detail');
    await page.getByRole('button', { name: 'Stop', exact: true }).click();
    await page.locator('.state', { hasText: 'Stopped' }).first().waitFor({ timeout: 10_000 });

    // Start an unnamed background job to verify raw commands/exit codes stay hidden in plain Activity.
    await page.goto(`${server.url}/#/machines/tower/command`, { waitUntil: 'domcontentloaded' });
    await page.getByLabel(/Keep running after I leave/i).check();
    await page.getByLabel('Name for background task').fill('');
    await page.locator('.dock input[aria-label="Command"]').fill('echo secret-shell-cmd-token');
    await page.locator('.dock button[type="submit"]').click();
    await page.locator('.notice', { hasText: 'Started Command' }).waitFor({ timeout: 10_000 });

    // Activity screen in plain mode: shows "Stream check" and "Command", NO raw shell command, NO exit 0.
    await page.goto(`${server.url}/#/activity`, { waitUntil: 'domcontentloaded' });
    await page.getByText('Stream check').first().waitFor({ timeout: 10_000 });
    await page.getByText('Command').first().waitFor({ timeout: 10_000 });
    const plainActivityText = await page.locator('main.main').innerText();
    assert.ok(!plainActivityText.includes('secret-shell-cmd-token'), `Raw command leaked in plain Activity: ${plainActivityText}`);
    assert.ok(!plainActivityText.includes('job-live-line'), `Raw command leaked in plain Activity: ${plainActivityText}`);
    assert.ok(!/\bexit \d+\b/i.test(plainActivityText), `Exit code leaked in plain Activity: ${plainActivityText}`);
    await assertNoToolNames(page, 'real phone plain activity');
    await maybeShot(page, 'real-phone-dark-activity-plain');

    // Actions screen: tiles + bottom-docked search, NO All tools while Technical details is off.
    await page.locator('nav.nav a', { hasText: 'Actions' }).click();
    await page.locator('.dock input[aria-label="Find something to do"]').waitFor();
    await page.locator('.tile[href="#/actions/restart"]').waitFor();
    assert.equal(await page.locator('a.row', { hasText: 'All tools' }).count(), 0);
    await assertNoToolNames(page, 'real phone plain actions');
    await maybeShot(page, 'real-phone-dark-actions');

    // Settings via top-right gear: turn Technical details on, inspect Appearance live, check This browser.
    await page.locator('header.top a[aria-label="Settings"]').click();
    await page.locator('header.top h1', { hasText: 'Settings' }).waitFor();
    await maybeShot(page, 'real-phone-dark-settings');

    // Left-edge swipe back from Settings works on non-Android browsers.
    await dispatchSwipe(page, 'main.main', { startX: 12, startY: 360, endX: 180, endY: 362 });
    await page.locator('header.top h1', { hasText: 'Machines' }).waitFor();

    // Open Settings -> Appearance and verify live theme/accent/text changes without reload.
    await page.locator('header.top a[aria-label="Settings"]').click();
    await page.locator('a.row', { hasText: 'Appearance' }).click();
    await page.getByRole('button', { name: 'Blue' }).click();
    assert.equal(await page.evaluate(() => document.documentElement.dataset.accent), 'blue');
    await page.getByRole('button', { name: 'Large' }).click();
    assert.equal(await page.evaluate(() => document.documentElement.dataset.text), 'large');
    await maybeShot(page, 'real-phone-dark-appearance');
    await page.getByRole('button', { name: 'Green' }).click();
    await page.getByRole('button', { name: 'Medium' }).click();

    // Back to Settings index, enable Technical details.
    await page.locator('header.top button[aria-label="Back"]').click();
    await page.getByLabel(/Technical details/i).check();

    // Activity in technical mode NOW shows raw command and exit 0.
    await page.goto(`${server.url}/#/activity`, { waitUntil: 'domcontentloaded' });
    await page.getByText('echo secret-shell-cmd-token').first().waitFor({ timeout: 10_000 });
    await page.locator('.state', { hasText: 'exit 0' }).first().waitFor({ timeout: 10_000 });
    await maybeShot(page, 'real-phone-dark-activity-tech');

    // Actions in technical mode shows All tools -> fan-out meta.version & power.restart dry-run + sheet.
    await page.locator('nav.nav a', { hasText: 'Actions' }).click();
    await page.locator('a.row', { hasText: 'All tools' }).click();
    await page.getByLabel('Search tools').fill('token.add');
    await page.locator('a.row.off', { hasText: 'token.add' }).waitFor();
    await page.getByLabel('Search tools').fill('meta.version');
    await page.locator('a.row', { hasText: 'meta.version' }).first().click();
    await page.locator('#tool-target-on').selectOption('all');
    await page.getByRole('button', { name: 'Run', exact: true }).click();
    await page.locator('.label', { hasText: /^pi$/i }).waitFor({ timeout: 10_000 });
    await page.locator('.label', { hasText: /^tower$/i }).waitFor();

    await page.goto(`${server.url}/#/actions/all/power.restart`, { waitUntil: 'domcontentloaded' });
    await page.getByRole('button', { name: 'Try it (dry run)' }).click();
    await page.locator('dl.kv').getByText('restart').first().waitFor({ timeout: 10_000 });
    await page.getByRole('button', { name: 'Restart tower', exact: true }).click();
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    await dialog.getByRole('heading', { name: 'Restart tower now?' }).waitFor();
    await maybeShot(page, 'real-phone-dark-power-sheet');
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await dialog.waitFor({ state: 'hidden' });

    const powerStatus = JSON.parse(
      runLegion(['power.status', '--json'], fleet.towerHome, fleet.towerUser),
    );
    assert.equal(powerStatus.pending, undefined);

    // Settings -> This browser: Forget this browser opens calm confirmation sheet.
    await page.goto(`${server.url}/#/settings/device`, { waitUntil: 'domcontentloaded' });
    const forgetBtn = page.getByRole('button', { name: 'Forget this browser' });
    await forgetBtn.waitFor();
    assert.ok(!(await forgetBtn.getAttribute('class')).includes('danger'));
    await forgetBtn.click();
    const forgetDialog = page.getByRole('dialog');
    await forgetDialog.waitFor();
    await forgetDialog.getByRole('button', { name: 'Cancel' }).click();
    await forgetDialog.waitFor({ state: 'hidden' });

    await phoneCtx.close();

    // ── Wide desktop viewport (1100x720) ───────────────────────────────────────────────────────
    const wideCtx = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });
    const wide = await wideCtx.newPage();
    wide.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    wide.on('pageerror', (err) => pageErrors.push(String(err)));

    // One-time #token= URL fragment login removes the token from the address bar immediately.
    await wide.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await wide.locator('.split-left .split-label-fact', { hasText: /^2$/ }).waitFor({ timeout: 15_000 });
    assert.ok(!wide.url().includes(fleet.token), `token leaked in URL: ${wide.url()}`);

    // Rail has 3 root places + Settings at the foot.
    await wide.locator('nav.nav a.last', { hasText: 'Settings' }).waitFor();
    await wide.locator('.split h2.pane-title', { hasText: 'tower' }).waitFor();
    await maybeShot(wide, 'real-wide-dark-machines');
    await setTheme(wide, 'light');
    await maybeShot(wide, 'real-wide-light-machines');
    await setTheme(wide, 'dark');

    await wide.locator('nav.nav a', { hasText: 'Activity' }).click();
    await wide.getByText('Stream check').first().waitFor({ timeout: 10_000 });
    await maybeShot(wide, 'real-wide-dark-activity');

    await wide.locator('nav.nav a', { hasText: 'Actions' }).click();
    await wide.locator('.tile[href="#/actions/restart"]').waitFor();
    await maybeShot(wide, 'real-wide-dark-actions');

    await wide.locator('nav.nav a.last', { hasText: 'Settings' }).click();
    await wide.getByText('Forget this browser').waitFor();
    const paneWidth = await wide.locator('.settings-pane').evaluate((el) => el.getBoundingClientRect().width);
    assert.ok(paneWidth <= 561, `Expected wide Settings reading column <= 560px, got ${paneWidth}`);
    await maybeShot(wide, 'real-wide-dark-settings');

    await wideCtx.close();

    // ── Mock mode checks (v2 full suite: off vs last-on, plain vs tech, QR invite, limited, multi)
    const mockViewports = SHOTS_DIR
      ? [
          ['phone', { width: 390, height: 844 }],
          ['wide', { width: 1100, height: 720 }],
        ]
      : [['phone', { width: 390, height: 844 }]];

    for (const [label, viewport] of mockViewports) {
      const ctx = await browser.newContext({ viewport, colorScheme: 'dark' });
      const p = await ctx.newPage();
      p.on('console', (msg) => {
        if (msg.type() === 'error') consoleErrors.push(msg.text());
      });
      p.on('pageerror', (err) => pageErrors.push(String(err)));

      // Default mock: 3 on, 2 off (laptop with "Last on 14 days ago" + Wake, nas with "Off" & no Wake).
      await p.goto(`${server.url}/?mock`, { waitUntil: 'domcontentloaded' });
      if (label === 'wide') {
        await p.locator('.split-left .split-label-text', { hasText: /^On$/i }).waitFor();
        await p.locator('.split-left .split-label-fact', { hasText: /^3$/ }).waitFor();
        await p.locator('.split-left .label-row', { hasText: 'Off' }).locator('.label-fact', { hasText: /^2$/ }).waitFor();
      } else {
        await p.getByText('3 are on, 2 are off').waitFor();
      }
      await p.locator('a.machine', { hasText: 'tower' }).getByText('Busy · 2 things running').waitFor();
      await p.locator('a.machine', { hasText: 'studio' }).getByText('Busy · agents').waitFor();
      await p.locator('a.machine', { hasText: 'pi' }).getByText('Idle').waitFor();
      const laptopRow = p.locator('.machine.off', { hasText: 'laptop' });
      await laptopRow.getByText(/Last on 14 days ago/).waitFor();
      await laptopRow.getByRole('button', { name: 'Wake' }).waitFor();
      const nasRow = p.locator('.machine.off', { hasText: 'nas' });
      await nasRow.getByText('Off').waitFor();
      assert.equal(await nasRow.getByRole('button', { name: 'Wake' }).count(), 0);
      await setTheme(p, 'dark');
      await assertNoToolNames(p, `mock ${label} machines`);
      await maybeShot(p, `mock-${label}-dark-machines`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-machines`);
        await setTheme(p, 'dark');
      }

      // Offline Wake button opens the shared wake sheet directly in the waking state.
      await laptopRow.getByRole('button', { name: 'Wake' }).click();
      const wakeDialog = p.getByRole('dialog');
      await wakeDialog.waitFor();
      await wakeDialog.getByRole('heading', { name: 'Waking laptop…' }).waitFor();
      await wakeDialog.getByText('The wake signal has already been sent.').waitFor();
      await assertNoToolNames(p, `mock ${label} wake sheet`);
      await wakeDialog.getByRole('button', { name: 'Stop waiting', exact: true }).click();
      await wakeDialog.waitFor({ state: 'hidden' });
      await p.locator('.machine.off', { hasText: 'laptop' }).getByText('Wake sent · waiting for it to answer').waitFor();
      assert.equal(
        await p.locator('.machine.off', { hasText: 'laptop' }).getByRole('button', { name: 'Wake' }).count(),
        0,
        'Wake button should hide while waiting for machine to answer',
      );
      await maybeShot(p, `mock-${label}-dark-wake-sent`);

      // Mock idle machine (pi) Power dry run in plain mode: one plain sentence, no commands.
      await p.goto(`${server.url}/?mock#/machines/pi/power`, { waitUntil: 'domcontentloaded' });
      await p.getByRole('button', { name: 'Sleep', exact: true }).click();
      const piSleepDialog = p.getByRole('dialog');
      await piSleepDialog.waitFor();
      await piSleepDialog.getByRole('button', { name: 'Try it (dry run)' }).click();
      await piSleepDialog.locator('.notice.neutral', { hasText: 'pi would go to sleep.' }).waitFor();
      assert.equal(await piSleepDialog.locator('pre.out').count(), 0, 'Plain dry run must not show commands');
      assert.ok(!(await piSleepDialog.innerText()).includes('Dry run commands'));
      await assertNoToolNames(p, `mock ${label} pi plain dry-run sheet`);
      await maybeShot(p, `mock-${label}-dark-power-sheet-plain`);
      await piSleepDialog.getByRole('button', { name: 'Cancel' }).click();
      await piSleepDialog.waitFor({ state: 'hidden' });

      // Activity plain vs technical + row swipe to reveal Stop on running item.
      await p.goto(`${server.url}/?mock#/activity`, { waitUntil: 'domcontentloaded' });
      await p.getByText('Photo backup').first().waitFor();
      await p.getByText('Tests for website').first().waitFor();
      await p.getByText('Restart').first().waitFor();
      const mockPlainAct = await p.locator('main.main').innerText();
      assert.ok(!mockPlainAct.includes('rsync -av'), `Raw command leaked: ${mockPlainAct}`);
      assert.ok(!mockPlainAct.includes('npm test'), `Raw command leaked: ${mockPlainAct}`);
      assert.ok(!/\bexit \d+\b/i.test(mockPlainAct), `Exit code leaked: ${mockPlainAct}`);
      await assertNoToolNames(p, `mock ${label} activity plain`);
      await maybeShot(p, `mock-${label}-dark-activity-plain`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-activity-plain`);
        await setTheme(p, 'dark');
      }

      // Swipe left on a running Activity row reveals Stop button.
      if (label === 'phone') {
        await dispatchSwipe(p, '.swipe-row', { startX: 310, startY: 200, endX: 160, endY: 202 });
        await p.locator('.swipe-action', { hasText: 'Stop' }).waitFor();
      }

      // Mock Actions + Which machine?
      await p.goto(`${server.url}/?mock#/actions`, { waitUntil: 'domcontentloaded' });
      await p.locator('.tile', { hasText: 'Sleep' }).waitFor();
      await assertNoToolNames(p, `mock ${label} actions plain`);
      await maybeShot(p, `mock-${label}-dark-actions`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-actions`);
        await setTheme(p, 'dark');
      }

      // Mock Connections (single machine): quiet second-machine hint above "Make a connection".
      await p.goto(`${server.url}/?mock#/settings/connections`, { waitUntil: 'domcontentloaded' });
      await p.getByText('Pair with a second machine so this device still works when tower is off.').waitFor();
      await assertNoToolNames(p, `mock ${label} connections single`);
      await maybeShot(p, `mock-${label}-dark-connections-single`);

      // Mock Connections (multi) + Let another device in (plain wording + square SVG QR code).
      await p.goto(`${server.url}/?mock=multi#/settings/connections`, { waitUntil: 'domcontentloaded' });
      await p.getByText('Paired machines').waitFor();
      await p.getByText('Devices that may come in').waitFor();
      await p.getByText('My phone').waitFor();
      assert.equal(await p.getByText(/Pair with a second machine so this device still works/).count(), 0);
      await maybeShot(p, `mock-${label}-dark-connections`);

      await p.locator('a.row', { hasText: 'Let another device in' }).click();
      await p.getByText('Use the identity').waitFor();
      const inviteFormText = await p.locator('main.main').innerText();
      assert.ok(!inviteFormText.includes('policy.toml'), `Jargon leaked in plain invite screen: ${inviteFormText}`);
      await p.getByRole('button', { name: 'Make invite' }).click();
      const qrSvg = p.locator('.qr-box svg path');
      await qrSvg.waitFor();
      const qrD = await qrSvg.getAttribute('d');
      assert.ok(qrD && qrD.length > 200, 'Expected non-trivial SVG QR path');
      const qrBoxWidth = await p.locator('.qr-box').evaluate((el) => el.getBoundingClientRect().width);
      assert.ok(qrBoxWidth <= 320, `Expected square QR box <= 320px wide, got ${qrBoxWidth}`);
      await p
        .getByText(
          'Works once, for the next 15 minutes. The device will be able to: do everything except change permissions.',
        )
        .waitFor();
      assert.ok(!(await p.locator('main.main').innerText()).includes('for phone (operator)'));
      await assertNoToolNames(p, `mock ${label} invite qr`);
      await maybeShot(p, `mock-${label}-dark-invite-qr`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-invite-qr`);
        await setTheme(p, 'dark');
      }

      // Mock Technical details mode.
      await p.goto(`${server.url}/?mock=tech#/activity`, { waitUntil: 'domcontentloaded' });
      await p.getByText('rsync -av /home/me/photos/ nas:/volume1/photos/').first().waitFor();
      await p.locator('.state', { hasText: 'exit 1' }).first().waitFor();
      await maybeShot(p, `mock-${label}-dark-activity-tech`);

      // Empty Activity state.
      await p.goto(`${server.url}/?mock=empty#/activity`, { waitUntil: 'domcontentloaded' });
      await p
        .getByText('Nothing has run yet. Things you start here or on a machine appear in this list.')
        .waitFor();
      await assertNoToolNames(p, `mock ${label} activity empty`);
      await maybeShot(p, `mock-${label}-dark-activity-empty`);

      // Mock limited role: groups with zero allowed actions (Power, Run) are hidden; remaining groups show cleanly.
      await p.goto(`${server.url}/?mock=limited#/actions`, { waitUntil: 'domcontentloaded' });
      await p.locator('.tile', { hasText: 'Browse' }).waitFor();
      assert.equal(await p.locator('.label', { hasText: /^Power$/i }).count(), 0, 'Power group should be hidden when none allowed');
      assert.equal(await p.locator('.label', { hasText: /^Run$/i }).count(), 0, 'Run group should be hidden when none allowed');
      await assertNoToolNames(p, `mock ${label} actions limited`);
      await maybeShot(p, `mock-${label}-dark-actions-limited`);

      // Mock role with zero actions allowed anywhere: one calm sentence.
      await p.goto(`${server.url}/?mock=none#/actions`, { waitUntil: 'domcontentloaded' });
      await p.getByText('This device is not allowed to run actions here.').waitFor();
      await assertNoToolNames(p, `mock ${label} actions empty`);
      await maybeShot(p, `mock-${label}-dark-actions-empty`);

      // Mock pairing start screen.
      await p.goto(`${server.url}/?mock=pair`, { waitUntil: 'domcontentloaded' });
      await p.getByRole('heading', { name: 'Pair this device' }).waitFor();
      await p.waitForFunction(
        () => {
          const el = document.querySelector('label.field input');
          return el && el.value === 'My phone';
        },
        null,
        { timeout: 5000 },
      );
      await maybeShot(p, `mock-${label}-dark-start-pair`);

      if (SHOTS_DIR) {
        await p.goto(`${server.url}/?mock#/machines/tower`, { waitUntil: 'domcontentloaded' });
        await p.getByText('Health').waitFor();
        await setTheme(p, 'dark');
        await maybeShot(p, `mock-${label}-dark-overview`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-overview`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/machines/tower/command`, { waitUntil: 'domcontentloaded' });
        await p.locator('input[aria-label="Command"]').fill('df -h /');
        await p.locator('form.prompt button[type="submit"]').click();
        await p.locator('pre.out').getByText('/dev/nvme0n1p5').waitFor();
        await maybeShot(p, `mock-${label}-dark-command`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-command`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/settings`, { waitUntil: 'domcontentloaded' });
        await p.getByText('Technical details').waitFor();
        await maybeShot(p, `mock-${label}-dark-settings`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-settings`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/settings/appearance`, { waitUntil: 'domcontentloaded' });
        await p.getByText('Accent').waitFor();
        await maybeShot(p, `mock-${label}-dark-appearance`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-appearance`);
      }

      await ctx.close();
    }

    assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('fast start with an offline dual-boot machine paints immediately and keeps known-off machines calm on reload', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const fleet = setupFleet(root);

  const hangCmd = [process.execPath, '-e', 'setInterval(() => {}, 60000)'];
  fs.appendFileSync(
    path.join(fleet.towerHome, 'config', 'fleet.toml'),
    [
      '',
      '[machines.dead]',
      '[[machines.dead.systems]]',
      'name = "linux"',
      `command = ${JSON.stringify(hangCmd)}`,
      '[[machines.dead.systems]]',
      'name = "windows"',
      `command = ${JSON.stringify(hangCmd)}`,
      '',
    ].join('\n'),
  );

  const server = await startServer(fleet.towerHome, fleet.towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    const ctx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
    });
    const page = await ctx.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    await page.goto(server.url, { waitUntil: 'domcontentloaded' });
    await page.getByLabel('Token', { exact: true }).fill(fleet.token);

    const tClick = performance.now();
    await page.getByRole('button', { name: 'Connect' }).click();

    // This node paints immediately (under 2s).
    await page.locator('a.machine', { hasText: 'tower' }).waitFor({ timeout: 2000 });
    const tTower = Math.round(performance.now() - tClick);
    assert.ok(tTower < 2000, `tower took ${tTower} ms to appear`);

    // Never-seen dead machine appears briefly as Checking… while pi answers on its own.
    await page.locator('.machine.asking', { hasText: 'dead' }).waitFor({ timeout: 2000 });
    await page.locator('a.machine', { hasText: 'pi' }).waitFor({ timeout: 2000 });

    // Full list settles to "2 are on, 1 is off" with dead calmly under Off.
    const deadRow = page.locator('.machine.off', { hasText: 'dead' });
    await deadRow.waitFor({ timeout: 10_000 });
    await page.getByText('2 are on, 1 is off').waitFor({ timeout: 5000 });
    const deadRowText = await deadRow.innerText();
    assert.match(deadRowText, /Off/);
    assert.ok(
      !deadRowText.includes("None of dead's systems answered"),
      `raw dual-boot error leaked into list row: ${deadRowText}`,
    );

    // Persistent cache stores non-secret machine metadata (no token).
    const cachedRaw = await page.evaluate(() => localStorage.getItem('legion.cache.fleet'));
    assert.ok(cachedRaw, 'expected legion.cache.fleet in localStorage');
    assert.ok(!cachedRaw.includes(fleet.token), 'token must never be written to fleet cache');
    const cached = JSON.parse(cachedRaw);
    assert.equal(cached.value.node, 'tower');
    assert.equal(cached.value.machines.length, 3);

    // Return visit keeps known-off `dead` calmly under Off (NEVER flips it back to Checking…).
    const tReload0 = performance.now();
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('.machine.off', { hasText: 'dead' }).waitFor({ timeout: 2000 });
    assert.equal(
      await page.locator('.machine.asking', { hasText: 'dead' }).count(),
      0,
      'Known-off machine must not flip back to asking/checking on reload',
    );
    const tReload = Math.round(performance.now() - tReload0);
    assert.ok(tReload < 2000, `Cached return paint took ${tReload} ms`);

    // Forgetting this browser via Settings -> This browser clears the persistent fleet cache too.
    await page.locator('header.top a[aria-label="Settings"]').click();
    await page.locator('a.row', { hasText: 'This browser' }).click();
    await page.getByRole('button', { name: 'Forget this browser' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    await dialog.getByRole('button', { name: 'Forget this browser' }).click();
    await page.getByRole('heading', { name: /Connect this browser to tower/ }).waitFor({ timeout: 5000 });
    const afterForget = await page.evaluate(() => localStorage.getItem('legion.cache.fleet'));
    assert.equal(afterForget, null, 'fleet cache should be cleared on forget');

    await ctx.close();
    assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('fleet connections routes, identity-change acceptance, pairing sync notice, and role invite delivery', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const fleet = setupFleet(root);
  const server = await startServer(fleet.towerHome, fleet.towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    const ctx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
    });
    const p = await ctx.newPage();
    p.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    p.on('pageerror', (err) => pageErrors.push(String(err)));

    // 1. Pairing from StartView shows plain fleet sync summary on Machines ONLY (not repeated on Connections).
    await p.goto(`${server.url}/?mock=pair`, { waitUntil: 'domcontentloaded' });
    await p.getByRole('button', { name: 'Paste invite' }).click();
    await p.locator('textarea').fill('legion-invite-1.mock.test');
    await p.getByRole('button', { name: 'Pair device' }).click();
    await p.getByText('Paired with tower and shared with pi.').waitFor({ timeout: 5000 });
    await p.getByText('studio did not answer and will get this device at the next sync.').waitFor();
    await assertNoToolNames(p, 'post-pair sync summary plain');

    await p.locator('header.top a[aria-label="Settings"]').click();
    await p.locator('a.row', { hasText: 'Connections' }).click();
    await p.getByText('Paired machines').waitFor();
    assert.equal(
      await p.getByText('Paired with tower and shared with pi.').count(),
      0,
      'Pairing sync notice must appear only on the screen the user landed on, not repeated on Connections',
    );

    // 2. Settings -> Connections shows direct, relay, unsynced (plain hint), and Review-only
    //    identity-changed link (NEVER an inline Accept button on Connections list!).
    await p.goto(`${server.url}/?mock=fleet,enrol#/settings/connections`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Paired machines').waitFor();
    await p.locator('a.row[href="#/settings/connections/node/pi"]').getByText('directly').waitFor();
    await p.locator('a.row[href="#/settings/connections/node/studio"]').getByText('not yet: tower has not passed this device on to it').waitFor();
    await p.locator('a.row[href="#/settings/connections/node/studio"]').getByText('Ask the owner to sync the machines').waitFor();
    await p.getByText('Reached through another machine').waitFor();
    await p.locator('.row', { hasText: 'nas' }).getByText('through tower').waitFor();
    await p.getByText('Identity changed for mini').waitFor();
    assert.equal(
      await p.getByRole('button', { name: 'Accept new identity' }).count(),
      0,
      'Connections list must never show an inline Accept button',
    );
    // "Devices that may come in" in plain mode shows plain access wording, NEVER `device:...`.
    const connectionsText = await p.locator('main.main').innerText();
    assert.ok(
      !connectionsText.includes('device:my-phone'),
      `Raw device principal leaked in plain Connections: ${connectionsText}`,
    );
    await assertNoToolNames(p, 'fleet connections plain');

    // Open a learned machine's connection page (`pi`) in plain mode and assert zero tool names.
    await p.locator('a.row[href="#/settings/connections/node/pi"]').click();
    await p.getByText('Learned from').waitFor();
    await p.getByText('tower').first().waitFor();
    await assertNoToolNames(p, 'learned machine connection page plain');
    await p.locator('header.top button[aria-label="Back"]').click();
    await p.getByText('Identity changed for mini').waitFor();

    // Open identity-changed machine (`mini`) via Review: verify old & new fingerprints AND changed
    // address/port are displayed BEFORE Accept new identity, then accept cleanly.
    await p.getByRole('link', { name: 'Review' }).first().click();
    await p.getByText('Previous fingerprint').waitFor();
    await p.getByText('SHA256:oldMiniKey1111111111111111111111111111111').waitFor();
    await p.getByText('Reported fingerprint').waitFor();
    await p.getByText('SHA256:newMiniKey2222222222222222222222222222222').waitFor();
    await p.getByText('mini.example.net').waitFor();
    await p.getByText('mini-new.example.net').waitFor();
    await p.getByText('2222', { exact: true }).waitFor();
    await assertNoToolNames(p, 'identity-changed connection page plain');

    await p.getByRole('button', { name: 'Accept new identity' }).click();
    await p.getByText('Identity changed for mini').waitFor({ state: 'hidden' });
    await p.getByText('directly').waitFor();

    // Paired machine with an identity-change report is informational only (no Accept button).
    await p.goto(`${server.url}/?mock=fleet,pairedchg#/settings/connections/node/tower`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('Identity changed for tower').waitFor();
    assert.equal(
      await p.getByRole('button', { name: 'Accept new identity' }).count(),
      0,
      'Directly paired machine must never offer Accept new identity',
    );
    await p.getByRole('button', { name: 'Dismiss' }).waitFor();
    await assertNoToolNames(p, 'paired identity-changed page plain');

    // Conflicting reports on a learned machine block Accept new identity.
    await p.goto(`${server.url}/?mock=fleet,conflict#/settings/connections/node/mini`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('Conflicting reports').waitFor();
    assert.equal(
      await p.getByRole('button', { name: 'Accept new identity' }).count(),
      0,
      'Conflicting reports must block Accept new identity',
    );
    await assertNoToolNames(p, 'conflicting identity-changed page plain');

    // 3. Technical mode shows the exact fleet.sync hint for unsynced machines.
    await p.goto(`${server.url}/?mock=fleet,tech#/settings/connections`, { waitUntil: 'domcontentloaded' });
    await p.locator('a.row', { hasText: 'studio' }).getByText('legion fleet.sync').waitFor();

    // 4. "Let another device in" — primary `device.roles` path: offers node-supplied role
    //    descriptions, owner-strength switch, and reports fleet delivery.
    await p.goto(`${server.url}/?mock=fleet,enrol#/settings/connections/invite`, { waitUntil: 'domcontentloaded' });
    const roleSelect = p.locator('label.field select').last();
    await roleSelect.waitFor();
    await p.getByText('Name for this device').waitFor();
    await p.getByText('Look only (read status and files, change nothing).').waitFor();
    await assertNoToolNames(p, 'fleet invite plain observer via device.roles');

    // Selecting owner reveals the Allow full owner access switch and disables Make invite until ticked.
    await p.locator('label.field input[type="text"]').fill('tablet');
    await roleSelect.selectOption('owner');
    await p.getByText('Allow full owner access').waitFor();
    const makeBtn = p.getByRole('button', { name: 'Make invite' });
    assert.equal(await makeBtn.isDisabled(), true, 'Owner invite must stay disabled until Allow full owner access is ticked');
    await p.locator('label.row-switch input[type="checkbox"]').check();
    assert.equal(await makeBtn.isDisabled(), false);

    // Switch back to operator and create invite; auto-enrol completes and shows fleet delivery.
    await roleSelect.selectOption('operator');
    await p
      .getByText('Operate (read, change files, and run commands — as strong as this account).')
      .waitFor();
    await makeBtn.click();
    await p.getByText('tablet is paired.').waitFor({ timeout: 5000 });
    await p.getByText('Received by pi and tower. laptop, nas and studio will get it at the next sync.').waitFor();
    await assertNoToolNames(p, 'fleet invite paired delivery plain');

    // 5. Fallback policy.toml path (`noroles`): still loads roles and plain descriptions from live policy.
    await p.goto(`${server.url}/?mock=fleet,noroles#/settings/connections/invite`, {
      waitUntil: 'domcontentloaded',
    });
    const fallbackSelect = p.locator('label.field select').last();
    await fallbackSelect.waitFor();
    await p.getByText('Look only (read status and files, change nothing).').waitFor();
    await assertNoToolNames(p, 'fleet invite fallback policy.toml plain');

    // 6. Refusal path (`noroles,denyread`): says plainly this device may not see the roles.
    await p.goto(`${server.url}/?mock=fleet,noroles,denyread#/settings/connections/invite`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('This device may not see the roles on tower; ask the owner.').waitFor();
    await assertNoToolNames(p, 'fleet invite policy refused plain');

    // 6b. Switching to a target whose tool catalogue cannot be read (`mini`) says so instead of
    //     falling back to another machine's catalogue.
    const machineSelect = p.locator('label.field:has-text("Machine") select');
    await machineSelect.selectOption('mini');
    await p.getByText('Could not read what mini supports right now.').waitFor();
    assert.equal(
      await p.getByRole('button', { name: 'Make invite' }).count(),
      0,
      'Invite form must not fall back to another machine catalogue when target catalogue fails',
    );
    await assertNoToolNames(p, 'fleet invite target catalogue error plain');

    // 7. Unreachable selected node (`aoff`): keeps selected node (`tower`) shown as unreachable
    //    with its last known role marked "as of <when>", never adopting answering peer (`pi`).
    await p.goto(`${server.url}/?mock=fleet,aoff#/settings/device`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('tower is unreachable right now.').waitFor();
    await p.getByText(/as of /i).first().waitFor();
    await p.getByText('tower (unreachable)').waitFor();
    await assertNoToolNames(p, 'offline selected node device page plain');

    // 8. Real introducer disagreement (`disagree`): plain wording in Connections, with per-reporter
    //    host/port/fingerprint detail in technical mode.
    await p.goto(`${server.url}/?mock=fleet,disagree#/settings/connections`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('tower and pi describe studio differently; not connecting to it directly').waitFor();
    await assertNoToolNames(p, 'fleet connections disagreement plain');
    await p.goto(`${server.url}/?mock=fleet,disagree,tech#/settings/connections`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText('tower and pi describe studio differently; not connecting to it directly').waitFor();
    await p.getByText(/SHA256:studioTowerKey1111/).waitFor();
    await p.getByText(/SHA256:studioPiKey2222/).waitFor();

    // 8b. Relayed machine (`nas`) with a learn rejection (`reject`): plain mode shows only one
    //     short line ("Through tower") and hides the rejection reason; Technical details reveals
    //     the learn rejection reason on that machine's row.
    await p.goto(`${server.url}/?mock=fleet,reject#/settings/connections`, {
      waitUntil: 'domcontentloaded',
    });
    await p.locator('.row', { hasText: 'nas' }).getByText('Through tower').waitFor();
    const plainRejectText = await p.locator('main.main').innerText();
    assert.ok(
      !plainRejectText.includes('name_mismatch') && !plainRejectText.includes('someones-nas'),
      `Learn rejection reason must stay hidden when Technical details is off:\n${plainRejectText}`,
    );
    await p.goto(`${server.url}/?mock=fleet,reject,tech#/settings/connections`, {
      waitUntil: 'domcontentloaded',
    });
    await p.getByText(/Direct route skipped \(name_mismatch\): nas calls itself someones-nas\./).waitFor();

    // 9. Effectful tool run from Tools with target "all" expands to the explicit list of machine
    //    names shown in the confirmation sheet and sends that exact array (never literal "all").
    await p.goto(`${server.url}/?mock=fleet,tech#/actions/all/power.restart`, {
      waitUntil: 'domcontentloaded',
    });
    const onSelect = p.locator('#tool-target-on');
    await onSelect.waitFor();
    await onSelect.selectOption('all');
    await p.getByRole('button', { name: 'Restart tower, pi, studio' }).click();
    await p.locator('.sheet').getByText('Restart tower, pi, studio now?').waitFor();
    await p.locator('.sheet .btn.danger').click();
    await p.locator('.sheet').waitFor({ state: 'hidden' });
    const lastCall = await p.evaluate(() => window.__LEGION_MOCK_LAST_CALL__);
    assert.equal(lastCall.tool, 'power.restart');
    assert.deepEqual(lastCall.on, ['tower', 'pi', 'studio']);

    // 10. Settings -> About -> Updates in browser door explains the browser stays as new as the server.
    await p.goto(`${server.url}/?mock=basic#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Updates', { exact: true }).waitFor();
    await p.getByText('Always up to date').waitFor();
    await p.getByText(/browser interface is always as new as tower/i).waitFor();

    // 11. Native app mode (`update`): plain mode hides channel/sequence/key, shows Update now & notes;
    //     Technical details reveals channel, sequence, key fingerprint, and source machine.
    await p.goto(`${server.url}/?mock=multi,update#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Version 0.2.0 is ready').waitFor();
    await p.getByRole('button', { name: 'Update now' }).waitFor();
    await p.getByText('Update automatically').waitFor();
    await p.getByText('Automatic fleet updates and faster status refresh.').waitFor();
    const plainAboutText = await p.locator('main.main').innerText();
    assert.ok(!plainAboutText.includes('SHA256:MockPreviewKey'), 'Key fingerprint must be hidden when Technical details is off');
    await assertNoToolNames(p, 'about updates plain');

    await p.goto(`${server.url}/?mock=multi,update,tech#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Update details').waitFor();
    await p.getByText('preview + stable').waitFor();
    await p.getByText(/SHA256:MockPreviewKey/).waitFor();

    // 12. Linux .deb mode (`update-deb`): never offers Update now, shows exact apt-get command
    //     and verified private copy SHA-256.
    await p.goto(`${server.url}/?mock=multi,update-deb#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Version 0.2.0 is available').waitFor();
    await p.getByText(/sudo apt-get install /).waitFor();
    await p.getByText(/SHA-256: a{64}/).waitFor();
    assert.equal(await p.getByRole('button', { name: 'Update now' }).count(), 0);

    // 12b. Failed update (`update-failed`): plain mode hides raw error detail and offers Try again;
    //      Technical details exposes raw error detail.
    await p.goto(`${server.url}/?mock=multi,update-failed#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Version 0.2.0 did not finish installing.').waitFor();
    await p.getByRole('button', { name: 'Try again' }).waitFor();
    assert.ok(
      !(await p.locator('main.main').innerText()).includes('INSTALL_FAILED_VERIFICATION_FAILURE'),
      'Raw installer error must stay hidden outside Technical details',
    );
    await p.goto(`${server.url}/?mock=multi,update-failed,tech#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('INSTALL_FAILED_VERIFICATION_FAILURE').waitFor();

    // 12c. Metered pause (`update-metered`) and sequence-0 build (`update-seq0`).
    await p.goto(`${server.url}/?mock=multi,update-metered#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText(/metered connection/i).waitFor();
    await p.getByRole('button', { name: 'Update now' }).waitFor();

    await p.goto(`${server.url}/?mock=multi,update-seq0,tech#/settings/about`, { waitUntil: 'domcontentloaded' });
    await p.getByText('Updates disabled in this build').waitFor();
    await p.getByText('0 (updates disabled)').waitFor();

    // 13. Quiet unbordered one-time "Updated to 0.2.0 · What is new" notice on first screen, sheet, and dismiss.
    await p.goto(`${server.url}/?mock=multi,updated#/machines`, { waitUntil: 'domcontentloaded' });
    const updateRow = p.locator('.update-note');
    await updateRow.waitFor();
    assert.equal(await updateRow.locator('.btn').count(), 0, 'Post-update row must not contain bordered buttons');
    assert.equal(await p.locator('nav.nav a').count(), 4, 'Root bottom bar has 4 tabs alongside update notice');
    await maybeShot(p, 'mock-phone-dark-updated');
    await updateRow.locator('.update-note-link').click();
    await p.locator('.sheet').getByText('What is new in 0.2.0').waitFor();
    await p.locator('.sheet').getByText('Automatic fleet updates and faster status refresh.').waitFor();
    await p.locator('.sheet').getByRole('button', { name: 'Done' }).click();
    await p.locator('.update-note').waitFor({ state: 'hidden' });

    await ctx.close();
    assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('content URLs isolate HTML and SVG from the interface origin and allow <video> seeking', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const sampleVideo = path.join(TEST_DIR, 'fixtures', 'sample.webm');
  assert.ok(fs.existsSync(sampleVideo), `Sample video fixture not found at ${sampleVideo}`);

  const root = makeWorkRoot();
  const fleet = setupFleet(root);
  const docsDir = path.join(fleet.towerUser, 'docs');

  // Craft adversarial HTML and SVG files that try to read localStorage (token), mutate storage,
  // access window.parent, and postMessage back to the interface origin.
  fs.writeFileSync(
    path.join(docsDir, 'xss.html'),
    [
      '<!doctype html>',
      '<html><body><script>',
      'window.__xss = true;',
      'try { localStorage.setItem("pwn_html", localStorage.getItem("legion.token") || "ran"); } catch (e) {}',
      'try { window.parent.postMessage({ pwn: "html", token: localStorage.getItem("legion.token") }, "*"); } catch (e) {}',
      'try { window.parent.__pwned_html = true; } catch (e) {}',
      '</script><p>malicious html</p></body></html>',
    ].join('\n'),
  );
  fs.writeFileSync(
    path.join(docsDir, 'xss.svg'),
    [
      '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" onload="window.__xss=true;try{localStorage.setItem(\'pwn_svg\',\'1\')}catch(e){}try{window.parent.__pwned_svg=true}catch(e){}try{window.parent.postMessage({pwn:\'svg\'},\'*\')}catch(e){}">',
      '<rect width="64" height="64" fill="#16a34a"/>',
      '<script>window.__xss=true;try{localStorage.setItem("pwn_svg_script",localStorage.getItem("legion.token")||"ran")}catch(e){}try{window.parent.__pwned_svg=true}catch(e){}try{window.parent.postMessage({pwn:"svg-script"},"*")}catch(e){}</script>',
      '</svg>',
    ].join('\n'),
  );
  fs.writeFileSync(
    path.join(docsDir, 'doc.pdf'),
    '%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n%%EOF\n',
  );
  fs.copyFileSync(sampleVideo, path.join(docsDir, 'sample.webm'));

  const server = await startServer(fleet.towerHome, fleet.towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const uiConsoleErrors = [];
  const uiPageErrors = [];

  try {
    const ctx = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });

    // 1. Connect the real Legion interface page so localStorage holds the live bearer token.
    const uiPage = await ctx.newPage();
    uiPage.on('console', (msg) => {
      if (msg.type() === 'error') uiConsoleErrors.push(msg.text());
    });
    uiPage.on('pageerror', (err) => uiPageErrors.push(String(err)));

    await uiPage.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await uiPage.locator('.split-left .split-label-fact', { hasText: /^2$/ }).waitFor({ timeout: 15_000 });
    const storedToken = await uiPage.evaluate(() => localStorage.getItem('legion.token'));
    assert.equal(storedToken, fleet.token, 'expected live token in interface localStorage');

    // Mint content URLs through the UI's own createHttpLink().contentUrl helper.
    const urls = await uiPage.evaluate(async () => {
      const { createHttpLink } = await import('/link/http.js');
      const link = createHttpLink();
      const [html, svg, pdf, video] = await Promise.all([
        link.contentUrl({ path: '~/docs/xss.html' }),
        link.contentUrl({ path: '~/docs/xss.svg' }),
        link.contentUrl({ path: '~/docs/doc.pdf' }),
        link.contentUrl({ path: '~/docs/sample.webm' }),
      ]);
      return { html, svg, pdf, video };
    });
    assert.match(urls.html.url, /^\/v1\/content\/[^/]+\/xss\.html$/);
    assert.match(urls.svg.url, /^\/v1\/content\/[^/]+\/xss\.svg$/);
    assert.match(urls.pdf.url, /^\/v1\/content\/[^/]+\/doc\.pdf$/);
    assert.match(urls.video.url, /^\/v1\/content\/[^/]+\/sample\.webm$/);

    // 2. Verify <video> seeking and <img> loading directly on the real Legion UI page
    //    (proving the UI CSP's `img-src 'self'` and `media-src 'self'` allow them with zero errors).
    const rangeStatuses = [];
    uiPage.on('response', (res) => {
      if (res.url().endsWith('/sample.webm')) {
        rangeStatuses.push(res.status());
      }
    });

    const mediaResult = await uiPage.evaluate(async ({ videoUrl, svgUrl }) => {
      const img = document.createElement('img');
      img.src = svgUrl;
      document.body.appendChild(img);
      await new Promise((resolve, reject) => {
        img.addEventListener('load', resolve, { once: true });
        img.addEventListener('error', () => reject(new Error('img failed to load')), { once: true });
      });

      const video = document.createElement('video');
      video.muted = true;
      video.preload = 'auto';
      video.src = videoUrl;
      document.body.appendChild(video);

      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for loadeddata')), 10_000);
        video.addEventListener(
          'loadeddata',
          () => {
            clearTimeout(timer);
            resolve();
          },
          { once: true },
        );
        video.addEventListener(
          'error',
          () => {
            clearTimeout(timer);
            reject(new Error(`video error code=${video.error?.code}`));
          },
          { once: true },
        );
      });

      const initialDuration = video.duration;
      const initialWidth = video.videoWidth;
      const initialHeight = video.videoHeight;

      const seekDone = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for seeked')), 10_000);
        video.addEventListener(
          'seeked',
          () => {
            clearTimeout(timer);
            resolve();
          },
          { once: true },
        );
        video.addEventListener(
          'error',
          () => {
            clearTimeout(timer);
            reject(new Error(`video seek error code=${video.error?.code}`));
          },
          { once: true },
        );
      });
      video.currentTime = 1.5;
      await seekDone;

      return {
        imgLoaded: img.complete && img.naturalWidth > 0,
        duration: initialDuration,
        width: initialWidth,
        height: initialHeight,
        currentTime: video.currentTime,
        hasError: video.error !== null,
      };
    }, { videoUrl: urls.video.url, svgUrl: urls.svg.url });

    assert.equal(mediaResult.imgLoaded, true, 'expected SVG <img> to load under UI CSP');
    assert.ok(mediaResult.duration > 2.0, `expected >2s video duration, got ${mediaResult.duration}`);
    assert.equal(mediaResult.width, 64);
    assert.equal(mediaResult.height, 64);
    assert.ok(
      mediaResult.currentTime >= 1.2 && mediaResult.currentTime <= 2.0,
      `expected seeked currentTime near 1.5, got ${mediaResult.currentTime}`,
    );
    assert.equal(mediaResult.hasError, false);
    assert.ok(
      rangeStatuses.includes(206),
      `expected at least one 206 Partial Content response for MP4 video, got ${JSON.stringify(rangeStatuses)}`,
    );

    assert.deepEqual(uiPageErrors, [], `UI page errors during media test: ${uiPageErrors.join('\n')}`);
    assert.deepEqual(uiConsoleErrors, [], `UI console/CSP errors during media test: ${uiConsoleErrors.join('\n')}`);

    // 3. Top-level navigation to .html and .svg content URLs: neither may run script or reach
    //    the interface's origin, storage, or token; PDF is always forced to download as
    //    application/octet-stream with the same sandbox + frame-ancestors 'none' CSP.
    const expectedCsp = "sandbox; default-src 'none'; frame-ancestors 'none'";
    for (const [kind, relUrl, expectedType] of [
      ['html', urls.html.url, 'text/plain; charset=utf-8'],
      ['svg', urls.svg.url, 'image/svg+xml'],
    ]) {
      const navPage = await ctx.newPage();
      const resp = await navPage.goto(`${server.url}${relUrl}`, { waitUntil: 'load' });
      assert.equal(resp.status(), 200, `${kind} navigation status`);
      assert.equal(resp.headers()['content-type'], expectedType, `${kind} content-type`);
      assert.equal(
        resp.headers()['content-security-policy'],
        expectedCsp,
        `${kind} CSP must sandbox, forbid script, and forbid framing`,
      );
      assert.equal(resp.headers()['x-content-type-options'], 'nosniff');
      await navPage.waitForTimeout(150);

      const probe = await navPage.evaluate(() => {
        let storageBlocked = false;
        let leakedToken = null;
        try {
          leakedToken = localStorage.getItem('legion.token');
        } catch {
          storageBlocked = true;
        }
        return {
          origin: window.origin,
          xssFlag: Boolean(window.__xss),
          storageBlocked,
          leakedToken,
        };
      });
      assert.equal(probe.origin, 'null', `${kind} navigation must have opaque sandbox origin`);
      assert.equal(probe.xssFlag, false, `${kind} navigation must not execute script`);
      assert.equal(probe.storageBlocked, true, `${kind} navigation must be denied localStorage`);
      assert.equal(probe.leakedToken, null, `${kind} navigation must never read token`);
      await navPage.close();
    }

    // PDF is never served inline: octet-stream + attachment + unconditional sandbox CSP.
    const pdfHeaders = await uiPage.evaluate(async (pdfUrl) => {
      const res = await fetch(pdfUrl);
      return {
        status: res.status,
        contentType: res.headers.get('content-type'),
        disposition: res.headers.get('content-disposition'),
        csp: res.headers.get('content-security-policy'),
        corp: res.headers.get('cross-origin-resource-policy'),
      };
    }, urls.pdf.url);
    assert.equal(pdfHeaders.status, 200);
    assert.equal(pdfHeaders.contentType, 'application/octet-stream');
    assert.equal(pdfHeaders.disposition, 'attachment; filename="doc.pdf"');
    assert.equal(pdfHeaders.csp, expectedCsp);
    assert.equal(pdfHeaders.corp, 'same-origin');

    // 4. Inside <iframe>, <object>, and <embed> on a same-origin host page (sharing the
    //    interface's origin and token): frame-ancestors 'none' + sandbox CSP prevent framing
    //    or script execution in every embedding vector.
    const hostPage = await ctx.newPage();
    await hostPage.route(`${server.url}/__test-iframe-host`, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: [
          '<!doctype html><html><body>',
          '<script>window.__messages = []; window.addEventListener("message", (e) => window.__messages.push(e.data));</script>',
          `<iframe id="f-html" src="${urls.html.url}"></iframe>`,
          `<iframe id="f-svg" src="${urls.svg.url}"></iframe>`,
          `<object id="o-svg" data="${urls.svg.url}" type="image/svg+xml"></object>`,
          `<embed id="e-svg" src="${urls.svg.url}" type="image/svg+xml">`,
          '</body></html>',
        ].join('\n'),
      });
    });

    const embedResponses = new Map();
    hostPage.on('response', (res) => {
      if (res.url().includes('/v1/content/')) {
        const key = new URL(res.url()).pathname;
        embedResponses.set(key, (embedResponses.get(key) || 0) + 1);
      }
    });

    await hostPage.goto(`${server.url}/__test-iframe-host`, { waitUntil: 'load' });
    await hostPage.waitForTimeout(250);

    assert.ok((embedResponses.get(urls.html.url) || 0) >= 1, 'HTML iframe must actually fetch from legion serve');
    assert.ok((embedResponses.get(urls.svg.url) || 0) >= 1, 'SVG embeds must actually fetch from legion serve');

    const embedCheck = await hostPage.evaluate(() => {
      const checkIsolated = (id) => {
        const el = document.getElementById(id);
        let domBlocked = false;
        try {
          const doc = el.contentDocument;
          // frame-ancestors 'none' or opaque sandbox leaves contentDocument null or empty error doc.
          domBlocked =
            doc === null ||
            doc.URL === 'about:blank' ||
            doc.URL === 'chrome-error://chromewebdata/' ||
            !doc.body ||
            doc.body.innerText === '';
        } catch {
          domBlocked = true;
        }
        return domBlocked;
      };
      return {
        htmlIsolated: checkIsolated('f-html'),
        svgFrameIsolated: checkIsolated('f-svg'),
        svgObjectIsolated: checkIsolated('o-svg'),
        pwnHtmlProp: Boolean(window.__pwned_html),
        pwnSvgProp: Boolean(window.__pwned_svg),
        messages: window.__messages,
        storageKeys: Object.keys(localStorage),
      };
    });

    assert.equal(embedCheck.htmlIsolated, true, 'HTML iframe must be isolated/blocked from parent origin');
    assert.equal(embedCheck.svgFrameIsolated, true, 'SVG iframe must be isolated/blocked from parent origin');
    assert.equal(embedCheck.svgObjectIsolated, true, 'SVG <object> must be isolated/blocked from parent origin');
    assert.equal(embedCheck.pwnHtmlProp, false, 'HTML iframe must not mutate parent');
    assert.equal(embedCheck.pwnSvgProp, false, 'SVG iframe/object/embed must not mutate parent');
    assert.deepEqual(embedCheck.messages, [], 'Embedded content must not run script or postMessage');
    assert.ok(!embedCheck.storageKeys.includes('pwn_html'), 'HTML must not write localStorage');
    assert.ok(!embedCheck.storageKeys.includes('pwn_svg'), 'SVG onload must not write localStorage');
    assert.ok(!embedCheck.storageKeys.includes('pwn_svg_script'), 'SVG script must not write localStorage');

    await hostPage.close();
    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('real Files tab opens pictures and plays/seeks video on local and remote nodes, with graceful older-node fallback', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const piHome = path.join(root, 'pi-legion');
  const piUser = path.join(root, 'pi-user');
  const legacyHome = path.join(root, 'legacy-legion');
  const legacyUser = path.join(root, 'legacy-user');

  const samplePng = fs.readFileSync(path.join(REPO_ROOT, 'apps', 'ui', 'icons', 'icon-512.png'));
  const sampleWebm = fs.readFileSync(path.join(TEST_DIR, 'fixtures', 'sample.webm'));

  for (const [home, user] of [
    [towerHome, towerUser],
    [piHome, piUser],
    [legacyHome, legacyUser],
  ]) {
    fs.mkdirSync(path.join(home, 'config'), { recursive: true });
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    const mediaDir = path.join(user, 'media');
    fs.mkdirSync(mediaDir, { recursive: true });
    fs.writeFileSync(path.join(mediaDir, 'icon.png'), samplePng);
    fs.writeFileSync(path.join(mediaDir, 'sample.webm'), sampleWebm);
  }

  const policy = [
    'default = "owner"',
    '',
    '[principals.ui]',
    'role = "observer"',
    '',
    '[roles.observer]',
    'allow = ["effect:read"]',
    '',
  ].join('\n');

  for (const [name, home] of [
    ['tower', towerHome],
    ['pi', piHome],
    ['legacy', legacyHome],
  ]) {
    fs.writeFileSync(path.join(home, 'config', 'node.toml'), `name = "${name}"\nsystem = "linux"\n`);
    fs.writeFileSync(path.join(home, 'config', 'policy.toml'), policy);
  }

  const piCmd = [process.execPath, path.join(TEST_DIR, 'rpc-helper.mjs'), LEGION_BIN, piHome, piUser, 'rpc'];
  const legacyCmd = [
    process.execPath,
    path.join(TEST_DIR, 'legacy-node-helper.mjs'),
    LEGION_BIN,
    legacyHome,
    legacyUser,
    'rpc',
  ];
  fs.writeFileSync(
    path.join(towerHome, 'config', 'fleet.toml'),
    [
      '[machines.tower]',
      'ssh = "tower.example"',
      '',
      '[machines.pi]',
      `command = ${JSON.stringify(piCmd)}`,
      '',
      '[machines.legacy]',
      `command = ${JSON.stringify(legacyCmd)}`,
      '',
    ].join('\n'),
  );

  const tokenOut = JSON.parse(
    runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser),
  );
  const server = await startServer(towerHome, towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  try {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
    const page = await ctx.newPage();
    const consoleErrors = [];
    const pageErrors = [];
    const contentPostsByMachine = new Map();
    const contentGetStatuses = [];

    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));
    page.on('request', (req) => {
      if (req.method() === 'POST' && new URL(req.url()).pathname === '/v1/content') {
        try {
          const parsed = JSON.parse(req.postData() || '{}');
          const m = parsed.on || 'tower';
          contentPostsByMachine.set(m, (contentPostsByMachine.get(m) || 0) + 1);
        } catch {
          // Ignore parse issues.
        }
      }
    });
    page.on('response', (res) => {
      if (res.request().method() === 'GET' && new URL(res.url()).pathname.startsWith('/v1/content/')) {
        contentGetStatuses.push(res.status());
      }
    });

    // Connect to the real UI (no ?mock) with a real bearer token.
    await page.goto(`${server.url}/#token=${encodeURIComponent(tokenOut.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await page.getByText('3 are on').waitFor({ timeout: 15_000 });

    const entryLocator = (name) =>
      page.locator(`a.cell[aria-label="${name}"]`).or(page.locator('a.row', { hasText: name }));

    async function openMachineMedia(machine) {
      await page.locator('nav.nav a', { hasText: 'Files' }).click();
      await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
      await page.locator('.list a.row', { hasText: machine }).first().click();
      await entryLocator('media').waitFor({ timeout: 10_000 });
      await entryLocator('media').click();
      await entryLocator('icon.png').waitFor({ timeout: 10_000 });
    }

    async function verifyPictureAndVideo(machine) {
      contentGetStatuses.length = 0;
      await openMachineMedia(machine);

      // Picture opens through real /v1/content and decodes cleanly.
      await entryLocator('icon.png').click();
      const img = page.locator('.viewer img.viewer-img');
      await img.waitFor({ timeout: 10_000 });
      await page.waitForFunction(
        () => {
          const el = document.querySelector('.viewer img.viewer-img');
          return Boolean(el && el.complete && el.naturalWidth > 0);
        },
        null,
        { timeout: 10_000 },
      );
      const imgInfo = await img.evaluate((el) => ({
        src: el.getAttribute('src') || '',
        w: el.naturalWidth,
        h: el.naturalHeight,
      }));
      assert.ok(imgInfo.src.startsWith('/v1/content/'), `${machine}: expected /v1/content img src, got ${imgInfo.src}`);
      assert.equal(imgInfo.w, 512, `${machine}: expected 512px PNG width`);
      assert.equal(imgInfo.h, 512, `${machine}: expected 512px PNG height`);
      await page.locator('.viewer-top button[aria-label="Close"]').click();
      await page.locator('.viewer').waitFor({ state: 'hidden' });

      // Video opens through real /v1/content, decodes VP8 WebM, and seeks cleanly with 206 ranges.
      await entryLocator('sample.webm').click();
      const video = page.locator('.viewer video.viewer-video');
      await video.waitFor({ timeout: 10_000 });
      const videoCheck = await video.evaluate(async (el) => {
        el.muted = true;
        if (el.readyState < 2) {
          el.preload = 'auto';
          el.load();
          await new Promise((resolve, reject) => {
            const t = setTimeout(() => reject(new Error('timed out waiting for loadeddata')), 10_000);
            el.addEventListener('loadeddata', () => { clearTimeout(t); resolve(); }, { once: true });
            el.addEventListener('error', () => { clearTimeout(t); reject(new Error(`video error ${el.error?.code}`)); }, { once: true });
          });
        }
        try {
          await el.play();
          el.pause();
        } catch {
          // Headless autoplay policy may still allow seeking without play.
        }
        const seeked = new Promise((resolve, reject) => {
          const t = setTimeout(() => reject(new Error('timed out waiting for seeked')), 10_000);
          el.addEventListener('seeked', () => { clearTimeout(t); resolve(); }, { once: true });
          el.addEventListener('error', () => { clearTimeout(t); reject(new Error(`seek error ${el.error?.code}`)); }, { once: true });
        });
        el.currentTime = 1.5;
        await seeked;
        return {
          src: el.getAttribute('src') || '',
          readyState: el.readyState,
          duration: el.duration,
          width: el.videoWidth,
          height: el.videoHeight,
          currentTime: el.currentTime,
          hasError: el.error !== null,
        };
      });
      assert.ok(videoCheck.src.startsWith('/v1/content/'), `${machine}: expected /v1/content video src`);
      assert.ok(videoCheck.readyState >= 2, `${machine}: expected readyState >= 2, got ${videoCheck.readyState}`);
      assert.ok(videoCheck.duration > 2.0, `${machine}: expected duration > 2s, got ${videoCheck.duration}`);
      assert.equal(videoCheck.width, 64, `${machine}: videoWidth`);
      assert.equal(videoCheck.height, 64, `${machine}: videoHeight`);
      assert.ok(
        videoCheck.currentTime >= 1.2 && videoCheck.currentTime <= 2.0,
        `${machine}: seeked currentTime ${videoCheck.currentTime}`,
      );
      assert.equal(videoCheck.hasError, false, `${machine}: video error`);
      assert.ok(
        contentGetStatuses.includes(206),
        `${machine}: expected 206 Partial Content for video range requests, got ${JSON.stringify(contentGetStatuses)}`,
      );
      await page.locator('.viewer-top button[aria-label="Close"]').click();
      await page.locator('.viewer').waitFor({ state: 'hidden' });
    }

    // 1. Door's own machine (tower) and second remote node (pi).
    await verifyPictureAndVideo('tower');
    await verifyPictureAndVideo('pi');

    // 2. Older node (legacy): POST /v1/content fails once (501 unsupported) and is remembered for
    //    the session; pictures fall back to the machine-made fs.thumb preview without Download,
    //    while video shows the plain unavailable message without retrying POST /v1/content.
    await openMachineMedia('legacy');
    await entryLocator('icon.png').click();
    const legacyImg = page.locator('.viewer img.viewer-img');
    await legacyImg.waitFor({ timeout: 10_000 });
    await page.waitForFunction(
      () => {
        const el = document.querySelector('.viewer img.viewer-img');
        return Boolean(el && el.complete && el.naturalWidth > 0 && el.naturalHeight > 0);
      },
      null,
      { timeout: 10_000 },
    );
    const legacyImgInfo = await legacyImg.evaluate((el) => ({
      src: el.getAttribute('src') || '',
      w: el.naturalWidth,
      h: el.naturalHeight,
    }));
    assert.ok(
      legacyImgInfo.src.startsWith('data:image/'),
      `legacy: expected data:image/ fs.thumb fallback preview, got ${legacyImgInfo.src.slice(0, 40)}`,
    );
    assert.equal(legacyImgInfo.w, 512, 'legacy: expected 512px preview width');
    assert.equal(legacyImgInfo.h, 512, 'legacy: expected 512px preview height');
    assert.equal(await page.locator('.viewer button', { hasText: 'Download' }).count(), 0);
    assert.equal(contentPostsByMachine.get('legacy') || 0, 1, 'expected exactly one POST /v1/content probe for legacy');
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    await entryLocator('sample.webm').click();
    await page.locator('.viewer .said', { hasText: "Video playback isn't available from legacy yet" }).waitFor({ timeout: 10_000 });
    assert.equal(await page.locator('.viewer video.viewer-video').count(), 0);
    assert.equal(
      contentPostsByMachine.get('legacy') || 0,
      1,
      'legacy unsupported state must be remembered for the session without retrying POST /v1/content',
    );
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 3. Supported remote machine still opens pictures after legacy was remembered unsupported.
    await openMachineMedia('pi');
    await entryLocator('icon.png').click();
    await page.waitForFunction(
      () => {
        const el = document.querySelector('.viewer img.viewer-img');
        return Boolean(el && el.complete && el.naturalWidth === 512);
      },
      null,
      { timeout: 10_000 },
    );
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // Only the single expected 501 network log from probing the older legacy node is allowed.
    const unexpectedConsole = consoleErrors.filter(
      (msg) => !msg.includes('status of 501'),
    );
    assert.deepEqual(unexpectedConsole, [], `Unexpected console/CSP errors: ${unexpectedConsole.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);

    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

function ebmlReadVint(buf, off, keepMarker) {
  const first = buf[off];
  let len = 1;
  while (len <= 8 && !(first & (0x80 >> (len - 1)))) len += 1;
  let val = keepMarker ? first : first & ((0x80 >> (len - 1)) - 1);
  for (let i = 1; i < len; i += 1) val = val * 256 + buf[off + i];
  return { val, len };
}

function ebmlSize8(n) {
  const out = Buffer.alloc(8);
  out.writeBigUInt64BE(BigInt(n) | 0x0100000000000000n);
  return out;
}

function ebmlUint(n) {
  if (n === 0) return Buffer.from([0]);
  const bytes = [];
  let v = BigInt(n);
  while (v > 0n) {
    bytes.unshift(Number(v & 0xffn));
    v >>= 8n;
  }
  return Buffer.from(bytes);
}

function ebmlElem(idBytes, payload) {
  const id = Buffer.from(idBytes);
  const len = payload.length;
  let sizeBuf;
  if (len < 0x7f) {
    sizeBuf = Buffer.from([0x80 | len]);
  } else if (len < 0x3fff) {
    sizeBuf = Buffer.from([0x40 | (len >> 8), len & 0xff]);
  } else {
    sizeBuf = ebmlSize8(len);
  }
  return Buffer.concat([id, sizeBuf, payload]);
}

// Re-muxes `sample.webm` into a 60-second seekable WebM with 1 MiB Matroska Void gaps between
// 1-second clusters and fresh SeekHead/Cues (~60 MiB total). Chromium buffers only the first ~11s
// and must send real HTTP Range GETs for later clusters, exercising true 404 recovery on ticket
// expiry and door restart.
function buildLongWebm(sampleBuf, repeatCount = 20, voidBytesPerGap = 1024 * 1024) {
  const ebmlId = ebmlReadVint(sampleBuf, 0, true);
  const ebmlSz = ebmlReadVint(sampleBuf, ebmlId.len, false);
  const ebmlEnd = ebmlId.len + ebmlSz.len + ebmlSz.val;
  const ebmlHeader = sampleBuf.subarray(0, ebmlEnd);

  const segId = ebmlReadVint(sampleBuf, ebmlEnd, true);
  const segSz = ebmlReadVint(sampleBuf, ebmlEnd + segId.len, false);
  let pos = ebmlEnd + segId.len + segSz.len;
  const segEnd = Math.min(sampleBuf.length, pos + segSz.val);

  let tracks = null;
  const baseClusters = [];

  while (pos < segEnd) {
    const id = ebmlReadVint(sampleBuf, pos, true);
    const sz = ebmlReadVint(sampleBuf, pos + id.len, false);
    const elemEnd = pos + id.len + sz.len + sz.val;
    const raw = sampleBuf.subarray(pos, elemEnd);
    if (id.val === 0x1654ae6b) tracks = Buffer.from(raw);
    else if (id.val === 0x1f43b675) {
      const inner = sampleBuf.subarray(pos + id.len + sz.len, elemEnd);
      const tcId = ebmlReadVint(inner, 0, true);
      const tcSz = ebmlReadVint(inner, tcId.len, false);
      baseClusters.push(inner.subarray(tcId.len + tcSz.len + tcSz.val));
    }
    pos = elemEnd;
  }
  assert.ok(tracks && baseClusters.length >= 2, 'expected Tracks and >=2 Clusters in sample.webm');

  const totalSec = baseClusters.length * repeatCount;
  const durBuf = Buffer.alloc(8);
  durBuf.writeDoubleBE(totalSec * 1000);
  const info = ebmlElem(
    [0x15, 0x49, 0xa9, 0x66],
    Buffer.concat([
      ebmlElem([0x2a, 0xd7, 0xb1], ebmlUint(1_000_000)),
      ebmlElem([0x44, 0x89], durBuf),
    ]),
  );

  const voidElem = Buffer.concat([
    Buffer.from([0xec]),
    ebmlSize8(voidBytesPerGap),
    Buffer.alloc(voidBytesPerGap),
  ]);

  const makeSeekEntry = (targetIdBytes, offset) =>
    ebmlElem(
      [0x4d, 0xbb],
      Buffer.concat([
        ebmlElem([0x53, 0xab], Buffer.from(targetIdBytes)),
        ebmlElem([0x53, 0xac], ebmlUint(offset)),
      ]),
    );

  // Reserve a fixed-size SeekHead + Void header block (128 bytes) so offsets stay deterministic.
  const SEEK_BLOCK_BYTES = 128;
  const infoOff = SEEK_BLOCK_BYTES;
  const tracksOff = infoOff + info.length;
  let cursor = tracksOff + tracks.length;

  const clusterPositions = [];
  const bodyParts = [];
  for (let i = 0; i < totalSec; i += 1) {
    const timecode = i * 1000;
    const rest = baseClusters[i % baseClusters.length];
    const cluster = ebmlElem(
      [0x1f, 0x43, 0xb6, 0x75],
      Buffer.concat([ebmlElem([0xe7], ebmlUint(timecode)), rest]),
    );
    clusterPositions.push({ timecode, offset: cursor });
    bodyParts.push(cluster);
    cursor += cluster.length;
    if (i + 1 < totalSec) {
      bodyParts.push(voidElem);
      cursor += voidElem.length;
    }
  }

  const cuesOff = cursor;
  const cuePoints = clusterPositions.map(({ timecode, offset }) =>
    ebmlElem(
      [0xbb],
      Buffer.concat([
        ebmlElem([0xb3], ebmlUint(timecode)),
        ebmlElem(
          [0xb7],
          Buffer.concat([
            ebmlElem([0xf7], Buffer.from([1])),
            ebmlElem([0xf1], ebmlUint(offset)),
          ]),
        ),
      ]),
    ),
  );
  const cues = ebmlElem([0x1c, 0x53, 0xbb, 0x6b], Buffer.concat(cuePoints));
  cursor += cues.length;

  const seekHead = ebmlElem(
    [0x11, 0x4d, 0x9b, 0x74],
    Buffer.concat([
      makeSeekEntry([0x15, 0x49, 0xa9, 0x66], infoOff),
      makeSeekEntry([0x16, 0x54, 0xae, 0x6b], tracksOff),
      makeSeekEntry([0x1c, 0x53, 0xbb, 0x6b], cuesOff),
    ]),
  );
  assert.ok(seekHead.length + 2 <= SEEK_BLOCK_BYTES, `SeekHead too large: ${seekHead.length}`);
  const padLen = SEEK_BLOCK_BYTES - seekHead.length - 2;
  const seekPad = Buffer.concat([Buffer.from([0xec, 0x80 | padLen]), Buffer.alloc(padLen)]);

  const segHeader = Buffer.concat([Buffer.from([0x18, 0x53, 0x80, 0x67]), ebmlSize8(cursor)]);
  return Buffer.concat([ebmlHeader, segHeader, seekHead, seekPad, info, tracks, ...bodyParts, cues]);
}

test('Files viewer recovers expired and door-restarted video playback, reports honest errors, and switches cleanly to a picture', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const sampleWebm = fs.readFileSync(path.join(TEST_DIR, 'fixtures', 'sample.webm'));
  const samplePng = fs.readFileSync(path.join(REPO_ROOT, 'apps', 'ui', 'icons', 'icon-512.png'));
  const paddedWebm = buildLongWebm(sampleWebm, 20, 1024 * 1024);

  const root = makeWorkRoot();
  const fleet = setupFleet(root);
  const mediaDir = path.join(fleet.towerUser, 'media');
  fs.mkdirSync(mediaDir, { recursive: true });
  fs.writeFileSync(path.join(mediaDir, 'padded.webm'), paddedWebm);
  fs.writeFileSync(path.join(mediaDir, 'sample.webm'), sampleWebm);
  fs.writeFileSync(path.join(mediaDir, 'icon.png'), samplePng);
  fs.writeFileSync(path.join(mediaDir, 'broken.webm'), Buffer.alloc(4096, 0x5a));

  let server = await startServer(fleet.towerHome, fleet.towerUser, {
    envExtra: {
      LEGION_CONTENT_TICKET_SLIDING_MS: '1200',
      LEGION_CONTENT_TICKET_ABSOLUTE_MS: '3000',
    },
  });
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'],
  });

  try {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
    const page = await ctx.newPage();
    const pageErrors = [];
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    const contentPosts = [];
    const contentGetStatuses = [];
    page.on('request', (req) => {
      if (req.method() === 'POST' && new URL(req.url()).pathname === '/v1/content') {
        try {
          contentPosts.push(JSON.parse(req.postData() || '{}'));
        } catch {
          contentPosts.push({});
        }
      }
    });
    page.on('response', (res) => {
      const p = new URL(res.url()).pathname;
      if (res.request().method() === 'GET' && p.startsWith('/v1/content/')) {
        contentGetStatuses.push({ path: p, status: res.status() });
      }
    });

    await page.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await page.getByText('2 are on').waitFor({ timeout: 15_000 });

    const viewerHashFor = (fileName) =>
      `#/files/tower/~/media?view=${encodeURIComponent(`~/media/${fileName}`)}`;
    const paddedHash = `${server.url}/${viewerHashFor('padded.webm')}`;

    // Helper: open padded.webm, play until >=0.4s, pause, and record recovery timestamps.
    async function openPlayAndPause() {
      await page.goto(paddedHash, { waitUntil: 'domcontentloaded' });
      const video = page.locator('.viewer video.viewer-video');
      await video.waitFor({ timeout: 10_000 });
      const state = await video.evaluate(async (el) => {
        el.muted = true;
        await new Promise((resolve, reject) => {
          if (el.readyState >= 2) return resolve();
          const t = setTimeout(() => reject(new Error('timed out waiting for loadeddata')), 10_000);
          el.addEventListener('loadeddata', () => { clearTimeout(t); resolve(); }, { once: true });
          el.addEventListener('error', () => { clearTimeout(t); reject(new Error(`initial video error ${el.error?.code}`)); }, { once: true });
        });
        await el.play();
        await new Promise((resolve, reject) => {
          const t = setTimeout(() => reject(new Error(`video did not advance past 0.4s (at ${el.currentTime})`)), 10_000);
          const check = () => {
            if (el.currentTime >= 0.4) {
              clearTimeout(t);
              el.removeEventListener('timeupdate', check);
              resolve();
            }
          };
          el.addEventListener('timeupdate', check);
          check();
        });
        el.pause();
        window.__recoveryAudit = { timeAtError: null, timeAtSwap: null };
        el.addEventListener(
          'error',
          () => {
            window.__recoveryAudit.timeAtError = el.currentTime;
          },
          { once: true },
        );
        return {
          src: el.getAttribute('src') || '',
          pausedAt: el.currentTime,
          bufferedEnd: el.buffered.length > 0 ? el.buffered.end(0) : 0,
        };
      });
      assert.ok(state.src.startsWith('/v1/content/'), `expected /v1/content src, got ${state.src}`);
      assert.ok(state.pausedAt >= 0.4 && state.pausedAt < 5.0, `unexpected pausedAt: ${state.pausedAt}`);
      assert.ok(state.bufferedEnd < 30, `expected partial buffering on 60s video, got ${state.bufferedEnd}s`);
      return state;
    }

    // Helper: resume paused video (at 8x so it quickly reaches the unbuffered boundary and hits
    // the 404 on the stale ticket), then verify silent single-shot recovery and continued playback
    // from the exact position where playback stopped.
    async function playAndAssertSilentRecovery(initialState, label) {
      contentGetStatuses.length = 0;
      const postsBefore = contentPosts.length;
      await page.locator('.viewer video.viewer-video').evaluate((el) => {
        el.playbackRate = 8;
        el.play().catch(() => {});
      });

      // Wait until src swaps to a fresh ticket and playback is actively continuing with no error.
      await page.waitForFunction(
        ({ oldSrc }) => {
          const v = document.querySelector('.viewer video.viewer-video');
          if (!v) return false;
          const src = v.getAttribute('src') || '';
          const swapped = src !== oldSrc && src.startsWith('/v1/content/');
          if (swapped && window.__recoveryAudit && window.__recoveryAudit.timeAtSwap === null && v.currentTime > 0.5) {
            window.__recoveryAudit.timeAtSwap = v.currentTime;
          }
          return swapped && !v.paused && v.error === null && v.readyState >= 2 && v.currentTime > 1.0;
        },
        { oldSrc: initialState.src },
        { timeout: 15_000 },
      );

      const tAfterSwap = await page.locator('.viewer video.viewer-video').evaluate((el) => el.currentTime);
      await page.waitForFunction(
        (minTime) => {
          const v = document.querySelector('.viewer video.viewer-video');
          return Boolean(v && !v.paused && v.error === null && v.currentTime >= minTime);
        },
        tAfterSwap + 0.5,
        { timeout: 10_000 },
      );
      const audit = await page.evaluate(() => window.__recoveryAudit);
      assert.equal(await page.locator('.viewer .said').count(), 0, `${label}: recovery must show no error UI`);
      assert.ok(
        contentGetStatuses.some((r) => r.path === initialState.src && r.status === 404),
        `${label}: expected player to hit 404 on stale ticket ${initialState.src}, saw ${JSON.stringify(contentGetStatuses)}`,
      );
      assert.equal(
        contentPosts.length - postsBefore,
        1,
        `${label}: recovery must mint exactly one fresh address`,
      );
      if (audit?.timeAtError > 1.0 && audit?.timeAtSwap != null) {
        assert.ok(
          Math.abs(audit.timeAtSwap - audit.timeAtError) <= 1.5,
          `${label}: expected playback to resume near ${audit.timeAtError}s, got ${audit.timeAtSwap}s`,
        );
      }
      await page.locator('.viewer video.viewer-video').evaluate((el) => el.pause());
    }

    // ── Part A: Pause video, let short-lived ticket expire, press play -> silent recovery ─────
    const openedAt = Date.now();
    const expState = await openPlayAndPause();
    // Wait past the 3.0s absolute ticket lifetime so expiry is deterministic even if Chromium
    // holds a paused Range connection open.
    const remainingMs = 3300 - (Date.now() - openedAt);
    if (remainingMs > 0) await page.waitForTimeout(remainingMs);
    const headStatus = await page.evaluate(async (url) => {
      const res = await fetch(url, { method: 'HEAD' });
      return res.status;
    }, expState.src);
    assert.equal(headStatus, 404, 'expected initial ticket to be expired (404) before pressing Play');
    await playAndAssertSilentRecovery(expState, 'expired ticket');

    // Close the viewer and restart server with normal 15-minute TTL on a fixed port for Part B.
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    const fixedPort = Number(new URL(server.url).port);
    await server.stop();
    server = await startServer(fleet.towerHome, fleet.towerUser, {
      listen: `127.0.0.1:${fixedPort}`,
    });

    // ── Part B: Restart the door during a pause, press play -> silent recovery ────────────────
    const restartState = await openPlayAndPause();
    await server.stop();
    server = await startServer(fleet.towerHome, fleet.towerUser, {
      listen: `127.0.0.1:${fixedPort}`,
    });
    await playAndAssertSilentRecovery(restartState, 'door restart');

    // ── Part C1: Corrupt video reachable with HTTP 200 -> 1 recovery, no loop, honest decode msg
    const postsBeforeBroken = contentPosts.length;
    const brokenHash = `${server.url}/${viewerHashFor('broken.webm')}`;
    await page.goto(brokenHash, { waitUntil: 'domcontentloaded' });
    await page.locator('.viewer .said', { hasText: "This browser can't play this kind of video" }).waitFor({ timeout: 10_000 });
    await page.waitForTimeout(300);
    assert.equal(
      contentPosts.length - postsBeforeBroken,
      2,
      'corrupt video must mint initial address plus at most one fresh recovery address (never loop)',
    );

    // ── Part C2: Deleted/unreachable file -> honest "Couldn't reach this file" + Try again ───
    const vanishedPath = path.join(mediaDir, 'vanished.webm');
    fs.writeFileSync(vanishedPath, paddedWebm);
    const vanishedHash = `${server.url}/${viewerHashFor('vanished.webm')}`;
    await page.goto(vanishedHash, { waitUntil: 'domcontentloaded' });
    await page.locator('.viewer video.viewer-video').waitFor({ timeout: 10_000 });
    await page.locator('.viewer video.viewer-video').evaluate(async (el) => {
      el.muted = true;
      await new Promise((resolve) => {
        if (el.readyState >= 2) return resolve();
        el.addEventListener('loadeddata', resolve, { once: true });
      });
      await el.play();
      await new Promise((resolve) => {
        const check = () => {
          if (el.currentTime >= 0.4) {
            el.removeEventListener('timeupdate', check);
            resolve();
          }
        };
        el.addEventListener('timeupdate', check);
        check();
      });
      el.pause();
    });

    // Remove the file and restart the door so both the old ticket and fresh mint fail.
    fs.unlinkSync(vanishedPath);
    await server.stop();
    server = await startServer(fleet.towerHome, fleet.towerUser, {
      listen: `127.0.0.1:${fixedPort}`,
    });

    const postsBeforeVanished = contentPosts.length;
    await page.locator('.viewer video.viewer-video').evaluate((el) => {
      el.playbackRate = 8;
      el.play().catch(() => {});
    });
    await page.locator('.viewer .said', { hasText: "Couldn't reach this file" }).waitFor({ timeout: 15_000 });
    assert.equal(
      await page.locator('.viewer .said', { hasText: "can't play this kind of video" }).count(),
      0,
      'unreachable/missing file must not blame video codec',
    );
    await page.locator('.viewer').getByRole('button', { name: 'Try again' }).waitFor();
    assert.equal(
      contentPosts.length - postsBeforeVanished,
      1,
      'unreachable file recovery must attempt at most one fresh address',
    );

    // Restore file on disk and click Try again -> video loads cleanly.
    fs.writeFileSync(vanishedPath, paddedWebm);
    await page.locator('.viewer').getByRole('button', { name: 'Try again' }).click();
    await page.locator('.viewer video.viewer-video').waitFor({ timeout: 10_000 });
    await page.waitForFunction(
      () => {
        const v = document.querySelector('.viewer video.viewer-video');
        return Boolean(v && v.readyState >= 1 && v.videoWidth === 64 && v.error === null);
      },
      null,
      { timeout: 10_000 },
    );

    // ── Part D: Switch mounted viewer from video to picture via hash change ──────────────────
    const videoHash = `${server.url}/${viewerHashFor('sample.webm')}`;
    const picHash = viewerHashFor('icon.png');
    await page.goto(videoHash, { waitUntil: 'domcontentloaded' });
    await page.locator('.viewer video.viewer-video').waitFor({ timeout: 10_000 });
    await page.waitForFunction(
      () => {
        const v = document.querySelector('.viewer video.viewer-video');
        return Boolean(v && v.readyState >= 1 && v.videoWidth === 64);
      },
      null,
      { timeout: 10_000 },
    );

    // Watch every <img> src rendered inside .viewer during the hash switch.
    await page.evaluate(() => {
      window.__seenImgSrcs = [];
      const record = (node) => {
        if (node && node.nodeType === 1) {
          if (node.tagName === 'IMG' && node.getAttribute('src')) {
            window.__seenImgSrcs.push(node.getAttribute('src'));
          }
          if (node.querySelectorAll) {
            for (const img of node.querySelectorAll('img')) {
              if (img.getAttribute('src')) window.__seenImgSrcs.push(img.getAttribute('src'));
            }
          }
        }
      };
      const viewer = document.querySelector('.viewer');
      const obs = new MutationObserver((mutations) => {
        for (const m of mutations) {
          if (m.type === 'attributes' && m.target.tagName === 'IMG') {
            record(m.target);
          }
          for (const n of m.addedNodes) record(n);
        }
      });
      if (viewer) obs.observe(viewer, { subtree: true, childList: true, attributes: true, attributeFilter: ['src'] });
      window.__imgObs = obs;
    });

    await page.evaluate((nextHash) => {
      window.location.hash = nextHash;
    }, picHash);

    await page.waitForFunction(
      () => {
        const img = document.querySelector('.viewer img.viewer-img');
        return Boolean(img && img.complete && img.naturalWidth === 512 && img.naturalHeight === 512);
      },
      null,
      { timeout: 10_000 },
    );
    const seenImgSrcs = await page.evaluate(() => {
      window.__imgObs?.disconnect();
      return window.__seenImgSrcs || [];
    });
    assert.equal(await page.locator('.viewer .said').count(), 0, 'switching from video to picture must show no error');
    assert.ok(
      seenImgSrcs.every((s) => !s.endsWith('/sample.webm')),
      `<img> must never receive the previous video address even for one frame: ${JSON.stringify(seenImgSrcs)}`,
    );

    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);

    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

const PNG_CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = (c & 1) !== 0 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function pngCrc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i += 1) {
    c = PNG_CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

function makePngChunk(type, data) {
  const typeBuf = Buffer.from(type, 'ascii');
  const lenBuf = Buffer.alloc(4);
  lenBuf.writeUInt32BE(data.length, 0);
  const crcBuf = Buffer.alloc(4);
  crcBuf.writeUInt32BE(pngCrc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([lenBuf, typeBuf, data, crcBuf]);
}

function makeSolidPng(width, height, r, g, b) {
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // 8-bit depth
  ihdr[9] = 2; // RGB
  const stride = 1 + width * 3;
  const raw = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y += 1) {
    const row = y * stride;
    raw[row] = 0; // filter: None
    for (let x = 0; x < width; x += 1) {
      const px = row + 1 + x * 3;
      raw[px] = r;
      raw[px + 1] = g;
      raw[px + 2] = b;
    }
  }
  return Buffer.concat([
    sig,
    makePngChunk('IHDR', ihdr),
    makePngChunk('IDAT', zlib.deflateSync(raw)),
    makePngChunk('IEND', Buffer.alloc(0)),
  ]);
}

function makeTaggedFlac({ title, artist, album, coverPng, durationSecs = 4 } = {}) {
  const sampleRate = 44_100;
  const totalSamples = sampleRate * durationSecs;
  const streaminfo = Buffer.alloc(34);
  streaminfo[10] = (sampleRate >> 12) & 0xff;
  streaminfo[11] = (sampleRate >> 4) & 0xff;
  streaminfo[12] = ((sampleRate & 0x0f) << 4) | 0x02;
  streaminfo[13] = 0xf0 | (Math.floor(totalSamples / 2 ** 32) & 0x0f);
  streaminfo.writeUInt32BE(totalSamples >>> 0, 14);

  const blocks = [[0, streaminfo]];
  const comments = [];
  if (title) comments.push(`TITLE=${title}`);
  if (artist) comments.push(`ARTIST=${artist}`);
  if (album) comments.push(`ALBUM=${album}`);
  if (comments.length > 0) {
    const vendor = Buffer.from('legion-test', 'utf8');
    const parts = [Buffer.alloc(4), vendor, Buffer.alloc(4)];
    parts[0].writeUInt32LE(vendor.length, 0);
    parts[2].writeUInt32LE(comments.length, 0);
    for (const c of comments) {
      const buf = Buffer.from(c, 'utf8');
      const len = Buffer.alloc(4);
      len.writeUInt32LE(buf.length, 0);
      parts.push(len, buf);
    }
    blocks.push([4, Buffer.concat(parts)]);
  }
  if (coverPng) {
    const mime = Buffer.from('image/png', 'ascii');
    const desc = Buffer.from('cover', 'ascii');
    const header = Buffer.alloc(8);
    header.writeUInt32BE(3, 0);
    header.writeUInt32BE(mime.length, 4);
    const descLen = Buffer.alloc(4);
    descLen.writeUInt32BE(desc.length, 0);
    const dims = Buffer.alloc(16);
    const imgLen = Buffer.alloc(4);
    imgLen.writeUInt32BE(coverPng.length, 0);
    blocks.push([6, Buffer.concat([header, mime, descLen, desc, dims, imgLen, coverPng])]);
  }

  const out = [Buffer.from('fLaC', 'ascii')];
  for (let i = 0; i < blocks.length; i += 1) {
    const [btype, payload] = blocks[i];
    const hdr = Buffer.alloc(4);
    hdr[0] = i === blocks.length - 1 ? btype | 0x80 : btype;
    hdr[1] = (payload.length >> 16) & 0xff;
    hdr[2] = (payload.length >> 8) & 0xff;
    hdr[3] = payload.length & 0xff;
    out.push(hdr, payload);
  }
  return Buffer.concat(out);
}

async function waitForLoadedThumb(locator, label) {
  const img = locator.locator('img').first();
  await img.waitFor({ timeout: 10_000 });
  await img.evaluate(
    (el) =>
      new Promise((resolve, reject) => {
        if (el.complete && el.naturalWidth > 0 && el.naturalHeight > 0) {
          resolve();
          return;
        }
        const timer = setTimeout(() => reject(new Error('Thumbnail image did not finish loading')), 10_000);
        el.addEventListener(
          'load',
          () => {
            clearTimeout(timer);
            resolve();
          },
          { once: true },
        );
        el.addEventListener(
          'error',
          () => {
            clearTimeout(timer);
            reject(new Error('Thumbnail image failed to decode'));
          },
          { once: true },
        );
      }),
  );
  const info = await img.evaluate((el) => ({
    src: el.getAttribute('src') || '',
    complete: el.complete,
    width: el.naturalWidth,
    height: el.naturalHeight,
  }));
  assert.ok(info.src.startsWith('data:image/'), `${label}: expected data:image/ thumbnail, got ${info.src.slice(0, 40)}`);
  assert.equal(info.complete, true, `${label}: expected thumbnail complete`);
  assert.ok(info.width > 0, `${label}: expected naturalWidth > 0, got ${info.width}`);
  assert.ok(info.height > 0, `${label}: expected naturalHeight > 0, got ${info.height}`);
  return info;
}

test('real legion serve renders real fs.thumb thumbnails in grid, list, pinned strip, and search hits', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const fleet = setupFleet(root);

  const galleryDir = path.join(fleet.towerUser, 'gallery');
  fs.mkdirSync(galleryDir, { recursive: true });
  const pictures = [
    { name: 'crimson-sunset.png', rgb: [210, 45, 60] },
    { name: 'forest-pine.png', rgb: [34, 139, 68] },
    { name: 'ocean-wave.png', rgb: [30, 110, 210] },
  ];
  for (const pic of pictures) {
    fs.writeFileSync(path.join(galleryDir, pic.name), makeSolidPng(160, 120, ...pic.rgb));
  }
  fs.writeFileSync(path.join(galleryDir, 'notes.txt'), 'Caption notes for gallery.\n');

  const crimsonPath = path.join(galleryDir, 'crimson-sunset.png');
  runLegion(
    ['fav.add', 'machine=tower', `path=${crimsonPath}`, 'pinned=true', '--json'],
    fleet.towerHome,
    fleet.towerUser,
  );

  // Confirm the real node returns the `{ results: [{ path, ok }] }` wire shape.
  const cliThumb = JSON.parse(
    runLegion(['fs.thumb', `paths=["${crimsonPath.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"]`, '--json'], fleet.towerHome, fleet.towerUser),
  );
  assert.ok(Array.isArray(cliThumb.results), 'Real node fs.thumb must return a results array');
  assert.equal(cliThumb.results.length, 1);
  assert.equal(cliThumb.results[0].path, crimsonPath);
  assert.ok(cliThumb.results[0].ok?.mime?.startsWith('image/'));
  assert.ok(cliThumb.results[0].ok?.content?.length > 0);
  assert.equal(cliThumb.thumbnails, undefined);

  const server = await startServer(fleet.towerHome, fleet.towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    const ctx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
    });
    const page = await ctx.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    // 1. Connect and open Files home: pinned picture renders a real thumbnail.
    await page.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, { waitUntil: 'domcontentloaded' });
    await page.getByText('2 are on').waitFor({ timeout: 15_000 });
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();

    const pinnedTile = page.locator('.tiles a.tile', { hasText: 'crimson-sunset.png' });
    await pinnedTile.waitFor({ timeout: 10_000 });
    await waitForLoadedThumb(pinnedTile, 'pinned strip crimson-sunset.png');

    // 2. Open tower -> gallery: defaults to grid view and renders real thumbnails for all 3 pictures.
    await page.locator('a.row[href="#/files/tower/~"]').click();
    await page.locator('header.top h1', { hasText: 'Home' }).waitFor();
    await page.locator('.list a.row', { hasText: 'gallery' }).click();
    await page.locator('header.top h1', { hasText: 'gallery' }).waitFor();

    const gridSrcs = new Set();
    for (const pic of pictures) {
      const cell = page.locator(`.grid a.cell[aria-label="${pic.name}"]`);
      await cell.waitFor({ timeout: 10_000 });
      const info = await waitForLoadedThumb(cell, `grid ${pic.name}`);
      gridSrcs.add(info.src);
    }
    assert.equal(gridSrcs.size, pictures.length, 'Different-coloured pictures should produce distinct thumbnails');

    // 3. Switch to list view: all 3 pictures render real thumbnails.
    await page.locator('.dock button[aria-label="Show as list"]').click();
    for (const pic of pictures) {
      const row = page.locator('.list a.row', { hasText: pic.name });
      await row.waitFor({ timeout: 10_000 });
      await waitForLoadedThumb(row, `list ${pic.name}`);
    }

    // 4. Clear cached thumbnails and reload Files home so search hits genuinely fetch via fs.thumb.
    await page.evaluate(() => {
      for (const k of Object.keys(localStorage)) {
        if (k.startsWith('legion.thumb.')) localStorage.removeItem(k);
      }
    });
    await page.goto(`${server.url}/#/files`, { waitUntil: 'domcontentloaded' });
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor({ timeout: 10_000 });

    const searchInput = page.locator('.dock input[aria-label="Search all machines"]');
    await searchInput.waitFor({ timeout: 10_000 });
    await searchInput.fill('ocean-wave');
    await page.locator('header.top h1', { hasText: 'Search' }).waitFor({ timeout: 10_000 });
    const searchRow = page.locator('.list a.row', { hasText: 'ocean-wave.png' }).first();
    await searchRow.waitFor({ timeout: 10_000 });
    await waitForLoadedThumb(searchRow, 'search hit ocean-wave.png');

    assert.deepEqual(consoleErrors, [], `Unexpected console/CSP errors: ${consoleErrors.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);

    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('delayed capability replies still render thumbnails in pinned strip, folder listing, and viewer', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const piHome = path.join(root, 'pi-legion');
  const piUser = path.join(root, 'pi-user');
  const legacyHome = path.join(root, 'legacy-legion');
  const legacyUser = path.join(root, 'legacy-user');

  const samplePng = fs.readFileSync(path.join(REPO_ROOT, 'apps', 'ui', 'icons', 'icon-512.png'));

  for (const [home, user] of [
    [towerHome, towerUser],
    [piHome, piUser],
    [legacyHome, legacyUser],
  ]) {
    fs.mkdirSync(path.join(home, 'config'), { recursive: true });
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    fs.mkdirSync(user, { recursive: true });
  }

  const piGallery = path.join(piUser, 'gallery');
  fs.mkdirSync(piGallery, { recursive: true });
  const piAmberPath = path.join(piGallery, 'pi-amber.png');
  const piVioletPath = path.join(piGallery, 'pi-violet.png');
  const piSongPath = path.join(piGallery, 'pi-song.flac');
  const piSongCover = makeSolidPng(128, 128, 45, 175, 155);
  fs.writeFileSync(piAmberPath, makeSolidPng(160, 120, 225, 165, 35));
  fs.writeFileSync(piVioletPath, makeSolidPng(160, 120, 125, 70, 200));
  fs.writeFileSync(
    piSongPath,
    makeTaggedFlac({ title: 'Pi Song', artist: 'Maren', album: 'Harbor', coverPng: piSongCover }),
  );

  const legacyMedia = path.join(legacyUser, 'media');
  fs.mkdirSync(legacyMedia, { recursive: true });
  const legacyIconPath = path.join(legacyMedia, 'icon.png');
  fs.writeFileSync(legacyIconPath, samplePng);

  const policy = [
    'default = "owner"',
    '',
    '[principals.ui]',
    'role = "operator"',
    '',
    '[roles.operator]',
    'allow = ["effect:read", "effect:write"]',
    '',
  ].join('\n');

  for (const [name, home] of [
    ['tower', towerHome],
    ['pi', piHome],
    ['legacy', legacyHome],
  ]) {
    fs.writeFileSync(path.join(home, 'config', 'node.toml'), `name = "${name}"\nsystem = "linux"\nbusy = "none"\n`);
    fs.writeFileSync(path.join(home, 'config', 'policy.toml'), policy);
  }

  const piCmd = [process.execPath, path.join(TEST_DIR, 'rpc-helper.mjs'), LEGION_BIN, piHome, piUser, 'rpc'];
  const legacyCmd = [process.execPath, path.join(TEST_DIR, 'legacy-node-helper.mjs'), LEGION_BIN, legacyHome, legacyUser, 'rpc'];
  fs.writeFileSync(
    path.join(towerHome, 'config', 'fleet.toml'),
    [
      '[machines.tower]',
      'ssh = "tower.example"',
      '',
      '[machines.pi]',
      `command = ${JSON.stringify(piCmd)}`,
      '',
      '[machines.legacy]',
      `command = ${JSON.stringify(legacyCmd)}`,
      '',
    ].join('\n'),
  );

  // Pin a picture and a song that live on `pi` so Files home renders both tiles via tower's
  // fav.list while pi's own capabilities are still held back.
  runLegion(['fav.add', 'machine=pi', `path=${piAmberPath}`, 'pinned=true', '--json'], towerHome, towerUser);
  runLegion(['fav.add', 'machine=pi', `path=${piSongPath}`, 'pinned=true', '--json'], towerHome, towerUser);
  const tokenOut = JSON.parse(runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser));

  const server = await startServer(towerHome, towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
    const page = await ctx.newPage();
    const cdp = await ctx.newCDPSession(page);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: 4 });

    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    function makeGate() {
      let release;
      let arrivedResolve;
      const promise = new Promise((r) => {
        release = r;
      });
      const arrived = new Promise((r) => {
        arrivedResolve = r;
      });
      let held = true;
      return {
        isHeld: () => held,
        waitArrived: () => arrived,
        markArrived: () => arrivedResolve(),
        waitRelease: () => promise,
        release() {
          held = false;
          release();
        },
      };
    }

    let piCapsGate = makeGate();
    let legacyCapsGate = makeGate();

    await page.route('**/v1/call', async (route) => {
      let body = null;
      try {
        body = JSON.parse(route.request().postData() || '{}');
      } catch {
        body = null;
      }
      const isCapProbe = body && (body.tool === 'meta.whoami' || body.tool === 'meta.version');
      if (isCapProbe && body.on === 'pi' && piCapsGate.isHeld()) {
        piCapsGate.markArrived();
        await piCapsGate.waitRelease();
      } else if (isCapProbe && body.on === 'legacy' && legacyCapsGate.isHeld()) {
        legacyCapsGate.markArrived();
        await legacyCapsGate.waitRelease();
      }
      await route.continue();
    });

    // 1. Pinned strip renders while pi's capability reply is still held back.
    await page.goto(`${server.url}/#token=${encodeURIComponent(tokenOut.token)}`, { waitUntil: 'domcontentloaded' });
    await page.getByText('3 are on').waitFor({ timeout: 15_000 });
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();

    const pinnedTile = page.locator('.tiles a.tile', { hasText: 'pi-amber.png' });
    const pinnedSongTile = page.locator('.tiles a.tile', { hasText: 'pi-song.flac' });
    await pinnedTile.waitFor({ timeout: 10_000 });
    await pinnedSongTile.waitFor({ timeout: 10_000 });
    await piCapsGate.waitArrived();
    assert.equal(await pinnedTile.locator('img').count(), 0, 'Pinned tile should show icon while pi caps are held');
    assert.equal(await pinnedSongTile.locator('img').count(), 0, 'Pinned song tile should show icon while pi caps are held');
    piCapsGate.release();
    await waitForLoadedThumb(pinnedTile, 'delayed-caps pinned pi-amber.png');
    await waitForLoadedThumb(pinnedSongTile, 'delayed-caps pinned pi-song.flac');

    // 2. Folder listing mounts while pi's capability reply is held back on a fresh reload.
    piCapsGate = makeGate();
    await page.goto(`${server.url}/#/files/pi/${encodeURIComponent(piGallery).replace(/%2F/g, '/')}`, {
      waitUntil: 'domcontentloaded',
    });
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'gallery' }).waitFor({ timeout: 10_000 });
    await piCapsGate.waitArrived();
    assert.equal(await page.locator('.grid a.cell img').count(), 0, 'Listing grid should wait until pi caps arrive');
    assert.equal(await page.locator('.list a.row img').count(), 0, 'Listing song cover should wait until pi caps arrive');
    piCapsGate.release();
    for (const name of ['pi-amber.png', 'pi-violet.png']) {
      const cell = page.locator(`.grid a.cell[aria-label="${name}"]`);
      await cell.waitFor({ timeout: 10_000 });
      await waitForLoadedThumb(cell, `delayed-caps listing ${name}`);
    }
    const songRow = page.locator('.list a.row', { hasText: 'pi-song.flac' });
    await songRow.waitFor({ timeout: 10_000 });
    await waitForLoadedThumb(songRow, 'delayed-caps listing pi-song.flac');

    // 3. Viewer renders while legacy's capability reply is held back; releasing loads the fs.thumb preview.
    legacyCapsGate = makeGate();
    await page.goto(
      `${server.url}/#/files/legacy/${encodeURIComponent(legacyMedia).replace(/%2F/g, '/')}?view=${encodeURIComponent(legacyIconPath)}`,
      { waitUntil: 'domcontentloaded' },
    );
    await page.locator('.viewer').waitFor({ timeout: 10_000 });
    await legacyCapsGate.waitArrived();
    assert.equal(await page.locator('.viewer img.viewer-img').count(), 0, 'Viewer should wait until legacy caps arrive');
    legacyCapsGate.release();
    const viewerImg = page.locator('.viewer img.viewer-img');
    await viewerImg.waitFor({ timeout: 10_000 });
    await page.waitForFunction(
      () => {
        const el = document.querySelector('.viewer img.viewer-img');
        return Boolean(el && el.complete && el.naturalWidth === 512 && el.naturalHeight === 512);
      },
      null,
      { timeout: 10_000 },
    );
    const viewerSrc = await viewerImg.getAttribute('src');
    assert.ok(
      viewerSrc && viewerSrc.startsWith('data:image/'),
      `Expected data:image/ fs.thumb preview after delayed caps, got ${String(viewerSrc).slice(0, 40)}`,
    );

    const unexpectedConsole = consoleErrors.filter((msg) => !msg.includes('status of 501'));
    assert.deepEqual(unexpectedConsole, [], `Unexpected console/CSP errors: ${unexpectedConsole.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);

    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
