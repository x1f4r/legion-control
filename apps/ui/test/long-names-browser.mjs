// Real-server headless Chromium tests for long file/folder names:
// - Folder with 12 screen recordings differing in time, 40 IMG_ photos, 4 reports differing in the
//   middle, and 4 long extensionless names on phone (390x844) and desktop (1440x900), list & grid,
//   dark & light.
// - Uniqueness of visible drawn text, full accessible names, shared-lead dimming, search match
//   highlight without dimming, phone item sheet (full name + Copy name), viewer title rule 2,
//   desktop hover tooltips, pinned strip, and 5,000-entry lazy on-screen measurement benchmark.

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
const SHOTS_DIR = process.env.LEGION_LONG_NAMES_SHOTS_DIR || '';

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
  return fs.mkdtempSync(path.join(base, 'long-names-'));
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

  return { url, stop };
}

function buildCapturesFixture(folder) {
  fs.mkdirSync(folder, { recursive: true });
  const expected = [];

  const times = [
    '14.01.12', '14.02.04', '14.03.19', '14.04.33', '14.05.47', '14.06.08',
    '14.07.22', '14.08.35', '14.09.51', '14.10.14', '14.11.29', '14.12.58',
  ];
  for (const t of times) {
    const name = `Screen Recording 2026-11-25 at ${t}.mov`;
    fs.writeFileSync(path.join(folder, name), 'fake-mov-data');
    expected.push(name);
  }
  for (let i = 1; i <= 40; i += 1) {
    const sec = String(10 + i).padStart(2, '0');
    const name = `IMG_20261125_1403${sec}.jpg`;
    fs.writeFileSync(path.join(folder, name), 'fake-jpg-data');
    expected.push(name);
  }
  for (const q of ['Q1', 'Q2', 'Q3', 'Q4']) {
    const name = `Quarterly_Architecture_Review_${q}_2026_Final_Approved.pdf`;
    fs.writeFileSync(path.join(folder, name), `Report ${q}`);
    expected.push(name);
  }
  for (const rev of ['rev1', 'rev2', 'rev3', 'rev4']) {
    const name = `deployment-checklist-europe-west-primary-cluster-rollout-plan-${rev}`;
    fs.writeFileSync(path.join(folder, name), `Checklist ${rev}`);
    expected.push(name);
  }

  return expected;
}

function buildFiveThousandFixture(folder) {
  fs.mkdirSync(folder, { recursive: true });
  for (let i = 1; i <= 5000; i += 1) {
    const num = String(i).padStart(4, '0');
    fs.writeFileSync(
      path.join(folder, `Archive_Backup_2026_11_25_Node_Primary_Snapshot_${num}.txt`),
      '',
    );
  }
}

async function setTheme(page, theme) {
  await page.evaluate((t) => {
    const raw = localStorage.getItem('legion.settings.v1');
    const obj = raw ? JSON.parse(raw) : {};
    obj.theme = t;
    localStorage.setItem('legion.settings.v1', JSON.stringify(obj));
    document.documentElement.dataset.theme = t;
  }, theme);
  await page.waitForTimeout(100);
}

