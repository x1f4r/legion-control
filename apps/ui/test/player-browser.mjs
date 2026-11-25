// Browser end-to-end tests for the custom video and music players:
// - No native `controls` attribute and zero `[disabled]` controls anywhere in the player
// - Viewer element is `document.fullscreenElement` (not `<video>`) in Fullscreen API mode
// - Controlled-clock overlay auto-hide (3 s), tap toggle, double-tap seek & restore, focus hold
// - Dragging position slider commits exactly ONE seek on pointerup, with spoken aria-valuetext
// - Folder queue order, absent prev/next/fullscreen buttons when unavailable, auto-next on songs
// - Music listening screen (cover, blurred backdrop, tags, no "Unknown artist", wide split,
//   speed in More, MediaSession, covers in folder listing, large 300-song library title-before-cover)
// - Picture hidden-bars calls immersive(true/false); reduced-motion disables transitions

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';
import { SAMPLE_VIDEO_WEBM_BASE64 } from '../link/mock-media.js';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(TEST_DIR, '..');
const REPO_ROOT = path.resolve(TEST_DIR, '..', '..', '..');
const LEGION_BIN =
  process.env.LEGION_BIN ||
  path.join(REPO_ROOT, 'target', 'release', process.platform === 'win32' ? 'legion.exe' : 'legion');

const MOCK_TEST_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self' blob: data:; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const MIME_BY_EXT = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json; charset=utf-8',
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
    const parsed = new URL(req.url || '/', 'http://127.0.0.1');
    let rel = decodeURIComponent(parsed.pathname);
    if (rel === '/') rel = '/index.html';
    const full = path.resolve(UI_ROOT, `.${rel}`);
    if (!full.startsWith(UI_ROOT) || !fs.existsSync(full) || fs.statSync(full).isDirectory()) {
      res.writeHead(404);
      res.end('Not found');
      return;
    }
    const ext = path.extname(full).toLowerCase();
    const headers = {
      'Content-Type': MIME_BY_EXT[ext] || 'application/octet-stream',
      'X-Content-Type-Options': 'nosniff',
    };
    if (ext === '.html') {
      headers['Content-Security-Policy'] = MOCK_TEST_CSP;
    }
    res.writeHead(200, headers);
    fs.createReadStream(full).pipe(res);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const addr = server.address();
  return {
    url: `http://127.0.0.1:${addr.port}`,
    stop: () => new Promise((resolve) => server.close(resolve)),
  };
}

