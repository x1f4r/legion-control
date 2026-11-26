// Real-server headless Chromium tests for long file/folder names:
// - Folder with 12 screen recordings differing in time, 40 IMG_ photos, 4 reports differing in the
//   middle, and 4 long extensionless names on phone (390x844) and desktop (1440x900), list & grid,
//   dark & light.
// - Uniqueness of visible drawn text, full accessible names, shared-lead dimming, search match
//   highlight without dimming, phone item sheet (full name + Copy name), viewer title rule 2,
//   desktop hover tooltips, pinned strip, and 5,000-entry lazy on-screen measurement benchmark.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { findDigitRuns, isNaturalBreak, toGraphemes } from '../lib/file-name.js';
import {
  browserTest,
  launchBrowser,
  LEGION_BIN,
  setTheme,
  startLegionServer,
  waitForLayoutSettled,
} from './browser-helpers.mjs';

const SHOTS_DIR = process.env.LEGION_LONG_NAMES_SHOTS_DIR || '';

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

async function maybeShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}.png`) });
}

async function waitForVisibleFitted(page, selector, containerSelector = null, tracker = null) {
  const step = `fitted names for ${selector}`;
  if (tracker) {
    await tracker.waitForLocator(page.locator(selector).first(), `${selector} slot`);
    await tracker.waitForFn(
      page,
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
      step,
    );
  } else {
    await page.locator(selector).first().waitFor();
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
    );
  }
  await waitForLayoutSettled(page, 2);
}

async function connectPage(page, url, token, tracker) {
  tracker.setStep(`connect page to ${url}`);
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  await tracker.waitForLocator(page.locator('input').first(), 'token input');
  await page.locator('input').first().fill(token);
  await page.getByRole('button', { name: 'Connect' }).click();
  await tracker.waitForLocator(page.locator('nav.nav a', { hasText: 'Files' }), 'Files nav link');
}

function extractDigitRunStrings(text) {
  const g = toGraphemes(text);
  const runs = findDigitRuns(g);
  const seen = new Set();
  const out = [];
  for (let i = 0; i < runs.length; i += 1) {
    const r = runs[i];
    if (!r || seen.has(r)) continue;
    seen.add(r);
    out.push({ start: r.start, end: r.end, text: g.slice(r.start, r.end).join('') });
  }
  return { g, runs, list: out };
}

function assertFontIndependentLayout(row, { maxLines, label }) {
  assert.ok(row.lineCount >= 1 && row.lineCount <= maxLines, `${label}: line count ${row.lineCount} out of 1..${maxLines}`);
  assert.ok(!row.drawn.endsWith('…'), `${label}: trailing ellipsis in "${row.drawn}"`);
  assert.equal(row.lineOverflows, false, `${label}: horizontal text overflow in "${row.drawn}"`);
  assert.equal(
    row.dimText + row.strongText,
    row.drawn,
    `${label}: quiet + strong spans must reconstruct drawn text ("${row.dimText}" + "${row.strongText}" !== "${row.drawn}")`,
  );

  const origRuns = new Set(extractDigitRunStrings(row.ariaLabel).list.map((r) => r.text));
  const { g: drawnG, runs: drawnRuns, list: drawnRunList } = extractDigitRunStrings(row.drawn);

  // Every line after the first must begin at a natural break (or right after an ellipsis) and
  // never split a digit run across lines.
  let offset = 0;
  for (let i = 0; i < row.lineTexts.length - 1; i += 1) {
    offset += toGraphemes(row.lineTexts[i]).length;
    assert.ok(
      !(drawnRuns[offset - 1] !== null && drawnRuns[offset - 1] === drawnRuns[offset]),
      `${label}: digit run split across lines: ${JSON.stringify(row.lineTexts)}`,
    );
    assert.ok(
      drawnG[offset - 1] === '…' || isNaturalBreak(drawnG, offset),
      `${label}: line ${i + 2} did not start at a natural break: ${JSON.stringify(row.lineTexts)}`,
    );
  }

  // No digit run may be split by an ellipsis.
  if (row.drawn.includes('…')) {
    assert.ok(
      !/[0-9][.:\-,]…|…[.:\-,][0-9]/.test(row.drawn),
      `${label}: ellipsis landed inside a digit run joiner in "${row.drawn}"`,
    );
    for (const run of drawnRunList) {
      const touchesEllipsis =
        (run.start > 0 && drawnG[run.start - 1] === '…') ||
        (run.end < drawnG.length && drawnG[run.end] === '…');
      if (touchesEllipsis) {
        assert.ok(
          origRuns.has(run.text),
          `${label}: ellipsis split digit run -> fragment "${run.text}" in "${row.drawn}" (original runs: ${JSON.stringify([...origRuns])})`,
        );
      }
    }
  }

  // Emphasis must never change inside a digit run.
  if (row.dimText.length > 0 && row.strongText.length > 0) {
    const dimBoundary = toGraphemes(row.dimText).length;
    assert.ok(
      !(drawnRuns[dimBoundary - 1] !== null && drawnRuns[dimBoundary - 1] === drawnRuns[dimBoundary]),
      `${label}: emphasis changed inside a digit run (dim="${row.dimText}", strong="${row.strongText}")`,
    );
  }
}

function assertSeparatorEmphasis(row, { expectedDim, expectedStrong, expectedDimSep, strongPrefix, strongSuffix, label }) {
  assert.ok(
    row.dimText.endsWith(expectedDimSep) || row.dimText.endsWith('…'),
    `${label}: quiet prefix must end at separator "${expectedDimSep}", got dim="${row.dimText}"`,
  );
  assert.ok(
    row.strongText.startsWith(strongPrefix),
    `${label}: strong part must start with "${strongPrefix}", got strong="${row.strongText}"`,
  );
  assert.ok(
    row.strongText.endsWith(strongSuffix),
    `${label}: strong part must end with "${strongSuffix}", got strong="${row.strongText}"`,
  );
  if (!row.drawn.includes('…')) {
    assert.equal(row.dimText, expectedDim, `${label}: expected exact quiet lead "${expectedDim}", got "${row.dimText}"`);
    assert.equal(row.strongText, expectedStrong, `${label}: expected exact strong part "${expectedStrong}", got "${row.strongText}"`);
  }
}

async function collectAllScrolledRows(page, scrollSelector, rowSelector, slotSelector, tracker) {
  const seen = new Map();
  let lastTop = -1;

  const readVisibleSnapshot = () =>
    page.evaluate(
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
          const lineTexts = lines.map((l) => l.textContent || '');
          const drawn = lineTexts.join('');
          let dimText = '';
          let strongText = '';
          for (const line of lines) {
            for (const node of line.childNodes) {
              const txt = node.textContent || '';
              if (node.nodeType === Node.ELEMENT_NODE && node.classList.contains('fn-dim')) {
                dimText += txt;
              } else {
                strongText += txt;
              }
            }
          }
          const lineOverflows = lines.some((l) => l.scrollWidth > slot.clientWidth + 2);
          out.push({
            href: row.getAttribute('href') || '',
            ariaLabel: row.getAttribute('aria-label') || '',
            titleAttr: row.getAttribute('title') || '',
            drawn,
            lineTexts,
            lineCount: lines.length,
            dimText,
            strongText,
            lineOverflows,
            rowHeight: Math.round(row.getBoundingClientRect().height),
          });
        }
        return out;
      },
      { rSel: rowSelector, sSel: slotSelector, cSel: scrollSelector },
    );

  for (let step = 0; step < 30; step += 1) {
    await waitForVisibleFitted(page, slotSelector, scrollSelector, tracker);
    for (const item of await readVisibleSnapshot()) {
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
      await waitForVisibleFitted(page, slotSelector, scrollSelector, tracker);
      for (const item of await readVisibleSnapshot()) {
        seen.set(item.ariaLabel, item);
      }
      break;
    }
    lastTop = pos.top;
  }

  return Array.from(seen.values());
}

browserTest(
  'long file names: real legion serve on phone & desktop, list & grid, dark & light, item sheet, hover, search, and 5,000-entry scroll',
  { timeout: 180_000 },
  async (_t, tracker) => {
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

  const server = await startLegionServer(towerHome, towerUser);
  tracker.setServerStderr(server.getStderr);
  const browser = await launchBrowser();

  const consoleErrors = [];
  const pageErrors = [];

  try {
    // ── 1. Phone viewport (390x844) ────────────────────────────────────────────────────────────
    const phoneCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      colorScheme: 'dark',
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const phone = tracker.attach(await phoneCtx.newPage(), {
      label: 'phone',
      collectConsole: consoleErrors,
      collectPage: pageErrors,
    });

    await connectPage(phone, server.url, tokenOut.token, tracker);

    // Check Pinned strip on Files home (2-line tiles, unique visible texts, full aria-label).
    await phone.locator('nav.nav a', { hasText: 'Files' }).click();
    await waitForVisibleFitted(phone, '.tiles .tile .fn-slot', null, tracker);
    const pinnedSnap = await phone.evaluate(() =>
      [...document.querySelectorAll('.tiles a.tile')].map((tile) => {
        const slot = tile.querySelector('.fn-slot');
        const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
        const lineTexts = lines.map((l) => l.textContent || '');
        let dimText = '';
        let strongText = '';
        for (const line of lines) {
          for (const node of line.childNodes) {
            const txt = node.textContent || '';
            if (node.nodeType === Node.ELEMENT_NODE && node.classList.contains('fn-dim')) {
              dimText += txt;
            } else {
              strongText += txt;
            }
          }
        }
        return {
          ariaLabel: tile.getAttribute('aria-label') || '',
          drawn: lineTexts.join(''),
          lineTexts,
          lineCount: lines.length,
          dimText,
          strongText,
          lineOverflows: lines.some((l) => l.scrollWidth > (slot?.clientWidth || 0) + 2),
        };
      }),
    );
    assert.equal(pinnedSnap.length, 2, 'Expected 2 pinned long-name tiles');
    assert.notEqual(pinnedSnap[0].drawn, pinnedSnap[1].drawn, 'Pinned tiles must draw distinct names');
    for (const p of pinnedSnap) {
      assertFontIndependentLayout(p, { maxLines: 2, label: `phone pinned "${p.ariaLabel}"` });
    }
    assert.deepEqual(
      pinnedSnap.map((p) => p.ariaLabel).sort(),
      ['Screen Recording 2026-11-25 at 14.01.12.mov', 'Screen Recording 2026-11-25 at 14.02.04.mov'],
    );
    await maybeShot(phone, 'after-phone-pinned-dark');

    // Check Favourites view on phone (uses FileNameList + full accessible names + distinct visible text).
    await phone.locator('a.row[href="#/files/@favourites"]').click();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
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
    await tracker.waitForLocator(
      phone.locator('.grid a.cell[aria-label="IMG_20261125_140311.jpg"], .list a.row[aria-label="IMG_20261125_140311.jpg"]'),
      'Captures folder entries on phone',
    );

    // Switch to List view first and check dark + light + capture list primary/dim colour tokens.
    const showListBtn = phone.locator('.dock button[aria-label="Show as list"]');
    if (await showListBtn.count()) {
      await showListBtn.click();
    }
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);

    const readNameColors = async (page, slotSelector) =>
      page.evaluate((sel) => {
        const slot = [...document.querySelectorAll(sel)].find((el) => el.querySelector('.fn-dim'));
        if (!slot) return null;
        const dim = slot.querySelector('.fn-dim');
        return {
          primary: getComputedStyle(slot).color,
          dim: getComputedStyle(dim).color,
          primaryWeight: getComputedStyle(slot).fontWeight,
          dimWeight: getComputedStyle(dim).fontWeight,
        };
      }, slotSelector);

    await setTheme(phone, 'dark', tracker);
    const phoneListColorsDark = await readNameColors(phone, '.list a.row .title.fn-title');
    assert.ok(phoneListColorsDark, 'Expected dimmed list row in dark theme');
    assert.notEqual(phoneListColorsDark.primary, phoneListColorsDark.dim);
    await maybeShot(phone, 'after-phone-list-top-dark');
    await setTheme(phone, 'light', tracker);
    const phoneListColorsLight = await readNameColors(phone, '.list a.row .title.fn-title');
    assert.ok(phoneListColorsLight, 'Expected dimmed list row in light theme');
    assert.notEqual(phoneListColorsLight.primary, phoneListColorsLight.dim);
    await maybeShot(phone, 'after-phone-list-top-light');
    await setTheme(phone, 'dark', tracker);

    // Scroll to Screen Recording section for direct before/after comparison shots.
    const firstRecRow = phone.locator('.list a.row[aria-label="Screen Recording 2026-11-25 at 14.01.12.mov"]');
    await firstRecRow.scrollIntoViewIfNeeded();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
    await maybeShot(phone, 'after-phone-list-recordings-dark');
    await setTheme(phone, 'light', tracker);
    await maybeShot(phone, 'after-phone-list-recordings-light');
    await setTheme(phone, 'dark', tracker);

    // Scroll back to top and scan ALL 60 rows in list view on phone.
    await phone.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 0;
    });
    const phoneListRows = await collectAllScrolledRows(phone, 'main.main', '.list a.row', '.fn-slot', tracker);
    assert.equal(phoneListRows.length, expectedCaptures.length, `Expected all ${expectedCaptures.length} rows on phone list`);

    const phoneDrawnSet = new Set(phoneListRows.map((r) => r.drawn));
    assert.equal(
      phoneDrawnSet.size,
      expectedCaptures.length,
      `Every row visible text must be unique on phone list (got ${phoneDrawnSet.size} of ${expectedCaptures.length})`,
    );
    for (const r of phoneListRows) {
      assert.ok(expectedCaptures.includes(r.ariaLabel), `Unexpected accessible name: ${r.ariaLabel}`);
      assertFontIndependentLayout(r, { maxLines: 2, label: `phone list "${r.ariaLabel}"` });
    }

    // Verify separator-aligned emphasis (joined across lines, independent of wrap point) on all four groups.
    const imgRow = phoneListRows.find((r) => r.ariaLabel === 'IMG_20261125_140311.jpg');
    assert.ok(imgRow, 'IMG_20261125_140311.jpg row missing');
    assertSeparatorEmphasis(imgRow, {
      expectedDim: 'IMG_20261125_',
      expectedStrong: '140311.jpg',
      expectedDimSep: '_',
      strongPrefix: '140311',
      strongSuffix: '140311.jpg',
      label: 'phone list IMG_',
    });

    const rec03Row = phoneListRows.find((r) => r.ariaLabel === 'Screen Recording 2026-11-25 at 14.03.19.mov');
    assert.ok(rec03Row, 'Screen Recording 2026-11-25 at 14.03.19.mov row missing');
    assertSeparatorEmphasis(rec03Row, {
      expectedDim: 'Screen Recording 2026-11-25 at ',
      expectedStrong: '14.03.19.mov',
      expectedDimSep: ' ',
      strongPrefix: '14.03.19',
      strongSuffix: '14.03.19.mov',
      label: 'phone list Screen Recording',
    });
    assert.ok(
      rec03Row.lineTexts.some((l) => l.includes('14.03.19')),
      `Expected whole time 14.03.19 on one line, got ${JSON.stringify(rec03Row.lineTexts)}`,
    );

    const repQ1Row = phoneListRows.find(
      (r) => r.ariaLabel === 'Quarterly_Architecture_Review_Q1_2026_Final_Approved.pdf',
    );
    assert.ok(repQ1Row, 'Quarterly_Architecture_Review_Q1_2026_Final_Approved.pdf row missing');
    assertSeparatorEmphasis(repQ1Row, {
      expectedDim: 'Quarterly_Architecture_Review_',
      expectedStrong: 'Q1_2026_Final_Approved.pdf',
      expectedDimSep: '_',
      strongPrefix: 'Q1',
      strongSuffix: 'Approved.pdf',
      label: 'phone list Quarterly report',
    });

    const depRev1Row = phoneListRows.find(
      (r) => r.ariaLabel === 'deployment-checklist-europe-west-primary-cluster-rollout-plan-rev1',
    );
    assert.ok(depRev1Row, 'deployment-checklist rev1 row missing');
    assertSeparatorEmphasis(depRev1Row, {
      expectedDim: 'deployment-checklist-europe-west-primary-cluster-rollout-plan-',
      expectedStrong: 'rev1',
      expectedDimSep: '-',
      strongPrefix: 'rev1',
      strongSuffix: 'rev1',
      label: 'phone list checklist',
    });

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
    const itemSheet = phone.locator('.sheet');
    await phone.mouse.move(recBox.x + recBox.width / 2, recBox.y + recBox.height / 2);
    await phone.mouse.down();
    try {
      await tracker.waitForLocator(itemSheet, 'long-press item sheet');
    } finally {
      await phone.mouse.up();
    }

    assert.equal(
      await itemSheet.locator('#sheet-title').textContent(),
      targetRec,
      'Item sheet title must carry the full untruncated name',
    );
    const copyNameBtn = itemSheet.getByRole('button', { name: 'Copy name', exact: true });
    await tracker.waitForLocator(copyNameBtn, 'Copy name button in item sheet');
    await maybeShot(phone, 'after-phone-item-sheet-dark');
    await setTheme(phone, 'light', tracker);
    await maybeShot(phone, 'after-phone-item-sheet-light');
    await setTheme(phone, 'dark', tracker);

    await copyNameBtn.click();
    await tracker.waitForLocator(itemSheet.getByRole('button', { name: 'Copied', exact: true }), 'Copied button confirmation');
    const copiedText = await phone.evaluate(() => navigator.clipboard.readText());
    assert.equal(copiedText, targetRec, 'Copy name must write exact full name to clipboard');
    await phone.keyboard.press('Escape');
    await tracker.waitForLocator(itemSheet, 'item sheet hidden after Escape', { state: 'hidden' });

    // Open a report in Viewer -> viewer title follows rule 2 (no trailing ellipsis, ending intact)
    // and viewer More sheet also carries full name + Copy name.
    const reportName = 'Quarterly_Architecture_Review_Q2_2026_Final_Approved.pdf';
    const reportLink = phone.getByRole('link', { name: reportName, exact: true });
    await reportLink.scrollIntoViewIfNeeded();
    await reportLink.click();
    await tracker.waitForLocator(phone.locator('.viewer'), 'report viewer');
    await waitForVisibleFitted(phone, '.viewer-top .name .fn-slot', null, tracker);
    const viewerTitleInfo = await phone.evaluate(() => {
      const nameEl = document.querySelector('.viewer-top .name');
      const slot = nameEl?.querySelector('.fn-slot');
      const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
      const lineTexts = lines.map((l) => l.textContent || '');
      let dimText = '';
      let strongText = '';
      for (const line of lines) {
        for (const node of line.childNodes) {
          const txt = node.textContent || '';
          if (node.nodeType === Node.ELEMENT_NODE && node.classList.contains('fn-dim')) {
            dimText += txt;
          } else {
            strongText += txt;
          }
        }
      }
      return {
        ariaLabel: nameEl?.getAttribute('aria-label') || '',
        drawn: lineTexts.join(''),
        lineTexts,
        lineCount: lines.length,
        dimText,
        strongText,
        lineOverflows: lines.some((l) => l.scrollWidth > (slot?.clientWidth || 0) + 2),
        overflow: document.documentElement.scrollWidth - window.innerWidth,
      };
    });
    assert.equal(viewerTitleInfo.ariaLabel, reportName);
    assertFontIndependentLayout(viewerTitleInfo, { maxLines: 1, label: 'phone viewer title' });
    assert.ok(viewerTitleInfo.drawn.endsWith('Approved.pdf'), `Viewer title must keep ending: "${viewerTitleInfo.drawn}"`);
    assert.ok(viewerTitleInfo.overflow <= 1, `Viewer overflow: ${viewerTitleInfo.overflow}px`);
    await maybeShot(phone, 'after-phone-viewer-dark');

    await phone.locator('.viewer-top button[aria-label="More options"]').click();
    await tracker.waitForLocator(
      phone.locator('.sheet').getByRole('button', { name: 'Copy name', exact: true }),
      'viewer More sheet Copy name button',
    );
    await tracker.waitForFn(
      phone,
      () => document.activeElement?.classList?.contains('sheet'),
      null,
      'viewer More sheet focus',
    );
    await phone.keyboard.press('Escape');
    await tracker.waitForLocator(phone.locator('.sheet'), 'viewer More sheet hidden', { state: 'hidden' });
    await phone.locator('.viewer-top button[aria-label="Close"]').click();
    await tracker.waitForLocator(phone.locator('.viewer'), 'viewer hidden', { state: 'hidden' });

    // Switch to Grid view on phone (dark + light): check up to 3 lines, all visible cells distinct.
    await phone.locator('.dock button[aria-label="Show as grid"]').click();
    await phone.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 0;
    });
    await waitForVisibleFitted(phone, '.grid a.cell .fn-slot', null, tracker);
    await setTheme(phone, 'dark', tracker);
    const phoneGridColorsDark = await readNameColors(phone, '.grid a.cell .cell-name');
    assert.ok(phoneGridColorsDark, 'Expected dimmed grid cell in dark theme');
    assert.equal(
      phoneGridColorsDark.primary,
      phoneListColorsDark.primary,
      'Grid cell primary text must match list primary token (--text) in dark theme',
    );
    assert.equal(
      phoneGridColorsDark.dim,
      phoneListColorsDark.dim,
      'Grid cell dimmed text must match list secondary token (--dim) in dark theme',
    );
    await maybeShot(phone, 'after-phone-grid-dark');
    await setTheme(phone, 'light', tracker);
    const phoneGridColorsLight = await readNameColors(phone, '.grid a.cell .cell-name');
    assert.ok(phoneGridColorsLight, 'Expected dimmed grid cell in light theme');
    assert.equal(
      phoneGridColorsLight.primary,
      phoneListColorsLight.primary,
      'Grid cell primary text must match list primary token (--text) in light theme',
    );
    assert.equal(
      phoneGridColorsLight.dim,
      phoneListColorsLight.dim,
      'Grid cell dimmed text must match list secondary token (--dim) in light theme',
    );
    await maybeShot(phone, 'after-phone-grid-light');
    await setTheme(phone, 'dark', tracker);

    const phoneGridCells = await collectAllScrolledRows(phone, 'main.main', '.grid a.cell', '.fn-slot', tracker);
    assert.equal(phoneGridCells.length, 52, 'Expected 52 media cells (40 jpg + 12 mov) in grid view');
    assert.equal(
      new Set(phoneGridCells.map((c) => c.drawn)).size,
      52,
      'Every grid cell visible text must be unique on phone',
    );
    for (const c of phoneGridCells) {
      assertFontIndependentLayout(c, { maxLines: 3, label: `phone grid "${c.ariaLabel}"` });
    }

    // Search results on phone: match highlight active, ZERO dimming (.fn-dim count == 0).
    await phone.goto(
      `${server.url}/#/files/@search?m=tower&p=${encodeURIComponent(capturesDir)}&q=${encodeURIComponent('Quarterly')}`,
      { waitUntil: 'domcontentloaded' },
    );
    await tracker.waitForFn(
      phone,
      () => location.hash.startsWith('#/files/@search') && document.querySelectorAll('.list .fn-match').length >= 4,
      null,
      'Quarterly search results on phone',
    );
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
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
    await tracker.waitForFn(
      phone,
      () =>
        location.hash === '#/files/@recent' &&
        document.querySelectorAll('.list .fn-match').length === 0 &&
        document.querySelectorAll('.list a.row').length >= 1,
      null,
      'Recent view entries on phone',
    );
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
    const recentRow = phone.getByRole('link', { name: reportName, exact: true });
    assert.equal(await recentRow.count(), 1, 'Recent view must expose full accessible name');

    // Text-size change recomputes cleanly without overflow.
    await phone.goto(capturesHref, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(
      phone.locator('.grid a.cell[aria-label="IMG_20261125_140311.jpg"], .list a.row[aria-label="IMG_20261125_140311.jpg"]'),
      'Captures folder entries before text-size check',
    );
    const showListBtn2 = phone.locator('.dock button[aria-label="Show as list"]');
    if (await showListBtn2.count()) await showListBtn2.click();
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
    const beforeBatches = await phone.evaluate(() => window.__LEGION_NAME_FIT_STATS__?.totalBatches || 0);
    await phone.evaluate(() => {
      document.documentElement.dataset.text = 'large';
    });
    await tracker.waitForFn(
      phone,
      (prev) => (window.__LEGION_NAME_FIT_STATS__?.totalBatches || 0) > prev,
      beforeBatches,
      'name-fit recompute after data-text=large',
    );
    await waitForVisibleFitted(phone, '.list a.row .fn-slot', null, tracker);
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
    const desk = tracker.attach(await deskCtx.newPage(), {
      label: 'desktop',
      collectConsole: consoleErrors,
      collectPage: pageErrors,
    });

    await connectPage(desk, server.url, tokenOut.token, tracker);
    await desk.goto(capturesHref, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(
      desk.locator('.split-right .grid a.cell[aria-label="IMG_20261125_140311.jpg"], .split-right .list a.row[aria-label="IMG_20261125_140311.jpg"]'),
      'Captures folder entries on desktop',
    );

    // Check pinned strip in left split pane.
    await waitForVisibleFitted(desk, '.split-left .tiles a.tile .fn-slot', null, tracker);
    const leftPinned = await desk.evaluate(() =>
      [...document.querySelectorAll('.split-left .tiles a.tile')].map((t) => {
        const slot = t.querySelector('.fn-slot');
        const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
        const lineTexts = lines.map((l) => l.textContent || '');
        let dimText = '';
        let strongText = '';
        for (const line of lines) {
          for (const node of line.childNodes) {
            const txt = node.textContent || '';
            if (node.nodeType === Node.ELEMENT_NODE && node.classList.contains('fn-dim')) {
              dimText += txt;
            } else {
              strongText += txt;
            }
          }
        }
        return {
          title: t.getAttribute('title') || '',
          ariaLabel: t.getAttribute('aria-label') || '',
          drawn: lineTexts.join(''),
          lineTexts,
          lineCount: lines.length,
          dimText,
          strongText,
          lineOverflows: lines.some((l) => l.scrollWidth > (slot?.clientWidth || 0) + 2),
        };
      }),
    );
    assert.equal(leftPinned.length, 2);
    assert.notEqual(leftPinned[0].drawn, leftPinned[1].drawn);
    for (const p of leftPinned) {
      assertFontIndependentLayout(p, { maxLines: 2, label: `desktop pinned "${p.ariaLabel}"` });
    }
    assert.deepEqual(
      leftPinned.map((p) => p.title).sort(),
      ['Screen Recording 2026-11-25 at 14.01.12.mov', 'Screen Recording 2026-11-25 at 14.02.04.mov'],
    );

    // Desktop Grid view screenshots & checks (dark + light).
    await waitForVisibleFitted(desk, '.split-right .grid a.cell .fn-slot', null, tracker);
    await setTheme(desk, 'dark', tracker);
    const deskGridColorsDark = await readNameColors(desk, '.split-right .grid a.cell .cell-name');
    assert.ok(deskGridColorsDark, 'Expected dimmed desktop grid cell in dark theme');
    await maybeShot(desk, 'after-desktop-grid-dark');
    await setTheme(desk, 'light', tracker);
    const deskGridColorsLight = await readNameColors(desk, '.split-right .grid a.cell .cell-name');
    assert.ok(deskGridColorsLight, 'Expected dimmed desktop grid cell in light theme');
    await maybeShot(desk, 'after-desktop-grid-light');
    await setTheme(desk, 'dark', tracker);

    const deskGridCells = await collectAllScrolledRows(
      desk,
      '.split-right .split-body',
      '.split-right .grid a.cell',
      '.fn-slot',
      tracker,
    );
    assert.equal(deskGridCells.length, 52);
    assert.equal(new Set(deskGridCells.map((c) => c.drawn)).size, 52, 'Desktop grid cells must all differ');
    for (const c of deskGridCells) {
      assert.equal(c.titleAttr, c.ariaLabel, 'Desktop grid hover title must equal full name');
      assertFontIndependentLayout(c, { maxLines: 3, label: `desktop grid "${c.ariaLabel}"` });
    }

    // Switch desktop to List view (dark + light), scroll to Screen Recordings to match before shot.
    await desk.locator('.split-right button[aria-label="Show as list"]').click();
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', null, tracker);
    await desk.locator('.split-right .list a.row[aria-label="Screen Recording 2026-11-25 at 14.01.12.mov"]').scrollIntoViewIfNeeded();
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', null, tracker);
    await setTheme(desk, 'dark', tracker);
    const deskListColorsDark = await readNameColors(desk, '.split-right .list a.row .title.fn-title');
    assert.ok(deskListColorsDark, 'Expected dimmed desktop list row in dark theme');
    assert.equal(deskGridColorsDark.primary, deskListColorsDark.primary);
    assert.equal(deskGridColorsDark.dim, deskListColorsDark.dim);
    await maybeShot(desk, 'after-desktop-list-dark');
    await setTheme(desk, 'light', tracker);
    const deskListColorsLight = await readNameColors(desk, '.split-right .list a.row .title.fn-title');
    assert.ok(deskListColorsLight, 'Expected dimmed desktop list row in light theme');
    assert.equal(deskGridColorsLight.primary, deskListColorsLight.primary);
    assert.equal(deskGridColorsLight.dim, deskListColorsLight.dim);
    await maybeShot(desk, 'after-desktop-list-light');
    await setTheme(desk, 'dark', tracker);

    await desk.evaluate(() => {
      const b = document.querySelector('.split-right .split-body');
      if (b) b.scrollTop = 0;
    });
    const deskListRows = await collectAllScrolledRows(
      desk,
      '.split-right .split-body',
      '.split-right .list a.row',
      '.fn-slot',
      tracker,
    );
    assert.equal(deskListRows.length, expectedCaptures.length);
    assert.equal(new Set(deskListRows.map((r) => r.drawn)).size, expectedCaptures.length);
    for (const r of deskListRows) {
      assert.equal(r.titleAttr, r.ariaLabel, 'Desktop list row hover title must equal full name');
      assertFontIndependentLayout(r, { maxLines: 2, label: `desktop list "${r.ariaLabel}"` });
    }
    const deskRec03 = deskListRows.find((r) => r.ariaLabel === 'Screen Recording 2026-11-25 at 14.03.19.mov');
    assert.ok(deskRec03);
    assertSeparatorEmphasis(deskRec03, {
      expectedDim: 'Screen Recording 2026-11-25 at ',
      expectedStrong: '14.03.19.mov',
      expectedDimSep: ' ',
      strongPrefix: '14.03.19',
      strongSuffix: '14.03.19.mov',
      label: 'desktop list Screen Recording',
    });
    const deskRepQ1 = deskListRows.find(
      (r) => r.ariaLabel === 'Quarterly_Architecture_Review_Q1_2026_Final_Approved.pdf',
    );
    assert.ok(deskRepQ1);
    assertSeparatorEmphasis(deskRepQ1, {
      expectedDim: 'Quarterly_Architecture_Review_',
      expectedStrong: 'Q1_2026_Final_Approved.pdf',
      expectedDimSep: '_',
      strongPrefix: 'Q1',
      strongSuffix: 'Approved.pdf',
      label: 'desktop list Quarterly report',
    });

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
    await tracker.waitForLocator(
      desk.locator('.split-right .list a.row[aria-label^="Archive_Backup_2026_11_25_Node_Primary_Snapshot_"]').first(),
      'ManyFiles entries rendered on desktop',
    );
    await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', '.split-right .split-body', tracker);
    await waitForLayoutSettled(desk, 2);

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
      await waitForVisibleFitted(desk, '.split-right .list a.row .fn-slot', '.split-right .split-body', tracker);
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
  },
);
