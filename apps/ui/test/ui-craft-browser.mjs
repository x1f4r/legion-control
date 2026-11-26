// Browser tests for Legion UI craft rules:
// 1. Every rendered icon across views resolves to an existing symbol in icons.js.
// 2. Every button kind responds to pointerdown within one animation frame with both scale(0.97)
//    and a visible surface/highlight step in dark and light themes; reduced motion keeps the colour
//    step and drops the scale; disabled controls share opacity 0.45 and ignore press.
// 3. Focus rings on .tabs and .filters fit completely inside their scroll containers without
//    clipping, and shape-matched focus rings hold on icon, dismiss, resume, and swipe controls.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(TEST_DIR, '..');

const MOCK_TEST_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self' blob: data:; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const MIME_BY_EXT = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.webmanifest': 'application/manifest+json; charset=utf-8',
};

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

function buildButtonFixtureHtml() {
  const icon = (name) => `<svg aria-hidden="true"><use href="#i-${name}"></use></svg>`;
  return `<!doctype html>
<html lang="en" data-theme="dark">
<head>
  <meta charset="utf-8" />
  <title>UI Craft Button Fixture</title>
  <link rel="stylesheet" href="/styles.css" />
</head>
<body style="padding:24px;display:grid;gap:14px;align-content:start;">
  <svg class="sprite" aria-hidden="true">
    <symbol id="i-more" viewBox="0 0 24 24"><circle cx="5" cy="12" r="2" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="2" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="2" fill="currentColor" stroke="none"/></symbol>
    <symbol id="i-back" viewBox="0 0 24 24"><path d="M15.7 4 7.7 12l8 8"/></symbol>
    <symbol id="i-play" viewBox="0 0 24 24"><path d="M7.5 4.2 21 12 7.5 19.8z" fill="currentColor" stroke="none"/></symbol>
    <symbol id="i-next" viewBox="0 0 24 24"><path d="M5.2 5 16.5 12 5.2 19z" fill="currentColor" stroke="none"/><rect x="17.7" y="5" width="2.5" height="14" rx="1" fill="currentColor" stroke="none"/></symbol>
    <symbol id="i-skip-back-10" viewBox="0 0 24 24"><path d="M12 4.6a7.4 7.4 0 1 1-6.409 3.7"/></symbol>
    <symbol id="i-download" viewBox="0 0 24 24"><path d="M12 3.2v9.8M7.2 8.2 12 13l4.8-4.8M6.5 17.6h11"/></symbol>
  </svg>
  <div style="display:flex;flex-wrap:wrap;gap:12px;align-items:center;">
    <button type="button" class="btn" data-kind="btn">Continue</button>
    <button type="button" class="btn primary" data-kind="btn-primary">Save</button>
    <button type="button" class="btn danger" data-kind="btn-danger">Delete</button>
    <button type="button" class="btn quiet" data-kind="btn-quiet">Cancel</button>
    <button type="button" class="btn small" data-kind="btn-small">Retry</button>
    <button type="button" class="btn icon" data-kind="btn-icon" data-uses-before="1" aria-label="More">${icon('more')}</button>
    <header class="top" style="padding:0;"><button type="button" class="btn icon back" data-kind="btn-icon-back" data-uses-before="1" aria-label="Back">${icon('back')}</button></header>
    <button type="button" class="chip-btn" aria-pressed="false" data-kind="chip-btn">1×</button>
    <button type="button" class="chip-btn" aria-pressed="true" data-kind="chip-btn-pressed">1.5×</button>
    <button type="button" class="update-note-dismiss" data-kind="update-note-dismiss" data-uses-before="1" aria-label="Dismiss">×</button>
  </div>
  <div class="seg" style="width:180px;">
    <button type="button" aria-pressed="false" data-kind="seg-btn">List</button>
    <button type="button" aria-pressed="true" data-kind="seg-btn-pressed">Grid</button>
  </div>
  <div class="tabs" id="fixture-tabs" style="width:260px;">
    <button type="button" aria-selected="false" data-kind="tabs-btn">Overview</button>
    <button type="button" aria-selected="true" data-kind="tabs-btn-selected">Files</button>
  </div>
  <div class="filters" id="fixture-filters" style="width:260px;">
    <button type="button" aria-pressed="false" data-kind="filters-btn">Video</button>
    <button type="button" aria-pressed="true" data-kind="filters-btn-pressed">Audio</button>
  </div>
  <div class="bar4" style="width:180px;">
    <button type="button" data-kind="bar4-btn">${icon('download')}Download</button>
    <button type="button" disabled data-disabled-kind="bar4-disabled">${icon('download')}Disabled</button>
  </div>
  <div class="swipe-row" style="width:260px;height:46px;border-radius:10px;overflow:hidden;">
    <div class="swipe-actions">
      <button type="button" class="swipe-action" data-kind="swipe-danger">Delete</button>
      <button type="button" class="swipe-action neutral" data-kind="swipe-neutral">Rename</button>
      <button type="button" class="swipe-action accent" data-kind="swipe-accent">Star</button>
    </div>
  </div>
  <div class="viewer" style="position:static;display:flex;flex-wrap:wrap;gap:12px;align-items:center;padding:16px;border-radius:12px;">
    <button type="button" class="btn icon player-center-btn" data-kind="player-center-btn" aria-label="Back 10s">${icon('skip-back-10')}</button>
    <button type="button" class="btn icon player-center-btn main" data-kind="player-center-main" aria-label="Play">${icon('play')}</button>
    <button type="button" class="btn player-speed-btn" data-kind="player-speed-btn">1.5×</button>
    <button type="button" class="btn icon music-ctl-btn" data-kind="music-ctl-btn" data-uses-before="1" aria-label="Next">${icon('next')}</button>
    <button type="button" class="btn icon music-play-btn" data-kind="music-play-btn" aria-label="Play">${icon('play')}</button>
    <div class="player-resume-chip inline">
      <span>Resumed at 12:40</span>
      <button type="button" class="player-resume-btn" data-kind="player-resume-btn">Start over</button>
    </div>
    <div class="viewer-bar" style="padding:0;background:none;">
      <div class="bar4" style="width:180px;">
        <button type="button" data-kind="viewer-bar4-btn">${icon('download')}Download</button>
        <button type="button" disabled data-disabled-kind="viewer-bar4-disabled">${icon('download')}Open in…</button>
      </div>
    </div>
  </div>
  <div style="display:flex;gap:12px;align-items:center;">
    <button type="button" class="btn" disabled data-disabled-kind="btn-disabled">Continue</button>
    <button type="button" class="btn primary" disabled data-disabled-kind="btn-primary-disabled">Save</button>
    <button type="button" class="btn icon" disabled data-disabled-kind="btn-icon-disabled" data-uses-before="1" aria-label="More">${icon('more')}</button>
  </div>
</body>
</html>`;
}