test('Player: video controls, viewer Fullscreen API, controlled-clock timing, single-seek drag, queue, keys, resume, and picture immersive', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'],
  });
  const consoleErrors = [];
  const pageErrors = [];

  try {
    const context = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });

    // Inject a deterministic test clock for player timers before page scripts run.
    await context.addInitScript(() => {
      let now = 1_000_000;
      let nextId = 1;
      const timers = new Map();
      window.__LEGION_TEST_PLAYER_CLOCK__ = {
        now: () => now,
        setTimeout(fn, ms) {
          const id = nextId++;
          timers.set(id, { at: now + Math.max(0, ms), fn });
          return id;
        },
        clearTimeout(id) {
          timers.delete(id);
        },
        advance(ms) {
          const target = now + ms;
          while (true) {
            let soonestId = null;
            let soonest = null;
            for (const [id, item] of timers.entries()) {
              if (item.at <= target && (!soonest || item.at < soonest.at)) {
                soonest = item;
                soonestId = id;
              }
            }
            if (!soonest) break;
            timers.delete(soonestId);
            now = soonest.at;
            soonest.fn();
          }
          now = target;
        },
      };
    });

    const page = await context.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    await page.goto(`${server.url}/?mock=browser-fs#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();

    // Open first video (`Arrival.mp4`).
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await page.locator('.viewer video.viewer-video').waitFor();
    await page.waitForFunction(
      () => {
        const v = document.querySelector('.viewer video.viewer-video');
        return v && v.readyState >= 1 && Number.isFinite(v.duration) && v.duration > 1;
      },
      null,
      { timeout: 10_000 },
    );

    // 1. Rules: NO native `controls` attribute, NO `[disabled]` control anywhere, every control is a named button,
    // slider has spoken aria-valuetext, every control is >= 44x44 CSS px, times use sans tabular-nums with U+2212,
    // and speed button is plain text without a border or box background.
    const rulesAudit = await page.evaluate(() => {
      const viewer = document.querySelector('.viewer');
      const videosWithControls = viewer.querySelectorAll('video[controls], audio[controls]').length;
      const disabledCount = viewer.querySelectorAll('[disabled], [aria-disabled="true"]').length;
      const buttons = Array.from(viewer.querySelectorAll('button'));
      const unnamedButtons = buttons.filter(
        (b) => !(b.getAttribute('aria-label') || b.textContent || '').trim(),
      ).length;
      const slider = viewer.querySelector('[role="slider"][aria-label="Position"]');
      const valueText = slider?.getAttribute('aria-valuetext') || '';
      const controlSizes = [...buttons, slider].filter(Boolean).map((el) => {
        const r = el.getBoundingClientRect();
        return {
          label: el.getAttribute('aria-label') || el.textContent.trim(),
          width: Math.round(r.width * 10) / 10,
          height: Math.round(r.height * 10) / 10,
        };
      });
      const speedBtn = viewer.querySelector('.player-speed-btn');
      const speedStyle = speedBtn ? getComputedStyle(speedBtn) : null;
      const timesEl = viewer.querySelector('.player-times');
      const timesStyle = timesEl ? getComputedStyle(timesEl) : null;
      const remainingText = timesEl?.children?.[1]?.textContent || '';
      const thumbBg = getComputedStyle(viewer.querySelector('.player-scrub-thumb')).backgroundColor;
      return {
        videosWithControls,
        disabledCount,
        buttonCount: buttons.length,
        unnamedButtons,
        hasSlider: Boolean(slider),
        valueText,
        controlSizes,
        speedBorderWidth: speedStyle?.borderTopWidth || '',
        speedBg: speedStyle?.backgroundColor || '',
        timesFont: timesStyle?.fontFamily || '',
        timesVariant: timesStyle?.fontVariantNumeric || '',
        timesLetterSpacing: timesStyle?.letterSpacing || '',
        remainingText,
        thumbBg,
      };
    });
    assert.equal(rulesAudit.videosWithControls, 0);
    assert.equal(rulesAudit.disabledCount, 0);
    assert.ok(rulesAudit.buttonCount >= 6);
    assert.equal(rulesAudit.unnamedButtons, 0);
    assert.equal(rulesAudit.hasSlider, true);
    assert.match(rulesAudit.valueText, /seconds?/);
    for (const c of rulesAudit.controlSizes) {
      assert.ok(c.width >= 44 && c.height >= 44, `Video control "${c.label}" too small: ${c.width}x${c.height}`);
    }
    assert.equal(rulesAudit.speedBorderWidth, '0px');
    assert.equal(rulesAudit.speedBg, 'rgba(0, 0, 0, 0)');
    assert.equal(rulesAudit.timesVariant, 'tabular-nums');
    assert.equal(rulesAudit.timesLetterSpacing, 'normal');
    assert.equal(rulesAudit.remainingText.codePointAt(0), 0x2212);
    assert.equal(rulesAudit.thumbBg, 'rgb(182, 242, 74)');

    // On first video in folder: Next is present, Previous is ABSENT (never disabled).
    assert.equal(await page.locator('.viewer button[aria-label="Next video"]').count(), 1);
    assert.equal(await page.locator('.viewer button[aria-label="Previous video"]').count(), 0);

    // 2. Full screen via Browser Fullscreen API: `.viewer` is `document.fullscreenElement`, NOT `<video>`.
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.waitForFunction(
      () => document.fullscreenElement === document.querySelector('.viewer'),
      null,
      { timeout: 5000 },
    );
    const fsTag = await page.evaluate(() => ({
      isViewer: document.fullscreenElement === document.querySelector('.viewer'),
      isVideo: document.fullscreenElement === document.querySelector('.viewer video'),
    }));
    assert.equal(fsTag.isViewer, true);
    assert.equal(fsTag.isVideo, false);

    // `F` leaves fullscreen while keeping the viewer open.
    await page.keyboard.press('f');
    await page.waitForFunction(() => document.fullscreenElement === null, null, { timeout: 5000 });
    assert.equal(await page.locator('.viewer').isVisible(), true);

    // 3. Controlled-clock timing: play -> stays visible at 2999 ms -> hides + inert + cursor-hidden at 3000 ms.
    await page.locator('.viewer button[aria-label="Play"]').click();
    await page.locator('.viewer button[aria-label="Pause"]').waitFor({ timeout: 5000 });
    await page.evaluate(async () => {
      window.__LEGION_TEST_PLAYER_CLOCK__.advance(2999);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    });
    assert.equal(
      await page.evaluate(() => document.querySelector('.player-overlay').classList.contains('hidden')),
      false,
    );
    await page.evaluate(async () => {
      window.__LEGION_TEST_PLAYER_CLOCK__.advance(1);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    });
    const hiddenState = await page.evaluate(() => ({
      hidden: document.querySelector('.player-overlay').classList.contains('hidden'),
      inert: document.querySelector('.player-overlay').inert,
      cursorHidden: document.querySelector('.player-video-wrap').classList.contains('cursor-hidden'),
    }));
    assert.equal(hiddenState.hidden, true);
    assert.equal(hiddenState.inert, true);
    assert.equal(hiddenState.cursorHidden, true);

    // Single tap toggles layer visible immediately; double tap on left third seeks -10 s and restores layer hidden.
    await page.evaluate(async () => {
      const stage = document.querySelector('.player-stage-hit');
      const r = stage.getBoundingClientRect();
      stage.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerId: 1, button: 0, clientX: r.left + r.width * 0.15, clientY: r.top + r.height * 0.5 }));
      stage.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 1, button: 0, clientX: r.left + r.width * 0.15, clientY: r.top + r.height * 0.5 }));
      await new Promise((r2) => requestAnimationFrame(() => requestAnimationFrame(r2)));
    });
    assert.equal(
      await page.evaluate(() => document.querySelector('.player-overlay').classList.contains('hidden')),
      false,
    );
    await page.evaluate(async () => {
      window.__LEGION_TEST_PLAYER_CLOCK__.advance(150);
      const stage = document.querySelector('.player-stage-hit');
      const r = stage.getBoundingClientRect();
      stage.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerId: 1, button: 0, clientX: r.left + r.width * 0.15, clientY: r.top + r.height * 0.5 }));
      stage.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 1, button: 0, clientX: r.left + r.width * 0.15, clientY: r.top + r.height * 0.5 }));
      await new Promise((r2) => requestAnimationFrame(() => requestAnimationFrame(r2)));
    });
    assert.equal(
      await page.evaluate(() => document.querySelector('.player-overlay').classList.contains('hidden')),
      true,
    );

    // `K` pauses -> overlay appears and stays visible even after 10 s.
    await page.keyboard.press('k');
    await page.locator('.viewer button[aria-label="Play"]').waitFor({ timeout: 5000 });
    await page.evaluate(async () => {
      window.__LEGION_TEST_PLAYER_CLOCK__.advance(10_000);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    });
    assert.equal(
      await page.evaluate(() => document.querySelector('.player-overlay').classList.contains('hidden')),
      false,
    );

    // 4. Dragging position bar commits EXACTLY ONE seek on release (zero writes during move).
    const dragResult = await page.evaluate(() => {
      const video = document.querySelector('.viewer video.viewer-video');
      const desc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'currentTime');
      let writes = 0;
      Object.defineProperty(video, 'currentTime', {
        configurable: true,
        get() {
          return desc.get.call(this);
        },
        set(val) {
          writes += 1;
          desc.set.call(this, val);
        },
      });
      const slider = document.querySelector('.player-scrub');
      const r = slider.getBoundingClientRect();
      const y = r.top + r.height / 2;
      slider.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerId: 2, button: 0, clientX: r.left + r.width * 0.2, clientY: y }));
      const afterDown = writes;
      for (const frac of [0.3, 0.4, 0.5, 0.6, 0.7]) {
        slider.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, pointerId: 2, button: 0, clientX: r.left + r.width * frac, clientY: y }));
      }
      const afterMove = writes;
      slider.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 2, button: 0, clientX: r.left + r.width * 0.7, clientY: y }));
      const afterUp = writes;
      delete video.currentTime;
      return { afterDown, afterMove, afterUp };
    });
    assert.equal(dragResult.afterDown, 0);
    assert.equal(dragResult.afterMove, 0);
    assert.equal(dragResult.afterUp, 1);

    // 5. Speed menu works (`1.5×`) and `M` toggles mute.
    await page.locator('.viewer button.player-speed-btn').click();
    await page.locator('.player-speed-menu button', { hasText: '1.5×' }).click();
    assert.equal(
      await page.evaluate(() => document.querySelector('.viewer video.viewer-video').playbackRate),
      1.5,
    );
    await page.keyboard.press('m');
    assert.equal(
      await page.evaluate(() => document.querySelector('.viewer video.viewer-video').muted),
      true,
    );

    // 6. `N` moves to next video (`Beach.mp4`) in folder order and revokes the previous content URL.
    const firstVideoUrl = await page.evaluate(() => document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '');
    assert.ok(firstVideoUrl.length > 0);
    await page.keyboard.press('n');
    await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
    await page.waitForFunction(
      (prev) => {
        const cur = document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '';
        return Boolean(cur && cur !== prev && window.__LEGION_MOCK_REVOKED_URLS__?.includes(prev));
      },
      firstVideoUrl,
      { timeout: 5000 },
    );
    const nextVideoUrl = await page.evaluate(() => document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '');
    assert.notEqual(nextVideoUrl, firstVideoUrl);
    assert.equal(await page.locator('.viewer button[aria-label="Previous video"]').count(), 1);
    assert.equal(await page.locator('.viewer button[aria-label="Next video"]').count(), 1);

    // `Esc` closes the viewer when not in fullscreen and revokes the active video URL.
    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.waitForFunction(
      (u) => Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(u),
      nextVideoUrl,
      { timeout: 5000 },
    );

    // 6b. Resume chip ("Resumed at ... · Start over") appears when a >= 10 min file has a saved position, and Start over resets it.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'Episode 12.mp3' }).waitFor();
    await page.evaluate(() => {
      localStorage.removeItem('legion.player.resume.v1');
      const durDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'duration');
      const curDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'currentTime');
      const fakeTimes = new WeakMap();
      window.__LEGION_RESTORE_AUDIO_PROTO__ = () => {
        if (durDesc) Object.defineProperty(HTMLAudioElement.prototype, 'duration', durDesc);
        else delete HTMLAudioElement.prototype.duration;
        if (curDesc) Object.defineProperty(HTMLAudioElement.prototype, 'currentTime', curDesc);
        else delete HTMLAudioElement.prototype.currentTime;
      };
      Object.defineProperty(HTMLAudioElement.prototype, 'duration', {
        configurable: true,
        get() {
          return 1440;
        },
      });
      Object.defineProperty(HTMLAudioElement.prototype, 'currentTime', {
        configurable: true,
        get() {
          return fakeTimes.has(this) ? fakeTimes.get(this) : 0;
        },
        set(val) {
          fakeTimes.set(this, Number(val) || 0);
        },
      });
    });
    await page.locator('.list a.row', { hasText: 'Episode 12.mp3' }).click();
    await page.locator('.music-screen').waitFor();
    await page.evaluate(() => {
      const a = document.querySelector('.viewer audio');
      a.currentTime = 125;
      a.dispatchEvent(new Event('seeked'));
    });
    await page.locator('.music-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });

    // Reopen Episode 12.mp3 -> resumes at 2:05 with "Resumed at 2:05 · Start over" chip.
    await page.locator('.list a.row', { hasText: 'Episode 12.mp3' }).click();
    await page.locator('.player-resume-chip').waitFor({ timeout: 5000 });
    const chipText = await page.locator('.player-resume-chip').innerText();
    assert.match(chipText, /Resumed at 2:05/);
    assert.match(chipText, /Start over/);
    await page.locator('.player-resume-chip button', { hasText: 'Start over' }).click();
    await page.locator('.player-resume-chip').waitFor({ state: 'hidden' });
    const afterStartOver = await page.evaluate(() => document.querySelector('.viewer audio')?.currentTime);
    assert.equal(afterStartOver, 0);
    await page.locator('.music-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.evaluate(() => {
      window.__LEGION_RESTORE_AUDIO_PROTO__?.();
      delete window.__LEGION_RESTORE_AUDIO_PROTO__;
    });

    // 7. Solo video folder (`/home/me/Videos/2025`): both Previous and Next are ABSENT.
    await page.goto(`${server.url}/?mock=no-fullscreen#/files/tower/%2Fhome%2Fme%2FVideos%2F2025`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'Arrival at the lake.mov' }).click();
    await page.locator('.viewer video.viewer-video').waitFor();
    assert.equal(await page.locator('.viewer button[aria-label="Previous video"]').count(), 0);
    assert.equal(await page.locator('.viewer button[aria-label="Next video"]').count(), 0);
    // `?mock=no-fullscreen`: Full screen button is ABSENT (never disabled).
    assert.equal(await page.locator('.viewer button[aria-label="Full screen"]').count(), 0);
    assert.equal(await page.locator('.viewer [disabled]').count(), 0);

    // 8. Picture hidden-bars calls `immersive(true)` then `immersive(false)` on close.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FPictures`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]').click();
    await page.locator('.viewer img.viewer-img').waitFor();
    await page.evaluate(() => {
      window.__LEGION_MOCK_IMMERSIVE_CALLS__.length = 0;
      const stage = document.querySelector('.viewer-stage');
      const r = stage.getBoundingClientRect();
      stage.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerId: 3, button: 0, clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 }));
      stage.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 3, button: 0, clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 }));
    });
    await page.waitForFunction(
      () =>
        document.querySelector('.viewer')?.classList.contains('chrome-hidden') &&
        window.__LEGION_MOCK_IMMERSIVE_CALLS__.some((c) => c.on === true && c.orientation === 'none'),
      null,
      { timeout: 3000 },
    );
    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    const picCalls = await page.evaluate(() => window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on));
    assert.equal(picCalls[picCalls.length - 1], false);

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

