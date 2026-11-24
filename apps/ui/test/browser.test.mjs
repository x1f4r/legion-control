// End-to-end browser test for the Legion web interface served by a real `legion serve`.
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

  fs.writeFileSync(
    path.join(piHome, 'config', 'node.toml'),
    'name = "pi"\nsystem = "linux"\nbusy = "none"\n',
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
    localStorage.setItem('legion.theme', t);
    document.documentElement.dataset.theme = t;
  }, theme);
  await page.waitForTimeout(80);
}

async function maybeShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}.png`) });
}

test('real legion serve UI end-to-end on phone (390x844) and desktop (1100x720)', async () => {
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
    await maybeShot(page, 'real-phone-dark-start');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-start');
    await setTheme(page, 'dark');

    // Enter token and connect.
    await page.getByText('What a token allows').waitFor();
    await page.getByLabel('Token', { exact: true }).fill(fleet.token);
    await page.getByRole('button', { name: 'Connect' }).click();

    // Machines appear (both local tower and remote pi over command pipe).
    await page.getByText('Machines · 2 of 2 answering').waitFor({ timeout: 15_000 });
    await page.locator('a.row', { hasText: 'tower' }).waitFor();
    await page.locator('a.row', { hasText: 'pi' }).waitFor();
    await page.locator('a.row .meters').nth(1).waitFor({ timeout: 5_000 });
    await maybeShot(page, 'real-phone-dark-machines');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-machines');
    await setTheme(page, 'dark');

    // Open a machine -> Overview.
    await page.locator('a.row', { hasText: 'tower' }).first().click();
    await page.getByRole('tab', { name: 'Overview', selected: true }).waitFor();
    await page.getByText('Processor').waitFor();
    await maybeShot(page, 'real-phone-dark-machine-overview');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-machine-overview');
    await setTheme(page, 'dark');

    // Run tab: run echo and observe streamed output.
    await page.getByRole('tab', { name: 'Run' }).click();
    await page.getByLabel('Command').fill('echo hello-stream-one; sleep 0.15; echo hello-stream-two');
    await page.getByRole('button', { name: 'Run', exact: true }).click();
    await page.locator('pre.out').getByText('hello-stream-two').waitFor({ timeout: 10_000 });
    const runOut = await page.locator('pre.out').innerText();
    assert.match(runOut, /hello-stream-one/);
    assert.match(runOut, /hello-stream-two/);
    assert.match(runOut, /exit 0/);
    await maybeShot(page, 'real-phone-dark-machine-run');

    // Start a background job using the switch and stop it.
    await page.getByLabel(/Keep running after I leave/i).check();
    await page.getByLabel('Command').fill('echo job-live-line; sleep 30');
    await page.getByRole('button', { name: 'Run', exact: true }).click();
    await page.getByRole('button', { name: 'Follow output' }).click();
    await page.locator('pre.out').getByText('job-live-line').waitFor({ timeout: 10_000 });
    await maybeShot(page, 'real-phone-dark-job-follow');
    await page.getByRole('button', { name: 'Stop job' }).click();
    await page.getByText('cancelled').first().waitFor({ timeout: 10_000 });

    // Files tab: browse docs/ and preview hello.txt.
    await page.getByRole('tab', { name: 'Files' }).click();
    await page.locator('button.row', { hasText: 'docs/' }).click();
    await page.locator('.crumbs a', { hasText: 'docs' }).waitFor();
    await page.getByRole('button', { name: 'Copy path' }).waitFor();
    await page.locator('button.row', { hasText: 'hello.txt' }).click();
    await page.locator('pre.out').getByText('Hello from Legion real server preview.').waitFor();
    await page.locator('button.btn.quiet', { hasText: 'Back to folder' }).waitFor();
    await maybeShot(page, 'real-phone-dark-machine-files');

    // Power tab screenshot and verb-labeled buttons.
    await page.getByRole('tab', { name: 'Power' }).click();
    await page.getByText('Power actions').waitFor();
    await page.getByText('Loading power state…').waitFor({ state: 'hidden' });
    await page.getByRole('button', { name: 'Sleep', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Restart', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Shut down', exact: true }).waitFor();
    await maybeShot(page, 'real-phone-dark-machine-power');

    // Jobs top-level tab.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Jobs' }).click();
    await page.getByText('job-live-line').first().waitFor();
    await maybeShot(page, 'real-phone-dark-jobs');

    // Tools tab: unavailable admin tool is listed, read tool runs with on: all.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tools' }).click();
    await page.getByLabel('Search tools').fill('token.add');
    await page.locator('a.row.off', { hasText: 'token.add' }).waitFor();
    await page.getByLabel('Search tools').fill('meta.version');
    await page.locator('a.row', { hasText: 'meta.version' }).first().click();
    await page.locator('#tool-target-on').selectOption('all');
    await page.getByRole('button', { name: 'Run', exact: true }).click();
    await page.locator('.label', { hasText: /^pi$/i }).waitFor({ timeout: 10_000 });
    await page.locator('.label', { hasText: /^tower$/i }).waitFor();
    await maybeShot(page, 'real-phone-dark-tools-fanout');

    // Power tool: long description collapses behind disclosure, inline code has no raw backticks,
    // dry run runs without confirmation, real button opens plain-word confirmation sheet.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tools' }).click();
    await page.getByLabel('Search tools').fill('power.restart');
    await page.locator('a.row', { hasText: 'power.restart' }).first().click();
    const ledeText = await page.locator('.lede').innerText();
    assert.ok(!ledeText.includes('`'), `raw backtick in tool lede: ${ledeText}`);
    const moreDetails = page.locator('details.more', { hasText: 'More about this tool' });
    if ((await moreDetails.count()) > 0) {
      assert.equal(await moreDetails.first().getAttribute('open'), null);
      await moreDetails.first().locator('summary').click();
      assert.notEqual(await moreDetails.first().getAttribute('open'), null);
      await moreDetails.first().locator('summary').click();
    }
    await page.getByRole('button', { name: 'Try it (dry run)' }).click();
    await page.locator('dl.kv').getByText('restart').first().waitFor({ timeout: 10_000 });
    const dryText = await page.locator('.main').innerText();
    assert.match(dryText, /dry_run/);
    assert.match(dryText, /scheduled\s+no/i);

    await page.getByRole('button', { name: 'Restart tower', exact: true }).click();
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    await dialog.getByRole('heading', { name: 'Restart tower now?' }).waitFor();
    await maybeShot(page, 'real-phone-dark-power-sheet');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-power-sheet');
    await setTheme(page, 'dark');
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await dialog.waitFor({ state: 'hidden' });

    // Confirm nothing was scheduled on tower.
    const powerStatus = JSON.parse(
      runLegion(['power.status', '--json'], fleet.towerHome, fleet.towerUser),
    );
    assert.equal(powerStatus.pending, undefined);

    // This device screen: multi-line allows + calm Forget this browser with confirmation sheet.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: /device/i }).click();
    await page.getByText(/May not change settings or permissions/i).waitFor();
    const forgetBtn = page.getByRole('button', { name: 'Forget this browser' });
    await forgetBtn.waitFor();
    assert.ok(!(await forgetBtn.getAttribute('class')).includes('danger'));
    await maybeShot(page, 'real-phone-dark-device');
    await setTheme(page, 'light');
    await maybeShot(page, 'real-phone-light-device');
    await setTheme(page, 'dark');
    await forgetBtn.click();
    const forgetDialog = page.getByRole('dialog');
    await forgetDialog.waitFor();
    await forgetDialog.getByText('This browser will need a token again.').waitFor();
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

    // Verify one-time #token= URL fragment login removes the token from the address bar immediately.
    await wide.goto(`${server.url}/#token=${encodeURIComponent(fleet.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await wide.getByText('Machines · 2 of 2 answering').waitFor({ timeout: 15_000 });
    assert.ok(!wide.url().includes(fleet.token), `token leaked in URL: ${wide.url()}`);

    // Wide split view shows machine list beside selected machine overview.
    await wide.locator('.split h2.pane-title', { hasText: 'tower' }).waitFor();
    await wide.locator('a.row .meters').nth(1).waitFor({ timeout: 5_000 });
    await maybeShot(wide, 'real-wide-dark-machines');
    await setTheme(wide, 'light');
    await maybeShot(wide, 'real-wide-light-machines');
    await setTheme(wide, 'dark');

    await wide.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tools' }).click();
    await wide.locator('a.row', { hasText: 'power.restart' }).first().click();
    await wide.locator('.split h2.pane-title', { hasText: 'power.restart' }).waitFor();
    await maybeShot(wide, 'real-wide-dark-tools');
    await setTheme(wide, 'light');
    await maybeShot(wide, 'real-wide-light-tools');
    await setTheme(wide, 'dark');

    await wide.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: /Device/i }).click();
    await wide.getByText('Forget this browser').waitFor();
    await maybeShot(wide, 'real-wide-dark-device');

    await wideCtx.close();

    // ── Mock mode checks (unreachable machine detail, remote tool error, pairing defaults) ────
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

      await p.goto(`${server.url}/?mock=pair`, { waitUntil: 'domcontentloaded' });
      await p.getByText(/Pair this device/).waitFor();
      assert.equal(await p.getByLabel('Name this device').inputValue(), 'Pixel 10');
      await maybeShot(p, `mock-${label}-dark-start-pair`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-start-pair`);
      }

      if (SHOTS_DIR) {
        await p.goto(`${server.url}/?mock=token`, { waitUntil: 'domcontentloaded' });
        await p.getByText(/Connect this browser/).waitFor();
        await setTheme(p, 'dark');
        await maybeShot(p, `mock-${label}-dark-start-token`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-start-token`);
      }

      await p.goto(`${server.url}/?mock`, { waitUntil: 'domcontentloaded' });
      await p.getByText('Machines · 3 of 4 answering').waitFor();
      await setTheme(p, 'dark');
      await maybeShot(p, `mock-${label}-dark-machines`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-machines`);
        await setTheme(p, 'dark');
      }

      // Unreachable machine detail shows full message and hint.
      await p.goto(`${server.url}/?mock#/machines/laptop`, { waitUntil: 'domcontentloaded' });
      await p.getByText(/SSH timed out after 8 s/i).waitFor();
      await p.getByText(/Check that laptop is powered on/i).waitFor();
      await maybeShot(p, `mock-${label}-dark-laptop`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-laptop`);
        await setTheme(p, 'dark');
      }

      // Remote tool target shows honest note and displays remote error cleanly.
      await p.goto(`${server.url}/?mock#/tools/audit.tail`, { waitUntil: 'domcontentloaded' });
      await p.locator('#tool-target-on').selectOption('pi');
      await p.getByText(/pi checks its own tools and permissions\s+when you run/i).waitFor();
      await p.getByRole('button', { name: 'Run', exact: true }).click();
      await p.locator('.notice.bad').getByText(/may not call audit\.tail on pi/i).waitFor();
      await maybeShot(p, `mock-${label}-dark-remote-error`);
      if (SHOTS_DIR) {
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-remote-error`);
        await setTheme(p, 'dark');
      }

      if (SHOTS_DIR) {
        await p.goto(`${server.url}/?mock#/machines/tower`, { waitUntil: 'domcontentloaded' });
        await p.getByText('Processor').waitFor();
        await setTheme(p, 'dark');
        await maybeShot(p, `mock-${label}-dark-overview`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-overview`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/machines/tower/run`, { waitUntil: 'domcontentloaded' });
        await p.getByLabel('Command').fill('df -h /');
        await p.getByRole('button', { name: 'Run', exact: true }).click();
        await p.locator('pre.out').getByText('/dev/nvme0n1p5').waitFor();
        await maybeShot(p, `mock-${label}-dark-run`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-run`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/machines/tower/files`, { waitUntil: 'domcontentloaded' });
        await p.locator('button.row', { hasText: 'projects/' }).waitFor();
        await maybeShot(p, `mock-${label}-dark-files`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-files`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/jobs`, { waitUntil: 'domcontentloaded' });
        await p.getByText('cargo build --release').first().waitFor();
        await maybeShot(p, `mock-${label}-dark-jobs`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-jobs`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/tools/power.restart`, { waitUntil: 'domcontentloaded' });
        await p.getByRole('button', { name: 'Restart tower', exact: true }).click();
        await p.getByRole('dialog').waitFor();
        await maybeShot(p, `mock-${label}-dark-tools-sheet`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-tools-sheet`);
        await setTheme(p, 'dark');

        await p.goto(`${server.url}/?mock#/device`, { waitUntil: 'domcontentloaded' });
        await p.getByText('Forget this browser').waitFor();
        await maybeShot(p, `mock-${label}-dark-device`);
        await setTheme(p, 'light');
        await maybeShot(p, `mock-${label}-light-device`);
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

test('fast start with an offline dual-boot machine paints this node immediately and caches across visits', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const fleet = setupFleet(root);

  // Add a dead dual-boot machine whose two command transports never answer.
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

    // This node paints immediately (well under 2 seconds), never waiting for the dead machine.
    await page.locator('a.row:not(.asking):not(.off)', { hasText: 'tower' }).waitFor({ timeout: 2000 });
    const tTower = Math.round(performance.now() - tClick);
    assert.ok(tTower < 2000, `tower took ${tTower} ms to appear`);

    // The dead machine appears as a quiet "asking…" row while pi answers on its own.
    await page.locator('a.row.asking', { hasText: 'dead' }).waitFor({ timeout: 2000 });
    await page.locator('a.row:not(.asking):not(.off)', { hasText: 'pi' }).waitFor({ timeout: 2000 });
    const tPi = Math.round(performance.now() - tClick);
    assert.ok(tPi < 2000, `pi took ${tPi} ms to appear`);
    await maybeShot(page, 'real-phone-dark-machines-asking');

    // Full list settles after a single ping timeout for the dead dual-boot machine.
    await page.locator('a.row.off', { hasText: 'dead' }).waitFor({ timeout: 10_000 });
    await page.getByText('Machines · 2 of 3 answering').waitFor({ timeout: 5000 });
    const tSettled = Math.round(performance.now() - tClick);
    assert.ok(tSettled < 8000, `fleet took ${tSettled} ms to settle (expected one timeout)`);
    await maybeShot(page, 'real-phone-dark-machines-offline');

    // Jobs view queries answering machines only and paints without waiting on dead.
    const tJobs0 = performance.now();
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Jobs' }).click();
    await page.getByText('No background jobs recorded.').waitFor({ timeout: 2000 });
    const tJobs = Math.round(performance.now() - tJobs0);
    assert.ok(tJobs < 2000, `Jobs view took ${tJobs} ms`);

    // Target picker labels the offline machine and "All answering machines" skips it.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tools' }).click();
    await page.getByLabel('Search tools').fill('meta.version');
    await page.locator('a.row', { hasText: 'meta.version' }).first().click();
    const deadOpt = await page.locator('#tool-target-on option[value="dead"]').textContent();
    assert.match(deadOpt || '', /offline/i);
    await page.locator('#tool-target-on').selectOption('all');
    await page.getByText(/skipping dead/i).waitFor();
    const tFan0 = performance.now();
    await page.getByRole('button', { name: 'Run', exact: true }).click();
    await page.locator('.label', { hasText: /^pi$/i }).waitFor({ timeout: 2000 });
    await page.locator('.label', { hasText: /^tower$/i }).waitFor({ timeout: 2000 });
    const tFan = Math.round(performance.now() - tFan0);
    assert.ok(tFan < 2000, `All-answering fan-out took ${tFan} ms`);

    // Persistent localStorage cache stores non-secret machine metadata and no token.
    const cachedRaw = await page.evaluate(() => localStorage.getItem('legion.cache.fleet'));
    assert.ok(cachedRaw, 'expected legion.cache.fleet in localStorage');
    assert.ok(!cachedRaw.includes(fleet.token), 'token must never be written to fleet cache');
    const cached = JSON.parse(cachedRaw);
    assert.equal(cached.value.node, 'tower');
    assert.equal(cached.value.machines.length, 3);

    // Return visit paints cached machines immediately with "as of …" while refreshing.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Machines' }).click();
    const tReload0 = performance.now();
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.getByText(/as of /i).waitFor({ timeout: 2000 });
    await page.locator('a.row', { hasText: 'dead' }).waitFor({ timeout: 2000 });
    const tReload = Math.round(performance.now() - tReload0);
    assert.ok(tReload < 2000, `Cached return paint took ${tReload} ms`);
    await maybeShot(page, 'real-phone-dark-machines-cached');
    await page.locator('a.row.off', { hasText: 'dead' }).waitFor({ timeout: 10_000 });

    // Forgetting this browser clears the persistent fleet cache too.
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: /device/i }).click();
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
