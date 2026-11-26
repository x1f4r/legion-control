// Structural split-view alignment tests across viewports (1100x720, 1440x900, 1920x1080),
// all five split tabs, breakpoint edges (819px phone vs 820px split), long names, empty right pane,
// and dual-pane scrolling. Optionally writes 1440x900 dark/light reference screenshots when
// LEGION_SPLIT_SHOTS_DIR is set.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { routeHref } from '../lib/route.js';
import {
  browserTest,
  launchBrowser,
  setTheme,
  startStaticUiServer,
  waitForLayoutSettled,
} from './browser-helpers.mjs';

const SHOTS_DIR = process.env.LEGION_SPLIT_SHOTS_DIR || '';

const VIEWPORTS = [
  { width: 1100, height: 720 },
  { width: 1440, height: 900 },
  { width: 1920, height: 1080 },
];

const LONG_MACHINE_NAME = 'studio-rendering-workstation-north-01';
const LONG_FOLDER_NAME =
  '2026-annual-architecture-deliverables-and-reference-assets-quarterly-verified-archive-release';

async function maybeShot(page, name) {
  if (!SHOTS_DIR) return;
  fs.mkdirSync(SHOTS_DIR, { recursive: true });
  await page.screenshot({ path: path.join(SHOTS_DIR, `${name}.png`) });
}