test('Player: music screen, tags, covers in listing, MediaSession, auto-advance, large library, and reduced motion', async () => {
  const server = await startStaticUiServer();
  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'],
  });
  const consoleErrors = [];
  const pageErrors = [];

  try {
    const context = await browser.newContext({
      viewport: { width: 1100, height: 720 },
      colorScheme: 'dark',
    });
    const page = await context.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    // 1. Folder listing shows covers for songs with embedded/folder art.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).waitFor();
    await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).locator('.thumb img').waitFor({
      timeout: 5000,
    });

    // 2. Open first song: cover, blurred backdrop, tags, wide cover-left layout, no disabled controls, MediaSession.
    await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).click();
    await page.locator('.music-screen').waitFor();
    await page.locator('.music-title', { hasText: 'Morning Light' }).waitFor();
    await page.locator('.music-subtitle', { hasText: 'Klara · Early Hours' }).waitFor();
    await page.locator('img.music-cover').waitFor();
    await page.locator('.music-backdrop img').waitFor();

    const musicAudit = await page.evaluate(() => {
      const viewer = document.querySelector('.viewer');
      const coverRect = document.querySelector('.music-cover-wrap').getBoundingClientRect();
      const panelRect = document.querySelector('.music-meta-panel').getBoundingClientRect();
      return {
        nativeControls: viewer.querySelectorAll('audio[controls]').length,
        disabledCount: viewer.querySelectorAll('[disabled], [aria-disabled="true"]').length,
        coverLeftOfPanel: coverRect.x + coverRect.width <= panelRect.x + 8,
        mediaSessionTitle: navigator.mediaSession?.metadata?.title || '',
        mediaSessionArtist: navigator.mediaSession?.metadata?.artist || '',
        mediaSessionAlbum: navigator.mediaSession?.metadata?.album || '',
      };
    });
    assert.equal(musicAudit.nativeControls, 0);
    assert.equal(musicAudit.disabledCount, 0);
    assert.equal(musicAudit.coverLeftOfPanel, true);
    assert.equal(musicAudit.mediaSessionTitle, 'Morning Light');
    assert.equal(musicAudit.mediaSessionArtist, 'Klara');
    assert.equal(musicAudit.mediaSessionAlbum, 'Early Hours');

    // First song at 0:00 -> Previous is ALWAYS shown on the music screen (restarts song), Next is present,
    // all controls are >= 44x44 CSS px, and Play never shifts between first, middle, and last song.
    assert.equal(await page.locator('.music-screen button[aria-label="Restart track"]').count(), 1);
    assert.equal(await page.locator('.music-screen button[aria-label="Next track"]').count(), 1);

    const firstPlayBox = await page.evaluate(() => {
      const viewer = document.querySelector('.viewer');
      const play = viewer.querySelector('.music-play-btn');
      const playRect = play.getBoundingClientRect();
      const playStyle = getComputedStyle(play);
      const controls = [...viewer.querySelectorAll('button'), viewer.querySelector('[role="slider"][aria-label="Position"]')].filter(Boolean);
      const sizes = controls.map((el) => {
        const r = el.getBoundingClientRect();
        return { label: el.getAttribute('aria-label') || el.textContent.trim(), width: r.width, height: r.height };
      });
      return { x: playRect.x, bg: playStyle.backgroundColor, color: playStyle.color, sizes };
    });
    assert.equal(firstPlayBox.bg, 'rgb(182, 242, 74)');
    assert.equal(firstPlayBox.color, 'rgb(17, 22, 10)');
    for (const c of firstPlayBox.sizes) {
      assert.ok(c.width >= 44 && c.height >= 44, `Music control "${c.label}" too small: ${c.width}x${c.height}`);
    }

    // Speed lives in More sheet.
    await page.locator('.music-top button[aria-label="More options"]').click();
    await page.locator('.sheet .music-speed-chips button', { hasText: '1.25×' }).click();
    await page.waitForFunction(
      () => document.querySelector('.viewer audio')?.playbackRate === 1.25,
      null,
      { timeout: 3000 },
    );
    await page.keyboard.press('Escape');
    await page.locator('.sheet').waitFor({ state: 'hidden' });

    // Ending a song auto-advances to the next track (`02 - Rain on Glass.mp3`), revokes the previous URL,
    // and keeps the Play button at the exact same horizontal position.
    const firstSongUrl = await page.evaluate(() => document.querySelector('.viewer audio')?.getAttribute('src') || '');
    assert.ok(firstSongUrl.length > 0);
    await page.evaluate(() => {
      document.querySelector('.viewer audio').dispatchEvent(new Event('ended'));
    });
    await page.locator('.music-title', { hasText: 'Rain on Glass' }).waitFor({ timeout: 5000 });
    const midPlayX = await page.evaluate(() => document.querySelector('.music-play-btn').getBoundingClientRect().x);
    assert.ok(Math.abs(midPlayX - firstPlayBox.x) < 0.5, `Play button shifted: ${firstPlayBox.x} -> ${midPlayX}`);
    const afterAutoNext = await page.evaluate((prev) => ({
      revokedPrev: Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(prev),
      nextUrl: document.querySelector('.viewer audio')?.getAttribute('src') || '',
    }), firstSongUrl);
    assert.equal(afterAutoNext.revokedPrev, true, 'Auto-advancing to next song must revoke previous content URL');
    assert.notEqual(afterAutoNext.nextUrl, firstSongUrl);

    // Close and open untagged song (last song in folder): title is filename without extension, no subtitle,
    // no "Unknown artist", plain bg, muted tile (NOT green), Next absent while Play stays in exact same slot!
    await page.locator('.music-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    await page.locator('.list a.row', { hasText: 'field_recording_untagged.wav' }).click();
    await page.locator('.music-title', { hasText: 'field_recording_untagged' }).waitFor();
    assert.equal(await page.locator('.music-subtitle').count(), 0);
    assert.equal(await page.locator('.music-backdrop').count(), 0);
    assert.equal(await page.locator('.music-cover-tile').count(), 1);
    assert.equal(await page.locator('.music-screen button[aria-label="Next track"]').count(), 0);
    assert.equal(await page.locator('.music-screen button[aria-label="Previous track"]').count(), 1);
    const lastTrackAudit = await page.evaluate(() => ({
      playX: document.querySelector('.music-play-btn').getBoundingClientRect().x,
      tileColor: getComputedStyle(document.querySelector('.music-cover-tile')).color,
    }));
    assert.ok(
      Math.abs(lastTrackAudit.playX - firstPlayBox.x) < 0.5,
      `Play button shifted on last track without Next: ${firstPlayBox.x} -> ${lastTrackAudit.playX}`,
    );
    assert.equal(lastTrackAudit.tileColor, 'rgb(157, 164, 173)');
    const untaggedText = await page.locator('.viewer').innerText();
    assert.equal(untaggedText.includes('Unknown artist'), false);

    // System light mode (prefers-color-scheme: light with default system theme) AND explicit light theme
    // still render bright dark-surface accent (#b6f24a fill, #11160a ink) on the music Play button and scrubber.
    await page.emulateMedia({ colorScheme: 'light' });
    const lightSystemColors = await page.evaluate(() => ({
      playBg: getComputedStyle(document.querySelector('.music-play-btn')).backgroundColor,
      playInk: getComputedStyle(document.querySelector('.music-play-btn')).color,
      thumbBg: getComputedStyle(document.querySelector('.player-scrub-thumb')).backgroundColor,
    }));
    assert.equal(lightSystemColors.playBg, 'rgb(182, 242, 74)');
    assert.equal(lightSystemColors.playInk, 'rgb(17, 22, 10)');
    assert.equal(lightSystemColors.thumbBg, 'rgb(182, 242, 74)');
    await page.evaluate(() => document.documentElement.setAttribute('data-theme', 'light'));
    const lightExplicitColors = await page.evaluate(() => ({
      playBg: getComputedStyle(document.querySelector('.music-play-btn')).backgroundColor,
      playInk: getComputedStyle(document.querySelector('.music-play-btn')).color,
    }));
    assert.equal(lightExplicitColors.playBg, 'rgb(182, 242, 74)');
    assert.equal(lightExplicitColors.playInk, 'rgb(17, 22, 10)');
    await page.evaluate(() => document.documentElement.removeAttribute('data-theme'));
    await page.emulateMedia({ colorScheme: 'dark' });

    // Portrait viewport: Play button is centered horizontally under the position bar even with Next absent.
    await page.setViewportSize({ width: 412, height: 915 });
    const portraitCenter = await page.evaluate(() => {
      const scrub = document.querySelector('.player-scrub').getBoundingClientRect();
      const play = document.querySelector('.music-play-btn').getBoundingClientRect();
      return {
        scrubCenterX: scrub.left + scrub.width / 2,
        playCenterX: play.left + play.width / 2,
      };
    });
    assert.ok(
      Math.abs(portraitCenter.scrubCenterX - portraitCenter.playCenterX) < 1,
      `Portrait Play button not centered under position bar: ${portraitCenter.playCenterX} vs ${portraitCenter.scrubCenterX}`,
    );
    await page.setViewportSize({ width: 1100, height: 720 });

    // 3. Large library (300 songs) with slow covers: title appears BEFORE cover loads.
    await page.goto(`${server.url}/?mock=big-music,slow-cover#/files/tower/%2Fhome%2Fme%2FMusic%2FLibrary300`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'Track-001.mp3' }).waitFor();
    await page.locator('.list a.row', { hasText: 'Track-001.mp3' }).click();
    await page.locator('.music-title').waitFor();
    const immediateCheck = await page.evaluate(() => ({
      title: document.querySelector('.music-title')?.textContent?.trim() || '',
      hasCoverAlready: Boolean(document.querySelector('img.music-cover')),
    }));
    assert.ok(immediateCheck.title.startsWith('Track'), `Expected title immediately, got "${immediateCheck.title}"`);
    assert.equal(immediateCheck.hasCoverAlready, false, 'Expected title to render before slow cover resolves');
    await page.locator('img.music-cover').waitFor({ timeout: 5000 });

    // 4. Reduced motion disables overlay transitions.
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await page.locator('.player-overlay').waitFor();
    const transitionDur = await page.evaluate(
      () => getComputedStyle(document.querySelector('.player-overlay')).transitionDuration,
    );
    assert.equal(transitionDur, '0s');

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

