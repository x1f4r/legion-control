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
import { chromium } from 'playwright-core';

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

async function startServer(legionHome, userHome) {
  const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  const child = spawn(LEGION_BIN, ['serve', '--listen', '127.0.0.1:0'], {
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
  /\b(?:power|fs|exec|job|svc|proj|fleet|sys|meta|device|token|config|net|env)\.[a-z_]+\b/g;

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

    // Machines appear with labelled Processor / Memory / Disk, no wordmark/role chip/refresh button.
    await page.getByText('2 are on').waitFor({ timeout: 15_000 });
    assert.equal(await page.locator('.wordmark').count(), 0);
    assert.equal(await page.locator('header.top .chip').count(), 0);
    assert.equal(await page.locator('button[aria-label="Refresh"]').count(), 0);
    await page.locator('header.top a[aria-label="Settings"]').waitFor();
    assert.equal(await page.locator('nav.nav a').count(), 3, 'Root bottom bar has 3 places');

    const towerCard = page.locator('a.machine', { hasText: 'tower' });
    await towerCard.waitFor();
    await towerCard.getByText('Processor').waitFor();
    await towerCard.getByText('Memory').waitFor();
    await towerCard.getByText('Disk').waitFor();

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

    // Horizontal swipe across root places: Machines -> Activity -> Actions -> Machines.
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Actions' }).waitFor();
    await dispatchSwipe(page, 'main.main', { startX: 100, startY: 360, endX: 300, endY: 365 });
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
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
    await assertNoToolNames(page, 'real phone tower overview');
    await maybeShot(page, 'real-phone-dark-machine-overview');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-machine-overview');
    await setTheme(page, 'dark');

    // Horizontal swipe across machine parts: Overview -> Files -> Power -> More.
    await dispatchSwipe(page, 'main.main', { startX: 300, startY: 360, endX: 100, endY: 362 });
    await page.locator('nav.nav.parts a[aria-current="page"]', { hasText: 'Files' }).waitFor();
    await page.locator('button.row', { hasText: 'docs/' }).click();
    await page.locator('button.row', { hasText: 'hello.txt' }).click();
    await page.locator('pre.out').getByText('Hello from Legion real server preview.').waitFor();
    await assertNoToolNames(page, 'real phone tower files');
    await maybeShot(page, 'real-phone-dark-machine-files');

    // Power part: plain dry run shows one plain sentence, no "Dry run commands" or raw commands.
    await page.locator('nav.nav.parts a', { hasText: 'Power' }).click();
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
    await wide.getByText('2 are on').waitFor({ timeout: 15_000 });
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
      await p.getByText('3 are on, 2 are off').waitFor();
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

      // Offline Wake button opens plain confirm sheet; confirming shows pending-wake feedback.
      await laptopRow.getByRole('button', { name: 'Wake' }).click();
      const wakeDialog = p.getByRole('dialog');
      await wakeDialog.waitFor();
      await wakeDialog.getByRole('heading', { name: 'Wake laptop?' }).waitFor();
      await wakeDialog.getByText('Send a wake signal to laptop.').waitFor();
      await wakeDialog.getByRole('button', { name: 'Try it (dry run)' }).click();
      await wakeDialog.locator('.notice.neutral', { hasText: 'A wake signal would be sent to laptop.' }).waitFor();
      assert.equal(await wakeDialog.locator('pre.out').count(), 0, 'Plain Wake dry run must not show raw WoL details');
      await assertNoToolNames(p, `mock ${label} wake confirm sheet`);
      await wakeDialog.getByRole('button', { name: 'Wake', exact: true }).click();
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
      await p.getByText('Pixel 10').waitFor();
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
          return el && el.value === 'Pixel 10';
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
      !connectionsText.includes('device:pixel-10'),
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

    await ctx.close();
    assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
