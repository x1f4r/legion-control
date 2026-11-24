#!/usr/bin/env node
// Playwright CDP driver for apps/legion-app/e2e-android.sh (two-node fleet suite).
// Drives the real Android WebView over `adb forward` + Chrome DevTools Protocol.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
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
const RUN_DIR = requiredEnv('E2E_RUN_DIR');
const LEGION_BIN = requiredEnv('E2E_LEGION_BIN');
const LINK_BIN = requiredEnv('E2E_LINK_BIN');
const SSHD_BIN = requiredEnv('E2E_SSHD_BIN');
const MODE = process.env.E2E_MODE || 'phone';

const NODES = {
  tower: {
    name: 'tower',
    home: requiredEnv('E2E_NODE_A_HOME'),
    port: Number(requiredEnv('E2E_PORT_A')),
    sshdCfg: requiredEnv('E2E_SSHD_A_CFG'),
    sshdLog: requiredEnv('E2E_SSHD_A_LOG'),
    pidFile: requiredEnv('E2E_SSHD_A_PIDFILE'),
  },
  pi: {
    name: 'pi',
    home: requiredEnv('E2E_NODE_B_HOME'),
    port: Number(requiredEnv('E2E_PORT_B')),
    sshdCfg: requiredEnv('E2E_SSHD_B_CFG'),
    sshdLog: requiredEnv('E2E_SSHD_B_LOG'),
    pidFile: requiredEnv('E2E_SSHD_B_PIDFILE'),
  },
};

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

function legionNode(nodeName, args) {
  const node = NODES[nodeName];
  assert.ok(node, `Unknown node ${nodeName}`);
  return execFileSync(LEGION_BIN, args, {
    encoding: 'utf8',
    env: { ...process.env, LEGION_HOME: node.home },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function createInvite(nodeName, { name, role, allowOwner = false }) {
  const node = NODES[nodeName];
  const args = [
    'pair',
    '--name',
    name,
    '--role',
    role,
    '--host',
    '10.0.2.2',
    '--port',
    String(node.port),
    '--json',
  ];
  if (allowOwner) args.push('--allow-owner');
  const out = legionNode(nodeName, args);
  const parsed = JSON.parse(out);
  assert.ok(
    typeof parsed.invite === 'string' && parsed.invite.startsWith('legion-invite-'),
    `Unexpected invite payload: ${out}`,
  );
  return parsed.invite;
}

function readPidFile(pidFile) {
  if (!fs.existsSync(pidFile)) return null;
  const raw = fs.readFileSync(pidFile, 'utf8').trim();
  const n = Number(raw);
  return Number.isInteger(n) && n > 1 ? n : null;
}

function findGatePids(nodeHome) {
  const pids = [];
  for (const entry of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    const pid = Number(entry);
    if (pid === process.pid) continue;
    try {
      const cmdline = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8');
      if (!cmdline.includes(LEGION_BIN) || (!cmdline.includes('gate') && !cmdline.includes('rpc'))) {
        continue;
      }
      const environ = fs.readFileSync(`/proc/${pid}/environ`, 'utf8');
      if (environ.includes(`LEGION_HOME=${nodeHome}\0`)) {
        pids.push(pid);
      }
    } catch {
      // Process exited or belongs to another user.
    }
  }
  return pids;
}

function findChildPids(parentPid) {
  if (!parentPid) return [];
  const pids = [];
  for (const entry of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    const pid = Number(entry);
    if (pid === parentPid) continue;
    try {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
      const closeParen = stat.lastIndexOf(')');
      if (closeParen < 0) continue;
      const fields = stat.slice(closeParen + 2).split(' ');
      if (Number(fields[1]) === parentPid) pids.push(pid);
    } catch {
      // Process exited or unreadable.
    }
  }
  return pids;
}

async function killNodeSessions(nodeName) {
  const node = NODES[nodeName];
  const sshdPid = readPidFile(node.pidFile);
  const targets = [...new Set([...findGatePids(node.home), ...findChildPids(sshdPid)])];
  for (const pid of targets) {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      // Already exited.
    }
  }
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (findGatePids(node.home).length === 0 && findChildPids(sshdPid).length === 0) break;
    await sleep(100);
  }
  log(`Killed ${targets.length} session process(es) on ${nodeName}`);
}

async function waitForPortState(port, wantOpen, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const isOpen = await new Promise((resolve) => {
      const s = net.connect(port, '127.0.0.1');
      s.once('connect', () => {
        s.destroy();
        resolve(true);
      });
      s.once('error', () => {
        s.destroy();
        resolve(false);
      });
    });
    if (isOpen === wantOpen) return;
    await sleep(100);
  }
  throw new Error(`Timed out waiting for 127.0.0.1:${port} to be ${wantOpen ? 'open' : 'closed'}`);
}