test('Player: real legion serve HTTP door enters Fullscreen API on viewer element with zero disabled controls', async (t) => {
  if (!fs.existsSync(LEGION_BIN)) {
    t.skip(`Release binary not found at ${LEGION_BIN}`);
    return;
  }
  const cacheBase = path.join(os.homedir(), '.cache', 'legion-ui-test');
  fs.mkdirSync(cacheBase, { recursive: true });
  const root = fs.mkdtempSync(path.join(cacheBase, 'player-http-'));
  const legionHome = path.join(root, 'legion');
  const userHome = path.join(root, 'home');
  const mediaDir = path.join(userHome, 'Clips');
  fs.mkdirSync(path.join(legionHome, 'config'), { recursive: true });
  fs.mkdirSync(path.join(legionHome, 'state'), { recursive: true });
  fs.mkdirSync(mediaDir, { recursive: true });
  fs.writeFileSync(path.join(mediaDir, 'clip.webm'), Buffer.from(SAMPLE_VIDEO_WEBM_BASE64, 'base64'));
  fs.writeFileSync(
    path.join(legionHome, 'config', 'node.toml'),
    'name = "tower"\nsystem = "linux"\nbusy = "none"\n',
  );
  fs.writeFileSync(
    path.join(legionHome, 'config', 'policy.toml'),
    '[roles.operator]\nallow = ["effect:read", "effect:write"]\n\n[principals.ui]\nrole = "operator"\n',
  );
  fs.writeFileSync(
    path.join(legionHome, 'config', 'fleet.toml'),
    '[machines.tower]\nssh = "tower.example"\n',
  );

  const env = { ...process.env, LEGION_HOME: legionHome, HOME: userHome, USERPROFILE: userHome };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  const tokenOut = JSON.parse(
    execFileSync(LEGION_BIN, ['token.add', 'name=ui', 'principal=ui', '--json'], { env, encoding: 'utf8' }),
  );

  const child = spawn(LEGION_BIN, ['serve', '--listen', '127.0.0.1:0'], {
    env,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`legion serve timeout:\n${stderr}`)), 15_000);
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
      reject(new Error(`legion serve exited (${code}):\n${stderr}`));
    });
  });

  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'],
  });
  const consoleErrors = [];
  const pageErrors = [];

  try {
    const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
    const page = await context.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => pageErrors.push(String(err)));

    await page.goto(`${url}/#token=${encodeURIComponent(tokenOut.token)}`, { waitUntil: 'domcontentloaded' });
    await page.locator('.nav a[href="#/files"]').waitFor({ timeout: 10_000 });
    await page.goto(`${url}/#/files/tower/${encodeURIComponent(mediaDir)}`, { waitUntil: 'domcontentloaded' });
    const clipEntry = page.locator('a.row:has-text("clip.webm"), a.cell[aria-label="clip.webm"]');
    await clipEntry.waitFor({ timeout: 10_000 });
    await clipEntry.click();
    await page.locator('.viewer video.viewer-video').waitFor({ timeout: 10_000 });

    // Real HTTP door (`http.js`): Fullscreen API targets `.viewer` (not `<video>`) and no control is disabled.
    assert.equal(await page.locator('.viewer [disabled]').count(), 0);
    assert.equal(await page.locator('.viewer video[controls]').count(), 0);
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.waitForFunction(
      () => document.fullscreenElement === document.querySelector('.viewer'),
      null,
      { timeout: 5000 },
    );
    const fsInfo = await page.evaluate(() => ({
      isViewer: document.fullscreenElement === document.querySelector('.viewer'),
      isVideo: document.fullscreenElement === document.querySelector('.viewer video'),
    }));
    assert.equal(fsInfo.isViewer, true);
    assert.equal(fsInfo.isVideo, false);

    await page.locator('.viewer button[aria-label="Exit full screen"]').click();
    await page.waitForFunction(() => document.fullscreenElement === null, null, { timeout: 5000 });
    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    child.kill('SIGTERM');
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        child.kill('SIGKILL');
        resolve();
      }, 5000);
      child.once('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
    fs.rmSync(root, { recursive: true, force: true });
  }
});