async function maybeShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}.png`) });
}

async function waitForVisibleFitted(page, selector, containerSelector = null) {
  await page.locator(selector).first().waitFor({ timeout: 10_000 });
  await page.waitForFunction(
    ({ sel, cSel }) => {
      const els = [...document.querySelectorAll(sel)];
      if (els.length === 0) return false;
      const boxEl = cSel ? document.querySelector(cSel) : null;
      const bounds = boxEl
        ? boxEl.getBoundingClientRect()
        : { top: 0, bottom: window.innerHeight };
      const onScreen = els.filter((el) => {
        const r = el.getBoundingClientRect();
        return r.height > 0 && r.bottom > bounds.top + 1 && r.top < bounds.bottom - 1;
      });
      if (onScreen.length === 0) return false;
      return onScreen.every((el) => el.getAttribute('data-fn-ready') === '1');
    },
    { sel: selector, cSel: containerSelector },
    { timeout: 10_000 },
  );
  await page.waitForTimeout(40);
}

async function connectPage(page, url, token) {
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  await page.locator('input').first().waitFor({ timeout: 10_000 });
  await page.locator('input').first().fill(token);
  await page.getByRole('button', { name: 'Connect' }).click();
  await page.locator('nav.nav a', { hasText: 'Files' }).waitFor({ timeout: 10_000 });
}

async function collectAllScrolledRows(page, scrollSelector, rowSelector, slotSelector) {
  const seen = new Map();
  let lastTop = -1;

    for (let step = 0; step < 30; step += 1) {
    await waitForVisibleFitted(page, slotSelector, scrollSelector);
    const snap = await page.evaluate(
      ({ rSel, sSel, cSel }) => {
        const rows = [...document.querySelectorAll(rSel)];
        const boxEl = cSel ? document.querySelector(cSel) : null;
        const bounds = boxEl ? boxEl.getBoundingClientRect() : { top: 0, bottom: window.innerHeight };
        const out = [];
        for (const row of rows) {
          const slot = row.querySelector(sSel);
          if (!slot) continue;
          const r = slot.getBoundingClientRect();
          if (r.height === 0 || r.bottom <= bounds.top + 1 || r.top >= bounds.bottom - 1) continue;
          if (slot.getAttribute('data-fn-ready') !== '1') continue;
          const lines = [...slot.querySelectorAll('.fn-line')];
          const drawn = lines.map((l) => l.textContent || '').join('');
          const dimTexts = [...slot.querySelectorAll('.fn-dim')].map((d) => d.textContent || '');
          const lineOverflows = lines.some((l) => l.scrollWidth > slot.clientWidth + 2);
          out.push({
            href: row.getAttribute('href') || '',
            ariaLabel: row.getAttribute('aria-label') || '',
            titleAttr: row.getAttribute('title') || '',
            drawn,
            lineCount: lines.length,
            dimText: dimTexts.join(''),
            lineOverflows,
            rowHeight: Math.round(row.getBoundingClientRect().height),
          });
        }
        return out;
      },
      { rSel: rowSelector, sSel: slotSelector, cSel: scrollSelector },
    );

    for (const item of snap) {
      seen.set(item.ariaLabel, item);
    }

    const pos = await page.evaluate((sel) => {
      const el = document.querySelector(sel);
      if (!el) return { top: 0, max: 0 };
      const next = Math.min(el.scrollTop + Math.max(240, el.clientHeight * 0.7), el.scrollHeight - el.clientHeight);
      el.scrollTop = next;
      return { top: el.scrollTop, max: el.scrollHeight - el.clientHeight };
    }, scrollSelector);

    if (Math.abs(pos.top - lastTop) < 2 || pos.top >= pos.max - 2) {
      await waitForVisibleFitted(page, slotSelector, scrollSelector);
      const finalSnap = await page.evaluate(
        ({ rSel, sSel, cSel }) => {
          const rows = [...document.querySelectorAll(rSel)];
          const boxEl = cSel ? document.querySelector(cSel) : null;
          const bounds = boxEl ? boxEl.getBoundingClientRect() : { top: 0, bottom: window.innerHeight };
          const out = [];
          for (const row of rows) {
            const slot = row.querySelector(sSel);
            if (!slot) continue;
            const r = slot.getBoundingClientRect();
            if (r.height === 0 || r.bottom <= bounds.top + 1 || r.top >= bounds.bottom - 1) continue;
            if (slot.getAttribute('data-fn-ready') !== '1') continue;
            const lines = [...slot.querySelectorAll('.fn-line')];
            out.push({
              href: row.getAttribute('href') || '',
              ariaLabel: row.getAttribute('aria-label') || '',
              titleAttr: row.getAttribute('title') || '',
              drawn: lines.map((l) => l.textContent || '').join(''),
              lineCount: lines.length,
              dimText: [...slot.querySelectorAll('.fn-dim')].map((d) => d.textContent || '').join(''),
              lineOverflows: lines.some((l) => l.scrollWidth > slot.clientWidth + 2),
              rowHeight: Math.round(row.getBoundingClientRect().height),
            });
          }
          return out;
        },
        { rSel: rowSelector, sSel: slotSelector, cSel: scrollSelector },
      );
      for (const item of finalSnap) seen.set(item.ariaLabel, item);
      break;
    }
    lastTop = pos.top;
  }

  return Array.from(seen.values());
}

test('long file names: real legion serve on phone & desktop, list & grid, dark & light, item sheet, hover, search, and 5,000-entry scroll', async () => {
  assert.ok(fs.existsSync(LEGION_BIN), `Release binary not found at ${LEGION_BIN}`);
  const root = makeWorkRoot();
  const towerHome = path.join(root, 'tower-legion');
  const towerUser = path.join(root, 'tower-user');
  const capturesDir = path.join(towerUser, 'Captures');
  const manyDir = path.join(towerUser, 'ManyFiles');

  fs.mkdirSync(path.join(towerHome, 'config'), { recursive: true });
  fs.mkdirSync(path.join(towerHome, 'state'), { recursive: true });

  const expectedCaptures = buildCapturesFixture(capturesDir);
  buildFiveThousandFixture(manyDir);

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

  fs.writeFileSync(path.join(towerHome, 'config', 'node.toml'), 'name = "tower"\nsystem = "linux"\nbusy = "none"\n');
  fs.writeFileSync(path.join(towerHome, 'config', 'policy.toml'), policy);
  fs.writeFileSync(path.join(towerHome, 'config', 'fleet.toml'), '[machines.tower]\nssh = "tower.example"\n');

  const tokenOut = JSON.parse(runLegion(['token.add', 'name=ui', 'principal=ui', '--json'], towerHome, towerUser));
  assert.ok(tokenOut.token && tokenOut.token.startsWith('lgn_'));

  // Pin two long-named items so the Pinned strip on Files home & split-view left pane is exercised.
  runLegion(
    [
      'fav.add',
      'machine=tower',
      `path=${path.join(capturesDir, 'Screen Recording 2026-11-25 at 14.01.12.mov')}`,
      'label=Screen Recording 2026-11-25 at 14.01.12.mov',
      'pinned=true',
      '--json',
    ],
    towerHome,
    towerUser,
  );
  runLegion(
    [
      'fav.add',
      'machine=tower',
      `path=${path.join(capturesDir, 'Screen Recording 2026-11-25 at 14.02.04.mov')}`,
      'label=Screen Recording 2026-11-25 at 14.02.04.mov',
      'pinned=true',
      '--json',
    ],
    towerHome,
    towerUser,
  );

  const server = await startServer(towerHome, towerUser);
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const consoleErrors = [];
  const pageErrors = [];

  try {
    // ── 1. Phone viewport (390x844) ────────────────────────────────────────────────────────────
    const phoneCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const phone = await phoneCtx.newPage();
    phone.on('console', (m) => {
      if (m.type() === 'error') consoleErrors.push(`phone: ${m.text()}`);
    });
    phone.on('pageerror', (e) => pageErrors.push(`phone: ${String(e)}`));

    await connectPage(phone, server.url, tokenOut.token);

    // Check Pinned strip on Files home (2-line tiles, unique visible texts, full aria-label).
    await phone.locator('nav.nav a', { hasText: 'Files' }).click();
    await waitForVisibleFitted(phone, '.tiles .tile .fn-slot');
    const pinnedSnap = await phone.evaluate(() =>
      [...document.querySelectorAll('.tiles a.tile')].map((tile) => {
        const slot = tile.querySelector('.fn-slot');
        const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
        return {
          ariaLabel: tile.getAttribute('aria-label') || '',
          drawn: lines.map((l) => l.textContent || '').join(''),
          lineCount: lines.length,
        };
      }),
    );
    assert.equal(pinnedSnap.length, 2, 'Expected 2 pinned long-name tiles');
    assert.notEqual(pinnedSnap[0].drawn, pinnedSnap[1].drawn, 'Pinned tiles must draw distinct names');
    assert.ok(pinnedSnap.every((p) => p.lineCount <= 2), 'Pinned tiles must wrap up to 2 lines');
    assert.deepEqual(
      pinnedSnap.map((p) => p.ariaLabel).sort(),
      ['Screen Recording 2026-11-25 at 14.01.12.mov', 'Screen Recording 2026-11-25 at 14.02.04.mov'],
    );
    await maybeShot(phone, 'after-phone-pinned-dark');

    // Check Favourites view on phone (uses FileNameList + full accessible names + distinct visible text).
    await phone.locator('a.row[href="#/files/@favourites"]').click();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    const favSnap = await phone.evaluate(() =>
      [...document.querySelectorAll('.list a.row')].map((row) => ({
        ariaLabel: row.getAttribute('aria-label') || '',
        drawn: [...row.querySelectorAll('.fn-line')].map((l) => l.textContent || '').join(''),
      })),
    );
    assert.equal(favSnap.length, 2, 'Expected 2 favourites');
    assert.notEqual(favSnap[0].drawn, favSnap[1].drawn, 'Favourites visible names must differ');
    assert.deepEqual(
      favSnap.map((f) => f.ariaLabel).sort(),
      ['Screen Recording 2026-11-25 at 14.01.12.mov', 'Screen Recording 2026-11-25 at 14.02.04.mov'],
    );

    // Open Captures folder on phone.
    const capturesHref = `${server.url}/#/files/tower/${encodeURIComponent(capturesDir)}`;
    await phone.goto(capturesHref, { waitUntil: 'domcontentloaded' });
    await phone.locator('.grid a.cell, .list a.row').first().waitFor({ timeout: 10_000 });

    // Switch to List view first and check dark + light.
    const showListBtn = phone.locator('.dock button[aria-label="Show as list"]');
    if (await showListBtn.count()) {
      await showListBtn.click();
    }
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    await setTheme(phone, 'dark');
    await maybeShot(phone, 'after-phone-list-top-dark');
    await setTheme(phone, 'light');
    await maybeShot(phone, 'after-phone-list-top-light');
    await setTheme(phone, 'dark');

    // Scroll to Screen Recording section for direct before/after comparison shots.
    const firstRecRow = phone.locator('.list a.row[aria-label="Screen Recording 2026-11-25 at 14.01.12.mov"]');
    await firstRecRow.scrollIntoViewIfNeeded();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    await maybeShot(phone, 'after-phone-list-recordings-dark');
    await setTheme(phone, 'light');
    await maybeShot(phone, 'after-phone-list-recordings-light');
    await setTheme(phone, 'dark');

    // Scroll back to top and scan ALL 60 rows in list view on phone.
    await phone.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 0;
    });
    const phoneListRows = await collectAllScrolledRows(phone, 'main.main', '.list a.row', '.fn-slot');
    assert.equal(phoneListRows.length, expectedCaptures.length, `Expected all ${expectedCaptures.length} rows on phone list`);

    const phoneDrawnSet = new Set(phoneListRows.map((r) => r.drawn));
    assert.equal(
      phoneDrawnSet.size,
      expectedCaptures.length,
      `Every row visible text must be unique on phone list (got ${phoneDrawnSet.size} of ${expectedCaptures.length})`,
    );
    for (const r of phoneListRows) {
      assert.ok(expectedCaptures.includes(r.ariaLabel), `Unexpected accessible name: ${r.ariaLabel}`);
      assert.ok(!r.drawn.endsWith('…'), `Trailing ellipsis on phone list: "${r.drawn}"`);
      assert.ok(r.lineCount >= 1 && r.lineCount <= 2, `Row lines ${r.lineCount} out of range for ${r.ariaLabel}`);
      assert.equal(r.lineOverflows, false, `Horizontal text overflow in row "${r.drawn}"`);
    }

    // Verify shared-lead dimming on IMG_ rows (`IMG_20261125_` steps back, `140311.jpg` stands out).
    const imgRow = phoneListRows.find((r) => r.ariaLabel === 'IMG_20261125_140311.jpg');
    assert.ok(imgRow, 'IMG_20261125_140311.jpg row missing');
    assert.equal(imgRow.dimText, 'IMG_20261125_', `Expected IMG_ shared lead dim, got "${imgRow.dimText}"`);
    assert.equal(imgRow.lineCount, 1, '1-line IMG_ name must stay on 1 line');

    // Verify zero page-level horizontal overflow on phone.
    const phoneOverflow = await phone.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(phoneOverflow <= 1, `Phone list horizontal overflow: ${phoneOverflow}px`);

    // Verify accessible name via Playwright ARIA role locator for a Screen Recording row.
    const targetRec = 'Screen Recording 2026-11-25 at 14.01.12.mov';
    const recLink = phone.getByRole('link', { name: targetRec, exact: true });
    await recLink.scrollIntoViewIfNeeded();
    assert.equal(await recLink.count(), 1, 'Accessible link name must equal full filename');

    // Test phone long-press -> item sheet shows full name + working Copy name button.
    const recBox = await recLink.boundingBox();
    assert.ok(recBox, 'Recording row bounding box missing');
    await phone.mouse.move(recBox.x + recBox.width / 2, recBox.y + recBox.height / 2);
    await phone.mouse.down();
    await phone.waitForTimeout(550);
    await phone.mouse.up();

    const itemSheet = phone.locator('.sheet');
    await itemSheet.waitFor({ timeout: 5000 });
    assert.equal(
      await itemSheet.locator('#sheet-title').textContent(),
      targetRec,
      'Item sheet title must carry the full untruncated name',
    );
    const copyNameBtn = itemSheet.getByRole('button', { name: 'Copy name', exact: true });
    await copyNameBtn.waitFor();
    await maybeShot(phone, 'after-phone-item-sheet-dark');
    await setTheme(phone, 'light');
    await maybeShot(phone, 'after-phone-item-sheet-light');
    await setTheme(phone, 'dark');

    await copyNameBtn.click();
    await itemSheet.getByRole('button', { name: 'Copied', exact: true }).waitFor({ timeout: 3000 });
    const copiedText = await phone.evaluate(() => navigator.clipboard.readText());
    assert.equal(copiedText, targetRec, 'Copy name must write exact full name to clipboard');
    await phone.keyboard.press('Escape');
    await itemSheet.waitFor({ state: 'hidden' });

    // Open a report in Viewer -> viewer title follows rule 2 (no trailing ellipsis, ending intact)
    // and viewer More sheet also carries full name + Copy name.
    const reportName = 'Quarterly_Architecture_Review_Q2_2026_Final_Approved.pdf';
    const reportLink = phone.getByRole('link', { name: reportName, exact: true });
    await reportLink.scrollIntoViewIfNeeded();
    await reportLink.click();
    await phone.locator('.viewer').waitFor({ timeout: 5000 });
    await waitForVisibleFitted(phone, '.viewer-top .name .fn-slot');
    const viewerTitleInfo = await phone.evaluate(() => {
      const nameEl = document.querySelector('.viewer-top .name');
      const slot = nameEl?.querySelector('.fn-slot');
      const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
      return {
        ariaLabel: nameEl?.getAttribute('aria-label') || '',
        drawn: lines.map((l) => l.textContent || '').join(''),
        lineCount: lines.length,
        overflow: document.documentElement.scrollWidth - window.innerWidth,
      };
    });
    assert.equal(viewerTitleInfo.ariaLabel, reportName);
    assert.equal(viewerTitleInfo.lineCount, 1);
    assert.ok(!viewerTitleInfo.drawn.endsWith('…'), `Viewer title must not end-truncate: "${viewerTitleInfo.drawn}"`);
    assert.ok(viewerTitleInfo.drawn.endsWith('Approved.pdf'), `Viewer title must keep ending: "${viewerTitleInfo.drawn}"`);
    assert.ok(viewerTitleInfo.overflow <= 1, `Viewer overflow: ${viewerTitleInfo.overflow}px`);
    await maybeShot(phone, 'after-phone-viewer-dark');

    await phone.locator('.viewer-top button[aria-label="More options"]').click();
    await phone.locator('.sheet').getByRole('button', { name: 'Copy name', exact: true }).waitFor();
    await phone.waitForFunction(() => document.activeElement?.classList?.contains('sheet'), null, { timeout: 3000 });
    await phone.keyboard.press('Escape');
    await phone.locator('.sheet').waitFor({ state: 'hidden' });
    await phone.locator('.viewer-top button[aria-label="Close"]').click();
    await phone.locator('.viewer').waitFor({ state: 'hidden' });

    // Switch to Grid view on phone (dark + light): check up to 3 lines, all visible cells distinct.
    await phone.locator('.dock button[aria-label="Show as grid"]').click();
    await phone.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 0;
    });
    await waitForVisibleFitted(phone, '.grid a.cell .fn-slot');
    await setTheme(phone, 'dark');
    await maybeShot(phone, 'after-phone-grid-dark');
    await setTheme(phone, 'light');
    await maybeShot(phone, 'after-phone-grid-light');
    await setTheme(phone, 'dark');

    const phoneGridCells = await collectAllScrolledRows(phone, 'main.main', '.grid a.cell', '.fn-slot');
    assert.equal(phoneGridCells.length, 52, 'Expected 52 media cells (40 jpg + 12 mov) in grid view');
    assert.equal(
      new Set(phoneGridCells.map((c) => c.drawn)).size,
      52,
      'Every grid cell visible text must be unique on phone',
    );
    for (const c of phoneGridCells) {
      assert.ok(c.lineCount >= 1 && c.lineCount <= 3, `Grid cell lines ${c.lineCount} > 3 for ${c.ariaLabel}`);
      assert.ok(!c.drawn.endsWith('…'), `Trailing ellipsis in grid cell: "${c.drawn}"`);
      assert.equal(c.lineOverflows, false, `Grid cell horizontal overflow: "${c.drawn}"`);
    }

    // Search results on phone: match highlight active, ZERO dimming (.fn-dim count == 0).
    await phone.goto(
      `${server.url}/#/files/@search?m=tower&p=${encodeURIComponent(capturesDir)}&q=${encodeURIComponent('Quarterly')}`,
      { waitUntil: 'domcontentloaded' },
    );
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    const searchInfo = await phone.evaluate(() => {
      const rows = [...document.querySelectorAll('.list a.row')];
      return {
        count: rows.length,
        marks: document.querySelectorAll('.list .fn-match').length,
        dims: document.querySelectorAll('.list .fn-dim').length,
        drawn: rows.map((r) => [...r.querySelectorAll('.fn-line')].map((l) => l.textContent).join('')),
        ariaLabels: rows.map((r) => r.getAttribute('aria-label') || ''),
      };
    });
    assert.equal(searchInfo.count, 4, 'Expected 4 Quarterly search hits');
    assert.ok(searchInfo.marks >= 4, 'Search results must highlight matches');
    assert.equal(searchInfo.dims, 0, 'Search results must not dim shared prefixes');
    assert.equal(new Set(searchInfo.drawn).size, 4, 'Search hit visible texts must all differ');
    assert.ok(
      searchInfo.ariaLabels.includes(reportName),
      `Search hit accessible name must equal full name: ${JSON.stringify(searchInfo.ariaLabels)}`,
    );
    await maybeShot(phone, 'after-phone-search-dark');

    // Recent view on phone carries the opened report with full accessible name and fitted text.
    await phone.goto(`${server.url}/#/files/@recent`, { waitUntil: 'domcontentloaded' });
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    const recentRow = phone.getByRole('link', { name: reportName, exact: true });
    assert.equal(await recentRow.count(), 1, 'Recent view must expose full accessible name');

    // Text-size change recomputes cleanly without overflow.
    await phone.goto(capturesHref, { waitUntil: 'domcontentloaded' });
    const showListBtn2 = phone.locator('.dock button[aria-label="Show as list"]');
    if (await showListBtn2.count()) await showListBtn2.click();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    await phone.evaluate(() => {
      document.documentElement.dataset.text = 'large';
    });
    await phone.waitForTimeout(120);
    await waitForVisibleFitted(phone, '.list a.row .fn-slot');
    const largeTextOverflow = await phone.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(largeTextOverflow <= 1, `Large text size caused overflow: ${largeTextOverflow}px`);
    await phone.evaluate(() => {
      delete document.documentElement.dataset.text;
    });

    await phoneCtx.close();

    // ── 2. Desktop viewport (1440x900) — split view, list & grid, dark & light, hover ──────────
    const deskCtx = await browser.newContext({
      viewport: { width: 1440, height: 900 },
      colorScheme: 'dark',
    });
    const desk = await deskCtx.newPage();
    desk.on('console', (m) => {
      if (m.type() === 'error') consoleErrors.push(`desktop: ${m.text()}`);
    });
    desk.on('pageerror', (e) => pageErrors.push(`desktop: ${String(e)}`));

    await connectPage(desk, server.url, tokenOut.token);
    await desk.goto(capturesHref, { waitUntil: 'domcontentloaded' });
    await desk.locator('.split-right .grid a.cell, .split-right .list a.row').first().waitFor({ timeout: 10_000 });

    // Check pinned strip in left split pane.
    await waitForVisibleFitted(desk, '.split-left .tiles a.tile .fn-slot');
    const leftPinned = await desk.evaluate(() =>
      [...document.querySelectorAll('.split-left .tiles a.tile')].map((t) => ({
        title: t.getAttribute('title') || '',
        ariaLabel: t.getAttribute('aria-label') || '',
        drawn: [...t.querySelectorAll('.fn-line')].map((l) => l.textContent || '').join(''),
      })),
    );
    assert.equal(leftPinned.length, 2);
    assert.notEqual(leftPinned[0].drawn, leftPinned[1].drawn);
    assert.deepEqual(
      leftPinned.map((p) => p.title).sort(),
      ['Screen Recording 2026-11-25 at 14.01.12.mov', 'Screen Recording 2026-11-25 at 14.02.04.mov'],
    );

    // Desktop Grid view screenshots & checks (dark + light).
    await waitForVisibleFitted(desk, '.split-right .grid a.cell .fn-slot');
    await setTheme(desk, 'dark');
    await maybeShot(desk, 'after-desktop-grid-dark');
    await setTheme(desk, 'light');
    await maybeShot(desk, 'after-desktop-grid-light');
    await setTheme(desk, 'dark');

    const deskGridCells = await collectAllScrolledRows(
      desk,
      '.split-right .split-body',
      '.split-right .grid a.cell',
      '.fn-slot',
    );
    assert.equal(deskGridCells.length, 52);
    assert.equal(new Set(deskGridCells.map((c) => c.drawn)).size, 52, 'Desktop grid cells must all differ');
    for (const c of deskGridCells) {
      assert.equal(c.titleAttr, c.ariaLabel, 'Desktop grid hover title must equal full name');
    }

    // Switch desktop to List view (dark + light), scroll to Screen Recordings to match before shot.
    await desk.locator('.split-right button[aria-label="Show as list"]').click();
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot');
    await desk.locator('.split-right .list a.row[aria-label="Screen Recording 2026-11-25 at 14.01.12.mov"]').scrollIntoViewIfNeeded();
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot');
    await setTheme(desk, 'dark');
    await maybeShot(desk, 'after-desktop-list-dark');
    await setTheme(desk, 'light');
    await maybeShot(desk, 'after-desktop-list-light');
    await setTheme(desk, 'dark');

    await desk.evaluate(() => {
      const b = document.querySelector('.split-right .split-body');
      if (b) b.scrollTop = 0;
    });
    const deskListRows = await collectAllScrolledRows(
      desk,
      '.split-right .split-body',
      '.split-right .list a.row',
      '.fn-slot',
    );
    assert.equal(deskListRows.length, expectedCaptures.length);
    assert.equal(new Set(deskListRows.map((r) => r.drawn)).size, expectedCaptures.length);
    for (const r of deskListRows) {
      assert.equal(r.titleAttr, r.ariaLabel, 'Desktop list row hover title must equal full name');
      assert.equal(r.lineOverflows, false, `Desktop row overflow: "${r.drawn}"`);
    }

    // Verify zero CSS text-overflow:ellipsis or -webkit-line-clamp on any file name slot.
    const cssLeak = await desk.evaluate(() => {
      const bad = [];
      for (const el of document.querySelectorAll('.fn-slot, .fn-title, .cell-name, .tiles .tile strong')) {
        const cs = getComputedStyle(el);
        if (cs.textOverflow === 'ellipsis') bad.push({ el: el.className, prop: 'text-overflow: ellipsis' });
        if (cs.webkitLineClamp && cs.webkitLineClamp !== 'none') {
          bad.push({ el: el.className, prop: `-webkit-line-clamp: ${cs.webkitLineClamp}` });
        }
      }
      return bad;
    });
    assert.deepEqual(cssLeak, [], `Found forbidden CSS truncation on file name elements: ${JSON.stringify(cssLeak)}`);

    // ── 3. 5,000-entry folder: lazy on-screen measurement & scroll performance ─────────────────
    await desk.goto(`${server.url}/#/files/tower/${encodeURIComponent(manyDir)}`, {
      waitUntil: 'domcontentloaded',
    });
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', '.split-right .split-body');
    await desk.waitForTimeout(100);

    const initialPerf = await desk.evaluate(() => {
      const slots = [...document.querySelectorAll('.split-right .list .fn-slot')];
      const ready = slots.filter((s) => s.getAttribute('data-fn-ready') === '1').length;
      const stats = window.__LEGION_NAME_FIT_STATS__ || {};
      return {
        renderedDomRows: slots.length,
        measuredReadyRows: ready,
        activeListResizeObservers: stats.activeListResizeObservers || 0,
        lastBatchMs: Number((stats.lastBatchMs || 0).toFixed(2)),
        lastBatchRows: stats.lastBatchRows || 0,
      };
    });

    assert.ok(initialPerf.renderedDomRows >= 100, `Expected first render slice (>=100 rows), got ${initialPerf.renderedDomRows}`);
    assert.ok(
      initialPerf.measuredReadyRows < initialPerf.renderedDomRows,
      `Only on-screen rows should be measured: ready=${initialPerf.measuredReadyRows}, rendered=${initialPerf.renderedDomRows}`,
    );
    assert.ok(
      initialPerf.activeListResizeObservers <= 6,
      `Expected one ResizeObserver per list, not per row: got ${initialPerf.activeListResizeObservers}`,
    );

    // Scroll through several screens of the 5,000-entry folder and measure frame batches.
    const scrollBatchMs = [];
    for (let s = 1; s <= 5; s += 1) {
      await desk.evaluate((step) => {
        const b = document.querySelector('.split-right .split-body');
        if (b) b.scrollTop = step * 1600;
      }, s);
      await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', '.split-right .split-body');
      const ms = await desk.evaluate(() => window.__LEGION_NAME_FIT_STATS__?.lastBatchMs || 0);
      scrollBatchMs.push(Number(ms.toFixed(2)));
    }
    const maxScrollBatchMs = Math.max(...scrollBatchMs);
    console.log(
      `[long-names perf] 5,000-entry folder: rendered=${initialPerf.renderedDomRows} measured=${initialPerf.measuredReadyRows} ` +
        `observers=${initialPerf.activeListResizeObservers} initialScreen=${initialPerf.lastBatchMs}ms (${initialPerf.lastBatchRows} rows) ` +
        `scrollBatchesMs=${JSON.stringify(scrollBatchMs)}`,
    );
    assert.ok(
      maxScrollBatchMs < 50,
      `Measuring a screen of rows should stay well under a frame hitch (max ${maxScrollBatchMs}ms)`,
    );

    await deskCtx.close();

    assert.deepEqual(consoleErrors, [], `Unexpected console errors: ${consoleErrors.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