async function stopSshd(nodeName) {
  const node = NODES[nodeName];
  const pid = readPidFile(node.pidFile);
  await killNodeSessions(nodeName);
  if (pid) {
    try {
      process.kill(pid, 'SIGTERM');
    } catch {
      // Already stopped.
    }
  }
  try {
    fs.unlinkSync(node.pidFile);
  } catch {
    // Ignore if already removed.
  }
  await waitForPortState(node.port, false);
  log(`Stopped sshd for ${nodeName} (port ${node.port})`);
}

async function startSshd(nodeName) {
  const node = NODES[nodeName];
  const existing = readPidFile(node.pidFile);
  if (existing) {
    try {
      process.kill(existing, 0);
      await waitForPortState(node.port, true);
      return;
    } catch {
      // Stale pidfile.
    }
  }
  const child = spawn(SSHD_BIN, ['-D', '-e', '-f', node.sshdCfg, '-E', node.sshdLog], {
    detached: true,
    stdio: 'ignore',
  });
  child.unref();
  fs.writeFileSync(node.pidFile, `${child.pid}\n`);
  await waitForPortState(node.port, true);
  log(`Started sshd for ${nodeName} (pid ${child.pid}, port ${node.port})`);
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
      if (msg.type() === 'error') log(`console.error: ${msg.text()}`);
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

async function waitForMachineWithStats(page, machineName, timeoutMs = 30000) {
  await page.locator('.start-heading').waitFor({ state: 'detached', timeout: timeoutMs }).catch(() => {});
  const card = page.locator('a.machine', { hasText: machineName }).first();
  await card.waitFor({ state: 'visible', timeout: timeoutMs });
  const stats = card.locator('.stats');
  await stats.waitFor({ state: 'visible', timeout: timeoutMs });
  const text = await stats.innerText();
  assert.match(text, /Processor[\s\S]*\d+%/, `Unexpected stats text for ${machineName}: ${text}`);
  assert.match(text, /Memory[\s\S]*\d+%/, `Unexpected stats text for ${machineName}: ${text}`);
  assert.match(text, /Disk[\s\S]*\d+%/, `Unexpected stats text for ${machineName}: ${text}`);
  return card;
}

async function triggerPullRefresh(page) {
  // Pull-down gesture on main.main (triggers forceOffline refresh + background learn).
  const box = await page.locator('main.main').boundingBox();
  assert.ok(box, 'Expected main.main bounding box');
  const cx = Math.round(box.x + box.width / 2);
  const sy = Math.round(box.y + 40);
  const ey = Math.round(box.y + 220);
  await page.evaluate(
    ({ x, startY, endY }) => {
      const target = document.querySelector('main.main');
      if (!target) return;
      target.scrollTop = 0;
      const down = new PointerEvent('pointerdown', {
        bubbles: true,
        pointerId: 1,
        button: 0,
        clientX: x,
        clientY: startY,
      });
      const move = new PointerEvent('pointermove', {
        bubbles: true,
        pointerId: 1,
        button: 0,
        clientX: x,
        clientY: endY,
      });
      const up = new PointerEvent('pointerup', {
        bubbles: true,
        pointerId: 1,
        button: 0,
        clientX: x,
        clientY: endY,
      });
      target.dispatchEvent(down);
      target.dispatchEvent(move);
      target.dispatchEvent(up);
    },
    { x: cx, startY: sy, endY: ey },
  );
}

async function runCommandOnMachine(page, machineName, cmdText, expectedOutput) {
  await page.evaluate((m) => {
    window.location.hash = `#/machines/${m}/command`;
  }, machineName);
  const cmdInput = page.locator('.dock input[aria-label="Command"]');
  await cmdInput.waitFor({ state: 'visible', timeout: 15000 });
  await cmdInput.fill(cmdText);
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.locator('pre.out', { hasText: expectedOutput }).waitFor({
    state: 'visible',
    timeout: 20000,
  });
}

function getDeviceFingerprint(nodeName, deviceName) {
  const out = legionNode(nodeName, ['device.list', '--json']);
  const parsed = JSON.parse(out);
  const found = (parsed.devices || []).find((d) => d.name === deviceName);
  return found?.fingerprint || null;
}

function enrolSecondDeviceFromHost(inviteText, name) {
  const invFile = path.join(RUN_DIR, `second-invite-${Date.now()}.txt`);
  const keyFile = path.join(RUN_DIR, `second-key-${Date.now()}.pem`);
  const cfgFile = path.join(RUN_DIR, `second-cfg-${Date.now()}.json`);
  fs.writeFileSync(invFile, `${inviteText.trim()}\n`, { mode: 0o600 });
  execFileSync(LINK_BIN, ['keygen', keyFile], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  const out = execFileSync(
    LINK_BIN,
    ['enrol', '--invite-file', invFile, '--key', keyFile, '--name', name, '--out', cfgFile],
    { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
  );
  return JSON.parse(out);
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

  // 2. Pair with A (tower) as owner; verify fleet sync summary shows pi received it, and both nodes load.
  log('Step 2: pairing with tower and verifying background fleet learning');
  const invite1 = createInvite('tower', { name: 'pixel-e2e', role: 'owner', allowOwner: true });
  await pairViaUi(page, invite1, 'pixel-e2e');
  await waitForMachineWithStats(page, 'tower');
  await waitForMachineWithStats(page, 'pi');
  await page.getByText('Paired with tower and shared with pi.').waitFor({ state: 'visible', timeout: 15000 });
  assert.equal(await page.locator('nav.nav a').count(), 3, 'Expected 3 root bottom nav items');
  await takeShot(page, '02-paired-fleet.png');

  // 3. Settings -> Connections shows BOTH tower and pi as "directly".
  log('Step 3: verifying Settings -> Connections shows direct routes to both tower and pi');
  await page.evaluate(() => {
    window.location.hash = '#/settings/connections';
  });
  await page.getByText('Paired machines').waitFor({ state: 'visible', timeout: 15000 });
  await page.locator('a.row', { hasText: 'tower' }).getByText('directly').waitFor({ timeout: 20000 });
  await page.locator('a.row', { hasText: 'pi' }).getByText('directly').waitFor({ timeout: 20000 });
  const connText = await page.locator('main.main').innerText();
  const connLeaks = connText.match(/\b(?:power|fs|exec|job|svc|proj|fleet|sys|meta|device|token|config)\.[a-z_]+\b/g) || [];
  assert.deepEqual(connLeaks, [], `Tool name(s) leaked in plain Connections: ${connLeaks.join(', ')}`);
  await takeShot(page, '03-connections-direct.png');

  // 4. Run a command on B (pi) and confirm it connected DIRECTLY to pi's sshd using pixel-e2e's key.
  log('Step 4: running a command on pi and proving it went directly to pi');
  const pixelFp = getDeviceFingerprint('pi', 'pixel-e2e');
  assert.ok(pixelFp, 'Expected pixel-e2e to be present on pi after pairing with tower');
  await runCommandOnMachine(page, 'pi', 'echo direct-to-pi', 'direct-to-pi');
  const sshdBLog = fs.readFileSync(NODES.pi.sshdLog, 'utf8');
  assert.ok(
    sshdBLog.includes(pixelFp),
    `Expected pi sshd log to record direct authentication by ${pixelFp}`,
  );
  log(`Confirmed direct SSH authentication by ${pixelFp} in pi sshd log`);
  await takeShot(page, '04-direct-command-pi.png');

  // 5. Stop A (tower)'s sshd: Machines still shows pi on with live metrics, tower shows under Off,
  //    and commands on pi continue working!
  log('Step 5: stopping tower sshd and verifying pi keeps working directly');
  await stopSshd('tower');
  await page.evaluate(() => {
    window.location.hash = '#/machines';
  });
  await page.locator('header.top h1', { hasText: 'Machines' }).waitFor({ state: 'visible', timeout: 10000 });
  await triggerPullRefresh(page);
  await page.locator('.machine.off', { hasText: 'tower' }).waitFor({ state: 'visible', timeout: 25000 });
  await waitForMachineWithStats(page, 'pi');
  await takeShot(page, '05-tower-off-machines.png');

  await runCommandOnMachine(page, 'pi', 'echo pi-while-tower-off', 'pi-while-tower-off');
  await takeShot(page, '06-pi-works-tower-off.png');

  // 6. Start A (tower)'s sshd, stop B (pi)'s sshd: tower works, pi shows under Off.
  log('Step 6: starting tower sshd, stopping pi sshd, and verifying tower keeps working');
  await startSshd('tower');
  await stopSshd('pi');
  await page.evaluate(() => {
    window.location.hash = '#/machines';
  });
  await page.locator('header.top h1', { hasText: 'Machines' }).waitFor({ state: 'visible', timeout: 10000 });
  await triggerPullRefresh(page);
  await waitForMachineWithStats(page, 'tower');
  await page.locator('.machine.off', { hasText: 'pi' }).waitFor({ state: 'visible', timeout: 25000 });
  await takeShot(page, '07-pi-off-machines.png');

  await runCommandOnMachine(page, 'tower', 'echo tower-while-pi-off', 'tower-while-pi-off');
  await takeShot(page, '08-tower-works-pi-off.png');

  // 7. Start B (pi)'s sshd again, then pair a second device from the app ("Let another device in")
  //    with a chosen role (observer) and verify it appears on both nodes.
  log('Step 7: inviting a second device from the app with chosen role and verifying fleet delivery');
  await startSshd('pi');
  await page.evaluate(() => {
    window.location.hash = '#/settings/connections/invite';
  });
  const roleSelect = page.locator('label.field select');
  await roleSelect.waitFor({ state: 'visible', timeout: 15000 });
  await page.getByText('Name for this device').waitFor({ state: 'visible' });
  await page.locator('label.field input[type="text"]').fill('tablet-peer');

  // Verify owner-strength switch gates owner role.
  await roleSelect.selectOption('owner');
  await page.getByText('Allow full owner access').waitFor({ state: 'visible' });
  const makeInviteBtn = page.getByRole('button', { name: 'Make invite' });
  assert.equal(await makeInviteBtn.isDisabled(), true, 'Owner invite must stay disabled until confirmed');

  // Choose observer role (plain description "Look only.") and create the invite.
  await roleSelect.selectOption('observer');
  await page.getByText('Look only.').waitFor({ state: 'visible' });
  const plainInviteText = await page.locator('main.main').innerText();
  const inviteLeaks = plainInviteText.match(/\b(?:power|fs|exec|job|svc|proj|fleet|sys|meta|device|token|config)\.[a-z_]+\b/g) || [];
  assert.deepEqual(inviteLeaks, [], `Tool name(s) leaked in plain invite screen: ${inviteLeaks.join(', ')}`);
  await makeInviteBtn.click();

  const codeEl = page.locator('.code code').first();
  await codeEl.waitFor({ state: 'visible', timeout: 15000 });
  const appInvite = (await codeEl.innerText()).trim();
  assert.ok(appInvite.startsWith('legion-invite-'), `Unexpected app-generated invite: ${appInvite}`);
  await takeShot(page, '09-invite-role-qr.png');

  // Enrol the second device from the host against the invite generated by the phone app.
  const enrolledSecond = enrolSecondDeviceFromHost(appInvite, 'tablet-peer');
  log(`Second device enrolled: ${JSON.stringify({ node: enrolledSecond.node, role: enrolledSecond.role })}`);

  // App detects pairing and reports both nodes received it.
  await page.getByText('tablet-peer is paired.').waitFor({ state: 'visible', timeout: 20000 });
  await page.getByText('Received by pi and tower.').waitFor({ state: 'visible', timeout: 15000 });
  await takeShot(page, '10-invite-paired-both.png');

  // Confirm both nodes independently list tablet-peer with role observer.
  for (const nodeName of ['tower', 'pi']) {
    const listOut = JSON.parse(legionNode(nodeName, ['device.list', '--json']));
    const dev = (listOut.devices || []).find((d) => d.name === 'tablet-peer');
    assert.ok(dev, `Expected tablet-peer in device.list on ${nodeName}`);
    assert.equal(dev.role, 'observer', `Expected observer role for tablet-peer on ${nodeName}`);
  }
  log('Verified tablet-peer (role observer) on both tower and pi');

  // 8. Force-stop and relaunch: still paired with both nodes, no pairing prompt.
  log('Step 8: force-stopping and relaunching app');
  await conn.close();
  adb(['shell', 'am', 'force-stop', APP_ID]);
  await sleep(1000);
  adb(['shell', 'am', 'start', '-W', '-n', ACTIVITY]);
  conn = await connectWebView();
  page = conn.page;
  await waitForMachineWithStats(page, 'tower');
  // pi was cached as off during Step 6; pull to refresh checks off machines immediately.
  await triggerPullRefresh(page);
  await waitForMachineWithStats(page, 'pi');
  assert.equal(
    await page.locator('.start-heading').count(),
    0,
    'Pairing screen should not appear after force-stop and relaunch',
  );
  await takeShot(page, '11-after-relaunch.png');

  // 9. Remove this phone device on B (pi) while A (tower) is stopped:
  //    app is refused by B; then after A starts and syncs, refused by A too!
  log('Step 9: removing phone on pi while tower is stopped, verifying refusal by pi');
  await stopSshd('tower');
  const rmOut = legionNode('pi', ['device.remove', 'name=pixel-e2e']);
  log(`device.remove on pi: ${rmOut.trim().replace(/\s+/g, ' ')}`);
  await killNodeSessions('pi');

  await page.evaluate(() => {
    window.location.hash = '#/machines/pi/command';
  });
  const piCmd = page.locator('.dock input[aria-label="Command"]');
  await piCmd.waitFor({ state: 'visible', timeout: 15000 });
  await piCmd.fill('echo after-revoke-on-b');
  await page.locator('.dock form.prompt button[type="submit"]').click();
  await page.getByRole('button', { name: 'Pair again' }).waitFor({ state: 'visible', timeout: 20000 });
  await takeShot(page, '12-revoked-on-b.png');

  log('Step 10: starting tower, syncing revocation from pi to tower, and verifying refusal by tower');
  await startSshd('tower');
  const syncOut = legionNode('pi', [
    'fleet.sync',
    'direction=push',
    'documents=["devices.toml"]',
    '--json',
  ]);
  log(`fleet.sync from pi to tower: ${syncOut.trim().replace(/\s+/g, ' ')}`);
  await killNodeSessions('tower');
  await killNodeSessions('pi');

  // Reload the app so state() probes tower/pi fresh: both refuse the removed device.
  await page.evaluate(() => {
    window.location.hash = '#/machines';
    window.location.reload();
  });
  await page.getByRole('button', { name: 'Pair again' }).waitFor({ state: 'visible', timeout: 25000 });
  const afterSyncListA = JSON.parse(legionNode('tower', ['device.list', '--json']));
  assert.ok(
    !(afterSyncListA.devices || []).some((d) => d.name === 'pixel-e2e'),
    'Expected pixel-e2e to be removed on tower after sync',
  );
  await takeShot(page, '13-revoked-on-a-after-sync.png');

  await conn.close();
  log('All two-node phone e2e assertions passed.');
}

async function runTabletSuite() {
  const conn = await connectWebView();
  const page = conn.page;

  log('Step 11: pairing on tablet AVD for wide paired fleet screenshot');
  await page
    .getByRole('heading', { name: /Pair this device/i })
    .waitFor({ state: 'visible', timeout: 20000 });
  const invite = createInvite('tower', { name: 'tablet-e2e', role: 'operator' });
  await pairViaUi(page, invite, 'tablet-e2e');
  await waitForMachineWithStats(page, 'tower');
  await waitForMachineWithStats(page, 'pi');
  await page.waitForFunction(
    () => {
      const text = document.body.innerText || '';
      return /health/i.test(text) && text.includes('Processor') && text.includes('tower') && text.includes('pi');
    },
    null,
    { timeout: 20000 },
  );
  await takeShot(page, '14-tablet-paired-fleet.png');

  await conn.close();
  log('Tablet wide paired fleet assertion passed.');
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
      const logcat = adb(['logcat', '-d', '-t', '40']).trim();
      console.error(`[e2e:${MODE}] logcat tail:\n${logcat}`);
    } catch (diagErr) {
      console.error(`[e2e:${MODE}] Could not capture failure diagnostics:`, diagErr.message);
    }
  }
  for (const nodeName of ['tower', 'pi']) {
    const logPath = NODES[nodeName].sshdLog;
    if (logPath && fs.existsSync(logPath)) {
      const tail = fs.readFileSync(logPath, 'utf8').split('\n').slice(-25).join('\n');
      console.error(`[e2e:${MODE}] ${nodeName} sshd.log tail:\n${tail}`);
    }
  }
  process.exit(1);
});
