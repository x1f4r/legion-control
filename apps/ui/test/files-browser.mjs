// Headless Chromium tests for the Files tab (full mock, old-node degradation, 5 000-entry paging,
// non-blocking transfers, media playback & seeking, history back stepping, and reference screenshots).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(TEST_DIR, '..');
const FILES_SHOTS_DIR = process.env.LEGION_FILES_SHOTS_DIR || '';

// Same strict CSP as the HTTP door plus `media-src 'self' blob: data:` for mock sample video/audio.
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

async function startStaticUiServer() {
  const server = http.createServer((req, res) => {
    const urlPath = (req.url || '/').split('?')[0];
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

async function maybeFilesShot(page, name, { alsoLight = false } = {}) {
  if (!FILES_SHOTS_DIR) return;
  fs.mkdirSync(FILES_SHOTS_DIR, { recursive: true });
  await setTheme(page, 'dark');
  await page.screenshot({ path: path.join(FILES_SHOTS_DIR, `${name}-dark.png`) });
  if (alsoLight) {
    await setTheme(page, 'light');
    await page.screenshot({ path: path.join(FILES_SHOTS_DIR, `${name}-light.png`) });
    await setTheme(page, 'dark');
  }
}

async function dispatchSwipe(page, selector, { startX, startY, endX, endY }) {
  await page
    .locator(selector)
    .first()
    .evaluate(
      (target, { sx, sy, ex, ey }) => {
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
      { sx: startX, sy: startY, ex: endX, ey: endY },
    );
  await page.waitForTimeout(140);
}

const CATALOG_TOOL_RE =
  /\b(?:power|fs|fav|sync|exec|job|svc|proj|fleet|sys|meta|device|token|config|net|env)\.[a-z_]+\b/g;

async function assertNoToolNames(page, label) {
  const text = await page.locator('body').innerText();
  const matches = [...new Set(text.match(CATALOG_TOOL_RE) || [])];
  assert.deepEqual(matches, [], `Tool name(s) leaked in plain mode (${label}): ${matches.join(', ')}`);
}

test('Files tab: history back, deep reload, viewer & video seek, selection & Send to, search, star/pin, sync, and transfers', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'],
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

    // 1. Start on Machines, open Files 4th tab, inspect Files first screen.
    await page.goto(`${server.url}/?mock#/machines`, { waitUntil: 'domcontentloaded' });
    await page.getByText('3 are on, 2 are off').waitFor();
    assert.equal(await page.locator('nav.nav a').count(), 4);
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await page.getByText('Pinned').waitFor();
    await page.getByText('Kept in sync').waitFor();
    await page.locator('.tile', { hasText: 'Photos' }).waitFor();
    await page.locator('.tile', { hasText: 'Passport.pdf' }).waitFor();
    // Machine dots in Files mean on or off only (no amber busy dot).
    assert.equal(await page.locator('.dot.busy').count(), 0, 'Files home must not show amber busy dots');
    await assertNoToolNames(page, 'files home');
    await maybeFilesShot(page, '01-files-home', { alsoLight: true });

    // 2. Walk 3 folders deep: tower (Home) -> Videos -> Holiday.
    await page.locator('a.row[href="#/files/tower/~"]').click();
    await page.locator('header.top h1', { hasText: 'Home' }).waitFor();
    await page.locator('.list a.row', { hasText: 'Videos' }).click();
    await page.locator('header.top h1', { hasText: 'Videos' }).waitFor();
    await page.locator('.list a.row', { hasText: 'Holiday' }).click();
    await page.locator('header.top h1', { hasText: 'Holiday' }).waitFor();
    // Holiday is mostly pictures and video, so it defaults to grid view with thumbnails.
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();
    await page.locator('.grid a.cell img').first().waitFor({ timeout: 5000 });
    await assertNoToolNames(page, 'files folder grid');
    await maybeFilesShot(page, '03-folder-grid', { alsoLight: true });

    // Switch Holiday to list view (remembered per folder) and capture list view.
    await page.locator('.dock button[aria-label="Show as list"]').click();
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).waitFor();
    await page.locator('.list a.row img').first().waitFor({ timeout: 5000 });
    await assertNoToolNames(page, 'files folder list');
    await maybeFilesShot(page, '02-folder-list', { alsoLight: true });

    // Sideways tab swiping must be OFF inside a folder.
    await dispatchSwipe(page, 'main.main', { startX: 310, startY: 360, endX: 90, endY: 362 });
    await page.locator('header.top h1', { hasText: 'Holiday' }).waitFor();

    // 3. Press back 3 times -> land on Files first screen; once more leaves Files (-> Machines).
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Videos' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Home' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Machines' }).waitFor();

    // 4. Reload on a deep address and verify remembered list view loads cleanly.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Videos/Holiday`, { waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'Holiday' }).waitFor();
    await page.locator('.list a.row', { hasText: 'IMG_2041.jpg' }).waitFor();
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'Holiday' }).waitFor();
    await page.locator('.list a.row', { hasText: 'IMG_2041.jpg' }).waitFor();

    // 5. Open a picture and swipe to the next picture in the current sort order.
    await page.locator('.list a.row', { hasText: 'IMG_2041.jpg' }).click();
    await page.locator('.viewer').waitFor();
    await page.locator('.viewer-top .name', { hasText: 'IMG_2041.jpg' }).waitFor();
    await page.locator('.viewer img.viewer-img').waitFor();
    await maybeFilesShot(page, '04-viewer-picture', { alsoLight: true });

    await dispatchSwipe(page, '.viewer-stage', { startX: 310, startY: 420, endX: 80, endY: 422 });
    await page.locator('.viewer-top .name', { hasText: 'IMG_2042.jpg' }).waitFor();
    // Back closes viewer and returns to Holiday.
    await page.goBack();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.locator('header.top h1', { hasText: 'Holiday' }).waitFor();

    // 7. Play the sample video and seek.
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).click();
    const videoEl = page.locator('.viewer video.viewer-video');
    await videoEl.waitFor();
    await page.waitForFunction(
      () => {
        const v = document.querySelector('.viewer video.viewer-video');
        return v && v.readyState >= 1 && Number.isFinite(v.duration) && v.duration > 1;
      },
      null,
      { timeout: 10_000 },
    );
    const seekedTime = await page.evaluate(async () => {
      const v = document.querySelector('.viewer video.viewer-video');
      await v.play().catch(() => {});
      v.currentTime = 1.5;
      await new Promise((r) => {
        const done = () => r();
        v.addEventListener('seeked', done, { once: true });
        setTimeout(done, 1200);
      });
      v.pause();
      return { currentTime: v.currentTime, duration: v.duration, err: v.error };
    });
    assert.equal(seekedTime.err, null);
    assert.ok(seekedTime.duration > 1, `Expected sample video duration > 1s, got ${seekedTime.duration}`);
    assert.ok(seekedTime.currentTime >= 1.0, `Expected video seek >= 1.0s, got ${seekedTime.currentTime}`);
    await maybeFilesShot(page, '05-viewer-video', { alsoLight: true });
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 8. Unplayable H.264 video hits the player's real error (not extension) and shows two actions.
    await page.locator('.list a.row', { hasText: 'Concert (H.264).mp4' }).click();
    await page.locator('.viewer').getByText("This phone can't play this kind of video").waitFor({ timeout: 8000 });
    await page.locator('.viewer').getByRole('button', { name: 'Open in another app' }).waitFor();
    await page.locator('.viewer').getByRole('button', { name: 'Download to this phone' }).waitFor();
    await maybeFilesShot(page, '06-viewer-unsupported', { alsoLight: true });
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 8b. PDF is treated as an "everything else" file (no inline iframe/embed/object).
    await page.locator('.list a.row', { hasText: 'Tickets.pdf' }).click();
    await page.locator('.viewer .said strong', { hasText: 'Tickets.pdf' }).waitFor();
    await page.locator('.viewer .said', { hasText: 'Document · 310 KB' }).waitFor();
    assert.equal(await page.locator('.viewer iframe, .viewer embed, .viewer object').count(), 0);
    await page.locator('.viewer').getByRole('button', { name: 'Open in another app' }).waitFor();
    await page.locator('.viewer').getByRole('button', { name: 'Download to this phone' }).waitFor();
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 9. Royalty-free Opus Ogg audio playback loads cleanly.
    await page.locator('.list a.row', { hasText: 'Voice note.ogg' }).click();
    await page.locator('.viewer audio').waitFor();
    await page.waitForFunction(
      () => {
        const a = document.querySelector('.viewer audio');
        return a && a.readyState >= 1 && Number.isFinite(a.duration) && a.duration > 0.5;
      },
      null,
      { timeout: 10_000 },
    );
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 9b. 123-char no-spaces filename fits cleanly with zero horizontal overflow in list, viewer, and Send to.
    const longNamePrefix = 'quarterly_photogrammetry_orthomosaic';
    const longRow = page.locator('.list a.row', { hasText: longNamePrefix });
    await longRow.scrollIntoViewIfNeeded();
    const listOverflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(listOverflow <= 1, `List horizontal overflow with 123-char name: ${listOverflow}px`);
    await maybeFilesShot(page, '15-long-name-list', { alsoLight: true });
    await longRow.click();
    await page.locator('.viewer').waitFor();
    const viewerBox = await page.evaluate(() => {
      const more = document.querySelector('.viewer-top button[aria-label="More options"]')?.getBoundingClientRect();
      return { overflow: document.documentElement.scrollWidth - window.innerWidth, moreRight: more?.right || 0, vw: window.innerWidth };
    });
    assert.ok(viewerBox.overflow <= 1, `Viewer overflow with long name: ${viewerBox.overflow}px`);
    assert.ok(viewerBox.moreRight <= viewerBox.vw + 1, `Viewer More button pushed off screen: ${viewerBox.moreRight} > ${viewerBox.vw}`);
    // Open Send to from viewer for the 123-char file and verify sheet stacks above viewer with zero overflow.
    await page.locator('.viewer-bar button', { hasText: 'Send to' }).click();
    await page.locator('.sheet').waitFor();
    await page.waitForFunction(() => document.activeElement?.classList?.contains('sheet'), null, { timeout: 3000 });
    const sheetOverflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(sheetOverflow <= 1, `Send to sheet overflow with long name: ${sheetOverflow}px`);
    await page.keyboard.press('Escape');
    await page.locator('.sheet').waitFor({ state: 'hidden' });
    // Escape must close only Send to and leave the viewer open behind it.
    await page.locator('.viewer').waitFor();
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // 10. Select files + folder: size hidden when a folder is selected, shown when only sized files are selected.
    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: 'Select' }).click();
    await page.locator('.list a.row', { hasText: 'Raw' }).click();
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).click();
    await page.locator('header.top h1', { hasText: '2 selected' }).waitFor();
    assert.equal(await page.locator('header.top h1 small').count(), 0, 'Total size must hide when a folder is selected');
    // Unselect Raw, select Beach.mp4 -> size appears.
    await page.locator('.list a.row', { hasText: 'Raw' }).click();
    await page.locator('.list a.row', { hasText: 'Beach.mp4' }).click();
    await page.locator('header.top h1 small', { hasText: '2 GB' }).waitFor();
    await maybeFilesShot(page, '08-folder-selecting', { alsoLight: true });

    await page.locator('.dock .bar4 button', { hasText: 'Send to' }).click();
    const sendSheet = page.locator('.sheet', { hasText: 'Send 2 items' });
    await sendSheet.getByRole('heading', { name: 'Send 2 items' }).waitFor();
    // No amber busy dot in Send to; source machine (`tower`) is last among machines with "Another folder on this machine".
    assert.equal(await sendSheet.locator('.dot.busy').count(), 0, 'Send to must not show amber busy dots');
    await sendSheet.getByText('Another folder on this machine').waitFor();
    const machineTitles = await sendSheet.locator('[role="radiogroup"] .title').allTextContents();
    assert.ok(
      machineTitles.indexOf('tower') > machineTitles.indexOf('nas'),
      `Expected source machine tower after other machines, got ${JSON.stringify(machineTitles)}`,
    );
    await sendSheet.locator('.title', { hasText: 'Films' }).waitFor();
    await maybeFilesShot(page, '07-sheet-send-to', { alsoLight: true });

    // Wake an off machine (`laptop`) right inside Send to via the shared wake sheet: shows
    // "Then: send 2 items to laptop", keeps Send to open underneath, and selects laptop once awake.
    await sendSheet.locator('.row', { hasText: 'laptop' }).getByRole('button', { name: 'Wake' }).click();
    const wakePrompt = page.locator('.sheet', { hasText: 'laptop is off.' });
    await wakePrompt.waitFor();
    await wakePrompt.getByText('Then: send 2 items to laptop').waitFor();
    await wakePrompt.getByRole('button', { name: 'Wake it and continue' }).click();
    const wakingSheet = page.locator('.sheet', { hasText: 'Waking laptop…' });
    await wakingSheet.waitFor();
    await wakingSheet.waitFor({ state: 'hidden', timeout: 8000 });
    // Send to sheet is still open; laptop transitions to a selected radio button once awake!
    const laptopRadio = sendSheet.locator('button[role="radio"]', { hasText: 'laptop' });
    await laptopRadio.waitFor({ timeout: 5000 });
    assert.equal(await laptopRadio.getAttribute('aria-checked'), 'true');

    // Send to sheet offers only Copy when sending to another machine (no Move button).
    assert.equal(
      await sendSheet.getByRole('button', { name: 'Move', exact: true }).count(),
      0,
      'Cross-machine Send to must offer only Copy when move capability is absent',
    );
    await sendSheet.locator('button[role="radio"]', { hasText: 'pi' }).click();
    assert.equal(
      await sendSheet.getByRole('button', { name: 'Move', exact: true }).count(),
      0,
      'Cross-machine Send to must offer only Copy when move capability is absent',
    );
    await sendSheet.getByRole('button', { name: 'Copy to pi' }).click();
    await sendSheet.waitFor({ state: 'hidden' });
    // Non-blocking toast confirms transfer started while user remains free to browse (no em-dash).
    const copyToast = page.locator('.toast-banner', {
      hasText: 'Copying 2 items to pi. Watch progress in Activity.',
    });
    await copyToast.waitFor();
    assert.ok(!(await copyToast.innerText()).includes('—'), 'Toast wording must not contain an em-dash');

    // 10b. Same-machine Copy uses `fs.copy` instantaneously (no background job).
    await dispatchSwipe(page, '.swipe-row:has-text("notes.txt")', { startX: 310, startY: 220, endX: 140, endY: 222 });
    await page.locator('.swipe-row:has-text("notes.txt") .swipe-action', { hasText: 'Send to' }).click();
    const localSend = page.locator('.sheet');
    await localSend.locator('button[role="radio"]', { hasText: 'tower' }).click();
    const callsBefore = await page.evaluate(() => (window.__LEGION_MOCK_CALLS__ || []).length);
    await localSend.getByRole('button', { name: 'Copy here' }).click();
    await localSend.waitFor({ state: 'hidden' });
    await page.locator('.toast-banner', { hasText: /Copied notes\.txt to/ }).waitFor();
    // The folder reloads right after the copy, so look at every call since the button was
    // pressed rather than at the last one.
    const toolsSince = await page.evaluate(
      (from) => (window.__LEGION_MOCK_CALLS__ || []).slice(from).map((call) => call.tool),
      callsBefore,
    );
    assert.ok(toolsSince.includes('fs.copy'), `Same-machine copy must call fs.copy: ${toolsSince}`);
    assert.ok(!toolsSince.includes('fs.transfer'), `Same-machine copy must not start a transfer: ${toolsSince}`);

    // 11. Star and Pin via swipe + folder More, then verify on Files first screen.
    await dispatchSwipe(page, '.swipe-row:has-text("Beach.mp4")', { startX: 310, startY: 220, endX: 140, endY: 222 });
    await page.locator('.swipe-row:has-text("Beach.mp4") .swipe-action', { hasText: 'Star' }).click();
    await page.locator('.list a.row:has-text("Beach.mp4") .star').waitFor();

    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: 'Pin to Files' }).click();
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('.tile', { hasText: 'Holiday' }).waitFor();
    const dismissToast = page.locator('.toast-banner button[aria-label="Dismiss"]');
    if (await dismissToast.count()) await dismissToast.click();

    // 12. Search and filter across all machines; off machine named.
    await page.locator('.dock input[aria-label="Search all machines"]').fill('arrival');
    await page.locator('header.top h1', { hasText: 'Search' }).waitFor();
    await page.locator('.filters button', { hasText: 'Videos' }).click();
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).first().waitFor();
    await page.locator('.list a.row', { hasText: 'Arrival (2016).mkv' }).waitFor();
    await page.getByText(/nas .* not searched/i).waitFor();
    await assertNoToolNames(page, 'files search');
    await maybeFilesShot(page, '09-search', { alsoLight: true });

    // 12b. Search with nothing found offers Clear filters when filters are active.
    await page.locator('.dock input[type="search"]').fill('zzzz_nonexistent_query');
    await page.getByText('Nothing found. Try a shorter word or fewer filters.').waitFor();
    await page.getByRole('button', { name: 'Clear filters' }).click();
    assert.equal(await page.locator('.filters button[aria-pressed="true"]').count(), 0);

    // 13. Kept in sync folder screen.
    await page.goto(`${server.url}/?mock#/files/@sync/sync-camera`, { waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'Camera' }).waitFor();
    await page.getByText('Getting 3 files · 410 MB left').waitFor();
    await page.getByText('1 file has two versions').waitFor();
    await page.getByText('6 deleted files · kept for 30 days').waitFor();
    // Only the actively syncing copy (`studio`) has an amber dot.
    assert.equal(await page.locator('.dot.busy').count(), 1);
    await assertNoToolNames(page, 'sync folder');
    await maybeFilesShot(page, '10-sync-folder', { alsoLight: true });

    // 13b. Single-picture folder has no "1 of 1" in viewer header.
    await page.goto(`${server.url}/?mock#/files/studio//home/me/Pictures/Camera`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row', { hasText: 'PXL_1003.jpg' }).click();
    await page.locator('.viewer-top .name', { hasText: 'PXL_1003.jpg' }).waitFor();
    const singlePicSub = await page.locator('.viewer-top .name small').innerText();
    assert.ok(!singlePicSub.includes('1 of 1'), `Viewer must omit "1 of 1", got: ${singlePicSub}`);
    await page.locator('.viewer-top button[aria-label="Close"]').click();

    // 13c. Empty folder & denied folder states with helpful actions.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Documents/Empty`, { waitUntil: 'domcontentloaded' });
    await page.getByText('This folder is empty.').waitFor();
    await page.getByRole('button', { name: 'New folder' }).waitFor();
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Private`, { waitUntil: 'domcontentloaded' });
    await page.getByText("You're not allowed to open this folder.").waitFor();
    await page.getByRole('button', { name: 'Back to Files' }).waitFor();

    // 14. Text viewer with 128 KiB chunk, "Showing X of Y", remembered "Wrap lines" toggle, and Load more.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
    // Broken thumbnail stays silently as the kind icon without errors.
    await page.locator('.list a.row', { hasText: 'broken-preview.jpg' }).waitFor();
    await page.locator('.list a.row', { hasText: 'long-log.txt' }).click();
    await page.locator('.viewer pre.viewer-text').waitFor();
    await page.locator('.viewer-text-bar', { hasText: /Showing \d+(\.\d+)? [KM]B of/ }).waitFor();
    const wrapBtn = page.locator('.viewer-text-bar button', { hasText: 'Wrap lines' });
    assert.equal(await wrapBtn.getAttribute('aria-pressed'), 'true');
    await maybeFilesShot(page, '16-viewer-text', { alsoLight: true });
    await wrapBtn.click();
    assert.equal(await wrapBtn.getAttribute('aria-pressed'), 'false');
    await page.locator('.viewer pre.viewer-text.nowrap').waitFor();
    const loadMoreBtn = page.locator('.viewer').getByRole('button', { name: 'Load more' });
    await loadMoreBtn.waitFor();
    const lenBefore = await page.locator('.viewer pre.viewer-text').evaluate((el) => el.textContent.length);
    await loadMoreBtn.click();
    await page.waitForFunction(
      (prevLen) => (document.querySelector('.viewer pre.viewer-text')?.textContent?.length || 0) > prevLen,
      lenBefore,
      { timeout: 5000 },
    );
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    // Reopen and verify Wrap lines preference was remembered.
    await page.locator('.list a.row', { hasText: 'README.md' }).click();
    await page.locator('.viewer pre.viewer-text.nowrap').waitFor();
    await page.locator('.viewer-text-bar button', { hasText: 'Wrap lines' }).click(); // restore default wrap
    await page.locator('.viewer-top button[aria-label="Close"]').click();

    // 15. Activity screen with transfers & non-blocking device transfer queue controls (pause/resume/open).
    await page.goto(`${server.url}/?mock=transfers#/activity`, { waitUntil: 'domcontentloaded' });
    await page.getByText('Copying Arrival.mp4 to pi').waitFor();
    await page.getByText('Downloading Beach.mp4').waitFor();
    await page.getByText('Downloaded Tickets.pdf').waitFor();
    await assertNoToolNames(page, 'activity transfers plain');
    await maybeFilesShot(page, '11-activity-transfers', { alsoLight: true });

    // Pause then Resume running download, and Resume failed download.
    const beachRow = page.locator('.row', { hasText: 'Downloading Beach.mp4' });
    await beachRow.getByRole('button', { name: 'Pause' }).click();
    await beachRow.locator('.state', { hasText: 'Paused' }).waitFor();
    await beachRow.getByRole('button', { name: 'Resume' }).click();
    await beachRow.locator('.state.running').waitFor();

    const failedRow = page.locator('.row', { hasText: 'photos-backup.zip' });
    await failedRow.getByRole('button', { name: 'Resume' }).click();
    await failedRow.locator('.state.running').waitFor();

    // Tap finished download to open it.
    await page.locator('button.row', { hasText: 'Downloaded Tickets.pdf' }).click();
    await page.locator('.viewer-top .name', { hasText: 'Tickets.pdf' }).waitFor();
    await page.locator('.viewer-top button[aria-label="Close"]').click();

    // 16. Large folder (5 000 entries, total_exact: false) paging + scroll position restored on Back.
    await page.goto(`${server.url}/?mock=large,inexact#/files/tower//home/me/Archive5000`, { waitUntil: 'domcontentloaded' });
    await page.locator('header.top h1', { hasText: 'Archive5000' }).waitFor();
    await page.getByText('Showing 120 of more than 5,000').waitFor();
    await page.getByRole('button', { name: 'Show more' }).click();
    await page.getByText(/Showing (200|240) of more than 5,000/).waitFor();
    await page.evaluate(() => {
      const m = document.querySelector('main.main');
      if (m) m.scrollTop = 900;
    });
    await page.locator('.list a.row').nth(15).click();
    await page.locator('.viewer').waitFor();
    // Close immediately (even while loading) and verify scroll position comes back.
    await page.locator('.viewer-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.waitForFunction(
      () => (document.querySelector('main.main')?.scrollTop || 0) >= 700,
      null,
      { timeout: 3000 },
    );

    // 16b. Retryable `fs.thumb` error (`busy`) retries quietly with backoff and fills thumbnail.
    await page.goto(`${server.url}/?mock=thumb-retry#/files/tower//home/me/Videos/Holiday`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row:has-text("IMG_2042.jpg") img, .grid a.cell[aria-label="IMG_2042.jpg"] img').first().waitFor({ timeout: 5000 });

    // 17. Kept in sync section hidden when sync.list does not exist (`?mock=nosync`).
    await page.goto(`${server.url}/?mock=nosync#/files`, { waitUntil: 'domcontentloaded' });
    await page.getByText('Pinned').waitFor();
    assert.equal(await page.getByText('Kept in sync').count(), 0);

    await ctx.close();

    // 18. Wide desktop layout (1100x720) screenshots and assertions.
    const wideCtx = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });
    const widePage = await wideCtx.newPage();
    widePage.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    widePage.on('pageerror', (err) => pageErrors.push(String(err)));

    await widePage.goto(`${server.url}/?mock#/files/tower//home/me/Videos/Holiday`, { waitUntil: 'domcontentloaded' });
    await widePage.locator('.split').waitFor();
    await widePage.locator('nav.nav a', { hasText: 'Files' }).waitFor();
    await widePage.locator('.grid .cell').first().waitFor();
    await widePage.locator('.grid .cell img').first().waitFor({ timeout: 5000 });
    await maybeFilesShot(widePage, '13-wide-grid');

    await widePage.locator('.files-wide-tools button[aria-label="Show as list"]').click();
    await widePage.locator('.list a.row', { hasText: 'Arrival.mp4' }).waitFor();
    await maybeFilesShot(widePage, '12-wide-files', { alsoLight: true });

    await widePage.goto(`${server.url}/?mock=transfers#/activity`, { waitUntil: 'domcontentloaded' });
    await widePage.getByText('Copying Arrival.mp4 to pi').waitFor();
    await maybeFilesShot(widePage, '14-wide-activity-transfers');

    await wideCtx.close();

    assert.deepEqual(pageErrors, [], `Uncaught page errors: ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors: ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
  }
});

test('Files tab degrades cleanly against an old node with none of the new tools (?mock=old)', async () => {
  const server = await startStaticUiServer();
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

    // 1. Start on Machines -> tap Files -> no Pinned, no Kept in sync, no Favourites.
    await page.goto(`${server.url}/?mock=old#/machines`, { waitUntil: 'domcontentloaded' });
    await page.getByText('3 are on, 2 are off').waitFor();
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await page.locator('.list a.row', { hasText: 'tower' }).waitFor();
    assert.equal(await page.getByText('Pinned').count(), 0, 'Pinned must be hidden without fav.*');
    assert.equal(await page.getByText('Kept in sync').count(), 0, 'Kept in sync must be hidden without sync.*');
    assert.equal(await page.locator('.list a.row', { hasText: 'Favourites' }).count(), 0, 'Favourites hidden without fav.*');
    await assertNoToolNames(page, 'old node files home');

    // 2. Walk 3 folders deep and Back 3 times -> Files home -> Back once more leaves Files.
    await page.locator('a.row[href="#/files/tower/~"]').click();
    await page.locator('.list a.row', { hasText: 'Videos' }).click();
    await page.locator('.list a.row', { hasText: 'Holiday' }).click();
    // Even without server categories, client extension table recognizes Holiday as media -> grid default!
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();
    await page.locator('.dock button[aria-label="Show as list"]').click();
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).waitFor();
    // Client-side extension table still classifies kind plainly ("Video", "Picture", "Text").
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).getByText(/Video/).waitFor();
    await page.locator('.list a.row', { hasText: 'IMG_2041.jpg' }).getByText(/Picture/).waitFor();
    // No star indicator when fav.* absent.
    assert.equal(await page.locator('.list .star').count(), 0);

    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Videos' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Home' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Files' }).waitFor();
    await page.goBack();
    await page.locator('header.top h1', { hasText: 'Machines' }).waitFor();

    // 3. Reload deep address on old node; open text file and picture fallback cleanly.
    await page.goto(`${server.url}/?mock=old#/files/tower//home/me/Videos/Holiday`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row', { hasText: 'notes.txt' }).click();
    await page.locator('.viewer pre.viewer-text', { hasText: 'Holiday itinerary' }).waitFor();
    // Download, Star, Open in... are hidden when link/node does not offer them.
    assert.equal(await page.locator('.viewer-bar button', { hasText: 'Download' }).count(), 0);
    assert.equal(await page.locator('.viewer-bar button', { hasText: 'Star' }).count(), 0);
    assert.equal(await page.locator('.viewer-bar button', { hasText: 'Open in' }).count(), 0);
    await page.locator('.viewer-top button[aria-label="Close"]').click();

    await page.locator('.list a.row', { hasText: 'IMG_2041.jpg' }).click();
    await page.locator('.viewer').getByText("Can't show this picture here").waitFor();
    await page.locator('.viewer-top button[aria-label="Close"]').click();

    // 4. Select two files and Send to (Copy only; "This phone" hidden without download).
    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: 'Select' }).click();
    await page.locator('.list a.row', { hasText: 'notes.txt' }).click();
    await page.locator('.list a.row', { hasText: 'Tickets.pdf' }).click();
    await page.locator('.dock .bar4 button', { hasText: 'Send to' }).click();
    const sheet = page.locator('.sheet');
    await sheet.waitFor();
    assert.equal(await sheet.getByText('This phone').count(), 0, 'This phone must hide without download');
    await sheet.getByRole('button', { name: 'Copy to pi' }).click();
    await sheet.waitFor({ state: 'hidden' });

    // 5. Search works on old node via client-filtered fs.find.
    await page.goto(`${server.url}/?mock=old#/files`, { waitUntil: 'domcontentloaded' });
    await page.locator('.dock input[aria-label="Search all machines"]').fill('arrival');
    await page.locator('.list a.row', { hasText: 'Arrival.mp4' }).waitFor();
    await assertNoToolNames(page, 'old node search');

    await ctx.close();

    assert.deepEqual(pageErrors, [], `Uncaught page errors (old node): ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors (old node): ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
  }
});

async function assertToastDoesNotCoverFirstRow(page, label) {
  const toastBox = await page.locator('.toast-banner').boundingBox();
  const rowBox = await page.locator('main .list .row').first().boundingBox();
  assert.ok(toastBox, `${label}: expected visible .toast-banner bounding box`);
  assert.ok(rowBox, `${label}: expected visible first list row bounding box`);
  assert.ok(
    toastBox.y + toastBox.height <= rowBox.y + 0.5,
    `${label}: toast (${toastBox.y}..${toastBox.y + toastBox.height}) must not overlap first row (${rowBox.y}..${rowBox.y + rowBox.height})`,
  );
}

test('Transfer toast rules: Files vs Activity, never covers rows, auto-dismisses on finish or failure', async () => {
  const server = await startStaticUiServer();
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

    // Case 1: Started from Files -> dash-free wording, never overlaps the top row,
    // and its Activity button navigates to Activity and clears the banner.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row', { hasText: 'README.md' }).waitFor();

    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: /Add from this/ }).click();

    const uploadToast = page.locator('.toast-banner');
    await uploadToast.waitFor();
    const uploadText = await uploadToast.innerText();
    assert.ok(
      uploadText.includes('Uploading to tower. Watch progress in Activity.'),
      `Unexpected upload toast text: ${uploadText}`,
    );
    assert.ok(!uploadText.includes('—'), `Upload toast must not contain an em-dash: ${uploadText}`);
    await assertToastDoesNotCoverFirstRow(page, 'narrow upload toast');

    await uploadToast.locator('a', { hasText: 'Activity' }).click();
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    assert.equal(await page.locator('.toast-banner').count(), 0, 'Toast must dismiss when opening Activity');
    await page.locator('.row', { hasText: /Upload-\d+\.jpg/ }).first().waitFor();

    // Download wording (no em-dash) + navigating via bottom nav Activity tab also clears the banner.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row', { hasText: 'README.md' }).waitFor();
    await dispatchSwipe(page, '.swipe-row:has-text("README.md")', {
      startX: 310,
      startY: 220,
      endX: 140,
      endY: 222,
    });
    await page.locator('.swipe-row:has-text("README.md") .swipe-action', { hasText: 'Send to' }).click();
    const dlSheet = page.locator('.sheet');
    await dlSheet.locator('button[role="radio"]', { hasText: 'This phone' }).click();
    await dlSheet.getByRole('button', { name: /Download to this phone/ }).click();
    await dlSheet.waitFor({ state: 'hidden' });

    const dlToast = page.locator('.toast-banner');
    await dlToast.waitFor();
    const dlText = await dlToast.innerText();
    assert.ok(
      dlText.includes('Downloading README.md. Watch progress in Activity.'),
      `Unexpected download toast text: ${dlText}`,
    );
    assert.ok(!dlText.includes('—'), `Download toast must not contain an em-dash: ${dlText}`);
    await assertToastDoesNotCoverFirstRow(page, 'narrow download toast');

    await page.locator('nav.nav a', { hasText: 'Activity' }).click();
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    assert.equal(
      await page.locator('.toast-banner').count(),
      0,
      'Transfer-started toast must dismiss when switching to Activity via nav',
    );

    // Case 2: Started while already on Activity -> no transfer-started toast is shown.
    await page.goto(`${server.url}/?mock=slow-upload#/files/tower//home/me/Documents`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'README.md' }).waitFor();
    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: /Add from this/ }).click();
    // Switch to Activity before the async upload picker finishes queuing the transfer.
    await page.locator('nav.nav a', { hasText: 'Activity' }).click();
    await page.locator('header.top h1', { hasText: 'Activity' }).waitFor();
    await page.locator('.row', { hasText: /Upload-\d+\.jpg/ }).first().waitFor({ timeout: 5000 });
    await page.waitForTimeout(200);
    assert.equal(
      await page.locator('.toast-banner').count(),
      0,
      'Transfer-started toast must not be shown when already on Activity',
    );

    // Resuming a failed transfer on Activity also shows no started toast.
    await page.goto(`${server.url}/?mock=transfers#/activity`, { waitUntil: 'domcontentloaded' });
    const failedRow = page.locator('.row', { hasText: 'photos-backup.zip' });
    await failedRow.getByRole('button', { name: 'Resume' }).click();
    await failedRow.locator('.state.running').waitFor();
    assert.equal(await page.locator('.toast-banner').count(), 0);

    // Case 3: Transfer finishes on its own -> toast goes away by itself without clicking Dismiss,
    // well before the 8s fallback timeout.
    await page.goto(`${server.url}/?mock#/files/tower//home/me/Documents`, { waitUntil: 'domcontentloaded' });
    await page.locator('.list a.row', { hasText: 'README.md' }).waitFor();
    const startedAt = Date.now();
    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: /Add from this/ }).click();
    await page.locator('.toast-banner', { hasText: 'Uploading to tower. Watch progress in Activity.' }).waitFor();
    await page.locator('.toast-banner').waitFor({ state: 'hidden', timeout: 5000 });
    const elapsedMs = Date.now() - startedAt;
    assert.ok(
      elapsedMs < 5500,
      `Expected toast to auto-dismiss as soon as transfer finished (~2.1s), took ${elapsedMs}ms`,
    );

    // Case 3b: Transfer fails -> started toast goes away and is replaced by its own failure message.
    await page.goto(`${server.url}/?mock=fail-upload#/files/tower//home/me/Documents`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'README.md' }).waitFor();
    await page.locator('.dock button[aria-label="More"]').click();
    await page.locator('.sheet button.row', { hasText: /Add from this/ }).click();
    await page.locator('.toast-banner', { hasText: 'Uploading to tower. Watch progress in Activity.' }).waitFor();
    const failToast = page.locator('.toast-banner.error', {
      hasText: /Upload of Upload-\d+\.jpg failed\. Connection dropped\./,
    });
    await failToast.waitFor({ timeout: 5000 });
    await assertToastDoesNotCoverFirstRow(page, 'narrow failure toast');

    await ctx.close();

    // Wide desktop check: toast sits in its own row above main and never overlaps the first row.
    const wideCtx = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });
    const widePage = await wideCtx.newPage();
    widePage.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    widePage.on('pageerror', (err) => pageErrors.push(String(err)));

    await widePage.goto(`${server.url}/?mock#/files/tower//home/me/Documents`, {
      waitUntil: 'domcontentloaded',
    });
    await widePage.locator('.list a.row', { hasText: 'README.md' }).waitFor();
    await widePage.locator('.files-wide-tools button[aria-label="More"]').click();
    await widePage.locator('.sheet button.row', { hasText: /Add from this/ }).click();
    await widePage
      .locator('.toast-banner', { hasText: 'Uploading to tower. Watch progress in Activity.' })
      .waitFor();
    await assertToastDoesNotCoverFirstRow(widePage, 'wide upload toast');
    await wideCtx.close();

    assert.deepEqual(pageErrors, [], `Uncaught page errors (toast rules): ${pageErrors.join('\n')}`);
    assert.deepEqual(consoleErrors, [], `Console/CSP errors (toast rules): ${consoleErrors.join('\n')}`);
  } finally {
    await browser.close();
    await server.stop();
  }
});