async function startStaticUiServer() {
  const fixtureHtml = buildButtonFixtureHtml();
  const server = http.createServer((req, res) => {
    const urlPath = (req.url || '/').split('?')[0];
    if (urlPath === '/__craft-fixture') {
      res.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Cache-Control': 'no-store',
      });
      res.end(fixtureHtml);
      return;
    }
    const rel = urlPath === '/' ? 'index.html' : urlPath.replace(/^\/+/, '');
    const full = path.resolve(UI_ROOT, rel);
    if (!full.startsWith(UI_ROOT) || !fs.existsSync(full) || fs.statSync(full).isDirectory()) {
      res.writeHead(404);
      res.end('Not found');
      return;
    }
    const ext = path.extname(full).toLowerCase();
    res.writeHead(200, {
      'Content-Type': MIME_BY_EXT[ext] || 'application/octet-stream',
      'Content-Security-Policy': MOCK_TEST_CSP,
      'Cache-Control': 'no-store',
    });
    fs.createReadStream(full).pipe(res);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}`,
    stop: () => new Promise((resolve) => server.close(resolve)),
  };
}

function parseScaleFromMatrix(transform) {
  if (!transform || transform === 'none') return 1;
  const m = transform.match(/^matrix\(([^)]+)\)$/);
  if (!m) return NaN;
  const parts = m[1].split(',').map((s) => Number(s.trim()));
  return parts[0];
}

test('UI craft: every rendered icon across views resolves to a symbol in icons.js', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({ executablePath: findBrowser(), headless: true });
  const consoleErrors = [];
  const pageErrors = [];
  try {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
    const page = await ctx.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    const routes = [
      '/?mock#/machines',
      '/?mock#/files',
      '/?mock#/files/tower/home',
      '/?mock#/activity',
      '/?mock#/settings',
      '/?mock=tech#/actions',
    ];

    const seenIcons = new Set();
    for (const route of routes) {
      await page.goto(`${server.url}${route}`, { waitUntil: 'domcontentloaded' });
      await page.locator('svg.sprite symbol').first().waitFor({ state: 'attached', timeout: 5000 });
      await page.waitForTimeout(180);

      const check = await page.evaluate(() => {
        const symbols = new Set(
          Array.from(document.querySelectorAll('svg.sprite symbol')).map((s) => s.id),
        );
        const badViewBox = Array.from(document.querySelectorAll('svg.sprite symbol'))
          .filter((s) => s.getAttribute('viewBox') !== '0 0 24 24')
          .map((s) => s.id);
        const missing = [];
        const used = [];
        for (const use of document.querySelectorAll('svg use')) {
          const href = use.getAttribute('href') || use.getAttribute('xlink:href') || '';
          if (!href.startsWith('#i-')) continue;
          const id = href.slice(1);
          used.push(id);
          if (!symbols.has(id)) missing.push(id);
        }
        return { missing, used, badViewBox, symbolCount: symbols.size };
      });

      assert.ok(check.symbolCount >= 30, `Expected full icon sprite on ${route}`);
      assert.deepEqual(check.badViewBox, [], `All symbols must use 24x24 viewBox (${route})`);
      assert.deepEqual(check.missing, [], `Missing icon symbol(s) on ${route}: ${check.missing.join(', ')}`);
      for (const id of check.used) seenIcons.add(id);
    }

    assert.ok(seenIcons.size >= 8, `Expected to inspect multiple live icons across views, saw ${seenIcons.size}`);
    assert.deepEqual(consoleErrors, [], `Unexpected console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Unexpected page errors: ${pageErrors.join('; ')}`);
    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

test('UI craft: every button kind shows immediate press feedback within one frame, obeys reduced motion, and unifies disabled state', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({ executablePath: findBrowser(), headless: true });
  try {
    const ctx = await browser.newContext({ viewport: { width: 1024, height: 900 }, colorScheme: 'dark' });
    const page = await ctx.newPage();
    await page.goto(`${server.url}/__craft-fixture`, { waitUntil: 'load' });

    const kinds = await page.evaluate(() =>
      Array.from(document.querySelectorAll('[data-kind]')).map((el) => ({
        kind: el.dataset.kind,
        usesBefore: el.dataset.usesBefore === '1',
      })),
    );
    assert.ok(kinds.length >= 20, `Expected every button kind in fixture, got ${kinds.length}`);

    for (const theme of ['dark', 'light']) {
      await page.evaluate((t) => {
        document.documentElement.dataset.theme = t;
      }, theme);
      await page.waitForTimeout(40);

      for (const { kind, usesBefore } of kinds) {
        const loc = page.locator(`[data-kind="${kind}"]`);
        await loc.scrollIntoViewIfNeeded();
        const box = await loc.boundingBox();
        assert.ok(box && box.width > 0 && box.height > 0, `Missing box for ${kind} (${theme})`);

        const before = await loc.evaluate((el) => {
          const cs = getComputedStyle(el);
          const bf = getComputedStyle(el, '::before');
          return {
            transform: cs.transform,
            bg: cs.backgroundColor,
            beforeOpacity: bf.opacity,
            beforeBg: bf.backgroundColor,
          };
        });

        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await page.mouse.down();
        // Wait exactly one animation frame to confirm press response arrives without lag.
        const pressed = await loc.evaluate(
          (el) =>
            new Promise((resolve) =>
              requestAnimationFrame(() => {
                const cs = getComputedStyle(el);
                const bf = getComputedStyle(el, '::before');
                resolve({
                  transform: cs.transform,
                  bg: cs.backgroundColor,
                  transitionDuration: cs.transitionDuration,
                  touchAction: cs.touchAction,
                  beforeOpacity: bf.opacity,
                  beforeBg: bf.backgroundColor,
                  beforeWidth: bf.width,
                  beforeHeight: bf.height,
                });
              }),
            ),
        );
        await page.mouse.up();

        const scale = parseScaleFromMatrix(pressed.transform);
        assert.ok(
          Math.abs(scale - 0.97) < 0.01,
          `${kind} (${theme}): expected scale(0.97) within 1 frame on pointerdown, got transform="${pressed.transform}"`,
        );
        assert.equal(
          pressed.touchAction,
          'manipulation',
          `${kind} (${theme}): expected touch-action: manipulation`,
        );

        if (usesBefore) {
          assert.equal(
            pressed.beforeOpacity,
            '1',
            `${kind} (${theme}): expected 40 px ::before highlight opacity 1 on pointerdown (was ${before.beforeOpacity})`,
          );
          assert.equal(pressed.beforeWidth, '40px', `${kind} (${theme}): expected 40px highlight width`);
          assert.equal(pressed.beforeHeight, '40px', `${kind} (${theme}): expected 40px highlight height`);
          assert.notEqual(
            pressed.beforeBg,
            'rgba(0, 0, 0, 0)',
            `${kind} (${theme}): expected visible highlight background color`,
          );
        } else {
          assert.notEqual(
            pressed.bg,
            before.bg,
            `${kind} (${theme}): expected background-color step within 1 frame on pointerdown (stayed ${pressed.bg})`,
          );
        }
      }

      // Disabled controls share opacity 0.45, cursor default, and zero press response.
      const disabledKinds = await page.evaluate(() =>
        Array.from(document.querySelectorAll('[data-disabled-kind]')).map((el) => ({
          kind: el.dataset.disabledKind,
          usesBefore: el.dataset.usesBefore === '1',
        })),
      );
      for (const { kind, usesBefore } of disabledKinds) {
        const loc = page.locator(`[data-disabled-kind="${kind}"]`);
        await loc.scrollIntoViewIfNeeded();
        const box = await loc.boundingBox();
        const before = await loc.evaluate((el) => {
          const cs = getComputedStyle(el);
          const bf = getComputedStyle(el, '::before');
          return {
            opacity: cs.opacity,
            cursor: cs.cursor,
            transform: cs.transform,
            bg: cs.backgroundColor,
            beforeOpacity: bf.opacity,
          };
        });
        assert.equal(before.opacity, '0.45', `${kind} (${theme}): expected disabled opacity 0.45`);
        assert.equal(before.cursor, 'default', `${kind} (${theme}): expected disabled cursor default`);

        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await page.mouse.down();
        const pressed = await loc.evaluate(
          (el) =>
            new Promise((resolve) =>
              requestAnimationFrame(() => {
                const cs = getComputedStyle(el);
                const bf = getComputedStyle(el, '::before');
                resolve({ transform: cs.transform, bg: cs.backgroundColor, beforeOpacity: bf.opacity });
              }),
            ),
        );
        await page.mouse.up();
        assert.equal(pressed.transform, 'none', `${kind} (${theme}): disabled control must not scale on press`);
        assert.equal(pressed.bg, before.bg, `${kind} (${theme}): disabled control must not change fill on press`);
        if (usesBefore) {
          assert.equal(
            pressed.beforeOpacity,
            '0',
            `${kind} (${theme}): disabled icon button must not show round highlight on press`,
          );
        }
      }
    }

    // Reduced motion: colour step stays, scale does not.
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.evaluate(() => {
      document.documentElement.dataset.theme = 'dark';
    });
    for (const kind of ['btn', 'btn-primary', 'btn-icon', 'chip-btn', 'tabs-btn', 'swipe-danger', 'update-note-dismiss']) {
      const loc = page.locator(`[data-kind="${kind}"]`);
      const usesBefore = (await loc.getAttribute('data-uses-before')) === '1';
      await loc.scrollIntoViewIfNeeded();
      const box = await loc.boundingBox();
      const before = await loc.evaluate((el) => ({
        bg: getComputedStyle(el).backgroundColor,
        beforeOpacity: getComputedStyle(el, '::before').opacity,
      }));
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
      const pressed = await loc.evaluate(
        (el) =>
          new Promise((resolve) =>
            requestAnimationFrame(() =>
              resolve({
                transform: getComputedStyle(el).transform,
                bg: getComputedStyle(el).backgroundColor,
                beforeOpacity: getComputedStyle(el, '::before').opacity,
              }),
            ),
          ),
      );
      await page.mouse.up();
      assert.equal(
        pressed.transform,
        'none',
        `${kind} (reduced-motion): scale must be dropped under prefers-reduced-motion: reduce`,
      );
      if (usesBefore) {
        assert.equal(
          pressed.beforeOpacity,
          '1',
          `${kind} (reduced-motion): colour highlight must stay under prefers-reduced-motion: reduce`,
        );
      } else {
        assert.notEqual(
          pressed.bg,
          before.bg,
          `${kind} (reduced-motion): colour step must stay under prefers-reduced-motion: reduce`,
        );
      }
    }

    await ctx.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

test('UI craft: focus rings on .tabs and .filters fit fully inside their scroll containers and follow element shapes', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({ executablePath: findBrowser(), headless: true });
  try {
    // 1. Live wide split view: .split-tools .tabs
    const wideCtx = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
    const widePage = await wideCtx.newPage();
    await widePage.goto(`${server.url}/?mock#/machines`, { waitUntil: 'domcontentloaded' });
    await widePage.locator('.split-tools .tabs button').first().waitFor({ timeout: 5000 });

    const wideTabsCheck = await widePage.evaluate(() => {
      const container = document.querySelector('.split-tools .tabs');
      const btn = container.querySelector('button');
      btn.focus({ focusVisible: true });
      const cr = container.getBoundingClientRect();
      const br = btn.getBoundingClientRect();
      const cs = getComputedStyle(btn);
      const outlineW = parseFloat(cs.outlineWidth) || 0;
      const outlineOff = parseFloat(cs.outlineOffset) || 0;
      const extent = outlineW + Math.max(0, outlineOff);
      return {
        containerTop: cr.top,
        containerBottom: cr.bottom,
        containerLeft: cr.left,
        containerRight: cr.right,
        ringTop: br.top - extent,
        ringBottom: br.bottom + extent,
        ringLeft: br.left - extent,
        ringRight: br.right + extent,
        outlineW,
        outlineStyle: cs.outlineStyle,
        borderRadius: parseFloat(cs.borderRadius) || 0,
      };
    });
    assert.ok(wideTabsCheck.outlineW >= 2, `Expected >= 2px outline on split .tabs button, got ${wideTabsCheck.outlineW}`);
    assert.ok(
      wideTabsCheck.ringTop >= wideTabsCheck.containerTop - 0.25,
      `Split .tabs focus ring clipped at top: ringTop=${wideTabsCheck.ringTop} < containerTop=${wideTabsCheck.containerTop}`,
    );
    assert.ok(
      wideTabsCheck.ringBottom <= wideTabsCheck.containerBottom + 0.25,
      `Split .tabs focus ring clipped at bottom: ringBottom=${wideTabsCheck.ringBottom} > containerBottom=${wideTabsCheck.containerBottom}`,
    );
    assert.ok(
      wideTabsCheck.ringLeft >= wideTabsCheck.containerLeft - 0.25,
      `Split .tabs focus ring clipped at left: ringLeft=${wideTabsCheck.ringLeft} < containerLeft=${wideTabsCheck.containerLeft}`,
    );
    await wideCtx.close();

    // 2. Live phone view: .filters in Files search
    const phoneCtx = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
    const phonePage = await phoneCtx.newPage();
    await phonePage.goto(`${server.url}/?mock#/files/tower/home`, { waitUntil: 'domcontentloaded' });
    await phonePage.locator('input[type="search"]').first().waitFor({ timeout: 5000 });
    await phonePage.locator('input[type="search"]').first().fill('a');
    await phonePage.locator('.filters button').first().waitFor({ timeout: 5000 });

    const filtersCheck = await phonePage.evaluate(() => {
      const container = document.querySelector('.filters');
      const btn = container.querySelector('button');
      btn.focus({ focusVisible: true });
      const cr = container.getBoundingClientRect();
      const br = btn.getBoundingClientRect();
      const cs = getComputedStyle(btn);
      const outlineW = parseFloat(cs.outlineWidth) || 0;
      const outlineOff = parseFloat(cs.outlineOffset) || 0;
      const extent = outlineW + Math.max(0, outlineOff);
      return {
        containerTop: cr.top,
        containerBottom: cr.bottom,
        containerLeft: cr.left,
        ringTop: br.top - extent,
        ringBottom: br.bottom + extent,
        ringLeft: br.left - extent,
        outlineW,
      };
    });
    assert.ok(filtersCheck.outlineW >= 2, `Expected >= 2px outline on .filters button, got ${filtersCheck.outlineW}`);
    assert.ok(
      filtersCheck.ringTop >= filtersCheck.containerTop - 0.25,
      `.filters focus ring clipped at top: ringTop=${filtersCheck.ringTop} < containerTop=${filtersCheck.containerTop}`,
    );
    assert.ok(
      filtersCheck.ringBottom <= filtersCheck.containerBottom + 0.25,
      `.filters focus ring clipped at bottom: ringBottom=${filtersCheck.ringBottom} > containerBottom=${filtersCheck.containerBottom}`,
    );
    assert.ok(
      filtersCheck.ringLeft >= filtersCheck.containerLeft - 0.25,
      `.filters focus ring clipped at left: ringLeft=${filtersCheck.ringLeft} < containerLeft=${filtersCheck.containerLeft}`,
    );

    // 3. Shape-matched focus rings on fixture: .tabs, .filters, .player-resume-btn, .update-note-dismiss, .btn.icon, .swipe-action
    await phonePage.goto(`${server.url}/__craft-fixture`, { waitUntil: 'load' });
    const shapeCheck = await phonePage.evaluate(() => {
      const inspect = (sel) => {
        const el = document.querySelector(sel);
        el.focus({ focusVisible: true });
        const cs = getComputedStyle(el);
        const br = el.getBoundingClientRect();
        return {
          outlineWidth: parseFloat(cs.outlineWidth) || 0,
          outlineOffset: parseFloat(cs.outlineOffset) || 0,
          borderRadius: cs.borderRadius,
          width: br.width,
          height: br.height,
          boxShadow: cs.boxShadow,
        };
      };
      const containerRing = (containerSel) => {
        const c = document.querySelector(containerSel);
        const btn = c.querySelector('button');
        btn.focus({ focusVisible: true });
        const cr = c.getBoundingClientRect();
        const br = btn.getBoundingClientRect();
        const cs = getComputedStyle(btn);
        const extent = (parseFloat(cs.outlineWidth) || 0) + Math.max(0, parseFloat(cs.outlineOffset) || 0);
        return {
          topOk: br.top - extent >= cr.top - 0.25,
          bottomOk: br.bottom + extent <= cr.bottom + 0.25,
          leftOk: br.left - extent >= cr.left - 0.25,
        };
      };
      return {
        tabs: containerRing('#fixture-tabs'),
        filters: containerRing('#fixture-filters'),
        resume: inspect('[data-kind="player-resume-btn"]'),
        dismiss: inspect('[data-kind="update-note-dismiss"]'),
        icon: inspect('[data-kind="btn-icon"]'),
        swipeDanger: inspect('[data-kind="swipe-danger"]'),
        swipeAccent: inspect('[data-kind="swipe-accent"]'),
      };
    });

    assert.equal(shapeCheck.tabs.topOk && shapeCheck.tabs.bottomOk && shapeCheck.tabs.leftOk, true, 'Fixture .tabs focus ring must fit inside container');
    assert.equal(shapeCheck.filters.topOk && shapeCheck.filters.bottomOk && shapeCheck.filters.leftOk, true, 'Fixture .filters focus ring must fit inside container');
    assert.equal(shapeCheck.resume.borderRadius, '999px', '.player-resume-btn focus ring must follow pill radius');
    assert.equal(shapeCheck.dismiss.borderRadius, '50%', '.update-note-dismiss focus ring must follow circular radius');
    assert.equal(shapeCheck.icon.borderRadius, '50%', '.btn.icon focus ring must follow circular radius');
    assert.ok(shapeCheck.swipeDanger.outlineWidth >= 2 && shapeCheck.swipeDanger.outlineOffset < 0, '.swipe-action focus ring must be a full inset >= 2px ring');
    assert.ok(shapeCheck.swipeAccent.outlineWidth >= 2 && shapeCheck.swipeAccent.outlineOffset < 0 && shapeCheck.swipeAccent.boxShadow !== 'none', '.swipe-action.accent focus ring must keep inset contrast band');

    await phoneCtx.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});