async function measureSplitGeometry(page) {
  return page.evaluate(() => {
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d');

    function textBaseline(root) {
      if (!root) return null;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      let node;
      while ((node = walker.nextNode())) {
        const text = node.nodeValue || '';
        const idx = text.search(/\S/);
        if (idx < 0) continue;
        const parentEl = node.parentElement;
        if (!parentEl) continue;
        const style = getComputedStyle(parentEl);
        if (style.visibility === 'hidden' || style.display === 'none') continue;
        const range = document.createRange();
        range.setStart(node, idx);
        range.setEnd(node, Math.min(text.length, idx + 8));
        const rect = range.getBoundingClientRect();
        if (rect.width === 0 || rect.height === 0) continue;
        ctx.font = `${style.fontStyle} ${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
        const m = ctx.measureText('Ag');
        const ascent = m.fontBoundingBoxAscent;
        const descent = m.fontBoundingBoxDescent;
        const ratio =
          Number.isFinite(ascent) && Number.isFinite(descent) && ascent + descent > 0
            ? ascent / (ascent + descent)
            : 0.78;
        return {
          text: text.trim(),
          top: rect.top,
          bottom: rect.bottom,
          baseline: rect.top + rect.height * ratio,
          fontSize: style.fontSize,
          fontWeight: style.fontWeight,
          lineHeight: style.lineHeight,
          fontFamily: style.fontFamily,
        };
      }
      return null;
    }

    function box(el) {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return {
        top: r.top,
        bottom: r.bottom,
        left: r.left,
        right: r.right,
        width: r.width,
        height: r.height,
        clientWidth: el.clientWidth,
        clientHeight: el.clientHeight,
        scrollWidth: el.scrollWidth,
        scrollHeight: el.scrollHeight,
      };
    }

    function firstVisualChildTop(body) {
      if (!body) return null;
      const candidates = body.querySelectorAll('.list, .tiles, .meters, .notice, .empty, .split-empty, .card, .field, .out, .actions');
      for (const el of candidates) {
        const r = el.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) return r.top;
      }
      return null;
    }

    function gutterViolations(row, name) {
      if (!row) return [];
      const rowRect = row.getBoundingClientRect();
      const cs = getComputedStyle(row);
      const borderLeft = parseFloat(cs.borderLeftWidth) || 0;
      const contentRightEdge = rowRect.left + borderLeft + row.clientWidth;
      const bad = [];
      const els = row.querySelectorAll(
        'h1, h2, a, button, input, select, textarea, .list, .tiles, .machine, .row, .tile, .split-fact, .split-label-text, .split-label-fact, .where, .machine-state, .tabs, .meters, .notice',
      );
      for (const el of els) {
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) continue;
        // Skip items scrolled out of view horizontally inside a clipped strip or vertically outside viewport.
        if (r.bottom <= rowRect.top || r.top >= rowRect.bottom) continue;
        if (r.right > contentRightEdge + 0.75) {
          bad.push({
            row: name,
            tag: el.tagName.toLowerCase(),
            cls: el.className || '',
            right: Number(r.right.toFixed(2)),
            contentRightEdge: Number(contentRightEdge.toFixed(2)),
            overflowPx: Number((r.right - contentRightEdge).toFixed(2)),
          });
        }
      }
      return bad;
    }

    const main = document.querySelector('main.main');
    const split = document.querySelector('.split');
    const leftPane = split?.querySelector('.split-left');
    const rightPane = split?.querySelector('.split-right');
    const leftTitleRow = leftPane?.querySelector('.split-title');
    const rightTitleRow = rightPane?.querySelector('.split-title');
    const leftToolsRow = leftPane?.querySelector('.split-tools');
    const rightToolsRow = rightPane?.querySelector('.split-tools');
    const leftLabelRow = leftPane?.querySelector('.split-label');
    const rightLabelRow = rightPane?.querySelector('.split-label');
    const leftBody = leftPane?.querySelector('.split-body');
    const rightBody = rightPane?.querySelector('.split-body');

    const leftH = leftTitleRow?.querySelector('.pane-title');
    const rightH = rightTitleRow?.querySelector('.pane-title');
    const leftFactEl = leftTitleRow?.querySelector('.split-fact');
    const rightFactEl = rightTitleRow?.querySelector('.split-fact');
    const leftLabelTextEl = leftLabelRow?.querySelector('.split-label-text') || leftLabelRow;
    const leftLabelFactEl = leftLabelRow?.querySelector('.split-label-fact');
    const rightLabelPrimaryEl =
      rightLabelRow?.querySelector('.where, .split-label-text') || rightLabelRow;
    const rightLabelFactEl = rightLabelRow?.querySelector('.split-label-fact, .machine-state');
    const leftInput = leftToolsRow?.querySelector('input');
    const rightInput = rightToolsRow?.querySelector('input');
    const rightListEl = rightBody?.querySelector('.list, .grid, .meters');

    // Check that side-by-side machine metric labels never collide horizontally inside narrow cards.
    const statOverlaps = [];
    for (const stats of document.querySelectorAll('.split-left .stats')) {
      const cells = Array.from(stats.querySelectorAll('.stat'));
      for (let i = 0; i < cells.length; i += 1) {
        const span = cells[i].querySelector('span');
        if (!span) continue;
        const cellRect = cells[i].getBoundingClientRect();
        if (span.scrollWidth > cellRect.width + 1) {
          statOverlaps.push({
            text: span.textContent?.trim() || '',
            scrollWidth: span.scrollWidth,
            cellWidth: Number(cellRect.width.toFixed(1)),
          });
        }
      }
    }

    return {
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
      hasTopHeader: Boolean(document.querySelector('header.top')),
      hasSplit: Boolean(split),
      main: box(main),
      leftPane: box(leftPane),
      rightPane: box(rightPane),
      dividerWidth: leftPane ? parseFloat(getComputedStyle(leftPane).borderRightWidth) || 0 : 0,
      leftTitleRow: box(leftTitleRow),
      rightTitleRow: box(rightTitleRow),
      leftToolsRow: box(leftToolsRow),
      rightToolsRow: box(rightToolsRow),
      leftLabelRow: box(leftLabelRow),
      rightLabelRow: box(rightLabelRow),
      leftBody: box(leftBody),
      rightBody: box(rightBody),
      leftBodyGutter: leftBody ? getComputedStyle(leftBody).scrollbarGutter : '',
      rightBodyGutter: rightBody ? getComputedStyle(rightBody).scrollbarGutter : '',
      leftTitleBase: textBaseline(leftH),
      leftFactBase: textBaseline(leftFactEl),
      rightTitleBase: textBaseline(rightH),
      rightFactBase: textBaseline(rightFactEl),
      rightFactBox: box(rightFactEl),
      leftLabelBase: textBaseline(leftLabelTextEl),
      leftLabelFactBase: textBaseline(leftLabelFactEl),
      rightLabelBase: textBaseline(rightLabelPrimaryEl),
      rightLabelFactBase: textBaseline(rightLabelFactEl),
      leftInputBox: box(leftInput),
      rightInputBox: box(rightInput),
      leftFirstChildTop: firstVisualChildTop(leftBody),
      rightFirstChildTop: firstVisualChildTop(rightBody),
      rightListBox: box(rightListEl),
      rightEmptyBox: box(rightBody?.querySelector('.split-empty')),
      statOverlaps,
      gutterViolations: [
        ...gutterViolations(leftTitleRow, 'left-title'),
        ...gutterViolations(rightTitleRow, 'right-title'),
        ...gutterViolations(leftToolsRow, 'left-tools'),
        ...gutterViolations(rightToolsRow, 'right-tools'),
        ...gutterViolations(leftLabelRow, 'left-label'),
        ...gutterViolations(rightLabelRow, 'right-label'),
        ...gutterViolations(leftBody, 'left-body'),
        ...gutterViolations(rightBody, 'right-body'),
      ],
    };
  });
}

function assertNear(actual, expected, tolerance, label) {
  assert.ok(
    actual != null && expected != null && Math.abs(actual - expected) <= tolerance,
    `${label}: expected ${actual} to be within ${tolerance}px of ${expected} (diff=${
      actual != null && expected != null ? (actual - expected).toFixed(2) : 'null'
    })`,
  );
}

function assertSplitAligned(m, label, { expectRightContent = true, expectMatchingFirstTop = true, expectRightFactAlignedWithList = false } = {}) {
  assert.equal(m.hasTopHeader, false, `${label}: header.top must be absent in wide split mode`);
  assert.equal(m.hasSplit, true, `${label}: .split must be mounted`);
  assert.equal(m.dividerWidth, 1, `${label}: 1px divider expected between panes`);

  // No page-level scroll.
  assert.ok(
    m.main.scrollHeight <= m.main.clientHeight + 1,
    `${label}: main should not scroll (scrollHeight=${m.main.scrollHeight}, clientHeight=${m.main.clientHeight})`,
  );

  // Left pane width matches clamp(300px, 28vw, 380px).
  const expectedLeftWidth = Math.max(300, Math.min(380, m.viewportWidth * 0.28));
  assertNear(m.leftPane.width, expectedLeftWidth, 1.5, `${label} left pane width`);

  // Header rows must not gain vertical scrollbars.
  for (const [rowName, rowBox] of [
    ['leftTitleRow', m.leftTitleRow],
    ['rightTitleRow', m.rightTitleRow],
    ['leftToolsRow', m.leftToolsRow],
    ['rightToolsRow', m.rightToolsRow],
    ['leftLabelRow', m.leftLabelRow],
    ['rightLabelRow', m.rightLabelRow],
  ]) {
    assert.ok(
      rowBox.scrollHeight <= rowBox.clientHeight + 1,
      `${label}: ${rowName} unexpectedly scrolls vertically (${rowBox.scrollHeight} > ${rowBox.clientHeight})`,
    );
  }

  // Shared row tracks: exact same top & height across left and right panes.
  assertNear(m.leftTitleRow.top, m.rightTitleRow.top, 1, `${label} title row top`);
  assertNear(m.leftTitleRow.height, m.rightTitleRow.height, 1, `${label} title row height`);
  assertNear(m.leftToolsRow.top, m.rightToolsRow.top, 1, `${label} tools row top`);
  assertNear(m.leftToolsRow.height, m.rightToolsRow.height, 1, `${label} tools row height`);
  assertNear(m.leftLabelRow.top, m.rightLabelRow.top, 1, `${label} label row top`);
  assertNear(m.leftLabelRow.height, m.rightLabelRow.height, 1, `${label} label row height`);
  assertNear(m.leftBody.top, m.rightBody.top, 1, `${label} body row top`);

  // Scrollbar gutters reserved on both bodies.
  assert.ok(m.leftBodyGutter.includes('stable'), `${label}: left body scrollbar-gutter should be stable (got "${m.leftBodyGutter}")`);
  assert.ok(m.rightBodyGutter.includes('stable'), `${label}: right body scrollbar-gutter should be stable (got "${m.rightBodyGutter}")`);

  // No element's right edge enters any scrollbar gutter.
  assert.deepEqual(
    m.gutterViolations,
    [],
    `${label}: elements entered scrollbar gutter: ${JSON.stringify(m.gutterViolations)}`,
  );

  // Machine card metric labels never overflow into adjacent columns.
  assert.deepEqual(
    m.statOverlaps,
    [],
    `${label}: machine stat labels overflowed their column: ${JSON.stringify(m.statOverlaps)}`,
  );

  // When both panes have search inputs, they share top and height.
  if (m.leftInputBox && m.rightInputBox) {
    assertNear(m.leftInputBox.top, m.rightInputBox.top, 1, `${label} search input top`);
    assertNear(m.leftInputBox.height, m.rightInputBox.height, 1, `${label} search input height`);
  }

  // Left label fact sits on same baseline as left label text.
  if (m.leftLabelBase && m.leftLabelFactBase) {
    assertNear(m.leftLabelBase.baseline, m.leftLabelFactBase.baseline, 1, `${label} left label vs fact baseline`);
  }

  // Left title fact sits on same baseline as left title text.
  if (m.leftTitleBase && m.leftFactBase) {
    assertNear(m.leftTitleBase.baseline, m.leftFactBase.baseline, 1, `${label} left title vs fact baseline`);
  }

  if (expectRightContent) {
    // Both pane titles share the exact same type style and sit on one baseline within 1 px.
    assert.ok(m.leftTitleBase && m.rightTitleBase, `${label}: both pane titles must render text`);
    assert.equal(m.leftTitleBase.fontSize, m.rightTitleBase.fontSize, `${label}: title font-size mismatch`);
    assert.equal(m.leftTitleBase.fontWeight, m.rightTitleBase.fontWeight, `${label}: title font-weight mismatch`);
    assert.equal(m.leftTitleBase.lineHeight, m.rightTitleBase.lineHeight, `${label}: title line-height mismatch`);
    assertNear(m.leftTitleBase.baseline, m.rightTitleBase.baseline, 1, `${label} title baseline`);

    if (m.rightFactBase) {
      assertNear(m.rightTitleBase.baseline, m.rightFactBase.baseline, 1, `${label} right title vs fact baseline`);
    }

    // Both label rows sit on one baseline within 1 px.
    assert.ok(m.leftLabelBase && m.rightLabelBase, `${label}: both label rows must render text`);
    assertNear(m.leftLabelBase.baseline, m.rightLabelBase.baseline, 1, `${label} label baseline`);

    if (m.rightLabelFactBase) {
      assertNear(m.rightLabelBase.baseline, m.rightLabelFactBase.baseline, 1, `${label} right label vs fact baseline`);
    }

    if (expectMatchingFirstTop && m.leftFirstChildTop != null && m.rightFirstChildTop != null) {
      assertNear(m.leftFirstChildTop, m.rightFirstChildTop, 1, `${label} first body card/row top`);
    }

    if (expectRightFactAlignedWithList && m.rightFactBox && m.rightListBox) {
      assertNear(m.rightFactBox.right, m.rightListBox.right, 1, `${label} right fact vs list right edge`);
    }
  }
}

browserTest(
  'split view aligns structurally across viewports, all five tabs, breakpoint edges, long names, empty right, and dual scrolling',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser();

    const consoleErrors = [];
    const pageErrors = [];

    try {
    // ── 1. All five tabs across 1100x720, 1440x900, and 1920x1080 ──────────────────────────────
    const tabs = [
      {
        name: 'machines',
        url: '/?mock#/machines',
        ready: async (page) => {
          await page.locator('.split-left a.machine', { hasText: 'tower' }).waitFor();
          await page.locator('.split-right h2.pane-title', { hasText: 'tower' }).waitFor();
          await page.locator('.split-right .list .gauge').first().waitFor();
          // Counts sit on the section labels they describe ("ON … 3", "OFF … 2").
          assert.equal(await page.locator('.split-left .split-label-text').innerText(), 'ON');
          assert.equal(await page.locator('.split-left .split-label-fact').innerText(), '3');
          assert.equal(
            await page.locator('.split-left .label-row', { hasText: 'OFF' }).locator('.label-fact').innerText(),
            '2',
          );
        },
        expectRightFactAlignedWithList: false,
      },
      {
        name: 'files',
        url: '/?mock#/files',
        ready: async (page) => {
          await page.locator('.split-left .pane-title', { hasText: 'Files' }).waitFor();
          await page.locator('.split-right .pane-title', { hasText: 'Home' }).waitFor();
          await page.locator('.split-right .split-fact', { hasText: /items/ }).waitFor();
          await page.locator('.split-right .split-body .list .row').first().waitFor();
          // When PINNED is first, its label has no machines fact; "3 ON" sits on the MACHINES label.
          assert.equal(await page.locator('.split-left .split-label-text').innerText(), 'PINNED');
          assert.equal(await page.locator('.split-left .split-label-fact').count(), 0);
          assert.equal(
            await page.locator('.split-left .label-row', { hasText: 'MACHINES' }).locator('.label-fact').innerText(),
            '3 ON',
          );
        },
        expectRightFactAlignedWithList: true,
      },
      {
        name: 'activity',
        url: '/?mock#/activity/tower/j-backup-1',
        ready: async (page) => {
          await page.locator('.split-left .pane-title', { hasText: 'Activity' }).waitFor();
          await page.locator('.split-right .pane-title', { hasText: 'Photo backup' }).waitFor();
          await page.locator('.split-right .split-body .list').first().waitFor();
          assert.equal(await page.locator('.split-left .split-label-text').innerText(), 'RUNNING NOW');
          assert.equal(await page.locator('.split-left .split-label-fact').innerText(), '2');
        },
        expectRightFactAlignedWithList: true,
      },
      {
        name: 'settings',
        url: '/?mock#/settings/device',
        ready: async (page) => {
          await page.locator('.split-left .pane-title', { hasText: 'Settings' }).waitFor();
          await page.locator('.split-right .pane-title', { hasText: /This / }).waitFor();
          await page.locator('.split-right .split-body .list').first().waitFor();
          assert.equal(await page.locator('.split-left .split-label-text').innerText(), 'GENERAL');
          assert.equal(await page.locator('.split-left .split-label-fact').count(), 0);
        },
        expectRightFactAlignedWithList: false,
      },
      {
        name: 'tools',
        url: '/?mock=tech#/actions/all',
        ready: async (page) => {
          await page.locator('.split-left .pane-title', { hasText: 'All tools' }).waitFor();
          await page.locator('.split-right .pane-title .pane-title-text').waitFor();
          await page.locator('.split-right .split-body .list, .split-right .split-body .field').first().waitFor();
          // Total tool count sits on the All tools title, not on the first family label ("AUDIT").
          await page.locator('.split-left .split-title .split-fact', { hasText: /^\d+$/ }).waitFor();
          assert.equal(await page.locator('.split-left .split-label-fact').count(), 0);
        },
        expectRightFactAlignedWithList: false,
        // Tools detail starts with a description paragraph before the first form card.
        expectMatchingFirstTop: false,
      },
    ];

    for (const vp of VIEWPORTS) {
      const ctx = await browser.newContext({ viewport: vp, colorScheme: 'dark' });
      const page = tracker.attach(await ctx.newPage(), {
        label: `${vp.width}x${vp.height}`,
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      for (const tab of tabs) {
        tracker.setStep(`split view ${tab.name} @ ${vp.width}x${vp.height}`);
        await page.goto(`${server.url}${tab.url}`, { waitUntil: 'domcontentloaded' });
        await tab.ready(page);
        await waitForLayoutSettled(page, 2);

        const metrics = await measureSplitGeometry(page);
        assertSplitAligned(metrics, `${tab.name} @ ${vp.width}x${vp.height}`, {
          expectRightContent: true,
          expectMatchingFirstTop: tab.expectMatchingFirstTop !== false,
          expectRightFactAlignedWithList: tab.expectRightFactAlignedWithList,
        });

        if (vp.width === 1440 && vp.height === 900 && SHOTS_DIR) {
          await setTheme(page, 'dark', tracker);
          await maybeShot(page, `${tab.name}-dark`);
          await setTheme(page, 'light', tracker);
          await maybeShot(page, `${tab.name}-light`);
          await setTheme(page, 'dark', tracker);
        }
      }

      await ctx.close();
    }

    // ── 2. Breakpoint edges: 819px (phone) vs 820px (split) ────────────────────────────────────
    for (const routeHash of ['#/files', '#/machines']) {
      tracker.setStep(`breakpoint 819px ${routeHash}`);
      const phoneCtx = await browser.newContext({ viewport: { width: 819, height: 720 }, colorScheme: 'dark' });
      const phonePage = tracker.attach(await phoneCtx.newPage(), { label: `819px ${routeHash}` });
      await phonePage.goto(`${server.url}/?mock${routeHash}`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(phonePage.locator('header.top h1'), `819px ${routeHash} header`);
      assert.equal(await phonePage.locator('.split').count(), 0, `819px ${routeHash}: must stay in phone layout`);
      const navBoxPhone = await phonePage.locator('nav.nav').boundingBox();
      assert.ok(navBoxPhone && navBoxPhone.y > 600 && navBoxPhone.width > 700, `819px ${routeHash}: bottom nav expected`);
      await phoneCtx.close();

      tracker.setStep(`breakpoint 820px ${routeHash}`);
      const splitCtx = await browser.newContext({ viewport: { width: 820, height: 720 }, colorScheme: 'dark' });
      const splitPage = tracker.attach(await splitCtx.newPage(), { label: `820px ${routeHash}` });
      await splitPage.goto(`${server.url}/?mock${routeHash}`, { waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(splitPage.locator('.split'), `820px ${routeHash} split container`);
      assert.equal(await splitPage.locator('header.top').count(), 0, `820px ${routeHash}: header.top must disappear in split layout`);
      const navBoxSplit = await splitPage.locator('nav.nav').boundingBox();
      assert.ok(navBoxSplit && navBoxSplit.width <= 90 && navBoxSplit.height >= 700, `820px ${routeHash}: left rail expected`);
      await splitCtx.close();
    }

    // ── 3. Edge cases at 1440x900: long names, empty right pane, dual scrolling ────────────────
    const edgeCtx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
    const edgePage = tracker.attach(await edgeCtx.newPage(), {
      label: 'edge',
      collectConsole: consoleErrors,
      collectPage: pageErrors,
    });

    // 3a. Files with a long folder name and a long machine name.
    tracker.setStep('files long names @ 1440x900');
    const longHref = routeHref({
      section: 'files',
      view: 'folder',
      machine: LONG_MACHINE_NAME,
      path: `/home/me/${LONG_FOLDER_NAME}`,
    });
    await edgePage.goto(`${server.url}/?mock=longnames${longHref}`, {
      waitUntil: 'domcontentloaded',
    });
    await tracker.waitForLocator(edgePage.locator('.split-right .pane-title', { hasText: LONG_FOLDER_NAME }), 'long folder title');
    await tracker.waitForLocator(edgePage.locator('.split-right .split-fact', { hasText: '6 items' }), 'long folder item count');
    await tracker.waitForLocator(edgePage.locator('.split-right .split-body .list .row').first(), 'long folder first row');
    await waitForLayoutSettled(edgePage, 2);
    const longMetrics = await measureSplitGeometry(edgePage);
    assertSplitAligned(longMetrics, 'files long names @ 1440x900', {
      expectRightContent: true,
      expectMatchingFirstTop: true,
      expectRightFactAlignedWithList: true,
    });
    const longTitleTruncated = await edgePage.evaluate(() => {
      const el = document.querySelector('.split-right .pane-title-text');
      const fact = document.querySelector('.split-right .split-fact');
      if (!el || !fact) return null;
      return {
        scrollWidth: el.scrollWidth,
        clientWidth: el.clientWidth,
        titleRight: el.getBoundingClientRect().right,
        factLeft: fact.getBoundingClientRect().left,
      };
    });
    assert.ok(
      longTitleTruncated && longTitleTruncated.scrollWidth > longTitleTruncated.clientWidth,
      `Expected long folder title to truncate cleanly: ${JSON.stringify(longTitleTruncated)}`,
    );
    assert.ok(
      longTitleTruncated.titleRight <= longTitleTruncated.factLeft + 0.5,
      `Long folder title must not collide with item count: ${JSON.stringify(longTitleTruncated)}`,
    );
    await maybeShot(edgePage, 'files-long-names');

    // 3b. Files with an empty right pane.
    tracker.setStep('files empty right @ 1440x900');
    await edgePage.goto(`${server.url}/?mock=emptyright#/files`, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(
      edgePage.locator('.split-right .split-empty', { hasText: 'Choose a place on the left.' }),
      'files empty right message',
    );
    await waitForLayoutSettled(edgePage, 2);
    const emptyMetrics = await measureSplitGeometry(edgePage);
    assertSplitAligned(emptyMetrics, 'files empty right @ 1440x900', {
      expectRightContent: false,
    });
    assert.ok(emptyMetrics.rightEmptyBox, 'empty right pane sentence must be visible');
    const bodyMidX = emptyMetrics.rightBody.left + emptyMetrics.rightBody.clientWidth / 2;
    const emptyMidX = emptyMetrics.rightEmptyBox.left + emptyMetrics.rightEmptyBox.width / 2;
    assertNear(emptyMidX, bodyMidX, 24, 'empty right pane sentence horizontally centred');
    await maybeShot(edgePage, 'files-empty-right');

    // Also verify Activity empty right pane when no item is chosen.
    tracker.setStep('activity empty right @ 1440x900');
    await edgePage.goto(`${server.url}/?mock#/activity`, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(
      edgePage.locator('.split-right .split-empty', { hasText: 'Choose an item on the left.' }),
      'activity empty right message',
    );
    await waitForLayoutSettled(edgePage, 2);
    const actEmptyMetrics = await measureSplitGeometry(edgePage);
    assertSplitAligned(actEmptyMetrics, 'activity empty right @ 1440x900', {
      expectRightContent: false,
    });

    // 3b-ii. Files when nothing is pinned or synced (?mock=old): MACHINES is first and carries "3 ON".
    tracker.setStep('files machines-first @ 1440x900');
    await edgePage.goto(`${server.url}/?mock=old#/files`, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(edgePage.locator('.split-left .split-label-text', { hasText: /^MACHINES$/i }), 'MACHINES label');
    await tracker.waitForLocator(edgePage.locator('.split-left .split-label-fact', { hasText: /^3 ON$/i }), '3 ON label fact');
    await tracker.waitForLocator(edgePage.locator('.split-right .pane-title', { hasText: 'Home' }), 'Home pane title');
    await waitForLayoutSettled(edgePage, 2);
    const oldFilesMetrics = await measureSplitGeometry(edgePage);
    assertSplitAligned(oldFilesMetrics, 'files machines-first @ 1440x900', {
      expectRightContent: true,
      expectMatchingFirstTop: false,
      expectRightFactAlignedWithList: false,
    });

    // 3c. Files with a list long enough to scroll in BOTH panes.
    tracker.setStep('files dual scrolling @ 1440x900');
    await edgePage.goto(`${server.url}/?mock=longlist#/files`, { waitUntil: 'domcontentloaded' });
    await tracker.waitForLocator(edgePage.locator('.split-right .split-fact', { hasText: /items/ }), 'longlist item count');
    await tracker.waitForLocator(edgePage.locator('.split-left .row', { hasText: 'node-12' }), 'node-12 left row');
    await tracker.waitForLocator(edgePage.locator('.split-right .split-body .list .row').nth(20), '21st right row');
    await waitForLayoutSettled(edgePage, 2);

    const scrollBefore = await measureSplitGeometry(edgePage);
    assert.ok(
      scrollBefore.leftBody.scrollHeight > scrollBefore.leftBody.clientHeight + 80,
      `Expected scrollable left pane (${scrollBefore.leftBody.scrollHeight} <= ${scrollBefore.leftBody.clientHeight})`,
    );
    assert.ok(
      scrollBefore.rightBody.scrollHeight > scrollBefore.rightBody.clientHeight + 80,
      `Expected scrollable right pane (${scrollBefore.rightBody.scrollHeight} <= ${scrollBefore.rightBody.clientHeight})`,
    );
    assertSplitAligned(scrollBefore, 'files both scroll (top) @ 1440x900', {
      expectRightContent: true,
      expectMatchingFirstTop: true,
      expectRightFactAlignedWithList: true,
    });
    await maybeShot(edgePage, 'files-both-scroll');

    // Scroll both bodies and verify titles, tools, and labels stay pinned and aligned.
    await edgePage.evaluate(() => {
      document.querySelector('.split-left .split-body').scrollTop = 220;
      document.querySelector('.split-right .split-body').scrollTop = 340;
    });
    await tracker.waitForFn(
      edgePage,
      () =>
        document.querySelector('.split-left .split-body')?.scrollTop >= 200 &&
        document.querySelector('.split-right .split-body')?.scrollTop >= 300,
      null,
      'dual panes scrollTop update',
    );
    await waitForLayoutSettled(edgePage, 2);
    const scrollAfter = await measureSplitGeometry(edgePage);
    assertNear(scrollAfter.leftTitleRow.top, scrollBefore.leftTitleRow.top, 0.5, 'left title pinned while scrolling');
    assertNear(scrollAfter.rightTitleRow.top, scrollBefore.rightTitleRow.top, 0.5, 'right title pinned while scrolling');
    assertNear(scrollAfter.leftToolsRow.top, scrollBefore.leftToolsRow.top, 0.5, 'left tools pinned while scrolling');
    assertNear(scrollAfter.rightToolsRow.top, scrollBefore.rightToolsRow.top, 0.5, 'right tools pinned while scrolling');
    assertNear(scrollAfter.leftLabelRow.top, scrollBefore.leftLabelRow.top, 0.5, 'left label pinned while scrolling');
    assertNear(scrollAfter.rightLabelRow.top, scrollBefore.rightLabelRow.top, 0.5, 'right label pinned while scrolling');
    assert.deepEqual(
      scrollAfter.gutterViolations,
      [],
      `Gutter violations after scrolling: ${JSON.stringify(scrollAfter.gutterViolations)}`,
    );

    await edgeCtx.close();

    assert.deepEqual(consoleErrors, [], `Unexpected console errors: ${consoleErrors.join('\n')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
  }
},
);
